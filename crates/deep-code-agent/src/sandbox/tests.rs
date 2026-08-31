use super::*;

#[test]
fn detect_capabilities_returns_structured_report() {
    let caps = detect_capabilities();
    assert!(!caps.detail.is_empty());
}

#[test]
fn no_gaps_is_full_and_any_gap_is_partial() {
    assert_eq!(Enforcement::from_gaps(Vec::new()), Enforcement::Full);
    assert_eq!(
        Enforcement::from_gaps(vec![EnforcementGap::LandlockTruncate]),
        Enforcement::Partial {
            gaps: vec![EnforcementGap::LandlockTruncate],
        }
    );
}

#[test]
fn partial_is_enforced_but_not_full() {
    // The distinction the whole report exists for: a partial dimension still
    // holds a boundary (so "is the network withheld" stays true), yet must
    // never be described to a human as "sandboxed".
    let partial = Enforcement::from_gaps(vec![EnforcementGap::LandlockTruncate]);
    assert!(partial.is_enforced());
    assert!(!partial.is_full());
    assert!(!Enforcement::None.is_enforced());
    assert!(Enforcement::Full.is_full());
}

#[test]
fn gaps_are_listed_only_for_partial() {
    assert!(Enforcement::None.gaps().is_empty());
    assert!(Enforcement::Full.gaps().is_empty());
    assert_eq!(
        Enforcement::from_gaps(vec![
            EnforcementGap::LandlockTruncate,
            EnforcementGap::LandlockIoctlDev,
        ])
        .gaps(),
        [
            EnforcementGap::LandlockTruncate,
            EnforcementGap::LandlockIoctlDev
        ]
    );
}

#[test]
fn weakest_never_claims_more_than_either_dimension() {
    // Every combination, because the host running this test only ever
    // exercises one of them (macOS is Full/Full, Windows None/None).
    let truncate = Enforcement::from_gaps(vec![EnforcementGap::LandlockTruncate]);
    let ioctl = Enforcement::from_gaps(vec![EnforcementGap::LandlockIoctlDev]);
    let levels = [Enforcement::None, truncate.clone(), Enforcement::Full];

    for filesystem in &levels {
        for network in &levels {
            let weakest = Enforcement::weakest(filesystem.clone(), network.clone());
            assert_eq!(
                weakest.is_full(),
                filesystem.is_full() && network.is_full(),
                "{filesystem:?} + {network:?}"
            );
            assert_eq!(
                weakest.is_enforced(),
                filesystem.is_enforced() && network.is_enforced(),
                "{filesystem:?} + {network:?}"
            );
        }
    }

    // Two partial dimensions: the answer names both gaps rather than
    // silently reporting one dimension's and dropping the other's.
    assert_eq!(
        Enforcement::weakest(truncate, ioctl),
        Enforcement::Partial {
            gaps: vec![
                EnforcementGap::LandlockTruncate,
                EnforcementGap::LandlockIoctlDev
            ],
        }
    );
}

#[test]
fn every_gap_explains_itself() {
    // The gap list is what `doctor` prints and what the READMEs promise is
    // nameable, so an empty or duplicated line would be a silent regression.
    for gap in [
        EnforcementGap::LandlockTruncate,
        EnforcementGap::LandlockIoctlDev,
    ] {
        assert!(!gap.detail().is_empty());
    }
    assert_ne!(
        EnforcementGap::LandlockTruncate.detail(),
        EnforcementGap::LandlockIoctlDev.detail()
    );
}

#[test]
fn sandbox_enforcement_reports_the_weaker_dimension() {
    // Linux before 6.2 is the real shape: writes partial, network full. The
    // approval panel must show the write answer, not the network one.
    let caps = detect_capabilities();
    let weaker = sandbox_enforcement();
    assert_eq!(
        weaker.is_full(),
        caps.filesystem.is_full() && caps.network.is_full()
    );
    assert_eq!(
        weaker.is_enforced(),
        caps.filesystem.is_enforced() && caps.network.is_enforced()
    );
    // A `None` summary has nothing left to qualify (see `weakest`); anything
    // else must carry every gap either dimension named.
    if weaker.is_enforced() {
        for gap in caps.filesystem.gaps().iter().chain(caps.network.gaps()) {
            assert!(
                weaker.gaps().contains(gap),
                "{gap:?} was reported by a dimension but dropped from the summary"
            );
        }
    }
}

#[test]
fn design_notes_and_ioctl_gap_are_mutually_exclusive() {
    // A design note says "the sandbox refuses device ioctl"; the gap says
    // "the kernel cannot check it". A description carrying both would tell
    // the model the same operation is simultaneously unchecked and refused.
    // And a host with no backend refuses nothing *by design* — there is no
    // design there to speak for.
    let caps = detect_capabilities();
    let notes = sandbox_design_notes();
    if !caps.available {
        assert!(
            notes.is_empty(),
            "an unavailable backend cannot refuse anything by design"
        );
    }
    if caps
        .filesystem
        .gaps()
        .contains(&EnforcementGap::LandlockIoctlDev)
    {
        assert!(
            notes.is_empty(),
            "device ioctl cannot be both ungoverned and deliberately denied"
        );
    }
    for note in notes {
        assert!(!note.is_empty());
    }
}

#[test]
fn write_denial_signature_matches_backend_denial_texts() {
    // The three texts the backends actually produce: Seatbelt EPERM,
    // Landlock EACCES, and a read-only remount.
    assert!(write_denial_signature(
        Some(1),
        "sh: /other/repo/f.txt: Operation not permitted"
    ));
    assert!(write_denial_signature(
        Some(1),
        "touch: cannot touch '/other/repo/f.txt': Permission denied"
    ));
    assert!(write_denial_signature(Some(1), "Read-only file system"));
    // A killed child reports no exit code; the stderr text still decides.
    assert!(write_denial_signature(None, "Operation not permitted"));

    // A successful command is never a denial, whatever stderr says.
    assert!(!write_denial_signature(
        Some(0),
        "warning: Operation not permitted (ignored)"
    ));
    // Ordinary failures don't match.
    assert!(!write_denial_signature(Some(1), "error: expected `;`"));
    assert!(!write_denial_signature(Some(2), ""));
}

/// Every stderr below was captured under the real no-network Seatbelt
/// profile (curl by name, curl by IP, node, python bind), so the list
/// matches what the sandbox actually produces — not what it plausibly
/// might.
#[test]
fn network_denial_signature_matches_offline_sandbox_texts() {
    // DNS family.
    assert!(network_denial_signature(
        Some(6),
        "curl: (6) Could not resolve host: example.com"
    ));
    assert!(network_denial_signature(
        Some(1),
        "Error: getaddrinfo ENOTFOUND example.com"
    ));
    assert!(network_denial_signature(
        Some(128),
        "fatal: unable to access 'https://github.com/x/': Could not resolve host: github.com"
    ));
    // Connect-by-IP family (DNS bypassed, connect refused by the kernel).
    assert!(network_denial_signature(
        Some(7),
        "curl: (7) Failed to connect to 1.1.1.1 port 80 after 1 ms: Couldn't connect to \
             server"
    ));
    // Socket EPERM next to a network word: python bind, node listen.
    assert!(network_denial_signature(
        Some(1),
        "s.bind((\"127.0.0.1\", 0))\nPermissionError: [Errno 1] Operation not permitted"
    ));
    assert!(network_denial_signature(
        Some(1),
        "Error: listen EPERM: operation not permitted 127.0.0.1:8899"
    ));

    // A write denial with no network word is NOT a network failure —
    // this is the boundary that keeps the write note working.
    assert!(!network_denial_signature(
        Some(1),
        "mkdir: /etc/x: Operation not permitted"
    ));
    // Success never qualifies, whatever stderr says.
    assert!(!network_denial_signature(
        Some(0),
        "warning: Could not resolve host (retried ok)"
    ));
    // "Connection refused" alone is a real diagnostic (nothing listening
    // on the other end), not a sandbox artifact — deliberately unmatched.
    assert!(!network_denial_signature(
        Some(1),
        "nc: 127.0.0.1 8080: Connection refused"
    ));
}

#[test]
fn refuse_bare_execution_only_when_wanted_unforced_and_unavailable() {
    // The one refusing case: policy wants a sandbox, no test override, and
    // the host has no backend.
    assert!(refuse_bare_execution(true, None, false));
    // Backend present → run (confined).
    assert!(!refuse_bare_execution(true, None, true));
    // Policy doesn't want a sandbox → bare by design, never refuse.
    assert!(!refuse_bare_execution(false, None, false));
    // Test override is authoritative in both directions — never refuse.
    assert!(!refuse_bare_execution(true, Some(false), false));
    assert!(!refuse_bare_execution(true, Some(true), false));
}

/// Regression guard for Windows argument passing.
///
/// This started life as a diagnostic probe and it caught a real defect:
/// `bare_shell_command` used `Command::arg` for the whole command line, which
/// applies the MSVC C-runtime quoting rules (an argument holding spaces or
/// quotes is wrapped in `"`, inner quotes escaped as `\"`). `cmd.exe`
/// implements none of that — `\` is an ordinary character and `"` a quote
/// toggle — so every command carrying a quoted argument arrived mangled. The
/// fix is `raw_arg`; this test fails loudly if anyone reverts to `arg`.
///
/// `echo` is the right probe because cmd's `echo` emits its argument
/// verbatim, quotes included — a correct pass-through prints exactly what was
/// typed, and a stray backslash means the escaping leaked through.
///
/// The observable behind the original report: on Windows the model stopped
/// using `git commit -m "<message>"` and started writing the message to a file
/// to commit with `-F`, i.e. it routed around the broken quoting.
#[cfg(windows)]
#[test]
fn windows_cmd_receives_quoted_arguments_verbatim() {
    let cwd = std::env::current_dir().expect("cwd");
    let run = |command: &str| {
        let output = bare_shell_command(command, &cwd)
            .output()
            .expect("spawn cmd");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    let got = run("echo \"a b\"");
    assert!(
        !got.contains('\\'),
        "cmd received escaped quotes: stdout={got:?}. `Command::arg` applies \
             MSVC quoting that cmd.exe cannot parse; use raw_arg."
    );
    assert_eq!(
        got, "\"a b\"",
        "quoted argument did not survive the trip to cmd.exe: stdout={got:?}"
    );

    // The shape from the real report: several quoted arguments in one command,
    // which is what `git commit -m "…"` looks like to cmd.
    let got = run("echo \"a b\" \"c d\"");
    assert_eq!(
        got, "\"a b\" \"c d\"",
        "multiple quoted arguments were mangled: stdout={got:?}"
    );
}

/// The manager-level gate, pinned in both directions. The kernel-level
/// seatbelt tests exercise the WRAPPER (`macos_seatbelt::wrap_shell_command`
/// directly), so before this test a mutation collapsing
/// `SandboxManager::should_sandbox` to `false` ran every confined command
/// bare while all 828 tests stayed green — the exact silent regression the
/// sandbox exists to prevent.
#[test]
fn manager_gate_composes_policy_veto_and_forced_override() {
    let confined = SandboxPolicy::workspace_write();
    let bare = SandboxPolicy::Unsandboxed;
    let on = SandboxManager::new().force_sandbox(Some(true));
    let off = SandboxManager::new().force_sandbox(Some(false));
    // The policy veto is absolute: no override sandboxes an Unsandboxed run.
    assert!(!on.should_sandbox(&bare));
    // A confining policy sandboxes when a backend is (forced) present…
    assert!(on.should_sandbox(&confined));
    // …and must not claim it would when the backend is absent.
    assert!(!off.should_sandbox(&confined));
}

/// `sandbox_unavailable_for` is the refuse-bare gate callers consult before
/// `wrap_shell_command` hands back a bare command. A test override is
/// authoritative EITHER WAY (see the method's doc): `force_sandbox(Some(_))`
/// is a deliberate test state, never a missing backend, so all three shapes
/// below are refusals that must NOT fire. The `true` row of the table needs
/// `forced = None` on a backendless host — unreachable from a unit test on a
/// Seatbelt machine by construction; `refuse_bare_execution` pins that row
/// directly, and DEEPCODE_REQUIRE_SANDBOX covers the host side in CI.
#[test]
fn refuse_gate_stays_quiet_for_overrides_and_bare_policies() {
    let confined = SandboxPolicy::workspace_write();
    let bare = SandboxPolicy::Unsandboxed;
    let off = SandboxManager::new().force_sandbox(Some(false));
    let on = SandboxManager::new().force_sandbox(Some(true));
    assert!(!off.sandbox_unavailable_for(&confined));
    assert!(!on.sandbox_unavailable_for(&confined));
    // A policy that wants no sandbox is never "unavailable".
    assert!(!off.sandbox_unavailable_for(&bare));
}

/// `wrap_shell_command` must wrap and bare by the SAME gate the asserts above
/// pin — and the bare command really is `sh -c <command>`, not a
/// `Default::default()` husk. Inverting the gate (`delete !`) swaps both
/// branches, so asserting the two directions separately kills the inversion.
#[test]
fn wrap_shell_command_bares_and_wraps_by_the_gate() {
    let cwd = std::env::temp_dir();
    let on = SandboxManager::new().force_sandbox(Some(true));

    let bare = on
        .wrap_shell_command("true", &cwd, &[], &SandboxPolicy::Unsandboxed)
        .expect("bare path cannot fail");
    let program = bare.get_program().to_string_lossy().into_owned();
    assert!(
        program.ends_with("sh"),
        "an Unsandboxed policy must yield the bare shell, got {program:?}"
    );
    let args: Vec<String> = bare
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args, ["-c", "true"], "bare form is `sh -c <command>`");

    #[cfg(target_os = "macos")]
    {
        let wrapped = on
            .wrap_shell_command(
                "true",
                &cwd,
                std::slice::from_ref(&cwd),
                &SandboxPolicy::workspace_write(),
            )
            .expect("seatbelt wrap cannot fail to build");
        let program = wrapped.get_program().to_string_lossy().into_owned();
        assert!(
            program.ends_with("sandbox-exec"),
            "a confined policy with the backend forced on must wrap, got {program:?}"
        );
    }
}
