//! `/diff`: the working tree against `HEAD`, through the user's own git.
//!
//! Read-only by construction. `--no-ext-diff --no-textconv` keep a
//! repository's configuration from running a diff driver of its choosing,
//! `--no-optional-locks` keeps git from refreshing the index on the side, and
//! the two secrets this process may hold are removed from the child's
//! environment — git needs neither. The path argument always follows `--`, so
//! whatever the user typed is a path, never an option.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

/// Chars of diff carried into the transcript. The same cap the write tools
/// apply to their own diffs: past it the output is a scroll, not a review.
pub(crate) const DIFF_MAX_CHARS: usize = 64 * 1024;

/// Bytes of a child's stdout kept in memory. [`DIFF_MAX_CHARS`] counts chars
/// and a char can take four bytes, so this is the smallest bound that can
/// still fill the char cap; past it the answer is already "truncated".
///
/// It exists because this runs on the thread that draws the screen: reading a
/// rewritten tree to the end buffered hundreds of megabytes and froze the UI
/// for as long as git kept writing. Past the cap the child is killed instead.
const STDOUT_BYTE_CAP: usize = DIFF_MAX_CHARS * 4;

/// Bytes of a child's stderr kept for the error message. Real messages are
/// short; a flood is a bug in what we spawned, not something to quote.
const STDERR_BYTE_CAP: usize = 8 * 1024;

/// How long a child gets to notice that its stdout is gone and exit on its own
/// before it is killed. Killing alone is not enough on Windows (the `git` on
/// PATH is a shim around the real process), and waiting alone is not guaranteed
/// either — so: close, wait, kill.
const EXIT_GRACE: std::time::Duration = std::time::Duration::from_millis(500);

/// Ceiling on waiting for the stderr reader. A pipe inherited by a process we
/// no longer control must never turn into a frozen screen.
const STDERR_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug)]
pub(crate) struct DiffReport {
    /// `git diff` output, `+++`/`---` headers included, cut at
    /// [`DIFF_MAX_CHARS`].
    pub diff: String,
    pub added: usize,
    pub removed: usize,
    pub truncated: bool,
    /// Files git does not track and does not ignore — outside the diff, and
    /// easy to forget without a reminder.
    pub untracked: Vec<String>,
}

#[derive(Debug)]
pub(crate) enum GitDiffError {
    GitMissing,
    NotARepository,
    Failed(String),
}

/// The working tree (index and unstaged changes alike) against `HEAD`,
/// optionally limited to `path`. A repository without a commit yet has no
/// `HEAD`; the diff then runs against the index, the only baseline there is.
pub(crate) fn working_tree_diff(
    workspace: &Path,
    path: Option<&str>,
) -> Result<DiffReport, GitDiffError> {
    let inside = git(workspace, &["rev-parse", "--is-inside-work-tree"])?;
    if !inside.status.success() {
        return Err(GitDiffError::NotARepository);
    }
    let mut output = git_bounded(workspace, &diff_args(Some("HEAD"), path))?;
    // A killed-for-length run is not a failure: its exit status is a failure
    // only because we stopped reading, and the prefix is the answer.
    if !output.ok && !output.truncated {
        let retry = output.stderr.contains("unknown revision")
            || output.stderr.contains("ambiguous argument 'HEAD'")
            || output.stderr.contains("bad revision");
        if retry {
            output = git_bounded(workspace, &diff_args(None, path))?;
        }
        if !output.ok && !output.truncated {
            return Err(GitDiffError::Failed(deep_code_agent::collapse_whitespace(
                &output.stderr,
            )));
        }
    }
    let raw = output.stdout;
    let (added, removed) = count_changes(&raw);
    // `output.truncated` means git was killed mid-write, so what is in hand is
    // a prefix of the answer whatever its char count happens to say.
    let truncated = output.truncated || raw.chars().count() > DIFF_MAX_CHARS;
    let diff = if truncated {
        raw.chars().take(DIFF_MAX_CHARS).collect()
    } else {
        raw
    };

    let mut untracked_args = vec!["ls-files", "--others", "--exclude-standard"];
    if let Some(path) = path {
        untracked_args.extend(["--", path]);
    }
    let untracked = git_bounded(workspace, &untracked_args)
        .ok()
        // A truncated listing still names the first files, which is all the
        // transcript shows (twenty, then an ellipsis).
        .filter(|output| output.ok || output.truncated)
        .map(|output| {
            output
                .stdout
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(DiffReport {
        diff,
        added,
        removed,
        truncated,
        untracked,
    })
}

fn diff_args<'a>(base: Option<&'a str>, path: Option<&'a str>) -> Vec<&'a str> {
    let mut args = vec![
        "--no-pager",
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
    ];
    args.extend(base);
    if let Some(path) = path {
        args.extend(["--", path]);
    }
    args
}

fn git(workspace: &Path, args: &[&str]) -> Result<std::process::Output, GitDiffError> {
    Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_PAGER")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("DEEP_CODE_RUNTIME_TOKEN")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                GitDiffError::GitMissing
            } else {
                GitDiffError::Failed(error.to_string())
            }
        })
}

/// One child's answer, with both streams bounded.
struct Bounded {
    /// The child exited successfully. False when [`Self::truncated`] too: a
    /// child killed for length exits non-zero, and that is not a failure.
    ok: bool,
    stdout: String,
    stderr: String,
    /// The child wrote past [`STDOUT_BYTE_CAP`] and was killed.
    truncated: bool,
}

/// [`git`] with a ceiling on what it can make this process hold.
///
/// `Command::output` reads both pipes to the end before returning, so one
/// `/diff` against a rewritten tree — a deleted `node_modules`, a regenerated
/// lock file, a checked-in fixture — buffered the whole thing and then threw
/// all but 64K chars away. It runs on the render thread, so that was a frozen
/// terminal, not a slow command. Here stdout stops at [`STDOUT_BYTE_CAP`] (and
/// the child is killed), stderr at [`STDERR_BYTE_CAP`], and stderr is drained
/// on its own thread so a child filling it cannot deadlock the wait.
fn git_bounded(workspace: &Path, args: &[&str]) -> Result<Bounded, GitDiffError> {
    let mut child = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_PAGER")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("DEEP_CODE_RUNTIME_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                GitDiffError::GitMissing
            } else {
                GitDiffError::Failed(error.to_string())
            }
        })?;
    let mut stderr_pipe = child.stderr.take();
    // Delivered over a channel, not joined: on Windows `git` on PATH is a shim
    // that launches the real `git.exe`, so a pipe can stay open after the
    // process we spawned is gone, and `read_to_end` would wait for an EOF that
    // never comes — the whole TUI froze there, with no keystroke able to
    // interrupt it.
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel();
    let _reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(stream) = stderr_pipe.as_mut() {
            let _ = stream.take(STDERR_BYTE_CAP as u64).read_to_end(&mut bytes);
        }
        let _ = stderr_tx.send(bytes);
    });
    let mut bytes = Vec::new();
    let mut stdout_pipe = child.stdout.take();
    if let Some(stdout) = stdout_pipe.as_mut() {
        // One byte past the cap, so "truncated" means "there was more".
        let _ = stdout
            .take(STDOUT_BYTE_CAP as u64 + 1)
            .read_to_end(&mut bytes);
    }
    let truncated = bytes.len() > STDOUT_BYTE_CAP;
    if truncated {
        bytes.truncate(STDOUT_BYTE_CAP);
        // Close our end first: a writer whose pipe is gone fails its next
        // write and exits on its own, including the real `git.exe` behind a
        // shim. Killing alone would only take the shim down.
        drop(stdout_pipe);
        let deadline = std::time::Instant::now() + EXIT_GRACE;
        while std::time::Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // And whatever is left gets killed, so `wait` below cannot sit on a
        // process that is waiting for a pipe we closed.
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|error| GitDiffError::Failed(error.to_string()))?;
    // Bounded: an inherited stderr pipe is not worth a frozen screen, and the
    // message it carries is the error text, which a truncated run already
    // reports as truncation.
    let stderr = stderr_rx.recv_timeout(STDERR_GRACE).unwrap_or_default();
    Ok(Bounded {
        ok: status.success(),
        stdout: String::from_utf8_lossy(&bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        truncated,
    })
}

/// Added and removed line counts of a diff; the `+++`/`---` file headers git
/// prints are not changes.
pub(crate) fn count_changes(diff: &str) -> (usize, usize) {
    diff.lines().fold((0, 0), |(added, removed), line| {
        if line.starts_with("+++ ") || line.starts_with("--- ") {
            (added, removed)
        } else if line.starts_with('+') {
            (added + 1, removed)
        } else if line.starts_with('-') {
            (added, removed + 1)
        } else {
            (added, removed)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(workspace: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(workspace)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    /// A repository whose working tree change is far larger than any cap we
    /// keep — the shape a `/diff` in a real repo takes (a regenerated file, a
    /// deleted vendor tree. `None` when git is not installed.
    fn repo_with_a_huge_change() -> Option<tempfile::TempDir> {
        let repo = tempfile::tempdir().ok()?;
        if !git(repo.path(), &["init", "-q"]) {
            return None;
        }
        let big = |tag: &str| {
            (0..20_000)
                .map(|line| format!("{tag} line {line}\n"))
                .collect::<String>()
        };
        std::fs::write(repo.path().join("big.txt"), big("old")).unwrap();
        if !git(repo.path(), &["add", "."]) || !git(repo.path(), &["commit", "-q", "-m", "base"]) {
            return None;
        }
        std::fs::write(repo.path().join("big.txt"), big("new")).unwrap();
        Some(repo)
    }

    /// `/diff` runs on the thread that draws the screen, so it may not read a
    /// child to the end: past the cap the child is killed and the prefix is
    /// what lands in the transcript, marked truncated.
    #[test]
    fn a_rewritten_tree_is_cut_at_the_cap_instead_of_being_buffered() {
        let Some(repo) = repo_with_a_huge_change() else {
            eprintln!("git not available; skipping");
            return;
        };
        let report = working_tree_diff(repo.path(), None).expect("the diff still answers");
        assert!(report.truncated, "a diff past the cap has to say so");
        assert_eq!(
            report.diff.chars().count(),
            DIFF_MAX_CHARS,
            "the prefix fills the cap exactly"
        );
        assert!(
            report.diff.starts_with("diff --git"),
            "{}",
            &report.diff[..40]
        );

        // The cap is a ceiling on holding, not on accuracy: a diff that fits is
        // still whole, and still not marked truncated.
        std::fs::write(repo.path().join("small.txt"), "one\n").unwrap();
        git(repo.path(), &["add", "."]);
        git(repo.path(), &["commit", "-q", "-m", "big"]);
        std::fs::write(repo.path().join("small.txt"), "two\n").unwrap();
        let small = working_tree_diff(repo.path(), Some("small.txt")).expect("a small diff");
        assert!(!small.truncated, "{small:?}");
        assert!(small.diff.contains("+two") && small.diff.contains("-one"));
    }

    #[test]
    fn counts_change_lines_but_not_file_headers() {
        let diff =
            "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n-old\n+new\n context\n+more\n";
        assert_eq!(count_changes(diff), (2, 1));
        assert_eq!(count_changes(""), (0, 0));
    }

    #[test]
    fn the_path_always_follows_the_option_terminator() {
        let args = diff_args(Some("HEAD"), Some("--help"));
        let dashdash = args.iter().position(|arg| *arg == "--").unwrap();
        assert_eq!(args[dashdash + 1], "--help");
        assert!(args[..dashdash].contains(&"HEAD"));
        assert!(!diff_args(None, None).contains(&"HEAD"));
    }
}
