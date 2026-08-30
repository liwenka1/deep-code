use super::*;

#[test]
fn session_list_preview_neutralizes_a_planted_session() {
    let line = session_list_preview("hi\u{1b}[2J\u{1b}[H FAKE\u{202e}x\u{2028}y");

    assert!(
        !line.chars().any(char::is_control),
        "an escape reached stdout: {line:?}"
    );
    assert!(
        !line.contains('\u{202e}') && !line.contains('\u{2028}'),
        "an invisible code point reached stdout: {line:?}"
    );
    assert!(line.starts_with("hi"), "the text must survive: {line:?}");
    assert!(line.contains('y'), "the text must survive: {line:?}");
}

#[test]
fn session_list_preview_flattens_and_caps() {
    let line = session_list_preview(&format!("a\nb{}", "z".repeat(200)));

    assert!(!line.contains('\n'), "newlines must be collapsed: {line:?}");
    assert!(line.starts_with("a b"), "the head must survive: {line:?}");
    assert!(
        line.ends_with(" (truncated)"),
        "over-long previews must say so: {line:?}"
    );
    assert_eq!(
        line.chars().count(),
        60 + " (truncated)".chars().count(),
        "the cap counts characters of the sanitized text: {line:?}"
    );
}

#[test]
fn parse_session_resume_subcommand() {
    let parsed = parse_session_command(vec!["resume".to_string(), "session_123_0".to_string()]);
    assert_eq!(
        parsed.mode,
        RunMode::Tui {
            intent: StartupIntent::ResumeId("session_123_0".to_string()),
            add_dirs: Vec::new(),
        }
    );
}

#[test]
fn parse_session_list_subcommand() {
    let parsed = parse_session_command(vec!["list".to_string()]);
    assert_eq!(parsed.mode, RunMode::SessionList);
}

fn tui_intent(args: &[&str]) -> StartupIntent {
    let parsed = parse_tui_args(args.iter().map(|s| (*s).to_string()).collect());
    match parsed.mode {
        RunMode::Tui { intent, .. } => intent,
        other => panic!("expected Tui, got {other:?}"),
    }
}

#[test]
fn add_dir_is_repeatable_deduped_and_canonical() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let first_arg = first.path().to_string_lossy().into_owned();
    let second_arg = second.path().to_string_lossy().into_owned();
    let parsed = parse_tui_args(vec![
        "--add-dir".to_string(),
        first_arg.clone(),
        format!("--add-dir={second_arg}"),
        "--add-dir".to_string(),
        first_arg,
    ]);
    match parsed.mode {
        RunMode::Tui { add_dirs, .. } => {
            assert_eq!(
                add_dirs,
                vec![
                    first.path().canonicalize().unwrap(),
                    second.path().canonicalize().unwrap(),
                ],
                "repeats dedupe, both spellings parse, values canonicalize"
            );
        }
        other => panic!("expected Tui, got {other:?}"),
    }
}

#[test]
fn print_args_carry_add_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let parsed = parse_print_args(vec![
        "-p".to_string(),
        "hello".to_string(),
        "--add-dir".to_string(),
        dir.path().to_string_lossy().into_owned(),
    ]);
    match parsed.mode {
        RunMode::Print(print_args) => {
            assert_eq!(
                print_args.add_dirs,
                vec![dir.path().canonicalize().unwrap()]
            );
            assert_eq!(print_args.prompt.as_deref(), Some("hello"));
        }
        other => panic!("expected Print, got {other:?}"),
    }
}

#[test]
fn serve_accepts_add_dir_in_both_spellings() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let parsed = parse_serve_command(vec![
        "--http".to_string(),
        "--add-dir".to_string(),
        first.path().to_string_lossy().into_owned(),
        format!("--add-dir={}", second.path().to_string_lossy()),
    ]);
    match parsed.mode {
        RunMode::Serve { add_dirs, .. } => assert_eq!(
            add_dirs,
            vec![
                first.path().canonicalize().unwrap(),
                second.path().canonicalize().unwrap(),
            ],
            "serve takes the same two spellings as tui/-p"
        ),
        other => panic!("expected Serve, got {other:?}"),
    }
}

#[test]
fn tui_flags_map_to_startup_intent() {
    assert_eq!(tui_intent(&[]), StartupIntent::New);
    assert_eq!(tui_intent(&["--new"]), StartupIntent::New);
    assert_eq!(tui_intent(&["-c"]), StartupIntent::ContinueLatest);
    assert_eq!(tui_intent(&["--continue"]), StartupIntent::ContinueLatest);
    assert_eq!(tui_intent(&["-r"]), StartupIntent::ResumePicker);
    assert_eq!(tui_intent(&["--resume"]), StartupIntent::ResumePicker);
    assert_eq!(
        tui_intent(&["--resume", "session_9_0"]),
        StartupIntent::ResumeId("session_9_0".to_string())
    );
    assert_eq!(
        tui_intent(&["--resume=session_9_0"]),
        StartupIntent::ResumeId("session_9_0".to_string())
    );
    assert_eq!(tui_intent(&["--resume="]), StartupIntent::ResumePicker);
}

#[test]
fn usage_names_the_invoked_binary_not_a_hardcoded_one() {
    let text = usage_text();
    // npm installs the binary as `deepcode`, `cargo build` produces
    // `deep-code`. Any hardcoded spelling sends half the users to a command
    // that does not exist for them, so usage must interpolate argv[0].
    assert!(
        !text.contains("deep-code"),
        "usage must not hardcode a binary name: {text}"
    );
    assert!(text.contains(&program_name()));
    assert!(text.contains("--help"), "help must advertise itself");
}

#[test]
fn program_name_falls_back_when_argv0_is_unusable() {
    // Only the fallback is assertable here: argv[0] of the test harness is
    // the test binary, so the happy path is covered by the test above.
    assert!(!program_name().is_empty());
}

#[test]
fn parse_doctor_json_flag() {
    let parsed = parse_doctor_command(vec!["--json".to_string()]);
    assert_eq!(parsed.mode, RunMode::Doctor { json: true });
}

fn print_args(args: &[&str]) -> PrintArgs {
    let parsed = parse_print_args(args.iter().map(|s| (*s).to_string()).collect());
    match parsed.mode {
        RunMode::Print(print) => print,
        other => panic!("expected Print, got {other:?}"),
    }
}

#[test]
fn print_defaults_are_new_session_text_output() {
    assert_eq!(
        print_args(&["-p"]),
        PrintArgs {
            prompt: None,
            intent: StartupIntent::New,
            output: OutputFormat::Text,
            permission_mode: None,
            timeout_secs: None,
            verbose: false,
            add_dirs: Vec::new(),
        }
    );
}

#[test]
fn print_prompt_is_positional_on_either_side_of_the_flag() {
    assert_eq!(
        print_args(&["-p", "fix the bug"]).prompt.as_deref(),
        Some("fix the bug")
    );
    assert_eq!(
        print_args(&["fix the bug", "--print"]).prompt.as_deref(),
        Some("fix the bug")
    );
}

#[test]
fn print_full_flag_set_parses() {
    let print = print_args(&[
        "-p",
        "do it",
        "--output-format",
        "json",
        "--permission-mode",
        "accept_edits",
        "--timeout",
        "60",
        "--verbose",
        "-c",
    ]);
    assert_eq!(print.prompt.as_deref(), Some("do it"));
    assert_eq!(print.intent, StartupIntent::ContinueLatest);
    assert_eq!(print.output, OutputFormat::Json);
    assert_eq!(print.permission_mode, Some(PermissionMode::AcceptEdits));
    assert_eq!(print.timeout_secs, Some(60));
    assert!(print.verbose);
}

#[test]
fn print_resume_takes_an_explicit_id() {
    assert_eq!(
        print_args(&["-p", "go", "--resume", "session_9_0"]).intent,
        StartupIntent::ResumeId("session_9_0".to_string())
    );
    assert_eq!(
        print_args(&["-p", "go", "--resume=session_9_0"]).intent,
        StartupIntent::ResumeId("session_9_0".to_string())
    );
}

/// `-p` must win the routing wherever it appears among TUI-style flags,
/// while never leaking into real subcommands (those return before the
/// print check in `parse_args`).
#[test]
fn print_mode_is_detected_in_any_position() {
    assert!(wants_print(&argv(&["-p"])));
    assert!(wants_print(&argv(&["-c", "--print"])));
    assert!(wants_print(&argv(&["fix it", "-p", "--verbose"])));
    assert!(!wants_print(&argv(&["-c"])));
    assert!(!wants_print(&argv(&["session", "list"])));
}

fn argv(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_string()).collect()
}

/// `--help` past the first position used to fall into each subcommand's own
/// unknown-argument branch — usage printed to *stderr*, exit 2 — which is the
/// same defect that was fixed for the bare `--help` but only at the top level.
#[test]
fn help_is_recognized_in_any_position() {
    let argv = |args: &[&str]| args.iter().map(|a| (*a).to_string()).collect::<Vec<_>>();

    for args in [
        vec!["--help"],
        vec!["-h"],
        vec!["doctor", "--help"],
        vec!["serve", "--help"],
        vec!["session", "--help"],
        vec!["session", "list", "--help"],
        vec!["-c", "--help"],
        vec!["eval", "--subset", "lite", "--help"],
    ] {
        assert!(wants_help(&argv(&args)), "{args:?} must ask for help");
    }

    for args in [
        vec!["doctor"],
        vec!["doctor", "--json"],
        vec!["serve", "--http", "--port", "8080"],
        vec!["session", "list"],
        vec!["-c"],
        // A value that merely contains the word must not count.
        vec!["session", "resume", "help-me"],
    ] {
        assert!(!wants_help(&argv(&args)), "{args:?} must not ask for help");
    }
}
