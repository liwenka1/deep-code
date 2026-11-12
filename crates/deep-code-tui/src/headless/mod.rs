//! Headless one-shot mode (`deep-code -p`): submit one prompt, run the full
//! agentic turn unattended, print the outcome, exit with a meaningful code.
//!
//! Posture matches the CI bot, not a hidden fifth permission tier: approvals
//! that would prompt are auto-denied (never parked), and capability is
//! granted through the same knobs as everywhere else — `--permission-mode`,
//! `approval.auto_allow`, `DEEP_CODE_APPROVAL_AUTO_ALLOW`. stdout carries
//! only the requested output; everything diagnostic goes to stderr, so
//! `deep-code -p … | jq` stays clean.

mod diagnostics;
mod drive;
mod input;
mod output;

#[cfg(all(test, unix))]
mod backpressure_tests;
#[cfg(test)]
mod contract_docs;
#[cfg(test)]
mod persistence_tests;

pub use output::OutputFormat;

#[cfg(all(test, unix))]
use crate::interrupt::signal_is_ignored;
use crate::interruptible_output::{Output, stderr_writer};
use deep_code_agent::CancellationToken;
use std::collections::HashMap;

use deep_code_agent::{
    AgentConfig, JsonSessionStore, PermissionMode, RuntimeEvent, SessionId, SessionRecord,
    SessionStore, launch_runtime, neutralize_display_text, now_ms,
};

use crate::cli::{PrintArgs, StartupIntent, program_name};
use diagnostics::Diagnostics;
use drive::DriveStatus;

/// The exit codes a `-p` run can end with. `pub(crate)` because `--help`
/// renders them: they are the one piece of the headless contract that has to
/// be in the help body, and a second hand-written copy of them there would be
/// free to say something the process no longer does.
pub(crate) const EXIT_OK: i32 = 0;
pub(crate) const EXIT_FAILURE: i32 = 1;
pub(crate) const EXIT_USAGE: i32 = 2;
/// Conventional shell codes: 124 mirrors GNU `timeout`, 130 mirrors SIGINT.
pub(crate) const EXIT_TIMEOUT: i32 = 124;
pub(crate) const EXIT_INTERRUPT: i32 = 130;

/// Headless runs are unattended, so an omitted timeout must still have a
/// finite wall-clock ceiling. Interactive TUI sessions may choose an
/// unlimited turn budget; a one-shot process must not inherit that setting and
/// run forever in CI or a wrapper that forgot `--timeout`.
pub(crate) const DEFAULT_HEADLESS_TIMEOUT_SECS: u64 = 900;
pub(crate) const DEFAULT_HEADLESS_TURN_STEPS: u32 = deep_code_agent::MAX_TURN_STEPS;

fn effective_headless_timeout_secs(requested: Option<u64>) -> u64 {
    requested.unwrap_or(DEFAULT_HEADLESS_TIMEOUT_SECS)
}

fn effective_headless_turn_steps(configured: u32) -> u32 {
    if configured == 0 {
        DEFAULT_HEADLESS_TURN_STEPS
    } else {
        configured
    }
}

pub async fn run_print(args: PrintArgs) -> i32 {
    let interrupted = match crate::interrupt::Interrupt::watch() {
        Ok(interrupted) => interrupted,
        Err(_) => return EXIT_FAILURE,
    };
    let diagnostics = match stderr_writer().and_then(Diagnostics::new) {
        Ok(diagnostics) => diagnostics,
        Err(_) => return EXIT_FAILURE,
    };
    let stdout = match Output::stdout(&interrupted.child_token()) {
        Ok(stdout) => stdout,
        Err(_) => return EXIT_FAILURE,
    };
    let interruption = interrupted.cancelled();
    tokio::pin!(interruption);
    let code =
        run_with_diagnostics(args, &diagnostics, &stdout, &interrupted, &mut interruption).await;
    if interrupted.is_cancelled() {
        return EXIT_INTERRUPT;
    }
    if code == EXIT_TIMEOUT {
        let _ =
            tokio::time::timeout(std::time::Duration::from_millis(200), diagnostics.flush()).await;
        return if interrupted.is_cancelled() {
            EXIT_INTERRUPT
        } else {
            code
        };
    }
    tokio::select! {
        biased;
        () = interrupted.cancelled() => EXIT_INTERRUPT,
        () = diagnostics.flush() => code,
    }
}

async fn run_with_diagnostics(
    args: PrintArgs,
    diagnostics: &Diagnostics,
    stdout: &Output,
    interrupted_token: &CancellationToken,
    interruption: &mut (impl std::future::Future<Output = ()> + Unpin),
) -> i32 {
    let has_positional = args
        .prompt
        .as_deref()
        .is_some_and(|text| !text.trim().is_empty());
    let piped = match crate::startup::blocking_io(interrupted_token, move || {
        input::read_piped_stdin(has_positional)
    })
    .await
    {
        Ok(piped) => piped,
        Err(_) => return EXIT_INTERRUPT,
    };
    let Some(prompt) = input::compose_prompt(args.prompt.as_deref(), piped.as_deref()) else {
        diagnostics.line(format!(
            "nothing to run: pass a prompt (`{} -p \"…\"`) or pipe one on stdin",
            program_name()
        ));
        return EXIT_USAGE;
    };

    let workspace = match std::env::current_dir() {
        Ok(workspace) => workspace,
        Err(error) => {
            diagnostics.line(format!("failed to resolve workspace: {error}"));
            return EXIT_FAILURE;
        }
    };
    let config_workspace = workspace.clone();
    let loaded = match crate::startup::blocking_io(interrupted_token, move || {
        AgentConfig::load(&config_workspace)
    })
    .await
    {
        Ok(loaded) => loaded,
        Err(_) => return EXIT_INTERRUPT,
    };
    // Sanitized for the same reason `trace_to_stderr` below is: under `-p`
    // stderr is a real terminal (`redirect_stderr_to_log` runs only from
    // `ui::run`). These interpolate `provider.base_url` and friends read out of
    // `<workspace>/.deep-code/config.toml`, which a repository ships — and the
    // message being concealed is the one saying a malicious repo must not
    // redirect where your credentials go.
    // English: `-p` prints English throughout and makes no `tr` call, while
    // `report.warnings` follows `ui.language` — see `ConfigLoadReport::warnings_in`.
    for warning in loaded.report.warnings_in(deep_code_agent::Lang::En) {
        emit(diagnostics, "config", &warning);
    }

    let resume = match resolve_resume_record(
        &args.intent,
        &workspace,
        &mut *interruption,
        diagnostics,
        interrupted_token,
    )
    .await
    {
        Ok(resume) => resume,
        Err(code) => return code,
    };
    // Drain startup diagnostics interruptibly before starting initialization.
    if tokio::select! {
        biased;
        () = &mut *interruption => true,
        () = diagnostics.flush() => false,
    } {
        diagnostics.line("interrupted before initialization");
        return EXIT_INTERRUPT;
    }

    // Skill/configuration reads can block. Keep polling the signal receivers
    // while initialization runs, and never start a turn from a late result.
    let roots = deep_code_agent::WorkspaceRoots::new(workspace, args.add_dirs.clone());
    let mut config = loaded.config;
    if config.turn_steps == 0 {
        emit(
            diagnostics,
            "budget",
            &format!(
                "headless mode does not allow an unlimited turn budget; using {} steps",
                DEFAULT_HEADLESS_TURN_STEPS
            ),
        );
        config.turn_steps = effective_headless_turn_steps(config.turn_steps);
    }
    let mut initializing =
        tokio::task::spawn_blocking(move || launch_runtime(&config, roots, resume));
    let launched = tokio::select! {
        biased;
        () = &mut *interruption => {
            // If this caller stays alive, clean up a runtime that finishes
            // after cancellation. The CLI itself exits with 130 immediately.
            tokio::spawn(async move {
                if let Ok(launched) = initializing.await {
                    launched.shutdown().await;
                }
            });
            diagnostics.line("interrupted: cancelling initialization");
            return EXIT_INTERRUPT;
        }
        result = &mut initializing => match result {
            Ok(launched) => launched,
            Err(error) => {
                diagnostics.line(format!("runtime initialization failed: {error}"));
                return EXIT_FAILURE;
            }
        },
    };
    // Same surface, same reason — and the same format string as the sanitized
    // one 190 lines below, which is how this one hid. These interpolate
    // `record.workspace` and `record.extra_roots[i]` straight out of the
    // session JSON, a file the model can write; the message being concealed is
    // "dropping N write grant(s)".
    for warning in &launched.warnings {
        emit(diagnostics, "warning", warning);
    }
    if launched.offline {
        diagnostics.line(
            "headless mode needs a DeepSeek API key: set DEEPSEEK_API_KEY, or store one once \
             via `/apikey` in the interactive TUI",
        );
        launched.shutdown().await;
        return EXIT_FAILURE;
    }
    if let Some(mode) = args.permission_mode {
        launched.permission_mode.set(mode);
        if mode == PermissionMode::Yolo {
            // The remaining containment depends on whether the OS sandbox is
            // actually in force: under `[sandbox] mode = "off"` it is NOT, so
            // naming it would promise a boundary that is not there — the deny
            // floor is then all that is left.
            if launched.sandbox_off {
                diagnostics.line(
                    "yolo: gated calls run without asking (a write-root request is the one \
                     exception, and unattended it is denied); [sandbox] mode = \"off\", so the \
                     OS sandbox is NOT running — the deny floor is the only remaining \
                     containment and shell/job commands otherwise run unconfined",
                );
            } else {
                diagnostics.line(
                    "yolo: gated calls run without asking (a write-root request is the one \
                     exception, and unattended it is denied); the deny floor and the OS \
                     sandbox (where available) are the remaining containment",
                );
            }
        }
    }

    if tokio::select! {
        biased;
        () = &mut *interruption => true,
        () = diagnostics.flush() => false,
    } {
        launched.shutdown().await;
        return EXIT_INTERRUPT;
    }

    run_turn(
        args,
        prompt,
        launched,
        diagnostics,
        stdout,
        interrupted_token,
    )
    .await
}

async fn run_turn(
    args: PrintArgs,
    prompt: String,
    launched: deep_code_agent::LaunchedRuntime,
    diagnostics: &Diagnostics,
    stdout: &Output,
    interrupted_token: &CancellationToken,
) -> i32 {
    let started = std::time::Instant::now();
    // An unrepresentably large timeout degrades to no deadline, never a panic.
    // Start before the first frame and retain the same budget for the result.
    let timeout_secs = effective_headless_timeout_secs(args.timeout_secs);
    let deadline = started.checked_add(std::time::Duration::from_secs(timeout_secs));
    let stdout = stdout.with_deadline(deadline);
    let interruption = interrupted_token.cancelled();
    tokio::pin!(interruption);
    // stream-json mirrors the SSE contract, including the leading
    // user.message envelope, so existing jq consumers port over unchanged.
    let mut emitter = matches!(args.output, OutputFormat::StreamJson)
        .then(|| output::NdjsonEmitter::new(format!("print_{}", now_ms()), Vec::new()));
    if let Some(emitter) = emitter.as_mut() {
        emitter.manual("user.message", serde_json::json!({ "content": prompt }));
        if let Err(error) = stdout.send(&emitter.take_pending()).await {
            launched.shutdown().await;
            return if interrupted_token.is_cancelled() {
                EXIT_INTERRUPT
            } else if deadline_reached(deadline) {
                diagnostics.line("timeout while writing the initial user.message");
                EXIT_TIMEOUT
            } else {
                emit(diagnostics, "output error", &error.to_string());
                EXIT_FAILURE
            };
        }
    }

    let handle = launched.handle.clone();
    let canceller = launched.handle.clone();
    let verbose = args.verbose;
    let mut interrupted = false;
    let mut timed_out = false;
    let output_abandoned = std::cell::Cell::new(false);

    let outcome = {
        let emitter = &mut emitter;
        let mut tool_names: HashMap<String, String> = HashMap::new();
        let mut on_event = async |event: &RuntimeEvent| {
            if let Some(emitter) = emitter.as_mut() {
                emitter.event(event);
                if let Err(error) = stdout.send(&emitter.take_pending()).await {
                    output_abandoned.set(true);
                    return Err(error);
                }
            }
            trace_to_stderr(diagnostics, event, verbose, &mut tool_names);
            Ok(())
        };

        let turn_started = std::cell::Cell::new(false);
        let drive = async {
            turn_started.set(true);
            drive::drive_to_completion(&handle, prompt.clone(), &mut on_event).await
        };
        tokio::pin!(drive);
        loop {
            tokio::select! {
                biased;
                // Cancel once, then let the driver observe the terminal event
                // or the cancelled output delivery and finalize the report.
                () = &mut interruption, if !interrupted => {
                    interrupted = true;
                    // An interrupt queued during initialization must win over
                    // the first poll of the drive (which submits the prompt).
                    if !turn_started.get() {
                        break drive::DriveOutcome {
                            status: DriveStatus::Cancelled,
                            reasoning: String::new(),
                            usage: None,
                            telemetry: None,
                            denied_approvals: 0,
                        };
                    }
                    diagnostics.line("interrupted: cancelling the turn…");
                    canceller.cancel_turn().await;
                }
                () = sleep_until(deadline), if deadline.is_some() && !timed_out && !interrupted => {
                    timed_out = true;
                    stdout.cancel();
                    // A deadline already reached before the driver's first
                    // poll must not submit a fresh, uncancelled turn afterwards.
                    if !turn_started.get() {
                        break drive::DriveOutcome {
                            status: DriveStatus::Cancelled,
                            reasoning: String::new(),
                            usage: None,
                            telemetry: None,
                            denied_approvals: 0,
                        };
                    }
                    diagnostics.line(format!(
                        "timeout after {}s: cancelling the turn…",
                        timeout_secs
                    ));
                    canceller.cancel_turn().await;
                }
                outcome = &mut drive => break outcome,
            }
        }
    };

    // Shut down tools and flush the final save before declaring success or
    // waiting for a final consumer that may never read again.
    let session_id = launched.session_id.clone();
    launched.shutdown().await;
    let save_error = handle
        .last_save_error()
        .map(|error| format!("session could not be saved: {error}"));
    // Output delivery can observe its deadline before the driver's timer arm.
    timed_out |= deadline_reached(deadline);
    interrupted |= interrupted_token.is_cancelled();
    let drive_status = if timed_out || (output_abandoned.get() && interrupted) {
        if let Some(error) = &save_error {
            emit(diagnostics, "error", error);
        }
        DriveStatus::Cancelled
    } else if let Some(error) = &save_error {
        if matches!(outcome.status, DriveStatus::Finished) {
            DriveStatus::Failed(error.clone())
        } else {
            emit(diagnostics, "error", error);
            outcome.status.clone()
        }
    } else {
        outcome.status.clone()
    };
    let (status, exit_code, error) = classify(&drive_status, interrupted, timed_out, &args);
    if let Some(message) = &error {
        // `RuntimeEvent::Error` carries tool failures, which quote the paths
        // and commands the model chose. `trace_to_stderr` does not handle that
        // variant, so this is its only surfacing point — while the TUI status
        // row sanitizes the identical value.
        emit(diagnostics, "error", message);
    }

    // The answer is read from the session, not reassembled from deltas: the
    // last assistant message with visible content is authoritative and keeps
    // the narration the model emitted between tool calls out of the result
    // (see `deep_code_agent::last_assistant_text`).
    let result = if matches!(drive_status, DriveStatus::Finished) {
        deep_code_agent::last_assistant_text(&handle.session_messages().await).unwrap_or_default()
    } else {
        String::new()
    };

    let report = output::PrintReport {
        status,
        result,
        reasoning: outcome.reasoning,
        error,
        session_id,
        denied_approvals: outcome.denied_approvals,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        cost: outcome
            .telemetry
            .as_ref()
            .map(|telemetry| telemetry.turn_cost),
        usage: outcome.usage,
        telemetry: outcome.telemetry,
    };

    // Cancellation reports get a bounded best-effort write.
    let final_stdout = if interrupted || timed_out {
        stdout.cleanup()
    } else {
        stdout.clone()
    };
    let bytes = match args.output {
        OutputFormat::StreamJson => {
            if let (Some(emitter), Ok(payload)) = (emitter.as_mut(), serde_json::to_value(&report))
            {
                emitter.manual("print.result", payload);
                emitter.take_pending()
            } else {
                Vec::new()
            }
        }
        OutputFormat::Json => format!("{}\n", output::report_to_json(&report)).into_bytes(),
        OutputFormat::Text => {
            if !report.result.is_empty() {
                format!("{}\n", report.result).into_bytes()
            } else {
                if report.status == "finished" {
                    diagnostics.line("(the turn finished without assistant text)");
                }
                Vec::new()
            }
        }
    };
    // Never append a new JSON envelope after cancelling a partial frame.
    let delivered = if output_abandoned.get() {
        Ok(())
    } else {
        final_stdout.send(&bytes).await
    };
    if let Some(session_id) = &report.session_id
        && save_error.is_none()
    {
        diagnostics.line(format!(
            "session: {session_id}  (continue interactively with `{} -c`)",
            program_name()
        ));
    }

    if interrupted_token.is_cancelled() {
        return EXIT_INTERRUPT;
    }
    if timed_out || deadline_reached(deadline) {
        diagnostics.line("timeout while completing the turn output");
        return EXIT_TIMEOUT;
    }
    if let Err(error) = &delivered {
        emit(diagnostics, "output error", &error.to_string());
    }
    let code = if delivered.is_err() && exit_code == EXIT_OK {
        EXIT_FAILURE
    } else {
        exit_code
    };
    // stderr is delivered by a separate writer too. Its final acknowledgement
    // must share the turn's budget rather than parking exit on a full pipe.
    tokio::select! {
        biased;
        () = interrupted_token.cancelled() => EXIT_INTERRUPT,
        () = sleep_until(deadline) => EXIT_TIMEOUT,
        () = diagnostics.flush() => code,
    }
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
                effective_headless_timeout_secs(args.timeout_secs)
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
fn emit(diagnostics: &Diagnostics, prefix: &str, text: &str) {
    diagnostics.line(emit_line(prefix, text));
}

/// The pure half of [`emit`], so the sanitizing can be asserted.
fn emit_line(prefix: &str, text: &str) -> String {
    format!("{prefix}: {}", neutralize_display_text(text))
}

/// One stderr line per notable event. Approval denials always print — they
/// are the honest answer to "why didn't it edit anything"; tool traffic only
/// with `--verbose`.
fn trace_to_stderr(
    diagnostics: &Diagnostics,
    event: &RuntimeEvent,
    verbose: bool,
    tool_names: &mut HashMap<String, String>,
) {
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
        RuntimeEvent::Warning { message } => {
            diagnostics.line(format!("warning: {}", clean(message)))
        }
        RuntimeEvent::ApprovalRequired { request, .. } => {
            diagnostics.line(format!(
                "approval auto-denied: {} — {}",
                clean(&request.tool_name),
                clean(&request.description)
            ));
        }
        RuntimeEvent::ToolCallStarted {
            tool_call_id,
            tool_name,
            ..
        } if verbose => {
            tool_names.insert(tool_call_id.as_str().to_string(), tool_name.clone());
            diagnostics.line(format!("→ {}", clean(tool_name)));
        }
        RuntimeEvent::ToolCallFinished { tool_call_id, .. } if verbose => {
            let name = tool_names
                .remove(tool_call_id.as_str())
                .unwrap_or_else(|| "tool".to_string());
            diagnostics.line(format!("← {}", clean(&name)));
        }
        _ => {}
    }
}

/// Resolve which stored session (if any) this run continues.
/// `Err` carries the process exit code.
async fn resolve_resume_record(
    intent: &StartupIntent,
    workspace: &std::path::Path,
    interruption: impl std::future::Future<Output = ()>,
    diagnostics: &Diagnostics,
    interrupted: &CancellationToken,
) -> Result<Option<SessionRecord>, i32> {
    match intent {
        StartupIntent::New => Ok(None),
        StartupIntent::ResumePicker => {
            diagnostics
                .line("the session picker is interactive; use `--resume <id>` (or `-c`) with -p");
            Err(EXIT_USAGE)
        }
        StartupIntent::ContinueLatest => {
            let store = open_store(workspace, diagnostics, interrupted).await?;
            diagnostics.line("scanning saved sessions… (Ctrl+C to cancel)");
            let listed = match crate::session_scan::SessionScan::start(store) {
                Ok(scan) => crate::session_scan::wait_with_interrupt(scan, interruption).await,
                Err(error) => Err(error),
            };
            match listed {
                // Newest first; skip sessions with no user message, exactly
                // like the TUI's `-c` (an empty session is not "the latest").
                Ok(listing) => {
                    for line in listing.diagnostics {
                        emit(diagnostics, "session", &line);
                    }
                    Ok(listing
                        .records
                        .into_iter()
                        .find(crate::startup::has_user_message))
                }
                Err(deep_code_agent::SessionStoreError::Cancelled) => {
                    diagnostics.line("session scan cancelled");
                    Err(EXIT_INTERRUPT)
                }
                Err(error) => {
                    diagnostics.line(format!("cannot list sessions: {error}"));
                    Err(EXIT_FAILURE)
                }
            }
        }
        StartupIntent::ResumeId(id) => {
            let store = open_store(workspace, diagnostics, interrupted).await?;
            let session_id = match SessionId::parse(id) {
                Ok(session_id) => session_id,
                Err(error) => {
                    diagnostics.line(format!("invalid session id '{id}': {error}"));
                    return Err(EXIT_FAILURE);
                }
            };
            let loaded = crate::startup::blocking_io(interrupted, move || store.load(&session_id))
                .await
                .map_err(|_| EXIT_INTERRUPT)?;
            match loaded {
                Ok(record) => Ok(Some(record)),
                Err(error) => {
                    diagnostics.line(format!("cannot resume '{id}': {error}"));
                    Err(EXIT_FAILURE)
                }
            }
        }
    }
}

async fn open_store(
    workspace: &std::path::Path,
    diagnostics: &Diagnostics,
    interrupted: &CancellationToken,
) -> Result<JsonSessionStore, i32> {
    let workspace = workspace.to_path_buf();
    crate::startup::blocking_io(interrupted, move || {
        JsonSessionStore::for_workspace(workspace)
    })
    .await
    .map_err(|_| EXIT_INTERRUPT)?
    .map_err(|error| {
        diagnostics.line(format!("session storage unavailable: {error}"));
        EXIT_FAILURE
    })
}

/// Total future for the optional wall-clock deadline; pends forever when no
/// timeout was requested (the select! guard also keeps it unpolled then).
fn deadline_reached(deadline: Option<std::time::Instant>) -> bool {
    deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline)
}

async fn sleep_until(deadline: Option<std::time::Instant>) {
    match deadline {
        // Tokio's timer wheel can defer even an already elapsed deadline to
        // its next tick. That must not let the driver's first poll submit work.
        Some(deadline) if std::time::Instant::now() >= deadline => {}
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_timeout_defaults_to_a_finite_wall_clock() {
        assert_eq!(
            effective_headless_timeout_secs(None),
            DEFAULT_HEADLESS_TIMEOUT_SECS
        );
        assert_eq!(effective_headless_timeout_secs(Some(42)), 42);
    }

    #[test]
    fn headless_budget_replaces_unlimited_configuration() {
        assert_eq!(
            effective_headless_turn_steps(0),
            DEFAULT_HEADLESS_TURN_STEPS
        );
        assert_eq!(effective_headless_turn_steps(7), 7);
    }

    #[tokio::test]
    async fn deadlines_cover_unstarted_turns_and_final_diagnostic_flushes() {
        use std::io::Write;
        use std::sync::mpsc;
        use std::time::Duration;

        struct StalledFlush {
            ready: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
        }
        impl Write for StalledFlush {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                let _ = self.ready.send(());
                let _ = self.release.recv_timeout(Duration::from_secs(5));
                Ok(())
            }
        }
        for before_turn in [true, false] {
            let workspace = tempfile::tempdir().unwrap();
            let launched = launch_runtime(
                &AgentConfig::builtin(),
                workspace.path().to_path_buf(),
                None,
            );
            assert!(launched.offline);
            let handle = launched.handle.clone();
            let token = CancellationToken::new();
            let output = Output::new(std::io::sink(), &token.child_token()).unwrap();
            let (ready_tx, ready) = mpsc::channel();
            let (release, blocked) = mpsc::channel();
            let diagnostics = Diagnostics::new(StalledFlush {
                ready: ready_tx,
                release: blocked,
            })
            .unwrap();
            let args = PrintArgs {
                prompt: None,
                intent: StartupIntent::New,
                output: OutputFormat::Json,
                permission_mode: None,
                timeout_secs: Some(if before_turn { 0 } else { 1 }),
                verbose: false,
                add_dirs: Vec::new(),
            };
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                run_turn(
                    args,
                    "hello".into(),
                    launched,
                    &diagnostics,
                    &output,
                    &token,
                ),
            )
            .await;
            let _ = release.send(());
            assert_eq!(result.unwrap(), EXIT_TIMEOUT);
            let submitted = handle
                .session_messages()
                .await
                .iter()
                .any(|message| message.role == deep_code_agent::Role::User);
            assert_eq!(
                submitted, !before_turn,
                "an expired deadline must not submit a prompt"
            );
            if !before_turn {
                ready
                    .try_recv()
                    .expect("the final diagnostic flush was reached");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn initialization_stays_interruptible_after_continue_scan() {
        use std::os::unix::fs::OpenOptionsExt;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        const CHILD: &str = "DEEP_CODE_HEADLESS_SIGNAL_TEST";
        if let Ok(intent) = std::env::var(CHILD) {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let code = runtime.block_on(run_print(PrintArgs {
                prompt: Some("must not start a turn".to_string()),
                intent: if intent == "continue" {
                    StartupIntent::ContinueLatest
                } else {
                    StartupIntent::New
                },
                output: OutputFormat::StreamJson,
                permission_mode: None,
                timeout_secs: None,
                verbose: false,
                add_dirs: Vec::new(),
            }));
            // Match main: exit without waiting for blocked initialization I/O.
            std::process::exit(code);
        }

        for intent in ["new", "continue"] {
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
                let workspace = tempfile::tempdir().unwrap();
                let skill = workspace.path().join("skills/blocked");
                std::fs::create_dir_all(&skill).unwrap();
                let path = skill.join("SKILL.md");
                let fifo = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
                // SAFETY: valid NUL-terminated path in this test's workspace.
                assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
                let mut child = Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "headless::tests::initialization_stays_interruptible_after_continue_scan",
                    ])
                    .env(CHILD, intent)
                    .current_dir(workspace.path())
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                // Opening the writer succeeds only once skill discovery has
                // opened the reader. Keep it open without data to block init.
                let writer = loop {
                    if let Ok(writer) = std::fs::OpenOptions::new()
                        .write(true)
                        .custom_flags(libc::O_NONBLOCK)
                        .open(&path)
                    {
                        break writer;
                    }
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let output = child.wait_with_output().unwrap();
                        panic!("initialization never reached skill loading: {output:?}");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                };
                // SAFETY: the PID belongs to this test's live child process.
                assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, signal) }, 0);
                let deadline = Instant::now() + Duration::from_secs(5);
                while child.try_wait().unwrap().is_none() {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("{intent}: signal {signal} was ignored during initialization");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                let output = child.wait_with_output().unwrap();
                drop(writer);
                assert_eq!(output.status.code(), Some(EXIT_INTERRUPT), "{output:?}");
                assert!(!String::from_utf8_lossy(&output.stdout).contains("user.message"));
            }
        }
    }

    /// `signal_is_ignored` must report an ignored disposition truthfully, so a
    /// `nohup`'d run leaves SIGHUP alone. Use a child so concurrently spawned
    /// regression processes cannot accidentally inherit a temporary SIG_IGN.
    #[cfg(unix)]
    #[test]
    fn detects_an_ignored_sighup() {
        const CHILD: &str = "DEEP_CODE_IGNORED_HUP_TEST";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "headless::tests::detects_an_ignored_sighup"])
                .env(CHILD, "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        assert!(
            !signal_is_ignored(libc::SIGHUP),
            "precondition: SIGHUP is not ignored by default under the test harness"
        );
        // SAFETY: standard signal(2) set/restore around a single assertion.
        unsafe {
            let previous = libc::signal(libc::SIGHUP, libc::SIG_IGN);
            let observed = signal_is_ignored(libc::SIGHUP);
            libc::signal(libc::SIGHUP, previous);
            assert!(observed, "an ignored SIGHUP must be detected");
        }
    }

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
