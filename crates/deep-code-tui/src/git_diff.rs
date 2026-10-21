//! `/diff`: the working tree against `HEAD`, through the user's own git.
//!
//! Read-only by construction. `--no-ext-diff --no-textconv` keep a
//! repository's configuration from running a diff driver of its choosing,
//! `--no-optional-locks` keeps git from refreshing the index on the side, and
//! the two secrets this process may hold are removed from the child's
//! environment — git needs neither. The path argument always follows `--`, so
//! whatever the user typed is a path, never an option.

use std::path::Path;
use std::process::{Command, Stdio};

/// Chars of diff carried into the transcript. The same cap the write tools
/// apply to their own diffs: past it the output is a scroll, not a review.
pub(crate) const DIFF_MAX_CHARS: usize = 64 * 1024;

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
    let mut output = git(workspace, &diff_args(Some("HEAD"), path))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if stderr.contains("unknown revision")
            || stderr.contains("ambiguous argument 'HEAD'")
            || stderr.contains("bad revision")
        {
            output = git(workspace, &diff_args(None, path))?;
        }
        if !output.status.success() {
            return Err(GitDiffError::Failed(deep_code_agent::collapse_whitespace(
                &String::from_utf8_lossy(&output.stderr),
            )));
        }
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let (added, removed) = count_changes(&raw);
    let truncated = raw.chars().count() > DIFF_MAX_CHARS;
    let diff = if truncated {
        raw.chars().take(DIFF_MAX_CHARS).collect()
    } else {
        raw.into_owned()
    };

    let mut untracked_args = vec!["ls-files", "--others", "--exclude-standard"];
    if let Some(path) = path {
        untracked_args.extend(["--", path]);
    }
    let untracked = git(workspace, &untracked_args)
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
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
