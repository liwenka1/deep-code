//! Clipboard copy. Locally we shell out to the OS clipboard tool (pbcopy /
//! wl-copy / xclip / clip), which is UTF-8 safe and reliable. Over SSH — where
//! no local clipboard tool is reachable — we fall back to the OSC 52 escape
//! sequence so the *local* terminal still receives the copy.

use std::io::Write;

/// Copy `text` to the system clipboard.
pub(crate) fn copy(text: &str) {
    // Over SSH the native tool would target the remote host, so the only way to
    // reach the user's local clipboard is OSC 52 (handled by their terminal).
    if !is_ssh() && copy_with_native_tool(text) {
        return;
    }
    copy_osc52(text);
}

fn is_ssh() -> bool {
    std::env::var_os("SSH_TTY").is_some() || std::env::var_os("SSH_CONNECTION").is_some()
}

/// Platform clipboard commands, tried in order. The first that spawns and exits
/// successfully wins.
#[cfg(target_os = "macos")]
const NATIVE_CLIPBOARD_COMMANDS: &[(&str, &[&str])] = &[("pbcopy", &[])];
#[cfg(target_os = "linux")]
const NATIVE_CLIPBOARD_COMMANDS: &[(&str, &[&str])] = &[
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["-ib"]),
];
#[cfg(target_os = "windows")]
const NATIVE_CLIPBOARD_COMMANDS: &[(&str, &[&str])] = &[("clip", &[])];
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
const NATIVE_CLIPBOARD_COMMANDS: &[(&str, &[&str])] = &[];

fn copy_with_native_tool(text: &str) -> bool {
    NATIVE_CLIPBOARD_COMMANDS
        .iter()
        .any(|(command, args)| write_to_command(command, args, text))
}

/// Pipe `text` to `command` via stdin. Returns true only when the tool exists
/// and exits successfully. stdin is dropped before waiting so the tool sees EOF.
fn write_to_command(command: &str, args: &[&str], text: &str) -> bool {
    use std::process::{Command, Stdio};
    let Ok(mut child) = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    matches!(child.wait(), Ok(status) if status.success())
}

/// Fallback: write the clipboard via the OSC 52 escape sequence.
fn copy_osc52(text: &str) {
    let seq = format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Minimal standard-alphabet base64 (avoids a crate just for OSC 52).
pub(crate) fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode("你好".as_bytes()), "5L2g5aW9");
    }

    #[test]
    fn native_clipboard_commands_present_on_desktop() {
        // macOS/Linux/Windows ship a clipboard tool; the list must be non-empty
        // there so `copy` doesn't silently depend on OSC 52 for the common case.
        if cfg!(any(target_os = "macos", target_os = "linux", target_os = "windows")) {
            assert!(!NATIVE_CLIPBOARD_COMMANDS.is_empty());
        }
    }

    #[test]
    fn write_to_command_reports_failure_for_missing_tool() {
        assert!(!write_to_command(
            "deep-code-no-such-clipboard-tool",
            &[],
            "hi"
        ));
    }
}
