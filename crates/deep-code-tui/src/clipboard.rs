//! Clipboard access. Locally we shell out to the OS clipboard tool for
//! *copying* (pbcopy / wl-copy / xclip / clip), which is UTF-8 safe and reliable
//! on macOS/Linux. On Windows, clip.exe uses the system ANSI code page (e.g.
//! GBK on Chinese Windows), which corrupts any non-ASCII text, so we use the
//! Win32 clipboard API (CF_UNICODETEXT) instead.
//! Over SSH — where no local clipboard tool is reachable — we fall back to
//! the OSC 52 escape sequence so the *local* terminal still receives the copy.
//!
//! *Reading* is a different problem, and an image is the only reason to do it.
//! A terminal delivers a paste as text, so a screenshot on the clipboard is
//! invisible unless we go and ask the OS for it; `arboard` is that ask, and it
//! is why this module has a dependency the copy path never needed.

use std::io::Write;

/// An image from the system clipboard.
///
/// Three outcomes, and the middle one is not a failure:
///
/// * `None` — there is nothing to read: the clipboard holds no image, or this
///   host has no readable clipboard at all (a bare container, no X11/Wayland), or
///   we are over SSH. All three mean "`Ctrl+V` is not the image channel here",
///   which is not something to tell the user about on every keypress. `Ctrl+V`
///   reaches us only in terminals that forward the key rather than pasting
///   themselves, and those users are usually copying text.
/// * `Some(Err(_))` — there is genuinely something to say: an image was found
///   and could not be encoded.
/// * `Some(Ok(bytes))` — image bytes, in one of the four formats
///   [`deep_code_agent::ImageFormat`] accepts. **Not necessarily PNG**: copying
///   a file in Finder puts the file itself on the clipboard, and re-encoding it
///   would only lose quality.
///
/// A **non-image file** on the clipboard is not a fourth outcome, and that is a
/// decision rather than an oversight: it reads as `None`. The alternative —
/// putting the copied path into the composer — was considered and dropped,
/// because `Ctrl+V` means "here is something to look at", and answering it with
/// sixty characters of path for a file the model still cannot open is a worse
/// surprise than silence. Dropping a file onto the terminal, or naming it with
/// `@`, is how a path gets into a prompt.
pub(crate) fn read_image() -> Option<Result<Vec<u8>, String>> {
    // Over SSH there is no *local* clipboard to read: the terminal's own paste is
    // how text gets in, and `arboard` would either fail or hand back the remote
    // host's clipboard — which is not what the user just copied. Reading is the
    // one direction OSC 52 cannot help with, so there is no fallback here; the
    // choice is between silence and a wrong answer, and silence wins.
    if is_ssh() {
        return None;
    }

    // A host with no clipboard at all is a fact about the machine, not a failure
    // of this paste. Reporting it would put a status line under every `Ctrl+V`,
    // which is the noise `None` exists to avoid.
    let mut clipboard = arboard::Clipboard::new().ok()?;

    // A file copied in Finder (or Explorer) is a file LIST, not pixels, and the
    // file is already where we want it — so prefer it, and hand back its bytes
    // untouched.
    if let Ok(files) = clipboard.get().file_list() {
        for file in files {
            // `inspect` before `read`: it is a metadata check plus a 16-byte
            // header sniff, so a file we would refuse anyway — too big, not an
            // image, already gone — costs nothing. Reading first would pull a
            // multi-gigabyte file into memory to find out.
            if deep_code_agent::inspect(&file).is_err() {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&file) {
                return Some(Ok(bytes));
            }
        }
    }

    // Otherwise it is pixels — from a screenshot, or an image copied out of a
    // browser — and those arrive as raw RGBA, which has to be encoded before it
    // can be sent anywhere.
    match clipboard.get_image() {
        Ok(image) => Some(encode_png(image)),
        Err(_) => None,
    }
}

/// Encode raw clipboard pixels as PNG.
fn encode_png(image: arboard::ImageData<'_>) -> Result<Vec<u8>, String> {
    let width = u32::try_from(image.width).map_err(|_| "image is impossibly wide".to_string())?;
    let height = u32::try_from(image.height).map_err(|_| "image is impossibly tall".to_string())?;
    let rgba = image::RgbaImage::from_raw(width, height, image.bytes.into_owned())
        .ok_or_else(|| "clipboard image buffer does not match its dimensions".to_string())?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|error| format!("could not encode the clipboard image: {error}"))?;
    Ok(png)
}

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
    // On Windows, use the Win32 clipboard API directly because clip.exe
    // interprets bytes using the system ANSI code page (e.g. GBK on Chinese
    // Windows), corrupting any non-ASCII text.
    #[cfg(target_os = "windows")]
    if copy_with_win32_api(text) {
        return true;
    }

    NATIVE_CLIPBOARD_COMMANDS
        .iter()
        .any(|(command, args)| write_to_command(command, args, text))
}

/// Windows-specific: use the Win32 clipboard API with CF_UNICODETEXT so that
/// all Unicode characters are copied correctly regardless of system code page.
#[cfg(target_os = "windows")]
fn copy_with_win32_api(text: &str) -> bool {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::*;
    use windows_sys::Win32::System::DataExchange::*;
    use windows_sys::Win32::System::Memory::*;

    // CF_UNICODETEXT = 13 — well-known Windows clipboard format for UTF-16 text.
    // Not exported by windows-sys 0.59, so we define it locally.
    const CF_UNICODETEXT: u32 = 13;

    unsafe {
        // Convert to null-terminated UTF-16
        let wide: Vec<u16> = OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let byte_size = wide.len() * 2;

        // Allocate movable global memory
        let handle = GlobalAlloc(GMEM_MOVEABLE, byte_size);
        if handle.is_null() {
            return false;
        }

        // Lock, copy UTF-16 data, unlock
        let ptr = GlobalLock(handle) as *mut u16;
        if ptr.is_null() {
            GlobalFree(handle);
            return false;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        GlobalUnlock(handle);

        // Open clipboard and set Unicode text
        if OpenClipboard(std::ptr::null_mut()) == FALSE {
            GlobalFree(handle);
            return false;
        }
        EmptyClipboard();
        let result = SetClipboardData(CF_UNICODETEXT, handle);
        CloseClipboard();

        if result.is_null() {
            // SetClipboardData failed; the handle is still ours, free it
            GlobalFree(handle);
            return false;
        }
        true
    }
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
///
/// The **standard** alphabet, not the URL-safe one: OSC 52 carries the payload
/// in a control sequence, and terminals decode it against RFC 4648 `base64`.
/// Swapping in the URL-safe engine would silently mangle every `+` and `/` in
/// the copied text, which is why the test below pins the vectors rather than
/// trusting the engine's name.
fn copy_osc52(text: &str) {
    use base64::Engine as _;

    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let seq = format!("\x1b]52;c;{encoded}\x07");
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The OSC 52 payload has to be RFC 4648 standard base64 — the URL-safe
    /// alphabet mangles `+` and `/`, and a terminal decoding against the
    /// standard one would render the copied text wrong with no error anywhere.
    /// Vectors rather than the engine's name, because the name is not what the
    /// terminal reads.
    #[test]
    fn osc52_payload_is_standard_base64() {
        use base64::Engine as _;

        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);

        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode("你好".as_bytes()), "5L2g5aW9");
        // The two bytes that separate the standard alphabet from the URL-safe
        // one, which is the whole point of pinning this.
        assert_eq!(encode(&[0xfb, 0xff]), "+/8=");
    }

    #[test]
    fn native_clipboard_commands_present_on_desktop() {
        // macOS/Linux/Windows ship a clipboard tool; the list must be non-empty
        // there so `copy` doesn't silently depend on OSC 52 for the common case.
        if cfg!(any(
            target_os = "macos",
            target_os = "linux",
            target_os = "windows"
        )) {
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
