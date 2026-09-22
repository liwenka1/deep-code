use super::*;

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

/// `--help` is English, like every other line the CLI prints.
///
/// It was the loudest exception to the rule `session_cli`'s
/// `the_age_column_is_english_like_every_other_column` states: an English
/// "Commands:" header over Chinese annotations, unreachable by `/lang` or
/// `DEEP_CODE_LANG` because it makes no `tr` call at all — so the very first
/// screen a new user saw contradicted the convention, and the test that
/// guarded the convention was scoped to one column of one subcommand.
///
/// Asserted on the whole body rather than on a phrase: a future line can only
/// regress by being non-ASCII, which is exactly the thing being ruled out.
/// (The TUI stays bilingual; this rule is for the command-line surfaces.)
#[test]
fn the_help_body_is_english_like_every_other_cli_line() {
    let usage = usage_text();
    assert!(
        usage.is_ascii(),
        "--help must not carry localized text: {usage}"
    );
    // Still a real help body, not an empty string that trivially passes.
    assert!(usage.contains("Commands:"));
    assert!(usage.contains("new session"));
    assert!(usage.contains("--add-dir"));
}

/// The npm package links `deepcode` and spawns `deepcode-bin`, so argv[0] names
/// a command that is on nobody's PATH. Every usage line, `--version` and every
/// "Try `… --help`" error printed it — the first lines a new user reads. The
/// launcher passes the name it was invoked as; a `cargo`-built binary sets
/// nothing and keeps reading argv[0].
#[test]
fn the_launcher_name_wins_over_the_spawned_binary_name() {
    assert_eq!(
        resolve_program_name(
            Some("deepcode".to_string()),
            Some("deepcode-bin".to_string())
        ),
        "deepcode"
    );
    // No override (cargo build, or a direct invocation of the binary).
    assert_eq!(
        resolve_program_name(None, Some("deep-code".to_string())),
        "deep-code"
    );
    // An empty or blank override is "not set", not "the empty name".
    for blank in ["", "   "] {
        assert_eq!(
            resolve_program_name(Some(blank.to_string()), Some("deep-code".to_string())),
            "deep-code"
        );
    }
    // Nothing to go on at all still names the shipped command.
    assert_eq!(resolve_program_name(None, None), "deepcode");
}

/// Both sources are outside this process's control and both are printed to a
/// terminal, so neither may carry an escape sequence or spend more than a word.
#[test]
fn the_program_name_is_neutralized_and_bounded() {
    let painted = resolve_program_name(Some("dee\u{1b}[2Kpcode\r".to_string()), None);
    assert!(
        !painted.chars().any(char::is_control),
        "control characters must not survive: {painted:?}"
    );
    let long = resolve_program_name(Some("x".repeat(500)), None);
    assert!(
        long.chars().count() <= 32,
        "got {} chars",
        long.chars().count()
    );
}

/// The sessions storage note is a command-line line like any other, and was the
/// last one spelling a program name of its own (`deep-code`, which npm users do
/// not have).
#[test]
fn the_sessions_storage_note_names_the_invoked_command() {
    let note =
        deep_code_agent::format_sessions_storage_note(std::path::Path::new("/tmp/ws"), "deepcode");
    assert!(note.contains("run deepcode from the same cwd"), "{note}");
    // The `.deep-code` storage directory keeps its own spelling; only the
    // command word follows the invocation.
    assert!(!note.contains("run deep-code"), "{note}");
}
