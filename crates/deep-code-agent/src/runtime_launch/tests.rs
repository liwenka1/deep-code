use super::*;
use crate::tool::MockEchoTool;

/// Tool names in the parent registry built for `client`.
fn parent_tool_names<C: LlmClient + Clone + 'static>(
    client: C,
    config: &AgentConfig,
    workspace: &Path,
) -> Vec<String> {
    let cancel = CancellationToken::new();
    let tools = build_parent_tools(
        Arc::new(client),
        config,
        &WorkspaceRoots::from(workspace),
        &cancel,
        &mut Vec::new(),
        &SharedLang::default(),
    );
    tools
        .registry
        .specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect()
}

/// A dead standing-consent entry must say so, and say WHICH kind of dead
/// it is: an exact-name miss (the old prefix spelling), the by-design
/// exempt root-grant tool, and — separately — a real tool name this
/// session simply did not mount. An entry naming a registered tool stays
/// silent, and a repeat of any entry is one mistake, not two.
#[test]
fn dead_auto_allow_entries_warn() {
    let mut warnings = Vec::new();
    warn_unmatched_auto_allow(
        &ToolRegistry::with_mock_tools(),
        &[
            MockEchoTool::NAME.to_string(),
            "mock_".to_string(),
            "mock_".to_string(),
            crate::root_grant::REQUEST_WRITE_ROOT_TOOL.to_string(),
            // Real tools, absent from the mock registry — exactly the shape
            // `DEEP_CODE_DISABLE_WEB=1` produces for a correct config.
            "fetch_url".to_string(),
            "web_search".to_string(),
        ],
        &mut warnings,
    );
    assert_eq!(warnings.len(), 3, "{warnings:?}");
    assert!(
        warnings[0].contains("\"mock_\"") && warnings[0].contains("exact"),
        "{warnings:?}"
    );
    assert!(
        warnings[1].contains("request_write_root") && warnings[1].contains("always prompts"),
        "{warnings:?}"
    );
    // One line for both, and it must NOT accuse them of being typos.
    assert!(
        warnings[2].contains("fetch_url")
            && warnings[2].contains("web_search")
            && warnings[2].contains("did not mount")
            && !warnings[2].contains("matches no tool name"),
        "an unmounted-but-real tool must not be reported as a misspelling: {warnings:?}"
    );
}

/// The check is wired into the real launch assembly, after every
/// registration (extensions included) — not just unit-testable in theory.
#[test]
fn launch_assembly_warns_about_dead_auto_allow_entries() {
    let dir = tempfile::TempDir::new().unwrap();
    let config = AgentConfig {
        approval_auto_allow: vec!["read_".to_string()],
        ..AgentConfig::builtin()
    };
    let mut warnings = Vec::new();
    let cancel = CancellationToken::new();
    let ui_lang = SharedLang::default();
    let _tools = build_parent_tools(
        Arc::new(EchoClient::new(ui_lang.clone())),
        &config,
        &WorkspaceRoots::from(dir.path()),
        &cancel,
        &mut warnings,
        &ui_lang,
    );
    assert!(
        warnings.iter().any(|warning| warning.contains("\"read_\"")),
        "{warnings:?}"
    );
}

/// `warn_unmatched_auto_allow`'s doc calls `classify_tool` "the tool-name
/// universe", and the whole distinction between "not mounted here" and "no
/// such tool" rests on that. Nothing enforced it: a twelfth tool registered
/// without a `classify_tool` arm would make the warning tell a user with a
/// perfectly good entry that it "matches no tool name" — the exact false
/// accusation the warning was rewritten to stop making.
///
/// (The gate itself fails safe — `ToolKind::Unknown` is `NeedsApproval` at
/// High — so this is about the warning telling the truth, not about access.)
#[test]
fn classify_tool_knows_every_registered_tool() {
    let dir = tempfile::TempDir::new().unwrap();
    let config = AgentConfig {
        api_key: Some("test-key".to_string()),
        ..AgentConfig::builtin()
    };
    let client = DeepSeekClient::new(config.clone()).expect("client builds without network");

    let mut names = parent_tool_names(client, &config, dir.path());
    assert!(
        names.len() >= 8,
        "an empty or truncated registry would satisfy this loop vacuously: {names:?}"
    );
    // `build_parent_tools` is not the whole universe: `agent` is mounted
    // later by `attach_agent_extensions` (see the count above), so a loop
    // over the builder alone would let `"agent" => ToolKind::SubAgent` be
    // deleted without a single test noticing — and `agent` is exactly the
    // kind of late-mounted name that would then be accused of matching no
    // tool. Named explicitly rather than left to the builder.
    names.extend(
        crate::subagent::SUBAGENT_TOOL_NAMES
            .iter()
            .map(|name| (*name).to_string()),
    );

    for name in names {
        assert_ne!(
            crate::execution_policy::ExecPolicy::classify_tool(&name),
            crate::execution_policy::ToolKind::Unknown,
            "{name} is registered but absent from classify_tool's match, so \
                 warn_unmatched_auto_allow would call a valid entry a typo"
        );
    }
}

#[test]
fn mock_echo_is_mounted_only_for_the_offline_echo_backend() {
    let dir = tempfile::TempDir::new().unwrap();
    let config = AgentConfig {
        api_key: Some("test-key".to_string()),
        ..AgentConfig::builtin()
    };

    let online = DeepSeekClient::new(config.clone()).expect("client builds without network");
    let names = parent_tool_names(online, &config, dir.path());
    assert!(
        !names.iter().any(|name| name == MockEchoTool::NAME),
        "online registry must not expose the mock tool: {names:?}"
    );

    let names = parent_tool_names(
        EchoClient::new(SharedLang::new(Lang::Zh)),
        &config,
        dir.path(),
    );
    assert!(
        !names.iter().any(|name| name == MockEchoTool::NAME),
        "the mock tool is a test fixture; no production registry mounts it: {names:?}"
    );
}

/// The grant doorbell mounts with the fs tool groups — same boundary,
/// same launch — so any session that can hit the write fence can also
/// request the fix.
#[test]
fn request_write_root_is_mounted_with_the_fs_tools() {
    let dir = tempfile::TempDir::new().unwrap();
    let config = AgentConfig::builtin();
    let names = parent_tool_names(
        EchoClient::new(SharedLang::new(Lang::Zh)),
        &config,
        dir.path(),
    );
    assert!(
        names.iter().any(|name| name == "request_write_root"),
        "grant doorbell missing: {names:?}"
    );
    assert!(names.iter().any(|name| name == "write_file"));
}

/// Stamp a record's grants the way a real save does. Tests that build a
/// record in memory and hand it straight to `launch_runtime` would
/// otherwise all exercise the unsigned-record rejection instead of the
/// behaviour they are actually about.
fn signed(mut record: SessionRecord) -> SessionRecord {
    record.extra_roots_mac = crate::session_integrity::sign_roots(
        record.id.as_str(),
        &record.workspace,
        &record.extra_roots,
    );
    record
}

#[test]
fn resume_unions_record_grants_with_cli_add_dirs() {
    let workspace = tempfile::TempDir::new().unwrap();
    let ws = workspace.path().canonicalize().unwrap();
    let recorded = tempfile::TempDir::new().unwrap();
    let recorded_root = recorded.path().canonicalize().unwrap();
    let cli = tempfile::TempDir::new().unwrap();
    let cli_root = cli.path().canonicalize().unwrap();

    let record = signed(
        SessionRecord::new(ws.clone(), "system").with_extra_roots(vec![recorded_root.clone()]),
    );
    // No api key → offline echo resume path; the union logic is shared
    // with the online path (it runs before the client branch).
    let config = AgentConfig::builtin();
    let launched = launch_runtime(
        &config,
        WorkspaceRoots::new(ws, vec![cli_root.clone(), recorded_root.clone()]),
        Some(record),
    );
    // Record grants come first, CLI additions after; the repeated
    // `recorded_root` from the CLI must not duplicate.
    assert_eq!(launched.extra_roots, vec![recorded_root, cli_root]);
}

#[test]
fn resume_drops_stale_recorded_grants_instead_of_disabling_tools() {
    let workspace = tempfile::TempDir::new().unwrap();
    let ws = workspace.path().canonicalize().unwrap();
    let stale = ws.join("gone");
    std::fs::create_dir(&stale).unwrap();
    let record = signed(SessionRecord::new(ws, "system").with_extra_roots(vec![stale.clone()]));
    std::fs::remove_dir(&stale).unwrap();

    let launched = launch_runtime(
        &AgentConfig::builtin(),
        record.workspace.clone(),
        Some(record),
    );
    // The stale grant is dropped with a warning; it must NOT reach
    // WorkspacePolicy, whose fail-closed constructor would otherwise take
    // every workspace/shell tool down with it on this resumed session —
    // build_tool_registry degrades that error into the "disabled"
    // warnings asserted absent below.
    assert!(launched.extra_roots.is_empty());
    assert!(
        launched
            .warnings
            .iter()
            .any(|warning| warning.contains("gone")),
        "dropped grant must be surfaced: {:?}",
        launched.warnings
    );
    assert!(
        !launched
            .warnings
            .iter()
            .any(|warning| warning.contains("tools disabled")),
        "tools must survive a stale grant: {:?}",
        launched.warnings
    );
}

/// A session record does not get to say which workspace it belongs to.
///
/// The record is an ordinary `write_file` target for the model — it lives
/// at `<workspace>/.deep-code/sessions/<id>.json`, inside the primary
/// writable root — and `-c` ranks candidates by an `updated_at_ms` read out
/// of the file itself. Trusting `record.workspace` therefore let a written
/// record move the primary write root (to `/`, say) and take the session
/// store with it, without any prompt: not a defeated approval, a skipped
/// one.
#[test]
fn resume_uses_the_callers_workspace_not_the_records_claim() {
    let caller = tempfile::TempDir::new().unwrap();
    let caller_ws = caller.path().canonicalize().unwrap();
    let elsewhere = tempfile::TempDir::new().unwrap();
    let claimed = elsewhere.path().canonicalize().unwrap();

    let record = SessionRecord::new(claimed.clone(), "system");
    let id = record.id.clone();
    let launched = launch_runtime(&AgentConfig::builtin(), caller_ws.clone(), Some(record));

    assert!(
        launched
            .warnings
            .iter()
            .any(|warning| warning.contains("recorded at")),
        "the redirect must be surfaced, not applied silently: {:?}",
        launched.warnings
    );
    assert!(
        JsonSessionStore::for_workspace(&caller_ws)
            .unwrap()
            .load(&id)
            .is_ok(),
        "the resumed session belongs to the workspace the user launched in"
    );
    assert!(
        JsonSessionStore::for_workspace(&claimed)
            .unwrap()
            .load(&id)
            .is_err(),
        "the record's claim must not redirect the session store"
    );
}

/// A recorded grant is re-vetted against the very floors a model-requested
/// grant has to clear, because a record is a file the model can write.
///
/// Before this, every floor the approval flow added — home, filesystem
/// root, credential overlap, display-equals-grant — was reachable around by
/// writing `extra_roots` into a record and letting `-c` pick it up. Those
/// checks all live in `resolve_grant_target`, which the resume path never
/// called.
#[test]
fn resume_refuses_a_recorded_grant_a_request_could_never_have_won() {
    let workspace = tempfile::TempDir::new().unwrap();
    let ws = workspace.path().canonicalize().unwrap();
    let Some(home) = crate::paths::home_dir().and_then(|home| home.canonicalize().ok()) else {
        eprintln!("no resolvable home dir on this host; skipping");
        return;
    };
    if home.starts_with(&ws) || ws.starts_with(&home) {
        eprintln!("tempdir overlaps home on this host; skipping");
        return;
    }

    // Signed, so this test is about the FLOOR: even a grant list this
    // host really did author may not name home or the filesystem root.
    let record = signed(
        SessionRecord::new(ws.clone(), "system")
            .with_extra_roots(vec![home.clone(), PathBuf::from("/")]),
    );
    let launched = launch_runtime(&AgentConfig::builtin(), ws.clone(), Some(record));

    assert!(
        launched.extra_roots.is_empty(),
        "neither the home directory nor the filesystem root may be restored as a \
             write root: {:?}",
        launched.extra_roots
    );
    assert!(
        launched
            .warnings
            .iter()
            .any(|warning| warning.contains("home directory")),
        "the refusal must be surfaced: {:?}",
        launched.warnings
    );
    assert!(
        launched
            .warnings
            .iter()
            .any(|warning| warning.contains("filesystem root")),
        "the refusal must be surfaced: {:?}",
        launched.warnings
    );
}

/// A signed root whose spelling has come to resolve somewhere else is
/// dropped, not silently redirected.
///
/// The tag authenticates the path's SPELLING, and every root that reaches
/// the record through `serialize_record` was canonical when it was signed.
/// So a recorded root that no longer resolves to itself is a root whose
/// meaning changed after approval: planting a symlink over an approved
/// directory redirects the grant without forging anything, because the
/// signature covers `<ws>/docs` while what gets enforced is wherever
/// `<ws>/docs` now points.
///
/// This test used to assert the opposite — that the redirect is enforced,
/// on the grounds that displaying the resolved path keeps display and
/// enforcement in agreement. That property is real and still holds (see
/// `resume_keeps_a_signed_root_that_still_resolves_to_itself`), but pinning
/// it this way also pinned the escalation: it made "a signed grant may be
/// redirected out of the workspace" a regression-gated contract.
///
/// Runs on Windows too: this is the RESUME half of the same TOCTOU story
/// whose approval half d84b22c deliberately moved off `#[cfg(unix)]`
/// (`root_grant_refuses_when_the_target_changes_under_the_approval`), and
/// the resume path itself is cross-platform. Covering one half and not the
/// other is how a boundary looks guarded on a platform where it is not.
#[test]
fn resume_drops_a_signed_root_that_now_resolves_elsewhere() {
    let workspace = tempfile::TempDir::new().unwrap();
    let ws = workspace.path().canonicalize().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    let real_target = outside.path().canonicalize().unwrap();

    // A spelling that reads as "inside the repo" but resolves out of it.
    let innocuous = ws.join("docs");
    if !crate::test_symlinks::symlink_dir_for_test(&real_target, &innocuous) {
        return;
    }

    let mut record = SessionRecord::new(ws.clone(), "prompt");
    record.extra_roots = vec![innocuous.clone()];
    let record = signed(record);

    let launched = launch_runtime(
        &AgentConfig::builtin(),
        WorkspaceRoots::new(ws.clone(), Vec::new()),
        Some(record),
    );

    assert!(
        launched.extra_roots.is_empty(),
        "a redirected grant must be dropped, not enforced: {:?}",
        launched.extra_roots
    );
    assert!(
        !launched.extra_roots.contains(&real_target),
        "the symlink's target must not become the boundary"
    );
    assert!(
        launched
            .warnings
            .iter()
            .any(|warning| warning.contains("no longer the directory that was approved")),
        "the drop must be surfaced: {:?}",
        launched.warnings
    );
}

/// A validly signed record copied VERBATIM into another workspace must not
/// carry its grants across.
///
/// The key is per-user, not per-workspace, and the tag was verified against
/// the record's own `workspace` field — so copying the whole file, that
/// field included, verified happily; the workspace substitution then
/// replaced it with the caller's and left the grants standing. Every
/// session record on the host is world-readable to a shell command (reads
/// are unrestricted under both sandboxes), so a grant approved once in any
/// checkout could be lifted into every other checkout. Nothing is forged
/// here — which is why the tag alone could not catch it, and why
/// `same_workspace` has to be a condition of accepting the grants.
#[test]
fn a_verbatim_record_copy_does_not_carry_grants_into_another_workspace() {
    let original = tempfile::TempDir::new().unwrap();
    let original_ws = original.path().canonicalize().unwrap();
    let elsewhere = tempfile::TempDir::new().unwrap();
    let other_ws = elsewhere.path().canonicalize().unwrap();
    let juicy = tempfile::TempDir::new().unwrap();
    let granted = juicy.path().canonicalize().unwrap();

    // Signed for `original_ws` — a genuine, human-approved grant there.
    let mut record = SessionRecord::new(original_ws.clone(), "prompt");
    record.extra_roots = vec![granted.clone()];
    let record = signed(record);
    // Resumed from a DIFFERENT workspace, byte-for-byte unchanged.
    let launched = launch_runtime(
        &AgentConfig::builtin(),
        WorkspaceRoots::new(other_ws, Vec::new()),
        Some(record),
    );

    assert!(
        launched.extra_roots.is_empty(),
        "a grant signed for another workspace must not be restored here: {:?}",
        launched.extra_roots
    );
    assert!(
        !launched.extra_roots.contains(&granted),
        "the lifted grant must not become the boundary"
    );
}

/// The other half: an honest signed root survives, and what is displayed is
/// what is enforced. Without this, the test above could be satisfied by
/// dropping every recorded grant.
#[test]
fn resume_keeps_a_signed_root_that_still_resolves_to_itself() {
    let workspace = tempfile::TempDir::new().unwrap();
    let ws = workspace.path().canonicalize().unwrap();
    let extra = tempfile::TempDir::new().unwrap();
    let granted = extra.path().canonicalize().unwrap();

    let mut record = SessionRecord::new(ws.clone(), "prompt");
    record.extra_roots = vec![granted.clone()];
    let record = signed(record);

    let launched = launch_runtime(
        &AgentConfig::builtin(),
        WorkspaceRoots::new(ws.clone(), Vec::new()),
        Some(record),
    );

    assert_eq!(
        launched.extra_roots,
        vec![granted],
        "an authentic grant must survive resume: {:?}",
        launched.warnings
    );
}

/// The record is a file the model can write, and on resume its grants
/// become the write boundary. The floor can only refuse what is
/// categorically off limits — it cannot refuse `~/.cargo`, whose
/// `rustc-wrapper` runs on the next build, or `~/Library/LaunchAgents`,
/// which runs at next login, because a human might grant either on
/// purpose. So the grants are authenticated rather than judged: a list
/// nobody on this host signed is dropped, loudly.
#[test]
fn resume_drops_recorded_grants_that_carry_no_authorship_tag() {
    let workspace = tempfile::TempDir::new().unwrap();
    let ws = workspace.path().canonicalize().unwrap();
    let smuggled = tempfile::TempDir::new().unwrap();
    let smuggled_root = smuggled.path().canonicalize().unwrap();

    // Exactly what a forged record looks like: a real, resolvable
    // directory that clears every floor, and no tag.
    let mut record = SessionRecord::new(ws.clone(), "system");
    record.extra_roots = vec![smuggled_root.clone()];
    assert!(record.extra_roots_mac.is_none());

    let launched = launch_runtime(&AgentConfig::builtin(), ws.clone(), Some(record));

    assert!(
        launched.extra_roots.is_empty(),
        "an unsigned grant must not become a write root: {:?}",
        launched.extra_roots
    );
    assert!(
        launched
            .warnings
            .iter()
            .any(|warning| warning.contains("authorship tag")),
        "the drop must be surfaced: {:?}",
        launched.warnings
    );

    // A tag for a DIFFERENT grant list must not carry this one either.
    let mut tampered = SessionRecord::new(ws.clone(), "system");
    tampered.extra_roots = vec![ws.join("docs")];
    std::fs::create_dir(ws.join("docs")).unwrap();
    let tampered = signed(tampered);
    let mut tampered = tampered;
    tampered.extra_roots = vec![smuggled_root];
    let launched = launch_runtime(&AgentConfig::builtin(), ws, Some(tampered));
    assert!(
        launched.extra_roots.is_empty(),
        "a tag lifted from another grant list must not verify: {:?}",
        launched.extra_roots
    );
}

#[test]
fn resume_refreshes_system_prompt_and_persists_grants_immediately() {
    let workspace = tempfile::TempDir::new().unwrap();
    let ws = workspace.path().canonicalize().unwrap();
    let extra = tempfile::TempDir::new().unwrap();
    let extra_root = extra.path().canonicalize().unwrap();

    // A session created WITHOUT the grant: its saved prompt cannot name it.
    let store = JsonSessionStore::for_workspace(&ws).unwrap();
    let mut record = SessionRecord::new(ws.clone(), "original prompt");
    let id = record.id.clone();
    store.save(&mut record).unwrap();

    // Resume with `--add-dir extra` (plus a primary-equal extra that the
    // union must skip), then exit without any turn.
    let launched = launch_runtime(
        &AgentConfig::builtin(),
        WorkspaceRoots::new(ws.clone(), vec![extra_root.clone(), ws.clone()]),
        Some(record),
    );
    assert_eq!(launched.extra_roots, vec![extra_root.clone()]);

    // The grant must be on disk already — not parked until the first
    // persist() that a turn would trigger.
    let reloaded = store.load(&id).unwrap();
    assert_eq!(reloaded.extra_roots, vec![extra_root.clone()]);

    // And the stored system prompt now names the root: enforceable but
    // unadvertised is exactly the gap this guards against.
    let crate::session_entry::EntryKind::System { content } = &reloaded.entries[0].kind else {
        panic!("entries[0] must stay the system prompt");
    };
    assert!(
        content.contains("additional writable roots"),
        "refreshed prompt must carry the extras section: {content}"
    );
    assert!(
        content.contains(extra_root.to_string_lossy().as_ref()),
        "refreshed prompt must name the granted root: {content}"
    );
}

#[test]
fn fresh_launch_drops_extras_equal_to_the_primary() {
    let workspace = tempfile::TempDir::new().unwrap();
    let ws = workspace.path().canonicalize().unwrap();
    // `--add-dir $(pwd)`: covered by the primary, so it must not surface
    // as an "additional" root in the banner/summary/record.
    let launched = launch_runtime(
        &AgentConfig::builtin(),
        WorkspaceRoots::new(ws.clone(), vec![ws]),
        None,
    );
    assert!(launched.extra_roots.is_empty());
}

#[test]
fn web_gate_defaults_on_and_respects_disable_flag() {
    // Unset or explicit "not disabled" values keep web on.
    assert!(web_enabled_from(None));
    assert!(web_enabled_from(Some("0")));
    assert!(web_enabled_from(Some("false")));
    assert!(web_enabled_from(Some("  OFF ")));
    assert!(web_enabled_from(Some("")));
    // Explicit disable values gate it off, case-insensitively.
    assert!(!web_enabled_from(Some("1")));
    assert!(!web_enabled_from(Some("true")));
    assert!(!web_enabled_from(Some("TRUE")));
    assert!(!web_enabled_from(Some("on")));
    assert!(!web_enabled_from(Some("yes")));
    // Fail-closed: an unrecognized/typo value disables rather than leaks web.
    assert!(!web_enabled_from(Some("disabel")));
}

/// With an API key both launch paths pick the online client. `offline` is
/// derived from the client's provider name (not a per-path literal), so it
/// must come out false here, and the label must name the provider — the
/// resumed path tags itself as such. Every other launch test runs the echo
/// backend, so before this neither online arm had ever executed under test.
#[test]
fn online_launch_names_the_provider_and_is_not_offline() {
    let workspace = tempfile::TempDir::new().unwrap();
    let ws = workspace.path().canonicalize().unwrap();
    let config = AgentConfig {
        api_key: Some("test-key".to_string()),
        ..AgentConfig::builtin()
    };

    let fresh = launch_runtime(&config, ws.clone(), None);
    assert!(!fresh.offline, "an API key means a working model");
    assert_eq!(fresh.backend_label, format!("DeepSeek {}", config.model));

    let record = signed(SessionRecord::new(ws.clone(), "system"));
    let resumed = launch_runtime(&config, ws.clone(), Some(record));
    assert!(!resumed.offline);
    assert_eq!(
        resumed.backend_label,
        format!("DeepSeek {} (resumed)", config.model)
    );

    // And the derivation flips for the placeholder backend on both paths.
    let echo_fresh = launch_runtime(&AgentConfig::builtin(), ws.clone(), None);
    assert!(echo_fresh.offline);
    let echo_resumed = launch_runtime(
        &AgentConfig::builtin(),
        ws.clone(),
        Some(signed(SessionRecord::new(ws, "system"))),
    );
    assert!(echo_resumed.offline);
    assert_eq!(echo_resumed.backend_label, "offline echo (resumed)");
}
