//! `/diff`: the working tree against `HEAD`, through the user's own git.
//!
//! The workspace is read-only: all index/worktree queries use isolated Git
//! metadata, with repository-defined filters, hooks and fsmonitor unavailable.
//! Output remains bounded and a path argument always follows `--`.

use std::path::Path;

use deep_code_agent::{IsolatedGit, IsolatedGitError};

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
    let repository = IsolatedGit::new_pinned(workspace).map_err(git_error)?;
    let base = Some(repository.diff_base());
    let output = git_bounded(&repository, &diff_args(base, path))?;
    if !output.ok && !output.truncated {
        return Err(GitDiffError::Failed(deep_code_agent::collapse_whitespace(
            &output.stderr,
        )));
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
    let untracked = git_bounded(&repository, &untracked_args)
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
        "--ignore-submodules=dirty",
        "--no-color",
    ];
    args.extend(base);
    if let Some(path) = path {
        args.extend(["--", path]);
    }
    args
}

fn git_error(error: IsolatedGitError) -> GitDiffError {
    match error {
        IsolatedGitError::GitMissing => GitDiffError::GitMissing,
        IsolatedGitError::NotARepository => GitDiffError::NotARepository,
        other => GitDiffError::Failed(other.to_string()),
    }
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

/// Run isolated Git with a ceiling on what it can make this process hold.
///
/// The shared runner bounds stdout and stderr, reads both concurrently, and
/// times out commands and inherited pipes that do not finish.
fn git_bounded(repository: &IsolatedGit, args: &[&str]) -> Result<Bounded, GitDiffError> {
    let result = repository
        .run_bounded(args, STDOUT_BYTE_CAP)
        .map_err(git_error)?;
    Ok(Bounded {
        ok: result.output.status.success(),
        stdout: String::from_utf8_lossy(&result.output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&result.output.stderr).into_owned(),
        truncated: result.truncated,
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
    use std::process::{Command, Stdio};

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
        let report = working_tree_diff(&repo.path().canonicalize().unwrap(), None)
            .expect("the diff still answers");
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
        let small = working_tree_diff(&repo.path().canonicalize().unwrap(), Some("small.txt"))
            .expect("a small diff");
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
    fn diff_does_not_run_fsmonitor_or_clean_filters() {
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        assert!(git(repo.path(), &["init", "-q"]));
        std::fs::write(repo.path().join("tracked.txt"), "before\n").unwrap();
        assert!(git(repo.path(), &["add", "."]));
        assert!(git(repo.path(), &["commit", "-qm", "base"]));
        let marker = outside
            .path()
            .join("executed")
            .to_string_lossy()
            .replace('\\', "/");
        let program = format!("echo executed > '{marker}'; cat");
        assert!(git(repo.path(), &["config", "core.fsmonitor", &program]));
        assert!(git(repo.path(), &["config", "filter.evil.clean", &program]));
        std::fs::write(
            repo.path().join(".gitattributes"),
            "tracked.txt filter=evil\n",
        )
        .unwrap();
        std::fs::write(repo.path().join("tracked.txt"), "after\n").unwrap();
        let index = std::fs::read(repo.path().join(".git/index")).unwrap();
        let report = working_tree_diff(&repo.path().canonicalize().unwrap(), None).unwrap();
        assert!(report.diff.contains("+after"));
        assert!(!outside.path().join("executed").exists());
        assert_eq!(
            std::fs::read(repo.path().join(".git/index")).unwrap(),
            index
        );
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
