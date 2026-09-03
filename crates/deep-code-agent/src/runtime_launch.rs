//! Shared helpers for launching a persisted agent runtime (TUI and HTTP).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::client::{DeepSeekClient, LlmClient};
use crate::config::AgentConfig;
use crate::echo_client::EchoClient;
use crate::execution_policy::SharedPermissionMode;
use crate::extensions::{attach_agent_extensions, build_runtime_system_prompt};
use crate::i18n::{Lang, SharedLang};
use crate::runtime::AgentRuntime;
use crate::session_store::{JsonSessionStore, SessionRecord, SessionStore};
use crate::shell_tools::{JobStore, shell_tool_registry_from};
use crate::subagent::SharedSubAgentManager;
use crate::tool::ToolRegistry;
use crate::workspace_policy::{WorkspacePolicy, WorkspaceRoots};
use crate::workspace_summary::build_workspace_summary;
use crate::workspace_tools::workspace_tool_registry_from;

pub const DEFAULT_SYSTEM_PROMPT: &str = "You are deep-code's coding assistant.";

/// A launched runtime plus cleanup hooks for sub-agents.
pub struct LaunchedRuntime {
    pub handle: Arc<AgentRuntime>,
    pub backend_label: String,
    pub session_id: Option<String>,
    pub subagent_manager: SharedSubAgentManager,
    pub job_store: JobStore,
    pub stop_hook: Box<dyn Fn() + Send + Sync>,
    /// True when running on the offline placeholder backend (no API key):
    /// the UI should point the user at `/apikey` instead of implying a
    /// working model. Replaces string-sniffing on `backend_label`.
    pub offline: bool,
    /// Non-fatal launch degradations (persistence unavailable, checkpoints
    /// disabled, …). The consumer must surface these — the library never
    /// writes to stderr because a raw-mode TUI may own the screen.
    pub warnings: Vec<String>,
    /// Session permission mode, shared (lock-free) with the runtime's approval
    /// gate. The TUI reads it for the status indicator and flips it on
    /// Shift+Tab; both sides see the same value.
    pub permission_mode: SharedPermissionMode,
    /// Effective extra writable roots this runtime was granted (`--add-dir`,
    /// unioned with a resumed record's own grants). Exposed so consumers can
    /// show the user the real boundary without re-deriving the union.
    pub extra_roots: Vec<PathBuf>,
}

impl LaunchedRuntime {
    pub async fn shutdown(self) {
        (self.stop_hook)();
        // Kill still-running background jobs before tearing the runtime down so
        // dev servers/watchers don't outlive the session. `kill_on_drop` is the
        // backstop for paths that drop the store without calling this.
        self.job_store.shutdown();
        self.handle.shutdown().await;
    }
}

/// Assemble the model-facing tool registry. Workspace and shell tools are L1
/// (unconditional core); web is L2 — a real capability kept built-in but
/// gated at runtime (see [`web_enabled`]).
///
/// Both filesystem-touching groups are built on ONE shared
/// [`WorkspacePolicy`], returned alongside the registry: it is the session's
/// live write boundary, and the runtime widens it in place when the user
/// approves a `request_write_root` — every registered tool and each newly
/// spawned sandboxed command sees the grant immediately, no rebuild. When the
/// boundary cannot be constructed (unresolvable root), both groups are
/// disabled and `request_write_root` is not mounted either — there is no
/// boundary to widen.
#[must_use]
pub fn build_tool_registry(
    roots: &WorkspaceRoots,
    network: crate::execution_policy::NetworkMode,
    warnings: &mut Vec<String>,
    ui_lang: &SharedLang,
) -> (ToolRegistry, JobStore, Option<WorkspacePolicy>) {
    let mut registry = ToolRegistry::new();
    // The exec policy set here is the one the runtime consults for every call,
    // and the one sub-agent registries clone — the single place config-driven
    // gating (network mode) enters.
    registry.set_policy(crate::execution_policy::ExecPolicy::default().with_network_mode(network));
    let mut job_store = JobStore::default();
    let boundary = match WorkspacePolicy::new(roots.clone()) {
        Ok(policy) => Some(policy),
        Err(error) => {
            warnings.push(format!("workspace tools disabled: {error}"));
            warnings.push(format!("shell tools disabled: {error}"));
            None
        }
    };
    if let Some(policy) = &boundary {
        registry.extend(workspace_tool_registry_from(policy.clone()));
        let (shell_tools, shell_jobs) = shell_tool_registry_from(policy.clone());
        registry.extend(shell_tools);
        job_store = shell_jobs;
        registry.register(crate::root_grant::RequestWriteRootTool);
    }
    if web_enabled() {
        registry.extend(crate::web_tools::web_tool_registry(ui_lang));
    }
    (registry, job_store, boundary)
}

/// Whether the L2 web tools (`web_search`, `fetch_url`) are mounted. On by
/// default; set `DEEP_CODE_DISABLE_WEB` to any non-empty value other than
/// `0`/`false`/`off`/`no` (case-insensitive; blank counts as unset) to gate
/// them off, e.g. for network-restricted or audit-sensitive sessions.
#[must_use]
pub fn web_enabled() -> bool {
    web_enabled_from(
        std::env::var(crate::config::DISABLE_WEB_ENV)
            .ok()
            .as_deref(),
    )
}

fn web_enabled_from(disable_flag: Option<&str>) -> bool {
    match disable_flag {
        None => true,
        // Fail-closed: web stays on only for an explicit "not disabled" value;
        // any other set value (including a typo) disables it, since this gate
        // exists for network-restricted / audit sessions.
        Some(raw) => matches!(
            raw.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "off" | "no"
        ),
    }
}

#[must_use]
pub fn runtime_system_prompt(roots: &WorkspaceRoots) -> String {
    let base = build_runtime_system_prompt(DEFAULT_SYSTEM_PROMPT, &roots.primary);
    let summary = build_workspace_summary(&roots.primary, &roots.extras);
    format!("{base}\n\n{summary}")
}

pub fn launch_runtime(
    config: &AgentConfig,
    roots: impl Into<WorkspaceRoots>,
    resume: Option<SessionRecord>,
) -> LaunchedRuntime {
    let parent_cancel = CancellationToken::new();
    let mut roots = roots.into();
    // `--add-dir $(pwd)` and friends: an extra equal to the primary grants
    // nothing (the workspace is always writable) and would list the workspace
    // as its own "additional" root in the startup banner, summary and record.
    // Enforcement already dedupes inside WorkspacePolicy; this keeps the
    // display surfaces honest. Both spellings are checked because CLI extras
    // arrive canonical while the primary may not be.
    let primary_raw = roots.primary.clone();
    let primary_canonical = crate::paths::canonicalize(&roots.primary).ok();
    roots
        .extras
        .retain(|extra| *extra != primary_raw && Some(extra) != primary_canonical.as_ref());

    if let Some(record) = resume {
        // The workspace the CALLER launched in wins over the one the record
        // claims, and the record's grants are re-vetted. Both because the
        // record is a model-writable file: it lives at
        // `<workspace>/.deep-code/sessions/<id>.json`, inside the primary
        // writable root, and `-c` selects the newest by an `updated_at_ms`
        // read out of that same file. Taking `record.workspace` on trust let a
        // written record move the primary root anywhere — `/` included — and
        // took the session store along with it.
        return launch_resumed(config, record, primary_raw, roots.extras, &parent_cancel);
    }

    let prompt = runtime_system_prompt(&roots);
    let ui_lang = SharedLang::new(Lang::from_env(&config.language));
    if config.api_key.is_some()
        && let Ok(client) = DeepSeekClient::new(config.clone())
    {
        return launch_fresh(
            client,
            format!("DeepSeek {}", config.model),
            config,
            roots,
            &prompt,
            &parent_cancel,
            ui_lang,
        );
    }

    launch_fresh(
        EchoClient::new(ui_lang.clone()),
        "offline echo (set DEEPSEEK_API_KEY for DeepSeek)".to_string(),
        config,
        roots,
        &prompt,
        &parent_cancel,
        ui_lang,
    )
}

/// Launch a new (non-resumed) runtime for any client: persist the session if
/// the store is available, fall back to an in-memory one with a warning.
fn launch_fresh<C: LlmClient + Clone + 'static>(
    client: C,
    backend_label: String,
    config: &AgentConfig,
    roots: WorkspaceRoots,
    prompt: &str,
    parent_cancel: &CancellationToken,
    ui_lang: SharedLang,
) -> LaunchedRuntime {
    let mut warnings = Vec::new();
    // Decide persistence before assembling tools: the fallback path must not
    // rebuild (and silently drop) a full extensions set, and the failure
    // reason must reach the user instead of being swallowed.
    let persisted = match prepare_persisted_session(&roots, prompt) {
        Ok(pair) => Some(pair),
        Err(reason) => {
            warnings.push(format!(
                "session persistence unavailable; this session will not be saved ({reason})"
            ));
            None
        }
    };
    assemble_launch(
        client,
        LaunchParts {
            backend_label,
            roots,
            warnings,
            ui_lang,
        },
        config,
        parent_cancel,
        |client, tools| match persisted {
            Some((store, record)) => {
                let session_id = record.id.as_str().to_string();
                (
                    AgentRuntime::from_session_record(client, tools, record, store, config.clone()),
                    Some(session_id),
                )
            }
            None => (
                AgentRuntime::with_system_prompt(client, tools, prompt, config.clone(), false),
                None,
            ),
        },
    )
}

/// What a launch has settled before the shared assembly runs: the label the UI
/// shows for the backend, the roots the tools are fenced to, the warnings
/// collected so far, and the language handle.
struct LaunchParts {
    backend_label: String,
    roots: WorkspaceRoots,
    warnings: Vec<String>,
    ui_lang: SharedLang,
}

/// The tail every launch shares once its client and parts are settled: build
/// the parent tools around the client, hand them to `make_runtime` — the one
/// step that differs (a fresh launch chooses between a persisted record and an
/// in-memory prompt; a resume always has its record) — then attach the
/// workspace helpers and package the `LaunchedRuntime`. One body, so no arm
/// can forget a `.with_ui_lang` or derive `offline` differently: the
/// fresh and resumed paths used to spell these thirty lines twice.
fn assemble_launch<C: LlmClient + Clone + 'static>(
    client: C,
    parts: LaunchParts,
    config: &AgentConfig,
    parent_cancel: &CancellationToken,
    make_runtime: impl FnOnce(C, ToolRegistry) -> (AgentRuntime, Option<String>),
) -> LaunchedRuntime {
    let LaunchParts {
        backend_label,
        roots,
        mut warnings,
        ui_lang,
    } = parts;
    let client = Arc::new(client);
    let ParentTools {
        registry: tools,
        subagent_manager,
        job_store,
        shutdown,
        boundary,
    } = build_parent_tools(
        Arc::clone(&client),
        config,
        &roots,
        parent_cancel,
        &mut warnings,
        &ui_lang,
    );
    let (runtime, session_id) = make_runtime((*client).clone(), tools);
    let permission_mode = SharedPermissionMode::new(config.default_permission_mode);
    let runtime = attach_workspace_helpers(runtime, &roots.primary, config, &mut warnings)
        .with_boundary(boundary)
        .with_permission_mode(permission_mode.clone())
        .with_ui_lang(ui_lang);
    LaunchedRuntime {
        handle: Arc::new(runtime),
        backend_label,
        session_id,
        subagent_manager,
        job_store,
        stop_hook: shutdown,
        offline: client.provider_name() == EchoClient::PROVIDER,
        warnings,
        permission_mode,
        extra_roots: roots.extras,
    }
}

/// Create the session store and save the fresh record, returning the reason
/// on failure so the caller can surface it.
fn prepare_persisted_session(
    roots: &WorkspaceRoots,
    system_prompt: &str,
) -> Result<(JsonSessionStore, SessionRecord), String> {
    let store =
        JsonSessionStore::for_workspace(&roots.primary).map_err(|error| error.to_string())?;
    let mut record = SessionRecord::new(roots.primary.clone(), system_prompt)
        .with_extra_roots(roots.extras.clone());
    store.save(&mut record).map_err(|error| error.to_string())?;
    Ok((store, record))
}

/// What `launch_resumed` has settled before it picks a client backend: the
/// vetted record, its store, the effective roots, and the warnings collected so
/// far. One value for the shared tail below instead of four loose parameters.
struct ResumedSession {
    record: SessionRecord,
    store: JsonSessionStore,
    roots: WorkspaceRoots,
    warnings: Vec<String>,
}

/// The resumed launch for either client backend: rebuild the runtime from the
/// vetted record, then the assembly every launch shares ([`assemble_launch`]).
/// The two arms of `launch_resumed` (DeepSeek and echo) differ only in the
/// client and the label — `offline` is derived from the client — so they meet
/// here, and this in turn meets `launch_fresh` in the shared tail.
fn finish_resumed_launch<C: LlmClient + Clone + 'static>(
    client: C,
    backend_label: String,
    session: ResumedSession,
    config: &AgentConfig,
    parent_cancel: &CancellationToken,
    ui_lang: SharedLang,
) -> LaunchedRuntime {
    let ResumedSession {
        record,
        store,
        roots,
        warnings,
    } = session;
    assemble_launch(
        client,
        LaunchParts {
            backend_label,
            roots,
            warnings,
            ui_lang,
        },
        config,
        parent_cancel,
        |client, tools| {
            let session_id = record.id.as_str().to_string();
            (
                AgentRuntime::from_session_record(client, tools, record, store, config.clone()),
                Some(session_id),
            )
        },
    )
}

fn launch_resumed(
    config: &AgentConfig,
    mut record: SessionRecord,
    caller_workspace: PathBuf,
    cli_extras: Vec<PathBuf>,
    parent_cancel: &CancellationToken,
) -> LaunchedRuntime {
    let mut warnings = Vec::new();
    // Do the grants in this record actually come from this host?
    //
    // The floor below can only refuse what is *categorically* off limits —
    // the filesystem root, the home directory, credential stores. It cannot
    // refuse `~/.cargo` (a `rustc-wrapper` there runs on the next build),
    // `~/Library/LaunchAgents` (runs at next login) or `~/.config/git` (alias
    // code execution), because those are ordinary directories that a human
    // might well grant on purpose. No enumeration of dangerous paths is ever
    // complete, so the grants are authenticated instead of judged: a record
    // the model wrote can still say anything, it just cannot be believed.
    //
    // Verified LAST, after the roots below have been resolved, and only for a
    // record that belongs to the workspace being resumed. Both conditions are
    // load-bearing; see `verify_recorded_grants`.
    //
    // The primary root is where the USER launched, not where the record says
    // the session lived. The record is an ordinary write target for the model
    // (it sits inside the primary root) and `-c` ranks candidates by a
    // timestamp field read out of the file itself, so `record.workspace` is
    // model-controlled input — taking it on trust moved both the write
    // boundary and the session store wherever it pointed.
    //
    // It also happens to be the right answer for the honest case: a workspace
    // that was moved or copied resumes against where it is now instead of the
    // path it was recorded at.
    // Compared canonically so a benign respelling (`/tmp` vs `/private/tmp` on
    // macOS, a trailing slash) is not reported as a redirect on every resume.
    // Anything that does not resolve to the same directory — including a
    // recorded path that no longer resolves at all — yields to the caller's.
    //
    // Kept for the tag check below: the signature covers this string, so
    // verifying has to use it and not the substituted value.
    let recorded_workspace = record.workspace.clone();
    let same_workspace = match (
        crate::paths::canonicalize(&record.workspace).ok(),
        crate::paths::canonicalize(&caller_workspace).ok(),
    ) {
        (Some(recorded), Some(caller)) => recorded == caller,
        _ => false,
    };
    if !same_workspace {
        warnings.push(format!(
            "resumed session was recorded at {}; using the current workspace {} instead",
            record.workspace.display(),
            caller_workspace.display()
        ));
        record.workspace = caller_workspace;
    }
    // Does this record's grant list come from this host AND belong to this
    // workspace? Checked over the list as recorded — before the resolution pass
    // below removes anything — so that a since-deleted sibling repo costs only
    // its own grant instead of invalidating the tag for the rest.
    //
    // `same_workspace` is a CONDITION of the tag, not merely context for it.
    // The signature covers the workspace, but it is verified against the
    // record's own `workspace` field, so a record copied verbatim into a
    // different workspace verifies happily — and the substitution just above
    // then swaps in the caller's workspace while leaving the grants in place.
    // A grant approved in one checkout could be lifted into every other
    // checkout on the host, which is exactly what putting the workspace in the
    // signed message was supposed to prevent. Requiring the two to agree is
    // what makes its presence there mean anything. The honest moved-workspace
    // case loses its grants as well: those roots were vetted relative to
    // somewhere else, the move is already reported, and narrowing is the safe
    // direction.
    let authentic = same_workspace
        && crate::session_integrity::verify_roots(
            record.id.as_str(),
            &recorded_workspace,
            &record.extra_roots,
            record.extra_roots_mac.as_deref(),
        );
    if !authentic && !record.extra_roots.is_empty() {
        warnings.push(format!(
            "dropping {} write grant(s) recorded in this session: they carry no valid \
             authorship tag for this workspace, and the session file is itself writable by \
             the model. Re-grant with --add-dir if you meant them.",
            record.extra_roots.len()
        ));
        record.extra_roots.clear();
    }
    // A recorded grant whose directory no longer resolves is dropped HERE,
    // with a warning, not handed to WorkspacePolicy. The policy constructor
    // is deliberately fail-closed, and `build_tool_registry` degrades a
    // constructor error into "that tool group is disabled" — so one stale
    // `--add-dir` entry (a sibling repo since deleted) would otherwise strip
    // a resumed session of ALL file and shell tools, with no way to remove
    // the grant. At launch a human is present and the warning is visible;
    // narrowing the session and saying so is the safe direction.
    let primary_canonical = crate::paths::canonicalize(&record.workspace).ok();
    // `retain_mut`, and the canonical value is written back: the floor below
    // vets `canonical`, but what gets ENFORCED is whatever this vector holds —
    // `WorkspacePolicy::new` canonicalizes it again on its own. Keeping the raw
    // spelling made those two disagree, which is the display-versus-grant split
    // the approval panel was hardened against, reappearing on the persistence
    // channel: a record naming `<workspace>/docs` (a symlink the model planted)
    // showed the user a path inside the repo on the startup banner and in the
    // rebuilt system prompt, while the boundary was wherever the link pointed.
    // A bare `..` was worse still — displayed verbatim, enforced against the
    // process cwd.
    record.extra_roots.retain_mut(|root| {
        let canonical = crate::paths::canonicalize(root)
            .ok()
            .filter(|path| path.is_dir());
        let Some(canonical) = canonical else {
            warnings.push(format!(
                "dropping recorded --add-dir grant {}: directory no longer resolves",
                root.display()
            ));
            return false;
        };
        // Re-vetted against the same floors a model-requested grant clears,
        // for the same reason: a record is a file the model can write, so a
        // root arriving this way has nobody vouching for it either. Without
        // this, writing `extra_roots: ["/"]` (or `~/.ssh`) into a record and
        // letting `-c` pick it up skipped every check the approval flow adds.
        if let Some(reason) = crate::workspace_policy::refuse_as_unattended_root(&canonical) {
            warnings.push(format!(
                "dropping recorded grant {}: {reason}",
                canonical.display()
            ));
            return false;
        }
        // A grant equal to the workspace is covered by the primary root;
        // dropped silently (nothing is lost) so the banner and summary never
        // list the workspace as its own "additional" root.
        if Some(&canonical) == primary_canonical.as_ref() {
            return false;
        }
        // The tag authenticates this path's SPELLING. Every path that reaches
        // the record through `serialize_record` was already canonical when it
        // was signed, so a recorded root that no longer resolves to itself is a
        // root whose meaning changed after it was approved — a symlink planted
        // over an approved directory redirects the grant without forging
        // anything, because the signature covers `<ws>/docs` while what gets
        // enforced is wherever `<ws>/docs` now points. Dropping just this root
        // rather than the whole list keeps one swapped entry from costing the
        // others, and the tag check below still covers the list as a whole.
        if &canonical != root {
            warnings.push(format!(
                "dropping recorded grant {}: it now resolves to {}, so it is no longer the \
                 directory that was approved",
                root.display(),
                canonical.display()
            ));
            return false;
        }
        *root = canonical;
        true
    });
    // Grants persist with the session: the record's extras are restored, and
    // any `--add-dir` passed on the resume command line is merged in. The
    // union is written back through the record, so the next plain `-c` keeps
    // every grant the session ever received.
    for extra in cli_extras {
        if primary_canonical.as_deref() == Some(extra.as_path()) {
            continue;
        }
        if !record.extra_roots.contains(&extra) {
            record.extra_roots.push(extra);
        }
    }
    let roots = WorkspaceRoots::new(record.workspace.clone(), record.extra_roots.clone());
    // The model reads its write boundary from the system prompt, and the
    // saved prompt names only the grants that existed when the session was
    // created — a root added on `-c --add-dir` (README's own mid-task flow)
    // would be enforceable yet invisible: the summary is what tells the model
    // absolute paths into that root are worth trying at all. Rebuild it from
    // the effective roots; a stale grant dropped above likewise stops being
    // advertised. Costs one provider prefix-cache miss on the resumed
    // conversation; an unusable grant costs the whole feature.
    record.set_system_prompt(runtime_system_prompt(&roots));
    let workspace = record.workspace.clone();
    let store = match JsonSessionStore::for_workspace(&workspace) {
        Ok(store) => store,
        Err(error) => {
            let mut launched = launch_runtime(config, roots, None);
            launched
                .warnings
                .insert(0, format!("session store unavailable: {error}"));
            launched.warnings.splice(1..1, warnings);
            return launched;
        }
    };
    // Save the merged grants and refreshed prompt NOW, not at the first
    // turn's persist(): "grants persist with the session" must hold even for
    // a resume that adds a grant and exits before any turn — the next plain
    // `-c` should still see it. A failure degrades to a warning; the
    // persistence actor retries the write on the first turn anyway.
    if let Err(error) = store.save(&mut record) {
        warnings.push(format!("failed to persist resumed session grants: {error}"));
    }

    let session = ResumedSession {
        record,
        store,
        roots,
        warnings,
    };
    let ui_lang = SharedLang::new(Lang::from_env(&config.language));
    if config.api_key.is_some()
        && let Ok(client) = DeepSeekClient::new(config.clone())
    {
        return finish_resumed_launch(
            client,
            format!("DeepSeek {} (resumed)", config.model),
            session,
            config,
            parent_cancel,
            ui_lang,
        );
    }

    finish_resumed_launch(
        EchoClient::new(ui_lang.clone()),
        "offline echo (resumed)".to_string(),
        session,
        config,
        parent_cancel,
        ui_lang,
    )
}

fn build_parent_tools<C: LlmClient + 'static>(
    client: Arc<C>,
    config: &AgentConfig,
    roots: &WorkspaceRoots,
    parent_cancel: &CancellationToken,
    warnings: &mut Vec<String>,
    ui_lang: &SharedLang,
) -> ParentTools {
    let (mut registry, job_store, boundary) =
        build_tool_registry(roots, config.sandbox_network, warnings, ui_lang);
    // Sub-agents inherit the parent's live boundary; without one (workspace
    // unresolvable — the fs tool groups are disabled too) there is nothing a
    // child could correctly work inside, so the dispatch tool stays unmounted.
    let (subagent_manager, shutdown): (SharedSubAgentManager, Box<dyn Fn() + Send + Sync>) =
        if let Some(policy) = &boundary {
            let extensions = attach_agent_extensions(
                &mut registry,
                client,
                config.clone(),
                policy.clone(),
                parent_cancel.clone(),
            );
            let shutdown: Box<dyn Fn() + Send + Sync> = Box::new({
                let extensions = Arc::clone(&extensions);
                move || extensions.cancel_all_running()
            });
            (extensions.subagent_manager(), shutdown)
        } else {
            warnings.push("sub-agent tools disabled: no workspace boundary".to_string());
            let idle = Arc::new(std::sync::RwLock::new(
                crate::subagent::SubAgentManager::new(0),
            ));
            (idle, Box::new(|| ()))
        };
    // Checked against the FINISHED registry (extensions included): standing
    // consent is an exact-name match, so an entry no registered tool answers
    // to can never fire — and stays silently "configured" forever unless
    // someone says so here, the first place config and the full tool set meet.
    warn_unmatched_auto_allow(&registry, &config.approval_auto_allow, warnings);
    ParentTools {
        registry,
        subagent_manager,
        job_store,
        shutdown,
        boundary,
    }
}

/// Warn about `approval.auto_allow` entries that pre-approve nothing, saying
/// WHICH of the three reasons applies. The registry is only what this session
/// mounted, not the tool universe, so treating every miss as "no such tool"
/// accused correct configs of being typos: run with `DEEP_CODE_DISABLE_WEB=1`
/// (a documented opt-out this repo's own release workflow sets) and a perfectly
/// good `fetch_url` entry was told it "matches no tool name — matching is
/// exact", every clause of which is false. Worse, one failed workspace boundary
/// unmounts eight tools at once and produced eight such lines on top of the
/// real "workspace tools disabled" one. (Counted, not estimated: a failed
/// boundary loses `read_file`, `list_dir`, `grep_files`, `write_file`,
/// `apply_patch`, `shell`, `job` and `request_write_root`; `fetch_url` and
/// `web_search` survive it, and `agent` is mounted by the extension pass.)
///
/// [`ExecPolicy::classify_tool`] is the tool-name universe (an exhaustive match
/// on exact names), so it separates "not mounted here" from "no such tool".
/// Entries matching a mounted, ungated tool stay silent — harmless today,
/// meaningful if the tool ever gains a gate.
fn warn_unmatched_auto_allow(
    registry: &ToolRegistry,
    auto_allow: &[String],
    warnings: &mut Vec<String>,
) {
    if auto_allow.is_empty() {
        return;
    }
    let names: Vec<String> = registry.specs().into_iter().map(|spec| spec.name).collect();
    let mut seen: Vec<&str> = Vec::new();
    let mut unmounted: Vec<&str> = Vec::new();
    for entry in auto_allow {
        // A repeated entry is one mistake, not two.
        if seen.contains(&entry.as_str()) {
            continue;
        }
        seen.push(entry.as_str());
        if entry == crate::root_grant::REQUEST_WRITE_ROOT_TOOL {
            warnings.push(format!(
                "approval.auto_allow entry \"{entry}\" has no effect: widening \
                 the write boundary always prompts"
            ));
        } else if names.iter().any(|name| name == entry) {
            // Mounted and gated (or ungated): the entry does its job.
        } else if crate::execution_policy::ExecPolicy::classify_tool(entry)
            != crate::execution_policy::ToolKind::Unknown
        {
            unmounted.push(entry);
        } else {
            warnings.push(format!(
                "approval.auto_allow entry \"{entry}\" matches no tool name — \
                 matching is exact (not a prefix), so it never pre-approves \
                 anything"
            ));
        }
    }
    // One line however many are unmounted — but it has to carry the cause
    // itself rather than defer to a warning elsewhere. A failed boundary does
    // report itself; `DEEP_CODE_DISABLE_WEB` does NOT (the state is visible
    // only under `/status`), and that is precisely the case this warning was
    // written for: `fetch_url, web_search` named with no reason anywhere.
    if !unmounted.is_empty() {
        warnings.push(format!(
            "approval.auto_allow entries name tools this session did not mount, \
             so they pre-approve nothing here: {} (a tool group can be off — \
             DEEP_CODE_DISABLE_WEB gates web_search/fetch_url — or the write \
             boundary failed to resolve, which warns separately)",
            unmounted.join(", ")
        ));
    }
}

/// Everything a launch assembles around the parent registry, in one bundle
/// (five positional returns had become unreadable at the call sites).
struct ParentTools {
    registry: ToolRegistry,
    subagent_manager: SharedSubAgentManager,
    job_store: JobStore,
    shutdown: Box<dyn Fn() + Send + Sync>,
    /// The session's live write boundary; `None` when the workspace could not
    /// be resolved (fs tool groups disabled).
    boundary: Option<WorkspacePolicy>,
}

fn attach_workspace_helpers(
    runtime: AgentRuntime,
    workspace: &Path,
    config: &AgentConfig,
    warnings: &mut Vec<String>,
) -> AgentRuntime {
    let runtime = runtime.with_checkpoints(workspace.to_path_buf(), warnings);
    // `[lsp] enabled = false` skips the manager entirely: no server spawns,
    // no post-edit polling latency.
    if config.lsp_enabled {
        runtime.with_diagnostics(workspace.to_path_buf())
    } else {
        runtime
    }
}

#[cfg(test)]
mod tests;
