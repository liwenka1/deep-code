//! Execution of the `session list|delete|export` subcommands.
//!
//! Kept out of `cli` (which is pure argument parsing) and out of `main`'s
//! dispatch, mirroring `doctor_cli`/`eval_cli`: parsing decides *which* command,
//! this runs it. `main` dispatches each `RunMode::Session*` variant to the
//! matching function here, so there is no catch-all `unreachable!` bridging two
//! modules.

use deep_code_agent::{
    JsonSessionStore, Lang, SessionId, SessionStore, format_sessions_storage_note, now_ms,
};

use crate::cli::workspace_root;

fn open_session_store() -> JsonSessionStore {
    match JsonSessionStore::for_workspace(workspace_root()) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("session storage unavailable: {error}");
            std::process::exit(1);
        }
    }
}

// This one-shot command keeps the OS interrupt disposition throughout file
// reads and stdout/stderr writes, including writes blocked by a full pipe.
// A scan-scoped Tokio signal receiver would leave SIGINT intercepted afterward.
pub fn list() -> anyhow::Result<()> {
    let workspace = workspace_root();
    let store = open_session_store();
    println!(
        "# {}",
        format_sessions_storage_note(&workspace, &crate::cli::program_name())
    );
    eprintln!("scanning saved sessions… (Ctrl+C to cancel)");
    let records = store.list()?;
    for line in store.take_diagnostics() {
        eprintln!("{line}");
    }
    if records.is_empty() {
        println!("No saved sessions.");
        return Ok(());
    }
    let now = now_ms();
    for record in records {
        let preview = session_list_preview(&record.preview());
        println!(
            "{}\t{}\t{} msgs\t{}",
            record.id.as_str(),
            // English, like every other word this command prints and like
            // `doctor_cli`. It used to resolve the configured UI language for
            // this one column, so a `ui.language = "zh"` user got
            // `session_1_0<TAB>20714 天前<TAB>2 msgs` wrapped in "No saved
            // sessions." / "skipping unreadable session" / the storage note —
            // one Chinese fragment in an otherwise English line, invisible to
            // anyone reviewing in English. `relative_time` stays localized for
            // the TUI pickers, which are localized throughout.
            crate::startup::relative_time(now, record.updated_at_ms, Lang::En),
            record.message_count(),
            preview
        );
    }
    Ok(())
}

pub fn delete(id: String) -> anyhow::Result<()> {
    let store = open_session_store();
    store.delete(&SessionId::parse(&id)?)?;
    println!("Deleted session {id}.");
    Ok(())
}

pub fn export(id: String) -> anyhow::Result<()> {
    let store = open_session_store();
    println!("{}", store.export(&SessionId::parse(&id)?)?);
    Ok(())
}

/// One `session list` preview column.
///
/// `SessionRecord::preview()` returns the last user entry VERBATIM out of
/// `<workspace>/.deep-code/sessions/*.json`, a file `workspace_policy` itself
/// documents as "an ordinary `write_file` target for the model". Collapsing
/// newlines and capping the length — all this used to do — touches neither
/// `\x1b` nor the invisible families, so a planted session could repaint the
/// terminal from a plain `deepcode session list`.
///
/// This is the third twin of the two resume pickers hardened in 806ee49 and
/// 6a08a86, and the only one of the three with no rendered-cell test, because
/// it prints rather than draws. Sanitize BEFORE collapsing and truncating: the
/// cap counts characters, and dropping the invisibles first keeps that count
/// describing what is actually shown.
fn session_list_preview(preview: &str) -> String {
    crate::history::truncate_chars(
        &deep_code_agent::neutralize_display_text(preview).replace('\n', " "),
        60,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn sigint_terminates_session_list_when_output_pipes_fill() {
        use std::io::{Read, Write};
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::process::ExitStatusExt;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        const CHILD: &str = "DEEP_CODE_SESSION_LIST_PIPE_TEST";
        if std::env::var_os(CHILD).is_some() {
            list().unwrap();
            std::process::exit(0);
        }

        for blocked in ["stdout", "stderr"] {
            let workspace = tempfile::tempdir().unwrap();
            let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
            // Enough output to exhaust the 4 KiB left free in the pipe below.
            for index in 0..32 {
                let id = format!("{index:02}_{}", "a".repeat(125));
                if blocked == "stderr" {
                    std::fs::write(
                        workspace
                            .path()
                            .join(".deep-code/sessions")
                            .join(format!("{id}.json")),
                        "{",
                    )
                    .unwrap();
                } else {
                    let mut record = deep_code_agent::SessionRecord::new(
                        workspace.path().to_path_buf(),
                        "system",
                    );
                    record.id = SessionId::parse(&id).unwrap();
                    record
                        .entries
                        .push(std::sync::Arc::new(deep_code_agent::SessionEntry::user(
                            "😀".repeat(61),
                        )));
                    store.save(&mut record).unwrap();
                }
            }
            let mut fds = [-1; 2];
            // SAFETY: pipe writes two owned file descriptors into this array.
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            // SAFETY: each descriptor is owned exactly once by its File.
            let (mut reader, mut writer) = unsafe {
                (
                    std::fs::File::from_raw_fd(fds[0]),
                    std::fs::File::from_raw_fd(fds[1]),
                )
            };
            // Fill without assuming the host's pipe capacity, then leave room
            // for the header/progress lines so blocking occurs AFTER the scan.
            // SAFETY: these fcntl calls operate on the live write descriptor.
            assert_eq!(
                unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
                0
            );
            let mut prefilled = 0;
            loop {
                match writer.write(&[b'x'; 4096]) {
                    Ok(count) => prefilled += count,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    other => panic!("could not fill output pipe: {other:?}"),
                }
            }
            reader.read_exact(&mut [0; 4096]).unwrap();
            // SAFETY: restore blocking writes on the descriptor inherited by the child.
            assert_eq!(
                unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, 0) },
                0
            );
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "session_cli::tests::sigint_terminates_session_list_when_output_pipes_fill",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .current_dir(workspace.path())
                .stdin(Stdio::null());
            if blocked == "stdout" {
                command.stdout(writer).stderr(Stdio::null());
            } else {
                command.stderr(writer).stdout(Stdio::null());
            }
            let mut child = command.spawn().unwrap();
            drop(command);
            let fd = reader.as_raw_fd();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let mut queued: libc::c_int = 0;
                // SAFETY: fd is live and FIONREAD writes one integer. Observe
                // output without draining it and accidentally unblocking writes.
                assert_eq!(unsafe { libc::ioctl(fd, libc::FIONREAD, &mut queued) }, 0);
                if queued as usize >= prefilled - 4096 + 2048 {
                    break;
                }
                if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
                    let _ = child.kill();
                    let output = child.wait_with_output().unwrap();
                    panic!("{blocked} never filled with list output: {output:?}");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            // Let the writer reach pipe capacity; the reader stays undrained
            // until the process has exited, including after the interrupt.
            std::thread::sleep(Duration::from_millis(50));
            assert!(
                child.try_wait().unwrap().is_none(),
                "fixture must block output"
            );
            // SAFETY: the PID belongs to this test's live child process.
            assert_eq!(
                unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) },
                0
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert_eq!(status.signal(), Some(libc::SIGINT), "{blocked}: {status}");
                    break;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("SIGINT was ignored with a full {blocked} pipe");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

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

    /// Every column `session list` prints is English, the age included.
    ///
    /// The age column used to resolve the configured UI language while the rest
    /// of the command's text was hardcoded English, so a `ui.language = "zh"`
    /// user saw exactly one Chinese fragment per row — and an English-speaking
    /// reviewer saw nothing wrong. `doctor_cli` makes no `tr` call at all; this
    /// is the same rule, asserted rather than assumed.
    #[test]
    fn the_age_column_is_english_like_every_other_column() {
        let now = 1_700_000_000_000;
        let age = crate::startup::relative_time(now, now - 7_200_000, Lang::En);
        assert_eq!(age, "2 h ago");
        assert!(
            age.is_ascii(),
            "the age column must not carry localized text: {age:?}"
        );
        // The localized spelling still exists — it is the TUI pickers' —
        // so this is a call-site rule, not a lost capability.
        assert_eq!(
            crate::startup::relative_time(now, now - 7_200_000, Lang::Zh),
            "2 小时前"
        );
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
}
