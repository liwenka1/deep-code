//! Headless one-shot mode (`deep-code -p`): submit one prompt, run the full
//! agentic turn unattended, print the outcome, exit with a meaningful code.
//!
//! Posture matches the CI bot, not a hidden fifth permission tier: approvals
//! that would prompt are auto-denied (never parked), and capability is
//! granted through the same knobs as everywhere else — `--permission-mode`,
//! `approval.auto_allow`, `DEEP_CODE_APPROVAL_AUTO_ALLOW`. stdout carries
//! only the requested output; everything diagnostic goes to stderr, so
//! `deep-code -p … | jq` stays clean.

mod drive;
mod input;
mod output;

pub use output::OutputFormat;

use std::collections::HashMap;

use deep_code_agent::{
    AgentConfig, JsonSessionStore, PermissionMode, RuntimeEvent, SessionId, SessionRecord,
    SessionStore, launch_runtime, neutralize_display_text, now_ms,
};

use crate::cli::{PrintArgs, StartupIntent, program_name, workspace_root};
use drive::DriveStatus;

const EXIT_OK: i32 = 0;
const EXIT_FAILURE: i32 = 1;
const EXIT_USAGE: i32 = 2;
/// Conventional shell codes: 124 mirrors GNU `timeout`, 130 mirrors SIGINT.
const EXIT_TIMEOUT: i32 = 124;
const EXIT_INTERRUPT: i32 = 130;

pub async fn run_print(args: PrintArgs) -> i32 {
    let Some(prompt) =
        input::compose_prompt(args.prompt.as_deref(), input::read_piped_stdin().as_deref())
    else {
        eprintln!(
            "nothing to run: pass a prompt (`{} -p \"…\"`) or pipe one on stdin",
            program_name()
        );
        return EXIT_USAGE;
    };

    let workspace = workspace_root();
    let loaded = AgentConfig::load(&workspace);
    // Sanitized for the same reason `trace_to_stderr` below is: under `-p`
    // stderr is a real terminal (`redirect_stderr_to_log` runs only from
    // `ui::run`). These interpolate `provider.base_url` and friends read out of
    // `<workspace>/.deep-code/config.toml`, which a repository ships — and the
    // message being concealed is the one saying a malicious repo must not
    // redirect where your credentials go.
    for warning in &loaded.report.warnings {
        emit("config", warning);
    }

    let resume = match resolve_resume_record(&args.intent, &workspace) {
        Ok(resume) => resume,
        Err(code) => return code,
    };

    let launched = launch_runtime(
        &loaded.config,
        deep_code_agent::WorkspaceRoots::new(workspace, args.add_dirs.clone()),
        resume,
    );
    // Same surface, same reason — and the same format string as the sanitized
    // one 190 lines below, which is how this one hid. These interpolate
    // `record.workspace` and `record.extra_roots[i]` straight out of the
    // session JSON, a file the model can write; the message being concealed is
    // "dropping N write grant(s)".
    for warning in &launched.warnings {
        emit("warning", warning);
    }
    if launched.offline {
        eprintln!(
            "headless mode needs a DeepSeek API key: set DEEPSEEK_API_KEY, or store one once \
             via `/apikey` in the interactive TUI"
        );
        launched.shutdown().await;
        return EXIT_FAILURE;
    }
    if let Some(mode) = args.permission_mode {
        launched.permission_mode.set(mode);
        if mode == PermissionMode::Yolo {
            eprintln!(
                "yolo: gated calls run without asking; the deny floor and the OS sandbox \
                 (where available) are the remaining containment"
            );
        }
    }

    // stream-json mirrors the SSE contract, including the leading
    // user.message envelope, so existing jq consumers port over unchanged.
    let mut emitter = matches!(args.output, OutputFormat::StreamJson)
        .then(|| output::NdjsonEmitter::new(format!("print_{}", now_ms()), std::io::stdout()));
    if let Some(emitter) = emitter.as_mut() {
        emitter.manual("user.message", serde_json::json!({ "content": prompt }));
    }

    let handle = launched.handle.clone();
    let canceller = launched.handle.clone();
    let verbose = args.verbose;
    let started = std::time::Instant::now();
    let mut interrupted = false;
    let mut timed_out = false;

    let outcome = {
        let emitter = &mut emitter;
        let mut tool_names: HashMap<String, String> = HashMap::new();
        let mut on_event = |event: &RuntimeEvent| {
            if let Some(emitter) = emitter.as_mut() {
                emitter.event(event);
            }
            trace_to_stderr(event, verbose, &mut tool_names);
        };

        let deadline = args
            .timeout_secs
            .map(|secs| tokio::time::Instant::now() + std::time::Duration::from_secs(secs));
        let drive = drive::drive_to_completion(&handle, prompt.clone(), &mut on_event);
        tokio::pin!(drive);
        loop {
            tokio::select! {
                outcome = &mut drive => break outcome,
                // First interrupt cancels the turn and lets the loop finalize
                // (the cancel arm kills tool process groups); the guard keeps
                // a second press from re-cancelling instead of exiting.
                _ = tokio::signal::ctrl_c(), if !interrupted => {
                    interrupted = true;
                    eprintln!("interrupted: cancelling the turn…");
                    canceller.cancel_turn().await;
                }
                () = sleep_until(deadline), if deadline.is_some() && !timed_out && !interrupted => {
                    timed_out = true;
                    eprintln!(
                        "timeout after {}s: cancelling the turn…",
                        args.timeout_secs.unwrap_or(0)
                    );
                    canceller.cancel_turn().await;
                }
            }
        }
    };

    let (status, exit_code, error) = classify(&outcome.status, interrupted, timed_out, &args);
    if let Some(message) = &error {
        // `RuntimeEvent::Error` carries tool failures, which quote the paths
        // and commands the model chose. `trace_to_stderr` does not handle that
        // variant, so this is its only surfacing point — while the TUI status
        // row sanitizes the identical value.
        emit("error", message);
    }

    // The answer is read from the session, not reassembled from deltas:
    // the last assistant message is authoritative and skips mid-turn
    // narration (see `drive::final_assistant_text`).
    let result = if matches!(outcome.status, DriveStatus::Finished) {
        drive::final_assistant_text(&handle.session_messages().await).unwrap_or_default()
    } else {
        String::new()
    };

    let report = output::PrintReport {
        status,
        result,
        reasoning: outcome.reasoning,
        error,
        session_id: launched.session_id.clone(),
        denied_approvals: outcome.denied_approvals,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        cost: outcome
            .telemetry
            .as_ref()
            .map(|telemetry| telemetry.turn_cost),
        usage: outcome.usage,
        telemetry: outcome.telemetry,
    };

    match args.output {
        OutputFormat::StreamJson => {
            if let (Some(emitter), Ok(payload)) = (emitter.as_mut(), serde_json::to_value(&report))
            {
                emitter.manual("print.result", payload);
            }
        }
        OutputFormat::Json => println!("{}", output::report_to_json(&report)),
        OutputFormat::Text => {
            if !report.result.is_empty() {
                println!("{}", report.result);
            } else if report.status == "finished" {
                eprintln!("(the turn finished without assistant text)");
            }
        }
    }
    if let Some(session_id) = &report.session_id {
        eprintln!(
            "session: {session_id}  (continue interactively with `{} -c`)",
            program_name()
        );
    }

    launched.shutdown().await;
    exit_code
}

/// Map the drive status plus what *we* did (interrupt, timeout) to the
/// report label and process exit code. `Incomplete` without either flag is a
/// defect worth reporting loudly, not a silent zero.
fn classify(
    status: &DriveStatus,
    interrupted: bool,
    timed_out: bool,
    args: &PrintArgs,
) -> (&'static str, i32, Option<String>) {
    match status {
        DriveStatus::Finished => ("finished", EXIT_OK, None),
        DriveStatus::Failed(message) => ("error", EXIT_FAILURE, Some(message.clone())),
        DriveStatus::Cancelled | DriveStatus::Incomplete if timed_out => (
            "timeout",
            EXIT_TIMEOUT,
            Some(format!(
                "turn cancelled after the {}s timeout",
                args.timeout_secs.unwrap_or(0)
            )),
        ),
        DriveStatus::Cancelled | DriveStatus::Incomplete if interrupted => {
            ("cancelled", EXIT_INTERRUPT, Some("interrupted".to_string()))
        }
        DriveStatus::Cancelled => (
            "cancelled",
            EXIT_FAILURE,
            Some("the turn was cancelled".to_string()),
        ),
        DriveStatus::Incomplete => (
            "error",
            EXIT_FAILURE,
            Some("the event stream ended without a terminal event".to_string()),
        ),
    }
}

/// The one way this module writes a decorated line to stderr.
///
/// A choke point rather than four `eprintln!`s, because four `eprintln!`s is
/// precisely what went wrong: `trace_to_stderr` was sanitized while three
/// siblings using the *same format string* sat 190 lines above it, untouched.
/// Under `-p` stderr is a real terminal — `redirect_stderr_to_log` runs only
/// from `ui::run` — so every one of them could conceal the lines after it.
///
/// Honest about what pins this: the test below covers the sanitizing, not the
/// wiring. Nothing stops a future caller from reaching for `eprintln!` again;
/// what this buys is that there is now an obvious right way to do it.
fn emit(prefix: &str, text: &str) {
    eprintln!("{}", emit_line(prefix, text));
}

/// The pure half of [`emit`], so the sanitizing can be asserted.
fn emit_line(prefix: &str, text: &str) -> String {
    format!("{prefix}: {}", neutralize_display_text(text))
}

/// One stderr line per notable event. Approval denials always print — they
/// are the honest answer to "why didn't it edit anything"; tool traffic only
/// with `--verbose`.
fn trace_to_stderr(event: &RuntimeEvent, verbose: bool, tool_names: &mut HashMap<String, String>) {
    // Every field below is model-chosen, and in `-p` mode stderr is a real
    // terminal: `redirect_stderr_to_log` is called from `ui::run` only, so the
    // TUI's protection does not extend here. An `\x1b[8m` in a hallucinated
    // tool name conceals everything printed after it — including the
    // `approval auto-denied` line, which is the honest answer to "why did it
    // not edit anything". Same data the status row was just sanitized for, on
    // the same surface class.
    //
    // Only this decoration is filtered. `--output-format`'s payload on STDOUT
    // stays verbatim, because that is a pipe contract and the caller may be
    // parsing it; nobody pipes `→ read_file`.
    let clean = neutralize_display_text;
    match event {
        RuntimeEvent::Warning { message } => eprintln!("warning: {}", clean(message)),
        RuntimeEvent::ApprovalRequired { request, .. } => {
            eprintln!(
                "approval auto-denied: {} — {}",
                clean(&request.tool_name),
                clean(&request.description)
            );
        }
        RuntimeEvent::ToolCallStarted {
            tool_call_id,
            tool_name,
            ..
        } if verbose => {
            tool_names.insert(tool_call_id.as_str().to_string(), tool_name.clone());
            eprintln!("→ {}", clean(tool_name));
        }
        RuntimeEvent::ToolCallFinished { tool_call_id, .. } if verbose => {
            let name = tool_names
                .remove(tool_call_id.as_str())
                .unwrap_or_else(|| "tool".to_string());
            eprintln!("← {}", clean(&name));
        }
        _ => {}
    }
}

/// Resolve which stored session (if any) this run continues.
/// `Err` carries the process exit code.
fn resolve_resume_record(
    intent: &StartupIntent,
    workspace: &std::path::Path,
) -> Result<Option<SessionRecord>, i32> {
    match intent {
        StartupIntent::New => Ok(None),
        StartupIntent::ResumePicker => {
            eprintln!("the session picker is interactive; use `--resume <id>` (or `-c`) with -p");
            Err(EXIT_USAGE)
        }
        StartupIntent::ContinueLatest => {
            let store = open_store(workspace)?;
            match store.list() {
                // Newest first; skip sessions with no user message, exactly
                // like the TUI's `-c` (an empty session is not "the latest").
                Ok(records) => Ok(records.into_iter().find(crate::startup::has_user_message)),
                Err(error) => {
                    eprintln!("cannot list sessions: {error}");
                    Err(EXIT_FAILURE)
                }
            }
        }
        StartupIntent::ResumeId(id) => {
            let store = open_store(workspace)?;
            let session_id = match SessionId::parse(id) {
                Ok(session_id) => session_id,
                Err(error) => {
                    eprintln!("invalid session id '{id}': {error}");
                    return Err(EXIT_FAILURE);
                }
            };
            match store.load(&session_id) {
                Ok(record) => Ok(Some(record)),
                Err(error) => {
                    eprintln!("cannot resume '{id}': {error}");
                    Err(EXIT_FAILURE)
                }
            }
        }
    }
}

fn open_store(workspace: &std::path::Path) -> Result<JsonSessionStore, i32> {
    JsonSessionStore::for_workspace(workspace).map_err(|error| {
        eprintln!("session storage unavailable: {error}");
        EXIT_FAILURE
    })
}

/// Total future for the optional wall-clock deadline; pends forever when no
/// timeout was requested (the select! guard also keeps it unpolled then).
async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Under `-p` stderr is a real terminal, so a `\x1b[8m` in any of these
    /// lines conceals everything drawn after it — including the
    /// "dropping N write grant(s)" and "a malicious repo must not redirect
    /// where your credentials go" notices, which are exactly the lines an
    /// attacker would want hidden. All three sources are attacker-influenced:
    /// config values come from a repo-shipped `.deep-code/config.toml`, launch
    /// warnings interpolate paths out of the model-writable session record,
    /// and the error line carries the model's own tool names and paths.
    #[test]
    fn every_stderr_line_is_neutralized() {
        for prefix in ["config", "warning", "error"] {
            let line = emit_line(prefix, "a\u{1b}[8mb\u{202e}c\u{2028}d");
            assert!(
                !line.chars().any(|ch| ch.is_control()),
                "{prefix}: a control character reached stderr: {line:?}"
            );
            assert!(
                !line.contains('\u{202e}') && !line.contains('\u{2028}'),
                "{prefix}: an invisible code point reached stderr: {line:?}"
            );
            assert!(
                line.starts_with(prefix),
                "the prefix must survive: {line:?}"
            );
            assert!(line.contains('d'), "the text must survive: {line:?}");
        }
    }
}
