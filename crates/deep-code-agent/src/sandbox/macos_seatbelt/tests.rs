use super::*;

/// Single-root granted list, the shape every pre-`--add-dir` call had.
fn single_root(ws: &Path) -> Vec<PathBuf> {
    vec![ws.to_path_buf()]
}

/// A symlinked credential store is denied by the path the kernel resolves,
/// not only by the spelling under `$HOME`.
///
/// Seatbelt matches resolved paths, so `(deny file-write* (subpath
/// $HOME/.aws))` never fires when `~/.aws` is a symlink into a dotfiles
/// checkout — which is how dotfile managers set these up. The write grants
/// in this file were already canonicalized for exactly this reason; the
/// denials were not, leaving the kernel fence that the in-process grant
/// floor defers to as a silent no-op.
#[cfg(unix)]
#[test]
fn a_symlinked_credential_store_is_denied_by_its_resolved_path() {
    let home = tempfile::TempDir::new().unwrap();
    let home = home.path().canonicalize().unwrap();
    let real = tempfile::TempDir::new().unwrap();
    let real = real.path().canonicalize().unwrap();
    // `~/.aws` → a directory outside home, the dotfiles-manager shape.
    std::os::unix::fs::symlink(&real, home.join(".aws")).unwrap();

    let dirs = credential_dirs_under(&home);
    let paths: Vec<&Path> = dirs.iter().map(|(_, dir)| dir.as_path()).collect();
    assert!(
        paths.contains(&real.as_path()),
        "the resolved store must be denied too: {paths:?}"
    );
    assert!(
        paths.contains(&home.join(".aws").as_path()),
        "and the spelling under home stays denied: {paths:?}"
    );
    // Parameter names must stay distinct, or one binding overwrites the
    // other and only a single deny is emitted.
    let names: std::collections::BTreeSet<&str> =
        dirs.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names.len(), dirs.len(), "duplicate SBPL parameter name");
}

#[test]
fn availability_probe_is_stable_and_panic_free() {
    // Two calls must agree (the verdict is cached process-wide).
    assert_eq!(is_available(), is_available());
}

#[test]
fn profile_opens_with_deny_by_default() {
    let ws = Path::new("/tmp/dc-ws");
    let text = compose_profile(&SandboxPolicy::workspace_write(), &single_root(ws), ws).render();
    assert!(text.starts_with("(version 1)\n(deny default)"));
    assert!(text.contains("(allow file-read*)"));
}

/// Every credential entry with an intermediate component gets that
/// component locked, so the subpath deny on the leaf cannot be walked
/// around by renaming the parent aside. `~/.config` (shared by gh and
/// gcloud) and `~/Library` (Keychains) are the cases today; a
/// single-component entry contributes nothing, being sealed by its own
/// subpath deny.
///
/// Enumerated from `CREDENTIAL_ENTRIES` so a future multi-component entry
/// is pinned automatically — a hand-listed subset is exactly how
/// `~/Library` came to be missing after `Library/Keychains` was added.
#[test]
fn every_multi_component_credential_parent_is_locked() {
    let home = tempfile::TempDir::new().unwrap();
    let home = home.path().canonicalize().unwrap();

    let parents = credential_parent_dirs_under(&home);

    for entry in crate::paths::CREDENTIAL_ENTRIES {
        // Walk every ancestor between the leaf and home; each must be
        // locked. Handles deeper nesting too, not just two-level entries.
        let mut ancestor = Path::new(entry).parent();
        while let Some(rel) = ancestor {
            if rel.as_os_str().is_empty() {
                break;
            }
            let abs = home.join(rel);
            assert!(
                parents.contains(&abs),
                "ancestor {} of entry {entry:?} must be locked against rename: {parents:?}",
                abs.display()
            );
            ancestor = rel.parent();
        }
    }
    // Leaves are NOT here: they are sealed by their own subpath deny, and
    // locking them would forbid the legitimate writes a tool makes inside.
    assert!(
        !parents.contains(&home.join(".config/gh")),
        "a leaf must not be locked here: {parents:?}"
    );
    // No duplicates: gh and gcloud share `~/.config`, locked once.
    let unique: std::collections::BTreeSet<&PathBuf> = parents.iter().collect();
    assert_eq!(
        unique.len(),
        parents.len(),
        "duplicate parent lock: {parents:?}"
    );
}

/// The profile must actually emit a `file-write-unlink` deny for each
/// intermediate directory (and only unlink, so the tree does not turn
/// read-only). The regression this guards: `~/Library`, the parent of the
/// newly added `Library/Keychains`, must be one of them — not just
/// `~/.config`, which was the only one the previous hand-coded block knew.
#[test]
fn intermediate_credential_dirs_are_locked_against_rename() {
    let Some(home) = crate::paths::home_dir() else {
        eprintln!("no home dir on this host; skipping");
        return;
    };
    let ws = Path::new("/tmp/dc-ws");
    let profile = compose_profile(&SandboxPolicy::workspace_write(), &single_root(ws), ws);
    let text = profile.render();

    for dir in credential_parent_dirs_under(&home) {
        let param = profile
            .bindings
            .iter()
            .find(|(_, path)| *path == dir)
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| panic!("{} must be bound: {:?}", dir.display(), profile.bindings));
        assert!(
            text.contains(&format!(
                "(deny file-write-unlink (literal (param \"{param}\")))"
            )),
            "renaming {} must be refused, or its nested store's deny is walked around: {text}",
            dir.display()
        );
        // Only unlink — ordinary writes elsewhere under it stay allowed.
        assert!(
            !text.contains(&format!("(deny file-write* (subpath (param \"{param}\")))")),
            "the whole of {} must not become read-only: {text}",
            dir.display()
        );
    }

    // The specific regression: Keychains' parent is locked, not only
    // `~/.config`.
    let library = crate::paths::canonicalize(&home)
        .unwrap_or(home)
        .join("Library");
    assert!(
        profile.bindings.iter().any(|(_, path)| *path == library),
        "~/Library must be locked so Keychains' deny cannot be moved out from under it: {:?}",
        profile.bindings
    );
}

#[test]
fn network_rules_appear_only_when_policy_grants_network() {
    let ws = Path::new("/tmp/dc-ws");
    let offline = compose_profile(&SandboxPolicy::workspace_write(), &single_root(ws), ws).render();
    assert!(!offline.contains("network-outbound"));

    let online = compose_profile(
        &SandboxPolicy::WorkspaceWrite {
            network_access: true,
        },
        &single_root(ws),
        ws,
    )
    .render();
    assert!(online.contains("(allow network-outbound)"));
    assert!(online.contains("(allow network-inbound)"));
}

#[test]
fn distinct_cwd_receives_its_own_write_grant() {
    let workspace = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let profile = compose_profile(
        &SandboxPolicy::workspace_write(),
        &single_root(workspace.path()),
        elsewhere.path(),
    );
    let write_roots: Vec<&str> = profile
        .bindings
        .iter()
        .filter(|(name, _)| name.starts_with("WRITE_ROOT_"))
        .map(|(name, _)| name.as_str())
        .collect();
    // Workspace, the distinct cwd, the temp dir, and /tmp — see
    // `SandboxPolicy::writable_roots` for why the last two are mandatory.
    assert_eq!(
        write_roots,
        [
            "WRITE_ROOT_0",
            "WRITE_ROOT_1",
            "WRITE_ROOT_2",
            "WRITE_ROOT_3"
        ]
    );
    assert!(profile.render().contains("WRITE_ROOT_1"));
}

#[test]
fn extra_granted_root_receives_its_own_write_grant() {
    // An `--add-dir` grant must surface as a real write root in the
    // profile — this is the kernel half of the multi-root feature; the
    // tool layer's WorkspacePolicy is the other half.
    let workspace = tempfile::tempdir().unwrap();
    let extra = tempfile::tempdir().unwrap();
    let granted = vec![workspace.path().to_path_buf(), extra.path().to_path_buf()];
    let profile = compose_profile(
        &SandboxPolicy::workspace_write(),
        &granted,
        workspace.path(),
    );
    let extra_canonical = extra.path().canonicalize().unwrap();
    assert!(
        profile
            .bindings
            .iter()
            .any(|(name, path)| name.starts_with("WRITE_ROOT_") && *path == extra_canonical),
        "extra root must be granted; bindings were {:?}",
        profile.bindings
    );
}

#[test]
fn temp_dir_is_always_a_write_root() {
    // Regression guard: `rustc`, `mktemp` and git's xcrun shim all write to
    // $TMPDIR unconditionally, so dropping this grant does not confine
    // `cargo build`, it breaks it. Both backends read `writable_roots`.
    let workspace = tempfile::tempdir().unwrap();
    let profile = compose_profile(
        &SandboxPolicy::workspace_write(),
        &single_root(workspace.path()),
        workspace.path(),
    );
    let temp = std::env::temp_dir().canonicalize().unwrap();
    assert!(
        profile
            .bindings
            .iter()
            .any(|(name, path)| { name.starts_with("WRITE_ROOT_") && *path == temp }),
        "temp dir must be granted; bindings were {:?}",
        profile.bindings
    );
}

#[test]
fn credential_denials_outrank_write_grants() {
    // SBPL resolves conflicts by last-match-wins, so the credential-dir
    // denials must render AFTER every write grant. If a writable root is
    // an ancestor of `~/.ssh` (HOME as workspace), an earlier deny would
    // be silently overridden and sandboxed commands could edit
    // authorized_keys.
    let home = std::env::var("HOME").expect("test environment has HOME");
    let profile = compose_profile(
        &SandboxPolicy::workspace_write(),
        &single_root(home.as_ref()),
        home.as_ref(),
    );
    let text = profile.render();
    let last_grant = text
        .rfind("(allow file-write* (subpath")
        .expect("profile grants a write root");
    let first_denial = text
        .find("(deny file-write* (subpath")
        .expect("profile denies credential dirs");
    assert!(
        first_denial > last_grant,
        "credential denials must come after write grants:\n{text}"
    );
}

#[test]
fn own_config_dir_is_read_and_write_denied() {
    // deep-code's own `~/.deep-code` (plaintext API key) must be denied for
    // BOTH read and write, and the read-deny must render after the blanket
    // `(allow file-read*)` so last-match-wins seals it.
    let home = std::env::var("HOME").expect("test environment has HOME");
    let profile = compose_profile(
        &SandboxPolicy::WorkspaceWrite {
            network_access: true,
        },
        &single_root(Path::new("/tmp/dc-ws")),
        Path::new("/tmp/dc-ws"),
    );
    let _ = home;
    let text = profile.render();
    assert!(profile.bindings.iter().any(|(n, _)| n == "KEEP_DEEP_CODE"));
    let allow_read = text.find("(allow file-read*)").expect("blanket read grant");
    let deny_read = text
        .find("(deny file-read* (subpath")
        .expect("own-store read deny");
    assert!(
        deny_read > allow_read,
        "own-store read deny must come after the blanket read allow:\n{text}"
    );
    // And its write is denied too.
    assert!(text.matches("(deny file-write* (subpath").count() >= 2);
}

#[test]
fn credential_param_names_are_legal_and_collision_free() {
    // sandbox-exec takes the LAST `-D` for a repeated name and silently
    // drops the earlier binding, so two entries that fold to the same
    // identifier would evaporate one deny with no error at all. Naming by
    // index makes a collision impossible instead of merely unlikely: the
    // old scheme folded every non-alphanumeric to `_`, so a future
    // `.ssh-r` entry would have collided with `.ssh`'s resolved variant.
    let Some(home) = crate::paths::home_dir() else {
        eprintln!("no home dir on this host; skipping");
        return;
    };
    let dirs = credential_dirs_under(&home);
    let mut names: Vec<&str> = dirs.iter().map(|(name, _)| name.as_str()).collect();
    let total = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), total, "duplicate -D name would drop a deny");
    for (name, _) in &dirs {
        assert!(
            name.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'),
            "{name} is not a legal SBPL identifier"
        );
    }
}

#[test]
fn every_referenced_param_is_bound() {
    // sandbox-exec rejects profiles referencing unbound params; make sure
    // rules and bindings cannot drift apart.
    let workspace = tempfile::tempdir().unwrap();
    let profile = compose_profile(
        &SandboxPolicy::workspace_write(),
        &single_root(workspace.path()),
        workspace.path(),
    );
    let text = profile.render();
    for fragment in text.split("(param \"").skip(1) {
        let name = fragment.split('"').next().unwrap();
        assert!(
            profile.bindings.iter().any(|(bound, _)| bound == name),
            "profile references unbound param {name}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn confined_command_cannot_write_outside_granted_roots() {
    if crate::sandbox::require_backend_or_skip(is_available(), "Seatbelt") {
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    // The escape target must sit outside every writable root, so it cannot
    // be a tempdir — the temp dir is itself granted. $HOME is not granted
    // (only its credential subdirs are explicitly denied), so a plain file
    // there is the honest "absence of grant" probe. Nothing is created when
    // the sandbox holds; the cleanup below only matters if it does not.
    let Some(home) = crate::paths::home_dir() else {
        eprintln!("no home dir on this host; skipping");
        return;
    };
    let escape = home.join(format!(
        ".deep-code-sandbox-escape-probe-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&escape);

    let status = wrap_shell_command(
        &format!("printf leaked > {}", escape.display()),
        workspace.path(),
        &single_root(workspace.path()),
        &SandboxPolicy::workspace_write(),
    )
    .status()
    .expect("sandbox-exec should launch");

    let leaked = escape.exists();
    let _ = std::fs::remove_file(&escape);
    assert!(
        !status.success() && !leaked,
        "write outside every writable root must be denied"
    );
}

/// Kernel-level pin for the mid-session grant: a directory added to the
/// live `WorkspacePolicy` via `grant_extra` (the approved
/// `request_write_root` path) must be writable for the NEXT sandboxed
/// command — through the same policy clone the shell tool has held since
/// launch, with no relaunch and no registry rebuild.
#[cfg(target_os = "macos")]
#[test]
fn confined_command_writes_into_a_root_granted_mid_session() {
    if crate::sandbox::require_backend_or_skip(is_available(), "Seatbelt") {
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    // The grant target must start OUTSIDE every writable root, so it
    // cannot be a tempdir (the temp dir is itself always granted). A
    // HOME subdirectory is the honest "no grant" starting point, exactly
    // like the escape probe in the denial test above.
    let Some(home) = crate::paths::home_dir() else {
        eprintln!("no home dir on this host; skipping");
        return;
    };
    let target = home.join(format!(
        ".deep-code-sandbox-grant-probe-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&target).unwrap();
    let target = target.canonicalize().unwrap();
    let probe = target.join("artifact.txt");

    let policy =
        crate::workspace_policy::WorkspacePolicy::new(workspace.path().to_path_buf()).unwrap();
    let tool_held_clone = policy.clone(); // held since "launch"
    let write_cmd = format!("echo built > {}", probe.display());
    let run = |roots: &[PathBuf]| {
        wrap_shell_command(
            &write_cmd,
            workspace.path(),
            roots,
            &SandboxPolicy::workspace_write(),
        )
        .output()
        .expect("sandbox-exec should launch")
    };

    // Before the grant the kernel refuses the write; after `grant_extra`
    // on the SHARED policy the same spelling succeeds through the clone
    // the tool has held since launch — the widened list reaches the
    // sandbox per-spawn, no relaunch, no registry rebuild.
    let denied = run(&tool_held_clone.granted_roots());
    policy.grant_extra(&target).unwrap();
    let allowed = run(&tool_held_clone.granted_roots());
    let written = std::fs::read_to_string(&probe);
    let _ = std::fs::remove_dir_all(&target);

    assert!(
        !denied.status.success(),
        "write outside the roots must be denied first"
    );
    assert!(
        allowed.status.success(),
        "granted root must be writable: {}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert_eq!(
        written.unwrap().trim(),
        "built",
        "the write landed in the granted directory"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn confined_command_reads_the_workspace_spill_dir() {
    if crate::sandbox::require_backend_or_skip(is_available(), "Seatbelt") {
        return;
    }
    // Behavioral pin for the spill design: overflow files live under the
    // WORKSPACE's `.deep-code/spill` precisely so a sandboxed `grep`/`tail`
    // can mine them. The read-deny sealing the HOME `~/.deep-code` secret
    // store must not catch a workspace directory of the same name.
    let workspace = tempfile::tempdir().unwrap();
    let spill = workspace.path().join(".deep-code/spill/run-1");
    std::fs::create_dir_all(&spill).unwrap();
    let log = spill.join("job_1.stdout.log");
    std::fs::write(&log, "first-error: E0308\n").unwrap();

    let output = wrap_shell_command(
        &format!("grep -c first-error {}", log.display()),
        workspace.path(),
        &single_root(workspace.path()),
        &SandboxPolicy::workspace_write(),
    )
    .output()
    .expect("sandbox-exec should launch");

    assert!(
        output.status.success(),
        "sandboxed read of a workspace spill file must pass: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "1");
}

#[cfg(target_os = "macos")]
#[test]
fn confined_command_writes_into_extra_granted_root() {
    if crate::sandbox::require_backend_or_skip(is_available(), "Seatbelt") {
        return;
    }
    // The multi-root scenario end to end at the kernel: a command run from
    // the workspace writes into the `--add-dir` root and Seatbelt allows
    // it. Before the grant existed this exact write was the user-visible
    // "Operation not permitted".
    let workspace = tempfile::tempdir().unwrap();
    let extra = tempfile::tempdir().unwrap();
    let granted = vec![workspace.path().to_path_buf(), extra.path().to_path_buf()];
    let target = extra.path().join("from-sandbox.txt");

    let status = wrap_shell_command(
        &format!("printf granted > {}", target.display()),
        workspace.path(),
        &granted,
        &SandboxPolicy::workspace_write(),
    )
    .status()
    .expect("sandbox-exec should launch");

    assert!(
        status.success(),
        "write into a granted extra root must pass"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "granted");
}

/// Behavioral counterpart to `tmp_is_always_a_writable_root_on_unix`: the
/// xcrun fallback path. Inside the sandbox `confstr` fails, so tools that
/// cannot learn the real `$TMPDIR` write to `/tmp` — that write must pass,
/// or every sandboxed `git` run carries EPERM noise that misclassifies
/// its failures as write-boundary denials.
#[cfg(target_os = "macos")]
#[test]
fn confined_command_can_write_to_slash_tmp() {
    if crate::sandbox::require_backend_or_skip(is_available(), "Seatbelt") {
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    let probe = format!("/tmp/.deep-code-tmp-probe-{}", std::process::id());

    let status = wrap_shell_command(
        &format!("printf tmp-ok > {probe} && rm -f {probe}"),
        workspace.path(),
        &single_root(workspace.path()),
        &SandboxPolicy::workspace_write(),
    )
    .status()
    .expect("sandbox-exec should launch");

    let _ = std::fs::remove_file(&probe);
    assert!(status.success(), "a write to /tmp must pass in the sandbox");
}

#[cfg(target_os = "macos")]
#[test]
fn confined_command_can_use_the_temp_dir() {
    if crate::sandbox::require_backend_or_skip(is_available(), "Seatbelt") {
        return;
    }
    let workspace = tempfile::tempdir().unwrap();

    // Behavioral counterpart to `temp_dir_is_always_a_write_root`: without
    // the grant this fails with "Operation not permitted", which is exactly
    // how `cargo build`/`rustc`/`git` broke. `mktemp` needs no toolchain.
    let output = wrap_shell_command(
        "mktemp",
        workspace.path(),
        &single_root(workspace.path()),
        &SandboxPolicy::workspace_write(),
    )
    .output()
    .expect("sandbox-exec should launch");

    assert!(
        output.status.success(),
        "mktemp must work inside the sandbox; stderr was {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "macos")]
#[test]
fn confined_command_writes_within_granted_root() {
    if crate::sandbox::require_backend_or_skip(is_available(), "Seatbelt") {
        return;
    }
    let workspace = tempfile::tempdir().unwrap();

    let status = wrap_shell_command(
        "printf written-inside > inside.txt",
        workspace.path(),
        &single_root(workspace.path()),
        &SandboxPolicy::workspace_write(),
    )
    .status()
    .expect("sandbox-exec should launch");

    assert!(status.success());
    let written = std::fs::read_to_string(workspace.path().join("inside.txt")).unwrap();
    assert_eq!(written, "written-inside");
}
