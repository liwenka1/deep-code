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

/// Longest prompt written to the file. A paste of a whole log stays in this
/// session's `↑` like any prompt, but past this it is not kept across
/// restarts: every launch reads the file, and the entries it keeps stay in
/// memory for the whole session.
const MAX_PERSISTED_PROMPT_BYTES: usize = 64 * 1024;

/// This workspace's history file, or `None` when there is no home directory
/// to keep it in — never a path under the working directory, which is the
/// workspace the module doc says to stay out of. The file name is a stable
/// hash of the canonical workspace path: stable across runs and toolchains
/// (FNV-1a, spelled out here rather than `DefaultHasher`, whose algorithm the
/// standard library does not promise to keep), and free of the path's own
/// characters, which a file name cannot carry on every platform.
pub(crate) fn history_path(workspace: &Path) -> Option<PathBuf> {
    history_path_under(deep_code_agent::default_config_path(), workspace)
}

/// [`history_path`] with the global config path handed in, so the no-home
/// case can be tested without touching the process environment.
fn history_path_under(global_config: Option<PathBuf>, workspace: &Path) -> Option<PathBuf> {
    let home = global_config?.parent()?.to_path_buf();
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

#[derive(serde::Deserialize)]
struct Entry {
    text: String,
}

/// The last `cap` prompts in `path`, oldest first.
///
/// Read from the end, parsing only until `cap` are found. A line that is not a
/// `{"text": …}` object, or not UTF-8, is skipped, not fatal: a crash or a
/// full disk mid-write leaves a broken line, and one bad byte used to empty
/// the whole history for good. A file past twice the cap is rewritten to the
/// last `cap` entries on the way out, so it does not grow without bound.
pub(crate) fn load(path: &Path, cap: usize) -> Vec<String> {
    let Ok(raw) = fs::read(path) else {
        return Vec::new();
    };
    // A record torn by a crash or a full disk leaves the file without its
    // trailing newline. Repaired here, at launch and before this process
    // appends anything, rather than on every append: the append path must stay
    // one atomic `write` (`concurrent_appends_keep_every_line_whole` is about
    // exactly that), and a "does the file end with a newline?" probe before
    // each write is a race against the other instance — a stale answer adds an
    // empty line, and eight threads appending two hundred records each was
    // enough to hit it.
    let raw: &[u8] = match torn_tail_start(&raw) {
        Some(start) => {
            drop_torn_tail(path, start);
            &raw[..start]
        }
        None => &raw,
    };
    let lines: Vec<&[u8]> = raw
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect();
    let mut prompts: Vec<String> = lines
        .iter()
        .rev()
        .filter_map(|line| std::str::from_utf8(line).ok())
        .filter_map(|line| serde_json::from_str::<Entry>(line).ok())
        .map(|entry| entry.text)
        .filter(|text| !text.trim().is_empty())
        .take(cap)
        .collect();
    prompts.reverse();
    if lines.len() > cap.saturating_mul(2) {
        rewrite(path, &prompts);
    }
    prompts
}

/// Where the last complete record ends, when the file's tail is a torn one:
/// `None` when the file ends with a newline (or is empty), `Some(len)` when it
/// does not — i.e. the last record never finished being written.
fn torn_tail_start(raw: &[u8]) -> Option<usize> {
    match raw.last() {
        None | Some(b'\n') => None,
        Some(_) => Some(
            raw.iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |index| index + 1),
        ),
    }
}

/// Cut a torn tail off the file, keeping `keep` bytes.
///
/// Not through a link, the same rule `append` follows: a link in the file's
/// place is somebody else's file, and truncating it would be worse than the
/// torn record. Best-effort throughout — history is a convenience.
fn drop_torn_tail(path: &Path, keep: usize) {
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    if let Ok(file) = options.open(path) {
        let _ = file.set_len(keep as u64);
    }
}

/// Replace the file with `prompts`, through a sibling temp file and a rename,
/// so a crash leaves the old file or the new one, never half of either.
fn rewrite(path: &Path, prompts: &[String]) {
    let Some(name) = path.file_name() else {
        return;
    };
    let mut temp_name = name.to_os_string();
    temp_name.push(format!(".{}.tmp", std::process::id()));
    let temp = path.with_file_name(temp_name);
    // A leftover from a crash, or a link planted in its place: removing a link
    // removes the link, and `create_new` below never opens an existing path.
    let _ = fs::remove_file(&temp);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    let Ok(mut file) = options.open(&temp) else {
        return;
    };
    let mut body = String::new();
    for prompt in prompts {
        body.push_str(&serde_json::json!({ "text": prompt }).to_string());
        body.push('\n');
    }
    let written = file.write_all(body.as_bytes()).is_ok();
    drop(file);
    if !written || fs::rename(&temp, path).is_err() {
        let _ = fs::remove_file(&temp);
    }
}

/// Append one prompt. Creates the directory on first use through the same
/// "must be a real directory" rule every other `~/.deep-code` writer follows,
/// and refuses a symlink in the file's place — with `O_NOFOLLOW` on unix, so
/// the refusal is the kernel's; Windows `OpenOptions` has no such flag, so
/// there the check-then-open window remains. The record goes out in one
/// write: two instances appending at once used to tear each other's lines
/// apart. (A launch that trims the file can still drop a line another
/// instance appends between the trim's read and its rename: history is a
/// convenience, and the trim is rare.)
pub(crate) fn append(path: &Path, text: &str) {
    if text.len() > MAX_PERSISTED_PROMPT_BYTES {
        return;
    }
    let Some(dir) = path.parent() else {
        return;
    };
    if deep_code_agent::ensure_owned_dirs(dir, 2).is_err() {
        return;
    }
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return;
    }
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let Ok(mut file) = options.open(path) else {
        return;
    };
    // One `write`, always: two instances appending at once keep their records
    // whole only because POSIX promises a single `O_APPEND` write lands whole.
    // Anything that has to inspect the file first belongs in `load`, which
    // runs at launch, before this process appends (see `torn_tail_start`).
    let mut line = serde_json::json!({ "text": text }).to_string();
    line.push('\n');
    let _ = file.write_all(line.as_bytes());
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

    /// A record torn by a crash (no trailing newline) used to take the next one
    /// with it: the append glued both into a single unparseable line, and
    /// `load` dropped that whole line — one lost prompt became two. The repair
    /// happens at launch, in `load`, before this process appends anything.
    #[test]
    fn a_torn_last_record_is_repaired_before_the_next_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        // Exactly what a crash mid-`write_all` leaves behind.
        fs::write(&path, r#"{"text":"torn"#).unwrap();
        // A launch reads the history first; that is where the tail is cut.
        assert_eq!(load(&path, 10), Vec::<String>::new());
        append(&path, "after the crash");
        assert_eq!(load(&path, 10), vec!["after the crash"]);
    }

    /// The repair only ever removes a tail that could not have been a record:
    /// a file that ends with its newline is left byte-for-byte alone.
    #[test]
    fn a_complete_file_is_not_touched_by_a_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        append(&path, "one");
        let before = fs::read_to_string(&path).unwrap();
        assert_eq!(load(&path, 10), vec!["one"]);
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
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

    /// Without a home directory there is no history file at all. The relative
    /// fallback put it under the working directory — the workspace, which a
    /// tool call can write to.
    #[test]
    fn without_a_home_directory_there_is_no_history_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(history_path_under(None, dir.path()), None);
    }

    /// One bad byte costs its own line, not the history: `read_to_string` failed
    /// on the whole file, and every later launch then started empty while
    /// appends kept writing to a file nobody could read.
    #[test]
    fn a_bad_byte_costs_its_line_not_the_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let mut raw = b"{\"text\":\"first\"}\n".to_vec();
        raw.extend_from_slice(b"{\"text\":\"\xe4\xbd\"}\n");
        raw.extend_from_slice(b"{\"text\":\"third\"}\n");
        fs::write(&path, raw).unwrap();
        assert_eq!(load(&path, 10), vec!["first", "third"]);
    }

    /// Two instances appending at once keep every record whole: each goes out
    /// in one write, where `writeln!` issued two and let another append land
    /// between a record and its line ending. Unix only: that one `O_APPEND`
    /// write lands whole is a promise POSIX makes for regular files, and the
    /// test would be measuring the platform rather than this code elsewhere.
    #[cfg(unix)]
    #[test]
    fn concurrent_appends_keep_every_line_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let writers: Vec<_> = (0..8)
            .map(|writer| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for index in 0..200 {
                        append(&path, &format!("{writer}/{index} {}", "x".repeat(200)));
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let raw = fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), 1600);
        assert!(
            raw.lines()
                .all(|line| serde_json::from_str::<Entry>(line).is_ok()),
            "a torn line"
        );
    }

    /// A pasted log is not written to the file — it stays in this session's
    /// `↑` — and a file past twice the cap is cut back to the cap when read.
    #[test]
    fn the_file_stays_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        append(&path, &"log line\n".repeat(MAX_PERSISTED_PROMPT_BYTES));
        assert!(!path.exists() || load(&path, 10).is_empty());

        for index in 0..25 {
            append(&path, &format!("prompt {index}"));
        }
        let expected: Vec<String> = (15..25).map(|index| format!("prompt {index}")).collect();
        assert_eq!(load(&path, 10), expected);
        assert_eq!(
            fs::read_to_string(&path).unwrap().lines().count(),
            10,
            "rewritten to the cap"
        );
        assert_eq!(load(&path, 10), expected, "and still the same prompts");
    }

    /// A link in the file's place is not written through.
    #[cfg(unix)]
    #[test]
    fn a_symlink_in_the_files_place_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, "untouched").unwrap();
        let path = dir.path().join("history.jsonl");
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        append(&path, "my prompt");
        assert_eq!(fs::read_to_string(&victim).unwrap(), "untouched");
    }
}
