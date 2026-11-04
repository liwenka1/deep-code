use super::*;
use crate::cli::workspace_root;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn pipe() -> (File, File) {
    let mut fds = [-1; 2];
    // SAFETY: pipe initializes two owned descriptors in this array.
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    // SAFETY: each descriptor is transferred to exactly one File.
    unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) }
}

fn fill_pipe(reader: &mut File, writer: &mut File, leave_free: usize) -> usize {
    // SAFETY: fcntl operates on the live write descriptor. The child is either
    // not started yet or waiting in skill I/O with no stderr writes pending.
    assert_eq!(
        unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
        0
    );
    let mut filled = 0;
    // Also fill the tail: a short cancellation notice must not fit in the
    // remainder left after a nonblocking PIPE_BUF-sized write is refused.
    for chunk in [&[b'x'; 4096][..], &b"x"[..]] {
        loop {
            match writer.write(chunk) {
                Ok(count) => filled += count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                other => panic!("cannot fill stderr: {other:?}"),
            }
        }
    }
    reader.read_exact(&mut vec![0; leave_free]).unwrap();
    // SAFETY: restore blocking writes before the code under test writes stderr.
    assert_eq!(
        unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, 0) },
        0
    );
    filled
}

fn wait_until(child: &mut Child, phase: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let status = child.wait().unwrap();
            panic!("{phase} did not complete: {status}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn signals_interrupt_blocked_diagnostics_and_cancel_notices() {
    const CHILD: &str = "DEEP_CODE_HEADLESS_STDERR_TEST";
    if let Ok(phase) = std::env::var(CHILD) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let code = runtime.block_on(run_print(PrintArgs {
            prompt: Some("must not start a turn".into()),
            intent: if phase == "final_flush" {
                StartupIntent::ResumeId("!".repeat(8192))
            } else {
                StartupIntent::ContinueLatest
            },
            output: OutputFormat::StreamJson,
            permission_mode: None,
            timeout_secs: None,
            verbose: false,
            add_dirs: Vec::new(),
        }));
        // Match main, including exit while the diagnostic writer is blocked.
        std::process::exit(code);
    }

    for phase in ["post_scan", "cancel_notice", "final_flush"] {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            let workspace = tempfile::tempdir().unwrap();
            let sessions = workspace.path().join(".deep-code/sessions");
            std::fs::create_dir_all(&sessions).unwrap();
            let fifo = workspace.path().join("skills/blocked/SKILL.md");
            let (mut reader, mut writer) = pipe();
            let prefilled = if phase != "cancel_notice" {
                if phase == "post_scan" {
                    for index in 0..32 {
                        std::fs::write(
                            sessions.join(format!("{index:02}_{}.json", "a".repeat(125))),
                            "{",
                        )
                        .unwrap();
                    }
                }
                fill_pipe(&mut reader, &mut writer, 4096)
            } else {
                std::fs::create_dir_all(fifo.parent().unwrap()).unwrap();
                let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
                // SAFETY: valid path inside this test's temporary workspace.
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
                0
            };
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "headless::backpressure_tests::signals_interrupt_blocked_diagnostics_and_cancel_notices", "--nocapture"])
                .env(CHILD, phase)
                .current_dir(workspace.path())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(writer.try_clone().unwrap())
                .spawn().unwrap();
            let mut held_skill = None;
            if phase != "cancel_notice" {
                wait_until(&mut child, phase, || {
                    let mut queued: libc::c_int = 0;
                    // SAFETY: observe the pipe without draining or unblocking it.
                    assert_eq!(
                        unsafe { libc::ioctl(reader.as_raw_fd(), libc::FIONREAD, &mut queued) },
                        0
                    );
                    queued as usize >= prefilled - 4096 + 2048
                });
            } else {
                wait_until(&mut child, "skill loading", || {
                    held_skill = std::fs::OpenOptions::new()
                        .write(true)
                        .custom_flags(libc::O_NONBLOCK)
                        .open(&fifo)
                        .ok();
                    held_skill.is_some()
                });
                // Now the scan has finished and initialization is blocked.
                // Leave no room at all for the cancellation notice.
                fill_pipe(&mut reader, &mut writer, 0);
            }
            // SAFETY: signal only this test's child, with stderr still undrained.
            assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, signal) }, 0);
            let deadline = Instant::now() + Duration::from_secs(5);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{phase}: signal {signal} could not stop blocked stderr output");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let output = child.wait_with_output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(EXIT_INTERRUPT),
                "{phase}: {output:?}"
            );
            assert!(!String::from_utf8_lossy(&output.stdout).contains("user.message"));
            drop(held_skill);
        }
    }
}

/// A local model exercises real turn execution without network or credentials.
struct LargeReply;

#[async_trait::async_trait]
impl deep_code_agent::LlmClient for LargeReply {
    fn provider_name(&self) -> &'static str {
        "test"
    }
    fn model(&self) -> &str {
        "test"
    }
    async fn stream_chat(
        &self,
        _: deep_code_agent::ChatRequest,
    ) -> deep_code_agent::AgentResult<deep_code_agent::AgentEventStream> {
        std::fs::write("turn-requested", "yes").unwrap();
        Ok(Box::pin(async_stream::try_stream! {
            yield deep_code_agent::AgentEvent::TextDelta { text: "answer".repeat(128 * 1024) };
            yield deep_code_agent::AgentEvent::Done { usage: None };
        }))
    }
}

/// Earlier frames are discarded so each case blocks exactly its target phase
/// on a real, undrained OS pipe (rather than accidentally blocking the header).
struct PhaseWriter<W> {
    out: W,
    target: &'static str,
    reached: bool,
}
impl<W: Write> Write for PhaseWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if !self.reached
            && (self.target == "all"
                || bytes
                    .windows(self.target.len())
                    .any(|part| part == self.target.as_bytes()))
        {
            self.reached = true;
            std::fs::write("output-blocked", "yes").unwrap();
        }
        if self.reached {
            self.out.write(bytes)
        } else {
            Ok(bytes.len())
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

#[test]
fn signals_and_timeout_interrupt_all_stdout_phases() {
    const CHILD: &str = "DEEP_CODE_STDOUT_PHASE_TEST";
    if let Ok(phase) = std::env::var(CHILD) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let code = runtime.block_on(async {
            let interrupt = crate::interrupt::Interrupt::watch().unwrap();
            let target = match phase.as_str() {
                "initial" => "user.message",
                "events" | "timeout" => "assistant.delta",
                "stream_result" => "print.result",
                _ => "all",
            };
            let output = Output::new(
                PhaseWriter {
                    out: crate::interruptible_output::stdout_writer().unwrap(),
                    target,
                    reached: false,
                },
                &interrupt.child_token(),
            )
            .unwrap();
            let diagnostics = Diagnostics::new(std::io::sink()).unwrap();
            let mut launched = launch_runtime(
                &AgentConfig::default(),
                deep_code_agent::WorkspaceRoots::new(workspace_root(), Vec::new()),
                None,
            );
            // Keep the normal shutdown hooks, substituting only a local model.
            let previous = std::mem::replace(
                &mut launched.handle,
                std::sync::Arc::new(deep_code_agent::AgentRuntime::new(
                    LargeReply,
                    deep_code_agent::ToolRegistry::new(),
                )),
            );
            previous.shutdown().await;
            let args = PrintArgs {
                prompt: None,
                intent: StartupIntent::New,
                output: match phase.as_str() {
                    "text_result" => OutputFormat::Text,
                    "json_result" => OutputFormat::Json,
                    _ => OutputFormat::StreamJson,
                },
                permission_mode: None,
                timeout_secs: (phase == "timeout").then_some(1),
                verbose: false,
                add_dirs: Vec::new(),
            };
            let prompt = if phase == "initial" {
                "prompt".repeat(128 * 1024)
            } else {
                "hi".into()
            };
            run_turn(args, prompt, launched, &diagnostics, &output, &interrupt).await
        });
        std::process::exit(code);
    }

    for phase in [
        "initial",
        "events",
        "stream_result",
        "json_result",
        "text_result",
        "timeout",
    ] {
        let signals = if phase == "timeout" {
            vec![0]
        } else {
            vec![libc::SIGINT, libc::SIGTERM, libc::SIGHUP]
        };
        for signal in signals {
            let workspace = tempfile::tempdir().unwrap();
            let (mut reader, mut writer) = pipe();
            fill_pipe(&mut reader, &mut writer, 4096);
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "headless::backpressure_tests::signals_and_timeout_interrupt_all_stdout_phases",
                    "--nocapture",
                ])
                .env(CHILD, phase)
                .current_dir(workspace.path())
                .stdin(Stdio::null())
                .stdout(writer.try_clone().unwrap())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            wait_until(&mut child, phase, || {
                workspace.path().join("output-blocked").exists()
            });
            if signal != 0 {
                // SAFETY: send only to the dedicated test child.
                assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, signal) }, 0);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let output = child.wait_with_output().unwrap();
                    panic!("{phase}, signal {signal}: stdout blocked cancellation: {output:?}");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let status = child.wait().unwrap();
            assert_eq!(
                status.code(),
                Some(if signal == 0 {
                    EXIT_TIMEOUT
                } else {
                    EXIT_INTERRUPT
                }),
                "{phase}, signal {signal}: {status}"
            );
            assert_eq!(
                workspace.path().join("turn-requested").exists(),
                phase != "initial"
            );
        }
    }
}

#[test]
fn tui_startup_and_error_output_remain_interruptible() {
    const CHILD: &str = "DEEP_CODE_TUI_STDERR_PHASE_TEST";
    if let Ok(phase) = std::env::var(CHILD) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let intent = match phase.as_str() {
            "picker_progress" => StartupIntent::ResumePicker,
            "error" => StartupIntent::ResumeId("!".repeat(8192)),
            _ => StartupIntent::ContinueLatest,
        };
        let code = runtime.block_on(async {
            // Let startup reach its first write before signalling the parent.
            // Error/diagnostic cases additionally wait for bytes in the pipe.
            tokio::spawn(async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                std::fs::write("startup-ready", "yes").unwrap();
            });
            crate::run_tui(intent, Vec::new()).await
        });
        std::process::exit(code);
    }
    for phase in [
        "continue_progress",
        "picker_progress",
        "diagnostics",
        "error",
        "cancel_notice",
    ] {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            let workspace = tempfile::tempdir().unwrap();
            let sessions = workspace.path().join(".deep-code/sessions");
            std::fs::create_dir_all(&sessions).unwrap();
            if phase == "diagnostics" {
                for index in 0..32 {
                    std::fs::write(
                        sessions.join(format!("{index:02}_{}.json", "a".repeat(125))),
                        "{",
                    )
                    .unwrap();
                }
            }
            let fifo = workspace.path().join(".deep-code/config.toml");
            if phase == "cancel_notice" {
                let path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
                // SAFETY: a temporary path owned by this test.
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            }
            let (mut reader, mut writer) = pipe();
            let partial = phase == "diagnostics" || phase == "error";
            let filled = fill_pipe(&mut reader, &mut writer, if partial { 4096 } else { 0 });
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "headless::backpressure_tests::tui_startup_and_error_output_remain_interruptible", "--nocapture"])
                .env(CHILD, phase).current_dir(workspace.path()).stdin(Stdio::null())
                .stdout(Stdio::piped()).stderr(writer.try_clone().unwrap()).spawn().unwrap();
            let mut held_fifo = None;
            wait_until(&mut child, phase, || {
                if phase == "cancel_notice" {
                    held_fifo = std::fs::OpenOptions::new()
                        .write(true)
                        .custom_flags(libc::O_NONBLOCK)
                        .open(&fifo)
                        .ok();
                    held_fifo.is_some()
                } else if partial {
                    let mut queued: libc::c_int = 0;
                    // SAFETY: inspect, without draining, the owned pipe.
                    assert_eq!(
                        unsafe { libc::ioctl(reader.as_raw_fd(), libc::FIONREAD, &mut queued) },
                        0
                    );
                    queued as usize > filled - 2048
                } else {
                    workspace.path().join("startup-ready").exists()
                }
            });
            // SAFETY: signal only this test's dedicated child.
            assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, signal) }, 0);
            let deadline = Instant::now() + Duration::from_secs(5);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{phase}, signal {signal}: TUI stderr blocked cancellation");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                child.wait().unwrap().code(),
                Some(EXIT_INTERRUPT),
                "{phase}, signal {signal}"
            );
            drop(held_fifo);
        }
    }
}

#[test]
fn picker_and_tui_cleanup_survive_both_output_pipes_being_full() {
    use std::os::unix::process::CommandExt;
    const CHILD: &str = "DEEP_CODE_TERMINAL_PIPE_TEST";
    if let Ok(mode) = std::env::var(CHILD) {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let intent = if mode == "picker" {
            StartupIntent::ResumePicker
        } else {
            StartupIntent::New
        };
        std::process::exit(runtime.block_on(crate::run_tui(intent, Vec::new())));
    }
    for mode in ["picker", "tui"] {
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            let workspace = tempfile::tempdir().unwrap();
            let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
            let mut record = SessionRecord::new(workspace.path().to_path_buf(), "system");
            record
                .entries
                .push(std::sync::Arc::new(deep_code_agent::SessionEntry::user(
                    "fixture",
                )));
            store.save(&mut record).unwrap();
            std::fs::write(
                workspace.path().join(".deep-code/config.toml"),
                "[ui]\nlanguage = 'en'\n",
            )
            .unwrap();
            // Log redirection is deliberately refused, keeping stderr on the
            // pipe so a synchronous destructor diagnostic would be observable.
            std::os::unix::fs::symlink(
                workspace.path().join("unused"),
                workspace.path().join(".deep-code/deep-code.log"),
            )
            .unwrap();
            let (mut out_reader, mut out_writer) = pipe();
            let (mut err_reader, mut err_writer) = pipe();
            let (mut master, mut slave) = (-1, -1);
            let mut size = libc::winsize {
                ws_row: 30,
                ws_col: 120,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            // SAFETY: openpty fills two new owned descriptors; valid winsize.
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::addr_of_mut!(size),
                    )
                },
                0
            );
            // SAFETY: each descriptor is adopted exactly once.
            let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
            let mut command = Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "headless::backpressure_tests::picker_and_tui_cleanup_survive_both_output_pipes_being_full", "--nocapture"])
                .env(CHILD, mode).env("TERM", "xterm-256color").current_dir(workspace.path())
                .stdin(slave.try_clone().unwrap()).stdout(out_writer.try_clone().unwrap()).stderr(err_writer.try_clone().unwrap());
            // SAFETY: only async-signal-safe syscalls run between fork and exec.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = command.spawn().unwrap();
            let mut display = Vec::new();
            wait_until(&mut child, "first terminal frame", || {
                let mut queued: libc::c_int = 0;
                // SAFETY: inspect the live pipe descriptor.
                assert_eq!(
                    unsafe { libc::ioctl(out_reader.as_raw_fd(), libc::FIONREAD, &mut queued) },
                    0
                );
                if queued > 0 {
                    let mut bytes = vec![0; queued as usize];
                    out_reader.read_exact(&mut bytes).unwrap();
                    display.extend(bytes);
                }
                let cursor = if mode == "picker" {
                    b"\x1b[?25l"
                } else {
                    b"\x1b[?25h"
                };
                display.windows(cursor.len()).any(|part| part == cursor)
            });
            // Stop writes briefly while filling both pipes, then restore the
            // descriptor flags before resuming. No reader drains them on exit.
            // SAFETY: the signals and wait target only our child process.
            unsafe {
                assert_eq!(libc::kill(child.id() as libc::pid_t, libc::SIGSTOP), 0);
                let mut status = 0;
                assert_eq!(
                    libc::waitpid(child.id() as libc::pid_t, &mut status, libc::WUNTRACED),
                    child.id() as libc::pid_t
                );
                assert!(libc::WIFSTOPPED(status));
            }
            fill_pipe(&mut out_reader, &mut out_writer, 0);
            fill_pipe(&mut err_reader, &mut err_writer, 0);
            // SAFETY: resume and interrupt the dedicated child.
            unsafe {
                assert_eq!(libc::kill(child.id() as libc::pid_t, libc::SIGCONT), 0);
                assert_eq!(libc::kill(child.id() as libc::pid_t, signal), 0);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while child.try_wait().unwrap().is_none() {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{mode}, signal {signal}: terminal cleanup blocked");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                child.wait().unwrap().code(),
                Some(EXIT_INTERRUPT),
                "{mode}, {signal}"
            );
            // SAFETY: read termios on the still-open PTY after child exit.
            let mut attrs: libc::termios = unsafe { std::mem::zeroed() };
            assert_eq!(
                unsafe { libc::tcgetattr(master.as_raw_fd(), &mut attrs) },
                0
            );
            assert_ne!(attrs.c_lflag & libc::ICANON, 0, "raw mode must be restored");
            assert_ne!(attrs.c_lflag & libc::ECHO, 0, "echo must be restored");
        }
    }
}
