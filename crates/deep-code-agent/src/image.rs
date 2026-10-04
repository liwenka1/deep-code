//! Image attachments: what the API accepts, what we refuse, and the one
//! conversion between a local file and the wire.
//!
//! The catalog's `supports_vision` says *which* models take an image; everything
//! else here comes from the Vision guide (api-docs.deepseek.com/guides/vision).
//! Two of its statements shape this module:
//!
//! * **The format is detected from the file's own bytes**, not from the name or
//!   a declared MIME type. Nothing here trusts an extension: the sniff below
//!   exists to build a correct `data:` prefix and to keep a file the request
//!   would fail on from being sent at all, and the server's verdict is still
//!   the final one.
//! * An image is legal **in a user message only** — a system or assistant
//!   message carrying one is a 400. That is why only user turns ever hold an
//!   [`ImageRef`], and why [`crate::message::Message::user_with_images`] is the
//!   only constructor that fills the field.
//!
//! Nothing in here fails a turn. An attachment that cannot be sent is replaced
//! by a note in the message text ([`ImageError::note`]), because one unreadable
//! file must not cost the user their message — and because the model has to be
//! told something was attached and is now missing, or it answers as if the user
//! had typed text alone.

use std::path::{Path, PathBuf};

use base64::Engine as _;

use crate::message::Message;

/// Where pasted images are kept, under the workspace root — the same
/// `.deep-code` tree the session files live in.
const IMAGES_DIR: &str = ".deep-code/images";
/// Both levels of [`IMAGES_DIR`] are ours and must be real directories.
const OWNED_STORE_DIRS: usize = 2;

/// Inline cap for one image, from the Vision guide's Limits table.
pub const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;
/// Inline cap for every image in one request, from the same table.
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// What one image adds to the context, at most.
///
/// The guide's rule: an image below roughly 544×544 is scaled *up*, anything
/// larger is scaled down to about a 1300×1300 pixel count, and the result is
/// bounded by 1024 tokens. So this is a ceiling, not an estimate, which is what
/// makes it safe to charge per image in [`crate::compaction`].
pub const MAX_TOKENS_PER_IMAGE: u32 = 1024;
/// Images allowed on one message.
///
/// The API's own cap is 600, which is a ceiling on the wire and not a budget:
/// every image is re-sent with every subsequent request of the session, so what
/// bounds us is the context window and the user's bill, not the protocol.
pub const MAX_IMAGES_PER_MESSAGE: usize = 4;

/// How the model should process an image (`image_url.detail`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ImageDetail {
    /// Downscaled to 512×512 first: faster and cheaper when fine detail does
    /// not matter.
    Low,
    /// The original image. Provided for compatibility; same as `Original`.
    High,
    /// The original image.
    Original,
    /// Automatic selection, currently equivalent to `Original`.
    #[default]
    Auto,
}

impl ImageDetail {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
            Self::Original => "original",
            Self::Auto => "auto",
        }
    }

    /// Parse a configured value. `None` for anything the API does not list, so
    /// a typo is reported rather than silently sent.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "high" => Some(Self::High),
            "original" => Some(Self::Original),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

/// The four formats the API accepts, detected from the file's own bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    WebP,
}

impl ImageFormat {
    #[must_use]
    pub fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::WebP => "image/webp",
        }
    }

    /// Short label for the composer chip.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Gif => "GIF",
            Self::WebP => "WebP",
        }
    }

    /// File extension, so a stored image's name tells the truth about the bytes
    /// that follow it.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::WebP => "webp",
        }
    }

    /// Identify a file from its leading bytes.
    ///
    /// `None` covers both "not one of ours" and "too short to tell", which need
    /// the same answer: do not attach it. The magic numbers are the formats'
    /// own signatures, so this cannot be fooled by a renamed file — which is the
    /// point, since a `.png` holding JSON is a request the API rejects.
    #[must_use]
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
            return Some(Self::Png);
        }
        // JPEG has no fixed trailer; every JFIF/Exif stream starts with SOI then
        // a marker byte, and `ff d8 ff` is the only safe prefix to require.
        if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            return Some(Self::Jpeg);
        }
        if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            return Some(Self::Gif);
        }
        // RIFF container: four bytes of little-endian length, then the form
        // type that says which RIFF flavour this is.
        if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            return Some(Self::WebP);
        }
        None
    }
}

/// An image on a turn, before and after it is resolved for the wire.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ImageRef {
    /// A local file. [`hydrate`] reads it and replaces this with [`Self::Url`].
    ///
    /// Serializing one of these is a hard error rather than a silent drop: a
    /// path is not a URL, and the only way it reaches a request is a bug in the
    /// request assembly, which should be loud.
    Path(PathBuf),
    /// What the API takes: a `data:` URL or an `http(s)` link.
    Url {
        url: String,
        detail: Option<ImageDetail>,
    },
}

impl ImageRef {
    /// The local path, when this has not been resolved yet.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Path(path) => Some(path),
            Self::Url { .. } => None,
        }
    }
}

/// A local file that passed inspection. The bytes are deliberately not kept:
/// the composer only needs the label and the size, and holding a 30 MiB image
/// in the UI until submit would cost that much resident memory per attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub path: PathBuf,
    pub format: ImageFormat,
    pub bytes: u64,
}

/// Why an image cannot be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    /// No such file. A session resumed after the user moved or deleted the
    /// image it referenced lands here.
    Missing,
    /// Exists, but is not a regular file (a directory, a device, a socket).
    NotAFile,
    Unreadable(String),
    /// Over the per-image cap, before the API gets a chance to reject it.
    TooLarge {
        bytes: u64,
    },
    /// Readable, but not one of the four formats (or too short to tell).
    UnsupportedFormat,
}

impl ImageError {
    /// The note that replaces an image the model cannot be shown.
    ///
    /// Bilingual like every other model-facing string the runtime writes (see
    /// `compaction::role_label`), because it lands in the context window and
    /// outlives the turn that produced it.
    #[must_use]
    pub fn note(&self, path: &Path) -> String {
        let path = path.display();
        match self {
            Self::Missing => format!("[图片不可用 / image unavailable: {path}]"),
            Self::NotAFile => {
                format!("[图片不可用 / image unavailable, not a file: {path}]")
            }
            Self::Unreadable(why) => format!("[图片不可读 / image unreadable: {path}: {why}]"),
            Self::TooLarge { bytes } => format!(
                "[图片过大 / image too large ({} > {}): {path}]",
                size_label(*bytes),
                size_label(MAX_IMAGE_BYTES),
            ),
            Self::UnsupportedFormat => format!(
                "[图片格式不支持 / unsupported image format, expected PNG/JPEG/GIF/WebP: {path}]"
            ),
        }
    }
}

/// A byte count for a model-facing note.
///
/// Rounds a MiB **up**, and falls back to raw bytes below one. Truncating
/// instead rendered a file one byte over the cap as `32 MiB > 32 MiB`, which
/// reads as a bug in the limit rather than in the file — and the note exists to
/// tell the model which of the two it is looking at.
#[must_use]
fn size_label(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB {
        format!("{} MiB", bytes.div_ceil(MIB))
    } else {
        format!("{bytes} B")
    }
}

/// The note for an image the chosen model cannot take.
///
/// A note rather than an error, and this is the whole point: a transcript that
/// already contains an image has to stay sendable. Refusing the turn instead
/// wedges the session — the image is in the history, every later turn re-derives
/// it, and a pinned model that lacks vision then refuses every request for the
/// rest of that session, including the text-only ones, with advice
/// ("remove the images") the user cannot act on because the images are not in the
/// draft any more. Degrading is also what keeps a session written by a build that
/// did not check this usable.
#[must_use]
fn not_accepted_note(image: &str) -> String {
    format!("[图片未发送 / image not sent, the current model does not accept images: {image}]")
}

/// Name an image for a note.
///
/// Never a `data:` URL: those are megabytes of base64, and every one of these
/// notes is model-facing context.
#[must_use]
fn describe_image(image: &ImageRef) -> String {
    match image {
        ImageRef::Path(path) => path.display().to_string(),
        ImageRef::Url { url, .. } if url.starts_with("data:") => "(inline image)".to_string(),
        ImageRef::Url { url, .. } => url.clone(),
    }
}

/// The note for an image dropped because the message already has enough.
#[must_use]
fn over_count_note(image: &str) -> String {
    format!("[图片已省略 / image omitted, at most {MAX_IMAGES_PER_MESSAGE} per message: {image}]")
}

/// The note for an image dropped because the request's total would be too big.
#[must_use]
fn over_total_note(image: &str, limit: u64) -> String {
    format!(
        "[图片已省略 / image omitted, request image total over {}: {image}]",
        size_label(limit)
    )
}

/// Check a local file and report what it is.
///
/// Reads the file's header only, so attaching a 30 MiB screenshot costs a few
/// bytes of I/O rather than the whole image — the full read happens once, in
/// [`data_url`], when the request is actually assembled.
pub fn inspect(path: &Path) -> Result<Attachment, ImageError> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            ImageError::Missing
        } else {
            ImageError::Unreadable(error.to_string())
        }
    })?;
    if !metadata.is_file() {
        return Err(ImageError::NotAFile);
    }
    let bytes = metadata.len();
    if bytes > MAX_IMAGE_BYTES {
        return Err(ImageError::TooLarge { bytes });
    }
    let header = read_header(path)?;
    let format = ImageFormat::sniff(&header).ok_or(ImageError::UnsupportedFormat)?;
    Ok(Attachment {
        path: path.to_path_buf(),
        format,
        bytes,
    })
}

/// The first bytes of a file, enough to identify any of the four formats.
fn read_header(path: &Path) -> Result<Vec<u8>, ImageError> {
    use std::io::Read as _;

    let mut file =
        std::fs::File::open(path).map_err(|error| ImageError::Unreadable(error.to_string()))?;
    let mut header = vec![0u8; 16];
    let read = file
        .read(&mut header)
        .map_err(|error| ImageError::Unreadable(error.to_string()))?;
    header.truncate(read);
    Ok(header)
}

/// The `data:` URL the API takes for a local file.
///
/// The prefix's media type comes from the sniff, not the extension: the server
/// detects the format from the bytes anyway, but a well-formed URL should not
/// need that leniency.
///
/// **Fence note.** This reads the file in the *process*, not through the tool
/// layer, so `workspace_policy` is not consulted. For a path that arrives from
/// the composer that is deliberate — the user chose it, by pasting, dropping, or
/// naming it, and the principle that leaves `--add-dir` unpoliced applies to
/// their own attachments too.
///
/// **A path from a session file is a weaker claim, and worth naming.** The
/// recorded path was a user's when it was written, but this crate's own threat
/// model holds the session file to be *model*-writable — that is why the root
/// grants in it carry an authorship tag at all (`runtime_launch`). So a model
/// that can write a `.deep-code/sessions/*.json` can name an image file
/// anywhere the process can read it and have its bytes sent to the provider on
/// the next turn. The window is narrow (the file must really be a PNG/JPEG/GIF/
/// WebP under the per-image cap, and the path must be known), and closing it
/// properly means resolving those paths through the workspace policy — which
/// costs the case the feature is for, a screenshot on the Desktop surviving a
/// `--continue`. Recorded here rather than left implied, because a future
/// `read_image` tool that reuses this function would inherit the gap with a much
/// shorter path to it.
pub fn data_url(path: &Path) -> Result<String, ImageError> {
    // The inspection is what rejects a missing file, a non-image, or an
    // oversized one *before* anything is read into memory.
    inspect(path)?;
    data_url_of(path)
}

/// Read a file and build the URL the API takes, trusting only its own bytes.
///
/// Split from [`data_url`] so `hydrate` — which inspects every image anyway, to
/// charge it against the request budget — does not pay for a second `metadata`
/// call and a second header read per image per request.
///
/// The media type comes from the bytes read **here**, never from the caller's
/// earlier inspection. The two reads are separate, so a file swapped in between
/// would otherwise be sent under a prefix that disagrees with its own payload —
/// and the server reads the payload, which is the 400 the sniff exists to
/// prevent. A non-image therefore fails here rather than being labelled anyway.
fn data_url_of(path: &Path) -> Result<String, ImageError> {
    let data = std::fs::read(path).map_err(|error| ImageError::Unreadable(error.to_string()))?;
    let format = ImageFormat::sniff(&data).ok_or(ImageError::UnsupportedFormat)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
    Ok(format!("data:{};base64,{encoded}", format.media_type()))
}

/// Turn every local path into a URL, degrading what cannot be sent.
///
/// Called once per model request, on the messages about to be serialized — and
/// therefore once per iteration of an agentic turn, because every iteration
/// re-derives the whole transcript. That is the cost of inlining images: each
/// image in the history is read and base64-encoded again for each request it
/// rides in. Per image per iteration that is a file read plus one encode; the
/// dominant cost is the body itself, which the request has to carry either way,
/// so this is not where a cache would pay. What it does NOT do is pay twice for
/// the same file: the inspection below is reused rather than repeated inside
/// [`data_url_of`].
///
/// Nothing here fails a turn. Four things can go wrong with one image — the file
/// is gone, it is not an image, it is too big, or the chosen model cannot take
/// images at all — and each becomes a note appended to that message's text, so
/// the model is told what it is missing instead of being silently shown less and
/// the user keeps their turn. A transcript that already contains an image must
/// stay sendable after a switch to a model without vision; refusing the request
/// instead would wedge the session for good (see [`not_accepted_note`]).
///
/// The caps are applied here too, for the same reason. `MAX_TOTAL_BYTES` is
/// charged across the *whole request* — a session that has accumulated images
/// over many turns is one request body, so checking per message would let a long
/// session sail past the limit and collect a 400 the client could have
/// prevented — and it is charged **newest-first**, within a message as well as
/// across them, so the budget is spent on what the user reached for last. Every
/// image that is dropped says so by path.
///
/// That order has a cost, and it is worth knowing before "fixing" it back: with
/// the oldest images charged first, whether a given message's images are sent
/// depends only on that message and the ones older than it, so the bytes of the
/// history never change as a session grows — the append-only property the prefix
/// cache and `telemetry::fingerprint` are built on. Charging newest-first makes
/// an old image's inclusion depend on the images *newer* than it, so once the
/// budget is exceeded the history is rewritten mid-way, the fingerprint reports
/// `Changed`, and the provider's cache misses from that point on. The two costs
/// appear in exactly the same situation (the request cannot hold everything) and
/// the choice is between them: a cache miss, or a turn whose own picture was
/// silently left out. A turn nobody can use is the worse of the two.
#[must_use]
pub fn hydrate(messages: Vec<Message>, detail: Option<ImageDetail>) -> Vec<Message> {
    hydrate_for(messages, detail, true, MAX_TOTAL_BYTES)
}

/// [`hydrate`] with the two things a caller has to know and a test has to vary:
/// whether the target model accepts images, and how big the request may get.
///
/// Parameters rather than constant reads so both can be exercised without a
/// 64 MiB fixture or a particular model — the same reason
/// [`crate::client`]'s `SseDecoder::with_limit` exists.
#[must_use]
pub(crate) fn hydrate_for(
    messages: Vec<Message>,
    detail: Option<ImageDetail>,
    accepted: bool,
    total_limit: u64,
) -> Vec<Message> {
    // One slot per message, decided newest-first and consumed in order.
    let mut outcomes: Vec<Vec<Result<ImageRef, String>>> = vec![Vec::new(); messages.len()];
    let mut remaining = total_limit;
    for (index, message) in messages.iter().enumerate().rev() {
        if message.images.is_empty() {
            continue;
        }
        // Newest-first *within* a message as well, so the byte budget is one
        // rule rather than two: what the user reached for last is what survives,
        // whether the overflow came from the images in this turn or from the
        // history behind it. Reversed back afterwards, because the parts array
        // has to line up with the chips the text refers to by number.
        //
        // The per-message *count* cap below is not part of that rule and stays
        // index-based, keeping the head: the text addresses chips as #1..#N, so
        // a message that somehow carries more than the cap is better off keeping
        // the ones its own text names first.
        let mut decisions = Vec::with_capacity(message.images.len());
        for (position, image) in message.images.iter().enumerate().rev() {
            // Both gates checked before the shape of the reference is looked at:
            // an image is an image whether or not it is already resolved, and
            // neither the model's capability nor the per-message cap may be
            // skipped because a URL happened to arrive pre-built.
            if !accepted {
                decisions.push(Err(not_accepted_note(&describe_image(image))));
                continue;
            }
            if position >= MAX_IMAGES_PER_MESSAGE {
                decisions.push(Err(over_count_note(&describe_image(image))));
                continue;
            }
            let path = match image {
                ImageRef::Path(path) => path,
                // Nothing to read and nothing to charge: already a URL.
                ImageRef::Url { .. } => {
                    decisions.push(Ok(image.clone()));
                    continue;
                }
            };
            match inspect(path) {
                Ok(attachment) => {
                    if attachment.bytes > remaining {
                        decisions.push(Err(over_total_note(&describe_image(image), total_limit)));
                        continue;
                    }
                    remaining -= attachment.bytes;
                    match data_url_of(path) {
                        Ok(url) => decisions.push(Ok(ImageRef::Url { url, detail })),
                        Err(error) => decisions.push(Err(error.note(path))),
                    }
                }
                Err(error) => decisions.push(Err(error.note(path))),
            }
        }
        decisions.reverse();
        outcomes[index] = decisions;
    }

    messages
        .into_iter()
        .zip(outcomes)
        .map(|(mut message, slots)| {
            let mut notes = Vec::new();
            message.images = slots
                .into_iter()
                .filter_map(|outcome| match outcome {
                    Ok(image) => Some(image),
                    Err(note) => {
                        notes.push(note);
                        None
                    }
                })
                .collect();
            if !notes.is_empty() {
                if !message.content.is_empty() {
                    message.content.push(' ');
                }
                message.content.push_str(&notes.join(" "));
            }
            message
        })
        .collect()
}

/// Why a pasted image could not be stored.
#[derive(Debug)]
pub enum StoreError {
    /// Not one of the four formats, so there is no honest name to give the
    /// file — and a `.png` holding something else is exactly the lie the
    /// content sniff exists to prevent.
    UnsupportedFormat,
    Io(std::io::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedFormat => {
                write!(
                    formatter,
                    "not a supported image format (PNG/JPEG/GIF/WebP)"
                )
            }
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Write pasted image bytes into the workspace store and return the path.
///
/// Content-addressed: the name is the SHA-256 of the bytes, so pasting the same
/// screenshot twice is one file, and the same image in two sessions is one file.
/// The extension comes from the sniff rather than from the caller, because a
/// clipboard file list can hand us a JPEG as easily as a PNG and the name has to
/// match the bytes.
///
/// Nothing sweeps this directory yet — a pasted image is only ever removed by
/// hand, which is why [`inspect`] treats a vanished file as a note rather than
/// an error.
///
/// `ring` rather than a fresh hash dependency: it is already a direct dependency
/// for the session-record grant tag, so this adds no build cost.
pub fn store(workspace: &Path, data: &[u8]) -> Result<PathBuf, StoreError> {
    let format = ImageFormat::sniff(data).ok_or(StoreError::UnsupportedFormat)?;
    let directory = workspace.join(IMAGES_DIR);
    // Two levels are ours — `.deep-code` and `images` under it — and both must
    // be real directories. Plain `create_dir_all` follows a symlink at either
    // level, which is how a repository shipping `.deep-code` as a link would
    // relocate every pasted screenshot outside the workspace. Same rule, same
    // reason, as `JsonSessionStore::for_workspace`.
    crate::paths::ensure_owned_dirs(&directory, OWNED_STORE_DIRS)?;
    // Images share `.deep-code` with the session transcripts, whose writer owns
    // the self-ignore. Doing it here too makes "this directory is not committed"
    // independent of which writer ran first.
    if let Some(state_dir) = directory.parent() {
        crate::session_store::write_self_ignore(state_dir);
    }
    let digest = ring::digest::digest(&ring::digest::SHA256, data);
    let path = directory.join(format!("{}.{}", hex(digest.as_ref()), format.extension()));
    // Same bytes, same name, same contents — rewriting is wasted I/O on the
    // common "pasted that again" path.
    if !path.exists() {
        std::fs::write(&path, data)?;
    }
    Ok(path)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4];
    const JPEG: &[u8] = &[0xff, 0xd8, 0xff, 0xe0, 1, 2, 3];
    const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00";
    const WEBP: &[u8] = b"RIFF\x24\x00\x00\x00WEBPVP8 ";

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn sniff_identifies_the_four_accepted_formats() {
        assert_eq!(ImageFormat::sniff(PNG), Some(ImageFormat::Png));
        assert_eq!(ImageFormat::sniff(JPEG), Some(ImageFormat::Jpeg));
        assert_eq!(ImageFormat::sniff(GIF), Some(ImageFormat::Gif));
        assert_eq!(ImageFormat::sniff(WEBP), Some(ImageFormat::WebP));
    }

    /// The extension is a claim; the bytes are the fact. A `.png` full of JSON
    /// is a request the API rejects, so it has to be caught here.
    #[test]
    fn sniff_refuses_what_is_not_an_image() {
        assert_eq!(ImageFormat::sniff(br#"{"hello":"world"}"#), None);
        assert_eq!(ImageFormat::sniff(b"<svg xmlns="), None);
        assert_eq!(ImageFormat::sniff(b""), None);
        assert_eq!(
            ImageFormat::sniff(b"\x89PNG"),
            None,
            "a truncated header is not a PNG"
        );
        // RIFF, but a WAVE file rather than a WEBP.
        assert_eq!(ImageFormat::sniff(b"RIFF\x24\x00\x00\x00WAVEfmt "), None);
    }

    #[test]
    fn magic_number_beats_a_lying_extension() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "not-really.png", br#"{"hello":"world"}"#);

        assert_eq!(inspect(&path), Err(ImageError::UnsupportedFormat));
    }

    /// The media type comes from the bytes, never from the caller's earlier
    /// inspection.
    ///
    /// Only a file swapped between the two reads can reach this branch — through
    /// `data_url` the inspection would have refused the file first — so the
    /// branch is asserted directly. That is the point: it is the guard that stops
    /// a payload from being sent under a prefix that contradicts it.
    #[test]
    fn the_media_type_comes_from_the_bytes_not_from_the_inspection() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "swapped.png", br#"{"now":"json"}"#);

        assert_eq!(data_url_of(&path), Err(ImageError::UnsupportedFormat));
    }

    #[test]
    fn data_url_carries_the_detected_media_type() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "shot.bin", JPEG);

        let url = data_url(&path).unwrap();

        assert!(url.starts_with("data:image/jpeg;base64,"), "{url}");
        assert_eq!(
            url,
            format!(
                "data:image/jpeg;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(JPEG)
            )
        );
    }

    #[test]
    fn inspect_reports_the_format_and_the_size() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "a.gif", GIF);

        let attachment = inspect(&path).unwrap();

        assert_eq!(attachment.format, ImageFormat::Gif);
        assert_eq!(attachment.bytes, GIF.len() as u64);
        assert_eq!(attachment.path, path);
    }

    #[test]
    fn a_missing_file_is_missing_not_a_generic_failure() {
        let dir = tempdir().unwrap();

        assert_eq!(
            inspect(&dir.path().join("gone.png")),
            Err(ImageError::Missing)
        );
        assert_eq!(
            data_url(&dir.path().join("gone.png")),
            Err(ImageError::Missing)
        );
    }

    #[test]
    fn a_directory_is_not_a_file() {
        let dir = tempdir().unwrap();

        assert_eq!(inspect(dir.path()), Err(ImageError::NotAFile));
    }

    #[test]
    fn an_oversized_file_is_refused_before_it_is_read() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("huge.png");
        // Sparse: the size is what is checked, and writing 32 MiB of real zeros
        // to prove a metadata comparison would only slow the suite down.
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_IMAGE_BYTES + 1).unwrap();
        drop(file);

        assert_eq!(
            inspect(&path),
            Err(ImageError::TooLarge {
                bytes: MAX_IMAGE_BYTES + 1
            })
        );
    }

    #[test]
    fn every_error_names_the_path_it_is_about() {
        let path = Path::new("/tmp/shot.png");

        for error in [
            ImageError::Missing,
            ImageError::NotAFile,
            ImageError::Unreadable("permission denied".to_string()),
            ImageError::TooLarge { bytes: u64::MAX },
            ImageError::UnsupportedFormat,
        ] {
            let note = error.note(path);
            assert!(note.contains("/tmp/shot.png"), "{note}");
            assert!(note.starts_with('[') && note.ends_with(']'), "{note}");
        }
    }

    // -----------------------------------------------------------------------
    // store
    // -----------------------------------------------------------------------

    #[test]
    fn store_is_content_addressed_and_lives_under_the_workspace() {
        let dir = tempdir().unwrap();

        let first = store(dir.path(), PNG).unwrap();
        let again = store(dir.path(), PNG).unwrap();
        let other = store(dir.path(), JPEG).unwrap();

        assert_eq!(first, again, "the same bytes are the same file");
        assert_ne!(first, other);
        assert!(first.starts_with(dir.path().join(IMAGES_DIR)), "{first:?}");
        assert!(first.to_string_lossy().ends_with(".png"));
        assert_eq!(std::fs::read(&first).unwrap(), PNG);
    }

    /// A clipboard file list can hand us a JPEG as easily as a PNG, so the name
    /// has to come from the bytes — a `.png` holding JPEG data is the lie the
    /// sniff exists to prevent.
    #[test]
    fn store_names_the_file_after_its_actual_format() {
        let dir = tempdir().unwrap();

        let jpeg = store(dir.path(), JPEG).unwrap();
        let gif = store(dir.path(), GIF).unwrap();

        assert!(jpeg.to_string_lossy().ends_with(".jpg"), "{jpeg:?}");
        assert!(gif.to_string_lossy().ends_with(".gif"), "{gif:?}");
    }

    #[test]
    fn store_refuses_bytes_that_are_not_an_image() {
        let dir = tempdir().unwrap();

        let error = store(dir.path(), br#"{"hello":"world"}"#).expect_err("JSON is not an image");

        assert!(matches!(error, StoreError::UnsupportedFormat));
        assert!(
            !dir.path().join(IMAGES_DIR).exists(),
            "nothing should be created for a file that cannot be stored"
        );
    }

    #[test]
    fn stored_bytes_read_back_as_an_attachment() {
        let dir = tempdir().unwrap();
        let path = store(dir.path(), PNG).unwrap();

        let attachment = inspect(&path).unwrap();

        assert_eq!(attachment.format, ImageFormat::Png);
    }

    // -----------------------------------------------------------------------
    // hydrate
    // -----------------------------------------------------------------------

    fn png_ref(path: &Path) -> ImageRef {
        ImageRef::Path(path.to_path_buf())
    }

    #[test]
    fn hydrate_resolves_paths_and_applies_the_configured_detail() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "shot.png", PNG);
        let messages = vec![Message::user_with_images(
            "what is this",
            vec![png_ref(&path)],
        )];

        let hydrated = hydrate(messages, Some(ImageDetail::Low));

        assert_eq!(hydrated.len(), 1);
        assert_eq!(hydrated[0].content, "what is this");
        match &hydrated[0].images[..] {
            [ImageRef::Url { url, detail }] => {
                assert!(url.starts_with("data:image/png;base64,"), "{url}");
                assert_eq!(*detail, Some(ImageDetail::Low));
            }
            other => panic!("expected one resolved URL, got {other:?}"),
        }
    }

    #[test]
    fn hydrate_leaves_a_message_without_images_alone() {
        let messages = vec![
            Message::system("you are deep-code"),
            Message::user("hello"),
            Message::assistant_turn("hi", "hmm", Vec::new()),
        ];

        let hydrated = hydrate(messages.clone(), None);

        assert_eq!(hydrated, messages);
    }

    /// One unreadable attachment must not cost the user their message.
    #[test]
    fn a_missing_image_becomes_a_note_in_the_text() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("gone.png");
        let messages = vec![Message::user_with_images(
            "look at this",
            vec![png_ref(&path)],
        )];

        let hydrated = hydrate(messages, None);

        assert!(hydrated[0].images.is_empty());
        assert!(
            hydrated[0].content.starts_with("look at this "),
            "{:?}",
            hydrated[0].content
        );
        assert!(
            hydrated[0].content.contains("image unavailable"),
            "{:?}",
            hydrated[0].content
        );
        assert!(hydrated[0].content.contains("gone.png"));
    }

    #[test]
    fn a_non_image_becomes_a_note_rather_than_an_error() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "data.json", br#"{"a":1}"#);
        let messages = vec![Message::user_with_images("", vec![png_ref(&path)])];

        let hydrated = hydrate(messages, None);

        assert!(hydrated[0].images.is_empty());
        assert!(hydrated[0].content.contains("unsupported image format"));
    }

    /// A good image survives next to a bad one: the failure costs the file, not
    /// the message.
    #[test]
    fn a_resolvable_image_survives_a_sibling_that_fails() {
        let dir = tempdir().unwrap();
        let good = write(dir.path(), "good.png", PNG);
        let bad = write(dir.path(), "bad.json", b"{}");
        let messages = vec![Message::user_with_images(
            "two files",
            vec![png_ref(&good), png_ref(&bad)],
        )];

        let hydrated = hydrate(messages, None);

        assert_eq!(hydrated[0].images.len(), 1);
        assert!(hydrated[0].content.contains("unsupported image format"));
    }

    #[test]
    fn hydrate_omits_images_past_the_per_message_cap() {
        let dir = tempdir().unwrap();
        let paths: Vec<PathBuf> = (0..MAX_IMAGES_PER_MESSAGE + 1)
            .map(|index| {
                // Distinct bytes each, so each is its own file.
                let mut bytes = PNG.to_vec();
                bytes.push(index as u8);
                write(dir.path(), &format!("shot-{index}.png"), &bytes)
            })
            .collect();
        let messages = vec![Message::user_with_images(
            "many",
            paths.iter().map(|path| png_ref(path)).collect(),
        )];

        let hydrated = hydrate(messages, None);

        assert_eq!(hydrated[0].images.len(), MAX_IMAGES_PER_MESSAGE);
        assert!(
            hydrated[0].content.contains("image omitted"),
            "{:?}",
            hydrated[0].content
        );
    }

    #[test]
    fn hydrate_passes_an_already_resolved_url_through() {
        let messages = vec![Message::user_with_images(
            "external",
            vec![ImageRef::Url {
                url: "https://example.com/a.jpg".to_string(),
                detail: Some(ImageDetail::High),
            }],
        )];

        let hydrated = hydrate(messages.clone(), Some(ImageDetail::Low));

        assert_eq!(hydrated, messages, "an existing detail is not overwritten");
    }

    /// A model that cannot take images gets a sentence instead of the picture —
    /// never a failed request. The note is what keeps a transcript sendable after
    /// a model switch, so the turn survives and only the image is lost.
    #[test]
    fn a_model_that_cannot_take_images_gets_a_note_instead_of_the_image() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "shot.png", PNG);
        let messages = vec![Message::user_with_images(
            "look at this",
            vec![png_ref(&path)],
        )];

        let hydrated = hydrate_for(messages, None, false, MAX_TOTAL_BYTES);

        assert!(hydrated[0].images.is_empty());
        assert!(
            hydrated[0].content.starts_with("look at this "),
            "the text is kept: {:?}",
            hydrated[0].content
        );
        assert!(
            hydrated[0].content.contains("image not sent"),
            "{:?}",
            hydrated[0].content
        );
        assert!(hydrated[0].content.contains("shot.png"));
    }

    /// Both gates come before the already-a-URL arm, so a URL cannot slip past
    /// either: a model that does not accept images must not be sent one just
    /// because it arrived pre-resolved, and the per-message cap has to count
    /// every image the message carries.
    #[test]
    fn an_already_a_url_image_obeys_both_gates() {
        let url = || {
            vec![ImageRef::Url {
                url: "data:image/png;base64,AA".to_string(),
                detail: None,
            }]
        };

        let refused = hydrate_for(
            vec![Message::user_with_images("look", url())],
            None,
            false,
            MAX_TOTAL_BYTES,
        );
        assert!(
            refused[0].images.is_empty(),
            "a pre-resolved URL is still an image, and the model cannot take one"
        );
        assert!(refused[0].content.contains("image not sent"));

        let over = vec![Message::user_with_images(
            "many",
            (0..MAX_IMAGES_PER_MESSAGE + 1)
                .flat_map(|_| url())
                .collect(),
        )];
        let capped = hydrate_for(over, None, true, MAX_TOTAL_BYTES);
        assert_eq!(capped[0].images.len(), MAX_IMAGES_PER_MESSAGE);
    }

    /// Newest-first inside one message too: three images that individually fit
    /// but together do not must lose the earliest, not the one just attached.
    #[test]
    fn an_overflowing_message_keeps_the_newest_images() {
        let dir = tempdir().unwrap();
        // Three 12-byte images and a 24-byte budget: two fit.
        let paths: Vec<PathBuf> = ["a.png", "b.png", "c.png"]
            .iter()
            .map(|name| write(dir.path(), name, PNG))
            .collect();
        let messages = vec![Message::user_with_images(
            "three",
            paths.iter().map(|path| png_ref(path)).collect(),
        )];

        let hydrated = hydrate_for(messages, None, true, 24);

        assert_eq!(hydrated[0].images.len(), 2, "two fit");
        assert!(
            hydrated[0].content.contains("a.png"),
            "the earliest gives way: {:?}",
            hydrated[0].content
        );
        assert!(
            !hydrated[0].content.contains("b.png") && !hydrated[0].content.contains("c.png"),
            "and the two newest are kept: {:?}",
            hydrated[0].content
        );
    }

    /// The same rule through the default entry point, which every production
    /// caller but the turn loop uses: images are accepted.
    #[test]
    fn the_default_entry_point_accepts_images() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "shot.png", PNG);
        let messages = vec![Message::user_with_images("look", vec![png_ref(&path)])];

        assert_eq!(hydrate(messages.clone(), None)[0].images.len(), 1);
        assert_eq!(
            hydrate_for(messages, None, true, MAX_TOTAL_BYTES)[0]
                .images
                .len(),
            1
        );
    }

    /// The request-wide budget, not a per-message one: two messages that each
    /// fit on their own still have to fit together, because they become one
    /// request body.
    ///
    /// And it is spent **newest-first**, which is the part a user would notice:
    /// a session with more image history than fits gives up its oldest
    /// screenshots, not the one that was just attached. Losing the new one is a
    /// picture the user can see in their own draft and watch fail to arrive.
    #[test]
    fn the_request_budget_is_spent_on_the_newest_images_first() {
        let dir = tempdir().unwrap();
        // `PNG` is 12 bytes, so a 20-byte budget admits one of the two.
        let first = write(dir.path(), "a.png", PNG);
        let second = write(dir.path(), "b.png", PNG);
        let messages = vec![
            Message::user_with_images("first", vec![png_ref(&first)]),
            Message::user_with_images("second", vec![png_ref(&second)]),
        ];

        let hydrated = hydrate_for(messages, None, true, 20);

        assert!(
            hydrated[0].images.is_empty(),
            "the older image gives way, not the newer one"
        );
        assert_eq!(hydrated[1].images.len(), 1, "the newest image is sent");
        assert!(
            hydrated[0].content.contains("image omitted"),
            "{:?}",
            hydrated[0].content
        );
        assert!(
            hydrated[0].content.contains("a.png"),
            "the note names what was dropped: {:?}",
            hydrated[0].content
        );
    }

    #[test]
    fn a_budget_that_fits_everything_drops_nothing() {
        let dir = tempdir().unwrap();
        let first = write(dir.path(), "a.png", PNG);
        let second = write(dir.path(), "b.png", PNG);
        let messages = vec![
            Message::user_with_images("first", vec![png_ref(&first)]),
            Message::user_with_images("second", vec![png_ref(&second)]),
        ];

        let hydrated = hydrate_for(messages, None, true, 24);

        assert_eq!(hydrated[0].images.len(), 1);
        assert_eq!(hydrated[1].images.len(), 1);
        assert_eq!(hydrated[0].content, "first");
        assert_eq!(hydrated[1].content, "second");
    }

    #[test]
    fn hydrate_keeps_several_images_in_order() {
        let dir = tempdir().unwrap();
        let first = write(dir.path(), "a.png", PNG);
        let second = {
            let mut bytes = PNG.to_vec();
            bytes.push(9);
            write(dir.path(), "b.png", &bytes)
        };
        let messages = vec![Message::user_with_images(
            "compare",
            vec![png_ref(&first), png_ref(&second)],
        )];

        let hydrated = hydrate(messages, None);

        let urls: Vec<&str> = hydrated[0]
            .images
            .iter()
            .map(|image| match image {
                ImageRef::Url { url, .. } => url.as_str(),
                ImageRef::Path(path) => panic!("unresolved: {path:?}"),
            })
            .collect();
        assert_eq!(urls.len(), 2);
        assert_ne!(urls[0], urls[1]);
        assert!(urls[0].contains(&base64::engine::general_purpose::STANDARD.encode(PNG)));
    }

    /// The whole chain, in one assertion: a local path becomes the exact request
    /// body DeepSeek documents. Every other test here pins one link; this is the
    /// one that catches the links disagreeing — a data URL the parts array does
    /// not accept, or a `detail` in the wrong place.
    #[test]
    fn a_hydrated_turn_serializes_into_the_documented_request_shape() {
        let dir = tempdir().unwrap();
        let path = write(dir.path(), "chart.png", PNG);
        let messages = vec![Message::user_with_images(
            "what does this chart show",
            vec![png_ref(&path)],
        )];

        let hydrated = hydrate(messages, Some(ImageDetail::Low));
        let request = crate::model::ChatRequest::streaming("deepseek-flash", hydrated);
        let json = serde_json::to_value(&request).unwrap();

        assert_eq!(json["model"], "deepseek-flash");
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(
            json["messages"][0]["content"][0],
            serde_json::json!({"type": "text", "text": "what does this chart show"})
        );
        assert_eq!(json["messages"][0]["content"][1]["type"], "image_url");
        assert_eq!(
            json["messages"][0]["content"][1]["image_url"]["detail"],
            "low"
        );
        let url = json["messages"][0]["content"][1]["image_url"]["url"]
            .as_str()
            .expect("the url is a string");
        assert!(url.starts_with("data:image/png;base64,"), "{url}");
    }

    /// The failure path, end to end: an image that has gone missing must reach
    /// the provider as a *sentence*, not as an empty parts array the model reads
    /// as a question with no subject.
    #[test]
    fn a_turn_whose_only_image_is_gone_still_serializes_as_text() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("deleted.png");
        let messages = vec![Message::user_with_images(
            "look at this",
            vec![png_ref(&path)],
        )];

        let hydrated = hydrate(messages, None);
        let request = crate::model::ChatRequest::streaming("deepseek-flash", hydrated);
        let json = serde_json::to_value(&request).unwrap();

        let content = json["messages"][0]["content"]
            .as_str()
            .expect("with no image left this is a plain string again");
        assert!(content.contains("image unavailable"), "{content}");
        assert!(content.contains("deleted.png"), "{content}");
    }

    // -----------------------------------------------------------------------
    // detail
    // -----------------------------------------------------------------------

    #[test]
    fn detail_round_trips_through_its_wire_spelling() {
        for detail in [
            ImageDetail::Low,
            ImageDetail::High,
            ImageDetail::Original,
            ImageDetail::Auto,
        ] {
            assert_eq!(ImageDetail::parse(detail.as_str()), Some(detail));
        }
        assert_eq!(ImageDetail::parse(" LOW "), Some(ImageDetail::Low));
    }

    #[test]
    fn an_unlisted_detail_is_refused_rather_than_sent() {
        assert_eq!(ImageDetail::parse("medium"), None);
        assert_eq!(ImageDetail::parse(""), None);
    }
}
