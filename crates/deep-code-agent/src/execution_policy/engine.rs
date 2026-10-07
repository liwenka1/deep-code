use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::command_shape;
use super::shell_deny;
use super::shell_lex;

/// Whether every word of `needle` appears in `haystack`, in order.
fn words_in_order(needle: &[&str], haystack: &[&str]) -> bool {
    let mut rest = haystack.iter();
    needle
        .iter()
        .all(|word| rest.any(|candidate| candidate == word))
}

/// The irreversible-outward rule one argv hits, as the words that matched.
///
/// Token-level rather than command-identity, and that distinction is
/// load-bearing: identity drops flags — `session_identity("git push origin
/// main")` is `git push` — so an identity-based rule could not separate a
/// force-push from the ordinary push an agent is *supposed* to be able to run
/// once egress is granted. Denying `git push` outright would undo the capability
/// this floor exists alongside.
///
/// The set is deliberately short and made of commands whose whole purpose is the
/// irreversible act. What keeps it short is the *shape* of the test, not a
/// judgement about consequences: a rule has to be readable off the words alone
/// ("is this invocation publishing / force-pushing / applying?"), and anything
/// needing a call about whether THIS one matters ("is that namespace a scratch
/// one?") stays out — the floor refuses the whole class and the user authorizes
/// the class once. An over-eager floor strands legitimate work, which is the
/// failure mode this codebase keeps choosing against.
///
/// Known misses, accepted: a verb in a position this rule does not read is not
/// refused, and a leading option is the usual reason — `kubectl -n prod delete
/// pod api` passes where `kubectl delete -n prod pod api` is refused, and
/// `npm -s publish` / `yarn --silent publish` pass where `npm publish` is
/// refused (that arm still reads the subcommand at position 1). Chasing it means
/// modelling each program's option arity, which is a parser, not a floor; the
/// honest boundary is that this is a consent gate over the routine spellings
/// (`SECURITY.md` says so in those words, the known misses included), and
/// containment is the sandbox's job rather than this list's.
fn irreversible_outward_argv(argv: &[String]) -> Option<String> {
    let program = argv
        .first()?
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim_end_matches(".exe")
        .to_string();
    let words: Vec<&str> = argv.iter().map(String::as_str).collect();
    let args = &words[1..];
    let second = args.first().copied();

    // `--dry-run` (and aws's `--dryrun`) is the one routine spelling that makes
    // these commands harmless: it reports what would happen and does nothing.
    // Refusing it would strand the way people check their own work — and a floor
    // that blocks `npm publish --dry-run` is a floor someone turns off.
    if args
        .iter()
        .any(|word| matches!(*word, "--dry-run" | "--dryrun") || word.starts_with("--dry-run="))
    {
        return None;
    }

    match program.as_str() {
        "git" => {
            // `push` is not always the first argument: `git -C <path> push -f`,
            // `git --no-pager push`, `git -c key=value push`. Anchoring on
            // position 1 missed every one of those, and they are routine
            // spellings rather than obfuscations.
            let push_at = args.iter().position(|word| *word == "push")?;
            let rest = &args[push_at + 1..];
            let forcing = rest.iter().copied().find(|word| {
                matches!(
                    *word,
                    "--force" | "--force-with-lease" | "--mirror" | "--delete"
                ) || word.starts_with("--force-with-lease=")
                    // `+<refspec>` IS a force push, spelled without any flag.
                    || (*word != "+" && word.starts_with('+'))
                    // A short-flag cluster: `-fu` forces, `-u` does not. Long
                    // options are excluded so `--force-if-includes` — a modifier
                    // that does nothing on its own — is not read as a force.
                    || (word.starts_with('-')
                        && !word.starts_with("--")
                        && word.len() > 1
                        && word[1..].contains('f'))
            })?;
            Some(format!("git push {forcing}"))
        }
        "npm" | "yarn" | "pnpm" | "bun" if second == Some("publish") => {
            Some(format!("{program} publish"))
        }
        "gh" if matches!(
            (words.get(1), words.get(2)),
            (Some(&"pr"), Some(&"merge")) | (Some(&"release"), Some(&"create"))
        ) =>
        {
            Some(format!("gh {} {}", words[1], words[2]))
        }
        "terraform" if matches!(second, Some("apply" | "destroy")) => {
            Some(format!("terraform {}", second.unwrap_or_default()))
        }
        "kubectl" if matches!(second, Some("delete" | "drain")) => {
            Some(format!("kubectl {}", second.unwrap_or_default()))
        }
        "docker" if second == Some("push") => Some("docker push".to_string()),
        "helm" if matches!(second, Some("uninstall" | "delete")) => {
            Some(format!("helm {}", second.unwrap_or_default()))
        }
        "aws"
            if matches!(
                (words.get(1), words.get(2)),
                (Some(&"s3"), Some(&"rm")) | (Some(&"s3api"), Some(&"delete-object"))
            ) =>
        {
            Some(format!("aws {} {}", words[1], words[2]))
        }
        _ => None,
    }
}

/// Tool category used by the policy engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    ReadOnlyFile,
    WriteFile,
    Search,
    Shell,
    /// The background-job tool; risk depends on the `action` argument.
    Job,
    Mock,
    SubAgent,
    Network,
    /// `request_write_root`: the model asking to widen the write boundary.
    /// Its whole point is the human decision, so it is never auto-approvable
    /// by any mode, standing consent, or session memory (see
    /// `auto_approval_granted`), and never session-allowable.
    RootGrant,
    Unknown,
}

/// Risk level surfaced to UIs and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    #[default]
    Low,
    Medium,
    High,
}

impl RiskLevel {
    /// The localization key for this tier's label. Lives on the enum so a UI
    /// renders risk by matching the real variant instead of a `format!("{:?}")`
    /// round-trip — a new variant then fails to compile at the render site
    /// rather than silently falling through to a default colour.
    #[must_use]
    pub fn text_id(self) -> crate::i18n::TextId {
        match self {
            Self::Low => crate::i18n::TextId::RiskLow,
            Self::Medium => crate::i18n::TextId::RiskMedium,
            Self::High => crate::i18n::TextId::RiskHigh,
        }
    }

    /// The wire spelling (`low`/`medium`/`high`), the same word serde emits.
    /// For a place that needs a stable machine word — the auto-mode judge's
    /// prompt — rather than the localized label or a `Debug` rendering that a
    /// variant rename would silently change.
    #[must_use]
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Policy outcome before user approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyVerdict {
    Allow,
    Deny { reason: String },
    NeedsApproval { reason: String },
}

/// How sandboxed shell/job commands get network access (`[sandbox] network`).
///
/// Trust ("this command may run") and egress ("it may reach the network") are
/// separate grants: reads stay broad in the sandbox, so pairing them silently
/// turns any auto-allowed command into an exfiltration path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkMode {
    /// Default: commands run without network unless the call declares
    /// `network: true`, and a declaration asks the human first unless a
    /// standing consent (`auto_allow`, or a shell identity remembered by
    /// "approve for session") or `Yolo` already covers the call.
    #[default]
    Prompt,
    /// Every sandboxed command gets network without asking (the old coupled
    /// behavior, as an explicit opt-in). Only the user/global layer may set it.
    Always,
    /// Network-declaring shell/job commands, networked sub-agent dispatches and
    /// the in-process web tools (`fetch_url`/`web_search`) are refused
    /// outright, and no sandboxed command gets ambient egress, `Yolo` included.
    /// (`DEEP_CODE_DISABLE_WEB` still unmounts the web tools entirely, so the
    /// model never sees them.)
    Never,
}

impl NetworkMode {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "prompt" => Some(Self::Prompt),
            "always" => Some(Self::Always),
            "never" => Some(Self::Never),
            _ => None,
        }
    }

    /// The setting spelling, for diagnostics.
    #[must_use]
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Always => "always",
            Self::Never => "never",
        }
    }

    /// Egress-permissiveness rank: `Never` < `Prompt` < `Always`. The project
    /// config layer may only *lower* this (tighten), never raise it — comparing
    /// against the current value is what stops a repo widening a globally-set
    /// `never` up to `prompt`, not just rejecting the top rung `always`.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Never => 0,
            Self::Prompt => 1,
            Self::Always => 2,
        }
    }
}

/// Whether the OS sandbox is applied to shell/job commands at all
/// (`[sandbox] mode`).
///
/// `Os` is the default and the product's entire containment story: writes are
/// bounded to the granted roots and egress is a separate, explicit grant. `Off`
/// exists for the one case where that boundary is already provided by something
/// larger — a container, a micro-VM, a disposable CI runner — and the in-process
/// sandbox is redundant work that also breaks tools needing kernel features the
/// outer sandbox already constrains.
///
/// It is a *loosening* switch, so the project layer may only set `os` (see the
/// layered loader): a repository must never be able to turn its own confinement
/// off. And it is loud wherever it applies — `doctor`, the approval panel and a
/// standing UI marker — because everything the sandbox docs promise is
/// conditional on this being `Os`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    #[default]
    Os,
    Off,
}

impl SandboxMode {
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "os" => Some(Self::Os),
            "off" | "none" => Some(Self::Off),
            _ => None,
        }
    }

    /// The setting spelling, for diagnostics.
    #[must_use]
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Os => "os",
            Self::Off => "off",
        }
    }

    /// Strictness rank, the direction the layered loader compares in:
    /// `Off` < `Os`. A project file may move this up (to `os`), never down.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Os => 1,
        }
    }

    /// Whether the OS sandbox is applied. The one accessor callers should use,
    /// so a future third mode cannot be mistaken for one of these two.
    #[must_use]
    pub fn is_os(self) -> bool {
        matches!(self, Self::Os)
    }
}

/// The plan for a call `[sandbox] network = "never"` refuses outright.
///
/// One home for what used to be three hand-copied blocks. The copies are how
/// the rule went missing: the `SubAgent` arm grew the check, the shell path
/// grew the check, and the `Network` arm — the one tool whose entire purpose
/// is reaching a host — did not, so `never` held everywhere except there, and
/// `Yolo` waved it through. Nothing counted the siblings, so nothing noticed.
/// `never_refuses_every_egress_path` now counts them by exhaustive match over
/// [`ToolKind`], which means a new egress-capable tool cannot compile without
/// someone deciding.
///
/// `read_only` is carried per call site because it rides into telemetry; no
/// gate reads it on a denial (the registry short-circuits on
/// [`ToolExecutionPlan::denied_reason`] first).
fn network_disabled_plan(subject: &str, read_only: bool) -> ToolExecutionPlan {
    ToolExecutionPlan {
        verdict: PolicyVerdict::Deny {
            reason: format!(
                "network access is disabled by configuration ([sandbox] network = \"never\"){subject}"
            ),
        },
        requires_approval: false,
        requires_sandbox: false,
        read_only,
        risk_level: RiskLevel::Medium,
        matched_rule: Some("deny:network_disabled".to_string()),
        network: false,
    }
}

/// Whether a shell/job call declares it needs network access (`network: true`
/// in the arguments). The declaration comes from the model, but it can only
/// narrow (default is no network) or route into an approval — never grant.
#[must_use]
pub fn network_requested(arguments: &Value) -> bool {
    arguments
        .get("network")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The model's stated reason for a gated call (`justification` in the
/// arguments), for the human at the approval prompt. Advisory text only: it
/// is the model's own claim, never fed back to the auto-mode judge (a
/// classifier reading the requester's sales pitch would let a prompt
/// injection argue itself through) and never a gate input.
#[must_use]
pub fn justification_claimed(arguments: &Value) -> Option<String> {
    arguments
        .get("justification")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Full plan for executing a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolExecutionPlan {
    pub verdict: PolicyVerdict,
    pub requires_approval: bool,
    pub requires_sandbox: bool,
    pub read_only: bool,
    pub risk_level: RiskLevel,
    pub matched_rule: Option<String>,
    /// Whether the sandbox grants network when this call runs. Only set after
    /// the gate has accounted for it: a declared request under `Prompt` (the
    /// plan then requires approval), or blanket `Always`.
    #[serde(default)]
    pub network: bool,
}

impl ToolExecutionPlan {
    pub fn denied_reason(&self) -> Option<&str> {
        match &self.verdict {
            PolicyVerdict::Deny { reason } => Some(reason),
            _ => None,
        }
    }
}

/// Central execution policy (agent-side, not TUI-specific).
///
/// Shell-command gating is layered and tighten-only: the built-in structured
/// deny rules ([`shell_deny::builtin_deny`]) always run and cannot be removed
/// by configuration. A `trusted_shell_prefix` grants auto-approval, but can
/// never override a deny — deny is evaluated first.
#[derive(Debug, Clone)]
pub struct ExecPolicy {
    /// Auto-approve rules, matched by command identity (`git status` covers
    /// `git status -s` but not `git push`).
    trusted_shell_prefixes: Vec<String>,
    network_mode: NetworkMode,
    /// `[sandbox] mode = "off"`: run commands bare because the boundary is
    /// outside this process. Read by the shell layer to pick the honest tool
    /// description and by `build_tool_registry` to build an unconfined
    /// `SandboxManager`.
    sandbox_off: bool,
    /// `[sandbox] allow_irreversible`: the words that authorize an
    /// irreversible-outward command (see [`irreversible_outward_argv`]).
    allow_irreversible: Vec<String>,
}

impl Default for ExecPolicy {
    fn default() -> Self {
        // `echo`/`printf` are trusted only where they name a real program.
        //
        // A trusted command is the one class that runs as argv with no shell
        // (`RunAuthority::Parse`, set in `ToolRegistry::run_tool_call_with_plan`),
        // which is what makes "the words the gate judged are the words that
        // run" true. On Unix that costs nothing: `/bin/echo` and
        // `/usr/bin/printf` exist, so the program word resolves on PATH the way
        // the shell would have resolved it.
        //
        // On Windows both are `cmd.exe` builtins with no `.exe` anywhere on a
        // stock PATH, and `sandbox::windows::resolve_executable` refuses a
        // builtin by design rather than routing it back through `cmd /C` — the
        // interpreter this path exists to keep out. Trusting them there turned
        // every `echo hi` into a spawn failure whose message blames the OS
        // sandbox, which on Windows is a Job Object that had nothing to do with
        // it. Untrusted, they take the ordinary approval path, run as approved
        // text through `cmd /C`, and work; the cost is one prompt.
        //
        // Spelled as a `cfg`'d element rather than a conditional `push`: a
        // `let mut` that only the non-Windows branch ever mutates is an
        // `unused_mut` on Windows, which `-D warnings` makes a build failure —
        // a lint that fires on one platform only is exactly what this file
        // cannot verify locally.
        Self {
            trusted_shell_prefixes: [
                "git status",
                "git diff",
                "git log",
                "cargo test",
                "cargo build",
                "cargo check",
                #[cfg(not(windows))]
                "printf",
                #[cfg(not(windows))]
                "echo",
            ]
            .iter()
            .map(|rule| (*rule).to_string())
            .collect(),
            network_mode: NetworkMode::Prompt,
            sandbox_off: false,
            allow_irreversible: Vec::new(),
        }
    }
}

impl ExecPolicy {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The trust list's rules, for the cross-module invariant in
    /// `sandbox::tests`: a trusted identity runs as argv with no shell, so its
    /// program word must be something this host can actually `execve`. That
    /// claim spans two modules — the list lives here, the resolver lives in
    /// `sandbox` — and it went untested in exactly the gap between them.
    #[cfg(test)]
    pub(crate) fn trusted_shell_prefixes(&self) -> &[String] {
        &self.trusted_shell_prefixes
    }

    #[must_use]
    pub fn with_network_mode(mut self, mode: NetworkMode) -> Self {
        self.network_mode = mode;
        self
    }

    #[must_use]
    pub fn network_mode(&self) -> NetworkMode {
        self.network_mode
    }

    /// Attach the OS-sandbox switch (`[sandbox] mode`). Kept beside
    /// `network_mode` because both are the same kind of thing: a config-driven
    /// gate that the runtime consults per call rather than a per-call decision.
    #[must_use]
    pub fn with_sandbox_mode(mut self, mode: crate::execution_policy::SandboxMode) -> Self {
        self.sandbox_off = !mode.is_os();
        self
    }

    #[must_use]
    pub fn sandbox_off(&self) -> bool {
        self.sandbox_off
    }

    /// Attach the irreversible-outward exemptions (`[sandbox]
    /// allow_irreversible`).
    #[must_use]
    pub fn with_allow_irreversible(mut self, allow: Vec<String>) -> Self {
        self.allow_irreversible = allow;
        self
    }

    /// The irreversible-outward rule a command line hits, if any, as the words
    /// that matched — used for both the denial message and the exemption
    /// comparison, so the two can never name different things.
    ///
    /// Best-effort by construction, like the deny floor: a line the unattended
    /// parser refuses is re-read by whitespace splitting, which an obfuscation
    /// can defeat. That is acceptable *here* and nowhere else, because this
    /// floor is a "did a human mean to authorize this" gate over a short list of
    /// everyday commands, not a containment boundary — containment is the
    /// sandbox's job, and the deny floor already handles the catastrophes.
    #[must_use]
    pub fn irreversible_outward(&self, command: &str) -> Option<String> {
        if let Some(segments) = shell_lex::parse_unattended(command) {
            return segments
                .iter()
                .find_map(|segment| irreversible_outward_argv(&segment.argv));
        }
        // Segment first, then words. Splitting the whole line into words would
        // run the matcher once against an "argv" starting with whatever the line
        // began with, so `echo x && npm publish` matched nothing at all — the
        // fallback has to keep the segmentation the parser would have given it.
        command.split([';', '&', '|']).find_map(|segment| {
            let words: Vec<String> = segment
                .split_whitespace()
                .map(|word| word.to_string())
                .collect();
            irreversible_outward_argv(&words)
        })
    }

    /// Whether an `[sandbox] allow_irreversible` entry authorizes the words a
    /// rule matched.
    ///
    /// The entry's words must appear among the matched rule's words **in order**
    /// — a subsequence, not a prefix. Order is enforced so a reordering cannot
    /// stretch an entry (`"push git"` authorizes nothing); length is not, so an
    /// entry naming fewer words than the rule is the user deliberately
    /// authorizing the wider thing: `"git push"` covers a force-push.
    ///
    /// Two consequences worth stating, because neither is obvious from the
    /// spelling of an entry:
    ///
    /// - A match is over words, not programs, so a one-word entry crosses
    ///   programs: `"push"` authorizes `docker push` as well as
    ///   `git push --force`. That is the user's choice to write, and the example
    ///   config says so.
    /// - Words need not be adjacent, so `"git --mirror"` authorizes
    ///   `git push --mirror` — and also `git --mirror push`, which is not a real
    ///   command. Over-covering a spelling nobody can type costs nothing;
    ///   under-covering a real one would strand it.
    #[must_use]
    pub fn irreversible_allowed(&self, matched: &str) -> bool {
        let wanted: Vec<&str> = matched.split_whitespace().collect();
        self.allow_irreversible.iter().any(|entry| {
            let entry: Vec<&str> = entry.split_whitespace().collect();
            !entry.is_empty() && words_in_order(&entry, &wanted)
        })
    }

    /// The same policy trusting one more identity.
    ///
    /// Production use is `allow_commands`: the identities a dispatch declared,
    /// added to the CHILD's policy alone (see `child_tool_registry`). The global
    /// trust list stays hardcoded — this widens one child's reach by exactly what
    /// a human approved at dispatch, and never the running session's own.
    ///
    /// `pub(crate)`, not `pub`: the only production caller is the child registry
    /// builder, and this widens a session-wide GATE. Exposing it would invite
    /// reuse as a general capability, on a signature that does not say it only
    /// means anything for the shell trust table.
    #[must_use]
    pub(crate) fn with_trusted_prefix(mut self, rule: &str) -> Self {
        self.trusted_shell_prefixes.push(rule.to_string());
        self
    }

    pub fn classify_tool(tool_name: &str) -> ToolKind {
        match tool_name {
            "read_file" | "list_dir" => ToolKind::ReadOnlyFile,
            "grep_files" => ToolKind::Search,
            "write_file" | "apply_patch" => ToolKind::WriteFile,
            "shell" => ToolKind::Shell,
            "job" => ToolKind::Job,
            "web_search" | "fetch_url" => ToolKind::Network,
            "mock_echo" => ToolKind::Mock,
            "agent" => ToolKind::SubAgent,
            "request_write_root" => ToolKind::RootGrant,
            _ => ToolKind::Unknown,
        }
    }

    pub fn evaluate_tool(&self, tool_name: &str, arguments: &Value) -> ToolExecutionPlan {
        let kind = Self::classify_tool(tool_name);
        match kind {
            ToolKind::ReadOnlyFile | ToolKind::Search => ToolExecutionPlan {
                verdict: PolicyVerdict::Allow,
                requires_approval: false,
                requires_sandbox: false,
                read_only: true,
                risk_level: RiskLevel::Low,
                matched_rule: Some("builtin:read_only_tool".to_string()),
                network: false,
            },
            ToolKind::WriteFile => ToolExecutionPlan {
                verdict: PolicyVerdict::NeedsApproval {
                    reason: "write tools can modify workspace files".to_string(),
                },
                requires_approval: true,
                requires_sandbox: false,
                read_only: false,
                risk_level: RiskLevel::Medium,
                matched_rule: Some("builtin:write_tool".to_string()),
                network: false,
            },
            ToolKind::Network => {
                // `[sandbox] network = "never"` means this agent opens no
                // connection but the model's. The web tools are egress by
                // definition, so they are refused under it exactly as a
                // network-declaring command or a networked dispatch is —
                // otherwise `never` held for every sandboxed command and not
                // for the one tool whose sole purpose is reaching a host, and
                // Yolo waved that tool through with no human in the loop.
                if self.network_mode == NetworkMode::Never {
                    return network_disabled_plan(", so the web tools cannot run", true);
                }
                ToolExecutionPlan {
                    verdict: PolicyVerdict::NeedsApproval {
                        reason: "network tools can send data to external hosts".to_string(),
                    },
                    requires_approval: true,
                    requires_sandbox: false,
                    read_only: true,
                    risk_level: RiskLevel::Medium,
                    matched_rule: Some("builtin:network_tool".to_string()),
                    network: false,
                }
            }
            ToolKind::Job => match arguments.get("action").and_then(Value::as_str) {
                // Launching a background command is exactly as risky as the
                // command itself: same deny/trust/approve gate as `shell`.
                Some("start") => self.evaluate_command_bearing(tool_name, arguments),
                Some("status" | "tail") => ToolExecutionPlan {
                    verdict: PolicyVerdict::Allow,
                    requires_approval: false,
                    requires_sandbox: false,
                    read_only: true,
                    risk_level: RiskLevel::Low,
                    matched_rule: Some("builtin:job_control".to_string()),
                    network: false,
                },
                Some("cancel") => ToolExecutionPlan {
                    verdict: PolicyVerdict::NeedsApproval {
                        reason: "cancelling a job kills its process".to_string(),
                    },
                    requires_approval: true,
                    requires_sandbox: false,
                    read_only: false,
                    risk_level: RiskLevel::Low,
                    matched_rule: Some("builtin:job_control".to_string()),
                    network: false,
                },
                // Missing/unknown action: gate defensively; the tool then
                // rejects it as InvalidArguments.
                _ => ToolExecutionPlan {
                    verdict: PolicyVerdict::NeedsApproval {
                        reason: "unknown job action".to_string(),
                    },
                    requires_approval: true,
                    requires_sandbox: false,
                    read_only: false,
                    risk_level: RiskLevel::High,
                    matched_rule: None,
                    network: false,
                },
            },
            ToolKind::Shell => self.evaluate_command_bearing(tool_name, arguments),
            ToolKind::Mock => ToolExecutionPlan {
                verdict: PolicyVerdict::NeedsApproval {
                    reason: "mock tool requires approval for tool-loop tests".to_string(),
                },
                requires_approval: true,
                requires_sandbox: false,
                read_only: true,
                risk_level: RiskLevel::Low,
                matched_rule: Some("builtin:mock_tool".to_string()),
                network: false,
            },
            // Dispatching a *writing* child is itself the write authorization:
            // `subagent_approval_decision` auto-approves the child's workspace
            // writes on the strength of the dispatch. That authorization must
            // therefore come from the human on the tiers where writes prompt —
            // otherwise spawning an implementer silently downgraded Default's
            // "approve every write" to "approve nothing". Read-only roles keep
            // spawning without a prompt, and `accept_edits_approvable` waves the
            // writing role through on AcceptEdits and above, so the prompt
            // appears exactly where a plain `write_file` would have.
            //
            // A `network: true` dispatch is the network authorization the same
            // way: a child runs unattended, so egress consent cannot be
            // collected at its own prompts (they are auto-denied) — it is
            // collected here, where a human still sees the request. An
            // approved networked child gets the web tools and ambient egress
            // for its allow-listed commands (see `child_tool_registry`); the
            // trusted-command wall itself does not widen.
            ToolKind::SubAgent => {
                let writes = subagent_role_writes(arguments);
                let network = network_requested(arguments);
                // Commands the dispatcher asks the child to be able to run
                // unattended. This is `allow_commands`' whole reason to be an
                // approval point: a child's own prompts are auto-denied (nobody
                // is watching them), so a command outside the built-in trust list
                // is otherwise unreachable for a child in EVERY mode — the gap
                // `an_inherited_mode_does_not_lift_the_childs_shell_wall` pins.
                // Declaring it here moves the decision to where a human still is.
                let allow_commands = subagent_allow_commands(arguments);
                // `[sandbox] network = "never"`: a networked dispatch is
                // refused outright, same as a network-declaring shell command
                // — the child would only burn a doomed attempt offline.
                if network && self.network_mode == NetworkMode::Never {
                    return network_disabled_plan(", so a networked sub-agent cannot run", false);
                }
                // Under `always`, egress is already ambient for every sandboxed
                // command by explicit config — a networked dispatch adds no
                // consent question. The write authorization still does.
                let network_gated = network && self.network_mode == NetworkMode::Prompt;
                if writes || network_gated || !allow_commands.is_empty() {
                    let mut reason = match (writes, network_gated) {
                        (true, true) => {
                            "dispatching a writing sub-agent with network access authorizes \
                             its workspace writes and its egress — anything it reads may be \
                             sent to external hosts."
                        }
                        (false, true) => {
                            "dispatching a networked sub-agent authorizes its egress — \
                             anything it reads may be sent to external hosts."
                        }
                        (true, false) => {
                            "dispatching a writing sub-agent authorizes its workspace writes."
                        }
                        // Only `allow_commands`: the role may be read-only, so
                        // the sentence must not claim a write authorization — and
                        // it still has to be a sentence, because the clause below
                        // is appended to it.
                        (false, false) => "dispatching a sub-agent.",
                    }
                    .to_string();
                    if !allow_commands.is_empty() {
                        // Named in full rather than counted: the human is deciding
                        // that this child may run THESE commands without anyone
                        // watching, and "3 commands" is not a decision anyone can
                        // make. Capped so a long list cannot push the rest of the
                        // panel off the screen — the count says the rest exists.
                        let shown: Vec<&str> = allow_commands
                            .iter()
                            .take(MAX_NAMED_ALLOW_COMMANDS)
                            .map(String::as_str)
                            .collect();
                        let extra = allow_commands.len().saturating_sub(shown.len());
                        // APPENDED, not substituted: a dispatch can be all three
                        // things at once, and the sentences above exist because
                        // each authorization is a different grant a human is being
                        // asked about. Replacing them with the generic one hid
                        // "authorizes its workspace writes" and "anything it reads
                        // may be sent to external hosts" behind "those commands
                        // may be read, written or sent".
                        //
                        // Joined as a second sentence, so the base has to end in a
                        // full stop — without one the panel read "…its workspace
                        // writes It also authorizes it to run git push …".
                        reason.push_str(&format!(
                            " It also authorizes it to run {} without anyone watching its \
                             prompts{} — anything those commands reach may be read, written \
                             or sent",
                            shown.join(", "),
                            if extra > 0 {
                                format!(" (and {extra} more)")
                            } else {
                                String::new()
                            }
                        ));
                    }
                    ToolExecutionPlan {
                        verdict: PolicyVerdict::NeedsApproval {
                            reason: reason.to_string(),
                        },
                        requires_approval: true,
                        requires_sandbox: false,
                        read_only: !writes,
                        risk_level: RiskLevel::Medium,
                        matched_rule: Some(
                            if !allow_commands.is_empty() {
                                "builtin:subagent_allow_commands"
                            } else if network_gated {
                                "builtin:subagent_network_dispatch"
                            } else {
                                "builtin:subagent_writing_role"
                            }
                            .to_string(),
                        ),
                        network: false,
                    }
                } else {
                    ToolExecutionPlan {
                        verdict: PolicyVerdict::Allow,
                        requires_approval: false,
                        requires_sandbox: false,
                        read_only: true,
                        risk_level: RiskLevel::Low,
                        matched_rule: Some("builtin:subagent_tool".to_string()),
                        network: false,
                    }
                }
            }
            // Widening the write boundary is the highest-consequence request a
            // model can make: everything the sandbox and the path fence deny
            // today becomes allowed under the new root. Always a prompt, top
            // risk tier — and the approval gate hard-excludes it from every
            // auto-approval channel on top of this plan.
            ToolKind::RootGrant => ToolExecutionPlan {
                verdict: PolicyVerdict::NeedsApproval {
                    reason: "grants write access to a directory outside the current roots, \
                             for the rest of the session"
                        .to_string(),
                },
                requires_approval: true,
                requires_sandbox: false,
                read_only: false,
                risk_level: RiskLevel::High,
                matched_rule: Some("builtin:root_grant".to_string()),
                network: false,
            },
            ToolKind::Unknown => ToolExecutionPlan {
                verdict: PolicyVerdict::NeedsApproval {
                    reason: format!("unknown tool '{tool_name}' requires approval"),
                },
                requires_approval: true,
                requires_sandbox: false,
                read_only: false,
                risk_level: RiskLevel::High,
                matched_rule: None,
                network: false,
            },
        }
    }

    /// The shared tail of the `shell` and `job action=start` arms: the command
    /// the call carries, read through [`shell_command_of`] (the one extraction
    /// rule), through the shell gate. No `command` key gates as the empty
    /// command.
    fn evaluate_command_bearing(&self, tool_name: &str, arguments: &Value) -> ToolExecutionPlan {
        let command = shell_command_of(tool_name, arguments).unwrap_or("");
        evaluate_shell_command(self, command, network_requested(arguments))
    }
}

pub fn evaluate_shell_command(
    policy: &ExecPolicy,
    command: &str,
    network_requested: bool,
) -> ToolExecutionPlan {
    // 1. Built-in structured deny (basename + flag aware, segment-split).
    //    Always runs; cannot be disabled by configuration.
    if let Some(reason) = shell_deny::builtin_deny(command) {
        return ToolExecutionPlan {
            verdict: PolicyVerdict::Deny {
                // The remedy rides on the MESSAGE only. `matched_rule` below
                // stays the bare rule id: it is logged and matched on, not read
                // as prose, and a sentence of advice in it would end up in
                // every log line and comparison.
                reason: match reason.remedy {
                    Some(remedy) => {
                        format!("shell command denied: {} — {remedy}", reason.rule)
                    }
                    None => format!("shell command denied: {}", reason.rule),
                },
            },
            requires_approval: false,
            requires_sandbox: false,
            read_only: false,
            risk_level: RiskLevel::High,
            matched_rule: Some(format!("deny:{}", reason.rule)),
            network: false,
        };
    }

    // 2. `[sandbox] network = "never"`: a network-declaring command is refused
    //    outright — running it offline anyway would just burn a doomed attempt.
    if network_requested && policy.network_mode == NetworkMode::Never {
        return network_disabled_plan("", false);
    }

    // The grant the sandbox applies once this call actually runs. Under
    // `Prompt` a declaration reaches execution only through the approval
    // forced below (or a standing consent the user granted earlier).
    let network = match policy.network_mode {
        NetworkMode::Always => true,
        NetworkMode::Prompt => network_requested,
        NetworkMode::Never => false,
    };

    // The irreversible-outward floor.
    //
    // Not local catastrophes (that is `shell_deny`, which cannot be lifted) and
    // not ordinary egress (a human approves that per call): these are single
    // commands that reach the world and cannot be undone by the next command —
    // publishing a package, force-pushing over someone's commits, merging a PR,
    // applying infrastructure, deleting a bucket.
    //
    // A `NeedsApproval` marked `needs-human`, which is the root grant's shape and
    // not a denial: the property that makes this a floor is that NO automatic
    // path may approve it (see `is_needs_human_rule` and its readers) — not that
    // a person is forbidden to. Asked rather than refused, because refusing also
    // refused the interactive case, where a human is right there: under `default`
    // a one-off force push had no way to be authorized short of editing global
    // config and restarting, which is a worse answer than a question.
    //
    // What is unchanged: yolo cannot auto-approve it (the reader below refuses
    // before the mode is consulted), a child gets a denial with a note naming the
    // floor, and an unattended run auto-denies. `[sandbox] allow_irreversible`
    // remains the "stop asking" path for a class of command the user has decided
    // about once.
    if let Some(matched) = policy
        .irreversible_outward(command)
        .filter(|matched| !policy.irreversible_allowed(matched))
    {
        return ToolExecutionPlan {
            verdict: PolicyVerdict::NeedsApproval {
                reason: format!(
                    "'{matched}' is irreversible and outward-facing: it reaches the world and \
                     the next command cannot undo it. Approve it for this run, or add it to \
                     [sandbox] allow_irreversible in the global config to authorize that class \
                     of command for good."
                ),
            },
            requires_approval: true,
            // It RUNS once approved, so it is sandboxed like any other shell
            // command: the Deny version could leave this false because a denied
            // plan never executes, and a plan that runs unconfined because of it
            // would be the worst bug in this file.
            requires_sandbox: true,
            read_only: false,
            risk_level: RiskLevel::High,
            matched_rule: Some(format!("{NEEDS_HUMAN_RULE_PREFIX}irreversible:{matched}")),
            // The egress it was granted, exactly as the ordinary path computes
            // it: an approved force push that then failed on a connection error
            // would make the approval look broken.
            network,
        };
    }

    let segments = shell_lex::segments(command);
    // Auto-trust only if EVERY segment is covered by a trusted rule
    // (identity-matched, so flags vary but subcommands don't) and the whole
    // line is one the executor can run WITHOUT a shell
    // (`shell_lex::parse_unattended`): plain words, quotes, `&&`/`;`
    // sequencing — no redirection, substitution, expansion, pipe or
    // background `&`. Those run programs, write paths or expand content a
    // trusted prefix never covered; and a trusted command is executed as the
    // argv that parse produced, so what the rules judged is what runs.
    let trusted = shell_lex::parse_unattended(command).is_some()
        && segments.iter().all(|segment| {
            policy
                .trusted_shell_prefixes
                .iter()
                .any(|prefix| command_shape::rule_covers(prefix, segment))
        });

    // 3. A network declaration under `Prompt` always asks, trusted or not:
    //    egress (or binding a port) is a capability the trust list never
    //    granted. "Approve for session" then remembers the command identity,
    //    so `git push` stops prompting after the first consent.
    if network_requested && policy.network_mode == NetworkMode::Prompt {
        return ToolExecutionPlan {
            verdict: PolicyVerdict::NeedsApproval {
                reason: "the command declares it needs network access (egress or listening)"
                    .to_string(),
            },
            requires_approval: true,
            requires_sandbox: true,
            read_only: false,
            risk_level: if trusted {
                RiskLevel::Medium
            } else {
                RiskLevel::High
            },
            matched_rule: Some("gate:network".to_string()),
            network,
        };
    }

    // 4. Trusted commands run without asking.
    if trusted {
        return ToolExecutionPlan {
            verdict: PolicyVerdict::Allow,
            requires_approval: false,
            requires_sandbox: true,
            read_only: false,
            risk_level: RiskLevel::Low,
            matched_rule: Some("trust:all_segments".to_string()),
            network,
        };
    }

    // 5. Anything else needs explicit user approval.
    ToolExecutionPlan {
        verdict: PolicyVerdict::NeedsApproval {
            reason: "shell commands can modify workspace files or run arbitrary code".to_string(),
        },
        requires_approval: true,
        requires_sandbox: true,
        read_only: false,
        risk_level: RiskLevel::High,
        matched_rule: Some("builtin:shell_default".to_string()),
        network,
    }
}

/// The shell command a tool call would run, if it is command-bearing: the
/// `command` argument for the `shell` tool, or a `job` with `action=start`.
/// `None` for every other tool (and for job status/tail/cancel). The one home
/// for the "where does the command live" rule: the gate itself
/// (`ExecPolicy::evaluate_tool`), accept-edits, the safety notes, and session
/// trust all read the command through here (see `ToolCall::shell_command`,
/// which delegates here). Plain code spans, not links: this function is
/// re-exported for the TUI's approval panel, and both of those are
/// crate-private, so a link would dangle in the public docs.
#[must_use]
pub fn shell_command_of<'a>(tool_name: &str, arguments: &'a Value) -> Option<&'a str> {
    let command_bearing = match ExecPolicy::classify_tool(tool_name) {
        ToolKind::Shell => true,
        ToolKind::Job => arguments.get("action").and_then(Value::as_str) == Some("start"),
        _ => false,
    };
    command_bearing
        .then(|| arguments.get("command").and_then(Value::as_str))
        .flatten()
}

/// Whether a gated call is auto-approvable under `AcceptEdits` mode: a workspace
/// file-edit tool, the dispatch of a writing sub-agent, or a filesystem-shaped
/// shell/job command ([`shell_deny::is_workspace_fs_edit`]: a bare program name
/// first on the line, operands spelled relative and in-tree — the sandbox
/// bounds the writes; cc's `acceptEdits` behavior). Everything else still
/// prompts. Hard denials never reach this — they short-circuit in the registry
/// before any decision.
#[must_use]
pub fn accept_edits_approvable(tool_name: &str, arguments: &Value) -> bool {
    // A network declaration is never covered by accept-edits: that mode's
    // standing consent is "edit files in the workspace", not "open egress".
    if network_requested(arguments) {
        return false;
    }
    match ExecPolicy::classify_tool(tool_name) {
        ToolKind::WriteFile => true,
        // Spawning a writing child is standing consent to its workspace writes,
        // which is exactly what AcceptEdits already grants per-write. Only the
        // writing role ever reaches this (read-only spawns don't prompt).
        ToolKind::SubAgent => subagent_role_writes(arguments),
        // Shell / job(start): an in-workspace filesystem-mutation command.
        // `shell_command_of` returns None for job status/tail/cancel, so the
        // bare `Job` arm needs no separate action guard.
        ToolKind::Shell | ToolKind::Job => {
            shell_command_of(tool_name, arguments).is_some_and(shell_deny::is_workspace_fs_edit)
        }
        _ => false,
    }
}

/// Whether an `agent` call's `role` argument names a role whose child may write
/// (see [`crate::subagent::SubAgentRole::allows_writes`]). Absent role means
/// `general` (read-only); an unparsable role fails closed to "writes" — the
/// tool itself will reject it, but if that ever drifts, prompt rather than pass.
/// The `matched_rule` prefix on a plan no automatic path may approve.
///
/// It rides `matched_rule` rather than a new plan field on purpose: that field is
/// documented as the machine-readable half ("logged and matched on, not read as
/// prose"), the same role `deny:` and `builtin:` already play, and the plan
/// struct has thirty-odd construction sites that a new field would have to touch.
/// `is_needs_human_rule` is the only reader; keep the prefix here.
pub const NEEDS_HUMAN_RULE_PREFIX: &str = "needs-human:";

/// Whether a rule id marks a call that only a person may authorize.
///
/// Read by `runtime::approval_flow` (above `auto_allow`, session memory, every
/// permission mode and the judge, so nothing automatic can approve it),
/// by `subagent_approval_decision` (which denies it with a note naming the floor
/// rather than the shell wall) and by `unattended_denial_note` (headless runs).
#[must_use]
pub fn is_needs_human_rule(rule: Option<&str>) -> bool {
    rule.is_some_and(|rule| rule.starts_with(NEEDS_HUMAN_RULE_PREFIX))
}

/// How many `allow_commands` entries the approval reason names before it says
/// "(and N more)". The panel has to stay readable; the count keeps the list from
/// silently looking complete.
const MAX_NAMED_ALLOW_COMMANDS: usize = 5;

/// The command identities a dispatch declares for its child, trimmed, empty
/// entries dropped. Unparsable shapes yield an empty list rather than an error:
/// the argument is model-written, and a malformed one should cost the child its
/// extra reach (and, by making the list empty, not raise an approval prompt for
/// authority nobody is getting).
fn subagent_allow_commands(arguments: &Value) -> Vec<String> {
    let entries: Vec<String> = arguments
        .get("allow_commands")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    normalize_allow_commands(&entries)
}

/// Trim, drop empties, drop duplicates.
///
/// ONE rule, shared by the policy engine (which reads the raw arguments to decide
/// whether the dispatch needs approval at all) and the `agent` tool (which hands
/// the list to the child). If the two ever disagreed, the panel could authorize a
/// set the child does not get, or worse: the child could receive a reach the
/// human was never shown.
#[must_use]
pub fn normalize_allow_commands(entries: &[String]) -> Vec<String> {
    let mut normalized: Vec<String> = Vec::with_capacity(entries.len());
    for entry in entries {
        let entry = entry.trim();
        if entry.is_empty() || normalized.iter().any(|known| known == entry) {
            continue;
        }
        normalized.push(entry.to_string());
    }
    normalized
}

fn subagent_role_writes(arguments: &Value) -> bool {
    let role = arguments
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("general");
    crate::subagent::SubAgentRole::parse(role)
        .map(crate::subagent::SubAgentRole::allows_writes)
        .unwrap_or(true)
}

#[cfg(test)]
mod tests;
