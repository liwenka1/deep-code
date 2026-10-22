//! Prompt history that survives a restart.
//!
//! One file per workspace under `~/.deep-code/prompt-history/`, one JSON
//! object per line (`{"text": …}`), newest last. The user's own directory
//! rather than `<workspace>/.deep-code`: the workspace copy would be
//! model-writable, and what `↑` puts back into the composer should not be
//! something a tool call could have planted. The macOS sandbox denies
//! sandboxed commands any read of `~/.deep-code` as well, so a prompt typed
//! here never reaches a shell command through this file either.
//!
//! Text only. Attached images are kept with the in-memory entry for the
//! session they belong to; across restarts their files may be gone, and a
//! recalled prompt that talks about a chip the composer no longer holds is the
//! exact confusion the in-memory history was built to avoid.
//!
//! Every failure here is swallowed: history is a convenience, and a read-only
//! home directory or a full disk must never stop a prompt from being sent.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const HISTORY_DIR: &str = "prompt-history";

/// This workspace's history file, or `None` when there is no home directory
/// to keep it in. The file name is a stable hash of the canonical workspace
/// path: stable across runs and toolchains (FNV-1a, spelled out here rather
/// than `DefaultHasher`, whose algorithm the standard library does not promise
/// to keep), and free of the path's own characters, which a file name cannot
/// carry on every platform.
pub(crate) fn history_path(workspace: &Path) -> Option<PathBuf> {
    let home = deep_code_agent::default_config_path()
        .parent()?
        .to_path_buf();
    let canonical =
        deep_code_agent::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    Some(home.join(HISTORY_DIR).join(format!(
        "{:016x}.jsonl",
        fnv1a(canonical.to_string_lossy().as_bytes())
    )))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The last `cap` prompts in `path`, oldest first. A line that is not a
/// `{"text": …}` object is skipped, not fatal: the file is append-only and a
/// crash mid-write leaves at most one broken tail line.
pub(crate) fn load(path: &Path, cap: usize) -> Vec<String> {
    let Ok(raw) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut prompts: Vec<String> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|value| value.get("text")?.as_str().map(str::to_string))
        .filter(|text| !text.trim().is_empty())
        .collect();
    if prompts.len() > cap {
        prompts.drain(..prompts.len() - cap);
    }
    prompts
}

/// Append one prompt. Creates the directory on first use through the same
/// "must be a real directory" rule every other `~/.deep-code` writer follows.
pub(crate) fn append(path: &Path, text: &str) {
    let Some(dir) = path.parent() else {
        return;
    };
    if deep_code_agent::ensure_owned_dirs(dir, 2).is_err() {
        return;
    }
    let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let line = serde_json::json!({ "text": text }).to_string();
    let _ = writeln!(file, "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_reloads_the_newest_prompts_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("history.jsonl");
        assert!(
            load(&path, 10).is_empty(),
            "a missing file is an empty history"
        );
        for index in 0..5 {
            append(&path, &format!("prompt {index}\nsecond line"));
        }
        assert_eq!(
            load(&path, 10),
            (0..5)
                .map(|index| format!("prompt {index}\nsecond line"))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            load(&path, 2),
            vec!["prompt 3\nsecond line", "prompt 4\nsecond line"]
        );
    }

    #[test]
    fn a_broken_tail_line_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        append(&path, "good");
        fs::write(
            &path,
            format!("{}{{\"text\": \"trunc", fs::read_to_string(&path).unwrap()),
        )
        .unwrap();
        assert_eq!(load(&path, 10), vec!["good"]);
    }

    #[test]
    fn the_file_name_is_a_stable_hash_of_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let first = history_path(dir.path());
        let second = history_path(dir.path());
        assert_eq!(first, second);
        let name = first
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        assert!(
            name.len() == 16 + ".jsonl".len() && name.ends_with(".jsonl"),
            "{name}"
        );
        assert!(
            first
                .as_ref()
                .is_some_and(|path| path.parent().is_some_and(|dir| dir.ends_with(HISTORY_DIR)))
        );
        let other = tempfile::tempdir().unwrap();
        assert_ne!(first, history_path(other.path()));
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
    }
}
