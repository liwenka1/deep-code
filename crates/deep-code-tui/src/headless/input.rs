//! Prompt assembly for headless runs.
//!
//! Two sources feed one submitted prompt: the positional argument (the
//! instruction) and piped stdin (the data — a diff, a log, a file). Keeping
//! them distinguishable in the composed text matters: pasting a log *as* the
//! instruction invites the model to obey text that was meant as evidence.

use std::io::{IsTerminal, Read};

/// Marker inserted between the instruction and piped data when both are
/// present. Part of the prompt users see in the transcript — change wording,
/// not meaning.
const STDIN_MARKER: &str = "--- stdin ---";

/// How long to wait for the first byte of piped stdin when a positional prompt
/// is already present. Long enough that a real producer (`git diff | …`) is
/// always caught — its output is buffered in the pipe before we look — short
/// enough that an *idle* inherited stdin (a Node `spawn` with default stdio, an
/// `ssh host deepcode -p …` without `-n`, `docker run -i`) does not hang the run
/// forever: after this it proceeds on the positional prompt alone.
#[cfg(unix)]
const STDIN_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Read piped stdin, if any. A TTY stdin means "nothing piped": reading it
/// would block on the keyboard, which a headless run must never do. Bytes are
/// converted lossily — logs are routinely not valid UTF-8, and refusing the
/// whole run over one byte helps nobody.
///
/// `has_positional` says a positional prompt was ALSO given, which makes stdin
/// optional extra data rather than the prompt itself. In that case a non-file
/// stdin that is idle (no data waiting) is skipped instead of blocked on: an
/// inherited-but-empty pipe otherwise hung `deepcode -p "…"` forever with no
/// output, and `--timeout` did not help because this read runs before the
/// deadline is armed. A real pipe with data waiting (`git diff | deepcode -p`)
/// is still read in full.
pub(crate) fn read_piped_stdin(has_positional: bool) -> Option<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    if has_positional && !stdin_has_input_ready() {
        return None;
    }
    let mut bytes = Vec::new();
    stdin.lock().read_to_end(&mut bytes).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Whether stdin has data (or EOF) ready within [`STDIN_WAIT`]. Only consulted
/// when a positional prompt makes stdin optional, so the answer decides "read
/// it" vs "skip the idle pipe". A regular file always polls ready (files never
/// block), so it needs no special case; only an idle pipe/FIFO times out.
#[cfg(unix)]
fn stdin_has_input_ready() -> bool {
    use std::os::unix::io::AsRawFd;
    let mut fds = libc::pollfd {
        fd: std::io::stdin().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: single pollfd, bounded timeout; poll(2) only reads/writes `fds`.
    let ready = unsafe { libc::poll(&mut fds, 1, STDIN_WAIT.as_millis() as libc::c_int) };
    // >0 with POLLIN/POLLHUP means data or EOF is available to read now; 0 is
    // the idle-pipe timeout (skip); <0 is an error (fall back to skipping, since
    // this path only runs when a positional prompt can carry the run).
    ready > 0 && (fds.revents & (libc::POLLIN | libc::POLLHUP)) != 0
}

/// Non-unix: no `poll(2)` here, so keep the historical behavior (read to end).
/// The idle-pipe hang is a documented residual on Windows headless runs.
#[cfg(not(unix))]
fn stdin_has_input_ready() -> bool {
    true
}

/// Merge the positional prompt and piped stdin into the submitted prompt.
/// `None` means "nothing to run" — the caller turns that into a usage error.
pub(crate) fn compose_prompt(positional: Option<&str>, piped: Option<&str>) -> Option<String> {
    let instruction = positional.map(str::trim).filter(|text| !text.is_empty());
    let data = piped.filter(|text| !text.trim().is_empty());
    match (instruction, data) {
        (Some(instruction), Some(data)) => Some(format!("{instruction}\n\n{STDIN_MARKER}\n{data}")),
        (Some(instruction), None) => Some(instruction.to_string()),
        (None, Some(data)) => Some(data.to_string()),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argument_alone_is_the_prompt() {
        assert_eq!(
            compose_prompt(Some("  fix the bug  "), None).as_deref(),
            Some("fix the bug")
        );
    }

    #[test]
    fn piped_stdin_alone_is_the_prompt_verbatim() {
        // Data content is not trimmed: leading/trailing whitespace can be
        // meaningful in logs and patches.
        assert_eq!(
            compose_prompt(None, Some("line 1\nline 2\n")).as_deref(),
            Some("line 1\nline 2\n")
        );
    }

    #[test]
    fn both_present_keeps_instruction_and_data_separated() {
        let composed = compose_prompt(Some("explain this"), Some("panic at main.rs:1")).unwrap();
        assert_eq!(
            composed,
            "explain this\n\n--- stdin ---\npanic at main.rs:1"
        );
    }

    #[test]
    fn blank_sources_yield_none() {
        assert_eq!(compose_prompt(None, None), None);
        assert_eq!(compose_prompt(Some("   "), Some(" \n ")), None);
    }
}
