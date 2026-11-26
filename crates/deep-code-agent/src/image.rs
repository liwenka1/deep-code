//! Image admission, content-addressed observations, and request projection.
//!
//! User-selected paths are input intentions. Persisted observations reference
//! exact stored bytes; they never reopen those source paths. Tool readers must
//! obtain bytes through their own filesystem authorization before admission.
//! Request preparation resolves references and reports every omitted image.

use std::cell::Cell;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use cap_fs_ext::DirExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::content::ContentBlock;
use crate::message::Message;
use crate::message::Role;
use crate::model::ChatRequest;

const IMAGES_DIR: &str = ".deep-code/images";
// Each full chunk encodes to base64 without padding.
const IMAGE_CHUNK_BYTES: usize = 24 * 1024;

/// Bound automatic reads by tools. User-selected files use the submission path.
pub(crate) const MAX_TOOL_IMAGE_BYTES: u64 = 32 * 1024 * 1024;
// These budgets apply only when replaying historical images.
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REQUEST_BYTES: u64 = 48 * 1024 * 1024;
pub const MAX_TOKENS_PER_IMAGE: u32 = 1024;

/// Provider-side processing detail. This does not resize the uploaded bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ImageDetail {
    Low,
    Original,
    #[default]
    Auto,
}

impl ImageDetail {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Original => "original",
            Self::Auto => "auto",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "original" => Some(Self::Original),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

/// Header identification, not a complete image decoder. The provider validates
/// the encoded image before inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Gif => "GIF",
            Self::WebP => "WebP",
        }
    }

    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::WebP => "webp",
        }
    }

    #[must_use]
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
            return Some(Self::Png);
        }
        if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            return Some(Self::Jpeg);
        }
        if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            return Some(Self::Gif);
        }
        if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            return Some(Self::WebP);
        }
        None
    }
}

/// A durable observation. The digest names the exact bytes admitted from an
/// authorized reader, rather than a mutable source path or provider file id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageAsset {
    pub workspace: PathBuf,
    pub digest: String,
    pub format: ImageFormat,
    pub bytes: u64,
}

impl ImageAsset {
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.workspace.join(IMAGES_DIR).join(self.file_name())
    }

    fn file_name(&self) -> String {
        format!("{}.{}", self.digest, self.format.extension())
    }

    pub(crate) fn validate_identity(&self) -> Result<(), ImageError> {
        if self.digest.len() != 64
            || !self
                .digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ImageError::Unreadable(
                "invalid image asset identity".into(),
            ));
        }
        Ok(())
    }

    fn open(&self) -> Result<cap_std::fs::File, ImageError> {
        self.validate_identity()?;
        let root = crate::pinned_dir::open_canonical(&self.workspace).map_err(image_io_error)?;
        let directory = root
            .open_dir_nofollow(".deep-code")
            .and_then(|state| state.open_dir_nofollow("images"))
            .map_err(image_io_error)?;
        let file = open_stored_file(&directory, &self.file_name()).map_err(image_io_error)?;
        let metadata = file.metadata().map_err(image_io_error)?;
        if !metadata.is_file() {
            return Err(ImageError::NotAFile);
        }
        if metadata.len() != self.bytes {
            return Err(ImageError::Unreadable("image asset size changed".into()));
        }
        Ok(file)
    }

    fn read_chunks(&self, consume: impl FnMut(&[u8]) -> io::Result<()>) -> Result<(), ImageError> {
        let digest = read_image_chunks(self.open()?, self.bytes, self.format, consume)
            .map_err(image_io_error)?;
        if digest != self.digest {
            return Err(ImageError::Unreadable("image asset content changed".into()));
        }
        Ok(())
    }

    /// Check the stored size, format and content digest using bounded memory.
    pub fn verify(&self) -> Result<(), ImageError> {
        self.read_chunks(|_| Ok(()))
    }

    /// Verify while the caller still owns the work.
    pub fn verify_cancellable(&self, cancel: &CancellationToken) -> Result<(), ImageError> {
        check_cancelled(cancel).map_err(image_io_error)?;
        self.read_chunks(|_| check_cancelled(cancel))
    }

    /// Copy the verified bytes to a destination that can be discarded on error.
    pub(crate) fn copy_verified_to(&self, mut destination: impl Write) -> Result<(), ImageError> {
        self.read_chunks(|chunk| destination.write_all(chunk))
    }

    /// Resolve only inside the recorded canonical store. Every component is
    /// opened without following links, then bytes, format and digest are checked.
    pub fn read(&self) -> Result<Vec<u8>, ImageError> {
        self.read_cancellable(&CancellationToken::new())
    }

    fn read_cancellable(&self, cancel: &CancellationToken) -> Result<Vec<u8>, ImageError> {
        let mut bytes = Vec::new();
        self.read_chunks(|chunk| {
            check_cancelled(cancel)?;
            if bytes.is_empty() {
                let size = usize::try_from(self.bytes).map_err(io::Error::other)?;
                bytes.try_reserve_exact(size).map_err(io::Error::other)?;
            }
            bytes.extend_from_slice(chunk);
            Ok(())
        })?;
        Ok(bytes)
    }

    pub fn data_url(&self) -> Result<String, ImageError> {
        self.read()
            .and_then(|bytes| encode_data_url(self.format, &bytes))
    }
}

impl fmt::Display for ImageAsset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{IMAGES_DIR}/{} ({} bytes)",
            self.file_name(),
            self.bytes
        )
    }
}

/// Request-only image representation. Prepared images encode in chunks.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ImageRef {
    Asset(ImageAsset),
    Url {
        url: String,
        detail: Option<ImageDetail>,
    },
    Prepared {
        source: PreparedImage,
        detail: Option<ImageDetail>,
    },
}

impl ImageRef {
    #[must_use]
    pub fn asset(&self) -> Option<&ImageAsset> {
        match self {
            Self::Asset(asset) => Some(asset),
            Self::Prepared { source, .. } => Some(&source.asset),
            Self::Url { .. } => None,
        }
    }
}

impl From<ImageAsset> for ImageRef {
    fn from(asset: ImageAsset) -> Self {
        Self::Asset(asset)
    }
}

impl fmt::Display for ImageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Asset(asset) => fmt::Display::fmt(asset, f),
            Self::Prepared { source, .. } => fmt::Display::fmt(&source.asset, f),
            Self::Url { url, .. } if url.starts_with("data:") => f.write_str("(inline image)"),
            Self::Url { url, .. } => f.write_str(url),
        }
    }
}

/// Draft metadata. Drafts retain a user-selected path until admission takes a
/// byte snapshot; they are not themselves persisted observations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub path: PathBuf,
    pub format: ImageFormat,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    Missing,
    NotAFile,
    Unreadable(String),
    UnsupportedFormat,
}

impl ImageError {
    #[must_use]
    pub fn note(&self, path: &Path) -> String {
        self.note_about(&path.display().to_string())
    }

    fn note_about(&self, image: &str) -> String {
        match self {
            Self::Missing => format!("[图片不可用 / image unavailable: {image}]"),
            Self::NotAFile => format!("[图片不可用 / image unavailable, not a file: {image}]"),
            Self::Unreadable(why) => format!("[图片不可读 / image unreadable: {image}: {why}]"),
            Self::UnsupportedFormat => format!(
                "[图片格式不支持 / unsupported image format, expected PNG/JPEG/GIF/WebP: {image}]"
            ),
        }
    }
}

fn image_io_error(error: io::Error) -> ImageError {
    if error.kind() == io::ErrorKind::NotFound {
        ImageError::Missing
    } else {
        ImageError::Unreadable(error.to_string())
    }
}

fn size_label(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB {
        format!("{} MiB", bytes.div_ceil(MIB))
    } else {
        format!("{bytes} B")
    }
}

fn not_accepted_note(image: &str) -> String {
    format!("[图片未发送 / image not sent, the current model does not accept images: {image}]")
}

fn over_total_note(image: &str, limit: u64) -> String {
    format!(
        "[图片已省略 / image omitted, request image total over {}: {image}]",
        size_label(limit)
    )
}

fn over_request_note(image: &str, limit: u64) -> String {
    format!(
        "[图片已省略 / image omitted, request body over {}: {image}]",
        size_label(limit)
    )
}

/// Inspect an explicitly user-selected path without reading the complete file.
/// The header is identified on the same opened file as the size metadata.
pub fn inspect(path: &Path) -> Result<Attachment, ImageError> {
    let mut file = open_user_file(path)?;
    inspect_open_file(&mut file, path)
}

fn inspect_open_file(file: &mut std::fs::File, path: &Path) -> Result<Attachment, ImageError> {
    let metadata = file.metadata().map_err(image_io_error)?;
    if !metadata.is_file() {
        return Err(ImageError::NotAFile);
    }
    let mut header = [0u8; 16];
    let count = file.read(&mut header).map_err(image_io_error)?;
    let format = ImageFormat::sniff(&header[..count]).ok_or(ImageError::UnsupportedFormat)?;
    Ok(Attachment {
        path: path.to_path_buf(),
        format,
        bytes: metadata.len(),
    })
}

fn open_user_file(path: &Path) -> Result<std::fs::File, ImageError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

        // Allow opening a directory handle so the same-handle metadata check
        // can report NotAFile instead of Windows returning access denied first.
        options.custom_flags(FILE_FLAG_BACKUP_SEMANTICS);
    }
    options.open(path).map_err(image_io_error)
}

fn read_image_bytes(file: impl Read, size: u64) -> Result<Vec<u8>, ImageError> {
    let capacity = usize::try_from(size.saturating_add(1))
        .map_err(|error| ImageError::Unreadable(error.to_string()))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|error| ImageError::Unreadable(error.to_string()))?;
    file.take(size.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(image_io_error)?;
    if bytes.len() as u64 != size {
        return Err(ImageError::Unreadable(
            "image size changed while reading".into(),
        ));
    }
    Ok(bytes)
}

/// Read an explicitly selected image, checking the type on the same file handle.
pub fn read_image_file(path: &Path) -> Result<Vec<u8>, ImageError> {
    let mut file = open_user_file(path)?;
    let attachment = inspect_open_file(&mut file, path)?;
    file.rewind().map_err(image_io_error)?;
    read_image_bytes(file, attachment.bytes)
}

/// Encode a user-selected file. Persisted request preparation uses verified
/// ImageAsset reads instead of reopening arbitrary paths.
pub fn data_url(path: &Path) -> Result<String, ImageError> {
    let bytes = read_image_file(path)?;
    let format = ImageFormat::sniff(&bytes).ok_or(ImageError::UnsupportedFormat)?;
    encode_data_url(format, &bytes)
}

fn encode_data_url(format: ImageFormat, bytes: &[u8]) -> Result<String, ImageError> {
    let mut url = format!("data:{};base64,", format.media_type());
    let encoded_bytes = base64::encoded_len(bytes.len(), true).ok_or_else(|| {
        ImageError::Unreadable("image encoding size could not be represented".into())
    })?;
    url.try_reserve_exact(encoded_bytes)
        .map_err(|error| ImageError::Unreadable(error.to_string()))?;
    base64::engine::general_purpose::STANDARD.encode_string(bytes, &mut url);
    Ok(url)
}

/// A new submission streams from its asset. A historical image keeps the
/// bytes verified within the replay budget for this request and its retries.
#[derive(Clone)]
pub struct PreparedImage {
    asset: ImageAsset,
    replay: Option<Arc<Vec<u8>>>,
}

impl PreparedImage {
    fn read_chunks(&self, consume: impl FnMut(&[u8]) -> io::Result<()>) -> Result<(), ImageError> {
        match &self.replay {
            Some(bytes) => bytes
                .chunks(IMAGE_CHUNK_BYTES)
                .try_for_each(consume)
                .map_err(image_io_error),
            None => self.asset.read_chunks(consume),
        }
    }
}

// The verified digest identifies the wire bytes for either backing. Cloning
// or fingerprinting a request must not copy, log, or hash the pixels again.
impl fmt::Debug for PreparedImage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.asset, f)
    }
}
impl PartialEq for PreparedImage {
    fn eq(&self, other: &Self) -> bool {
        self.asset == other.asset
    }
}
impl Eq for PreparedImage {}
impl Hash for PreparedImage {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.asset.hash(state);
    }
}

impl Serialize for PreparedImage {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let url = DataUrlChunks {
            source: self,
            image_error: Cell::new(None),
        };
        let serialized = serializer.collect_str(&url);
        match url.image_error.into_inner() {
            Some(error) => Err(serde::ser::Error::custom(
                error.note_about(&self.asset.to_string()),
            )),
            None => serialized,
        }
    }
}

struct DataUrlChunks<'a> {
    source: &'a PreparedImage,
    image_error: Cell<Option<ImageError>>,
}

impl fmt::Display for DataUrlChunks<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            out,
            "data:{};base64,",
            self.source.asset.format.media_type()
        )?;
        let mut encoded = [0; IMAGE_CHUNK_BYTES / 3 * 4];
        let mut output_failed = false;
        let result = self.source.read_chunks(|chunk| {
            let len = base64::engine::general_purpose::STANDARD
                .encode_slice(chunk, &mut encoded)
                .expect("room for one encoded image chunk");
            let text = std::str::from_utf8(&encoded[..len]).expect("base64 is ASCII");
            out.write_str(text).map_err(|error| {
                output_failed = true;
                io::Error::other(error)
            })
        });
        if output_failed {
            return Err(fmt::Error);
        }
        // JSON's collect_str expects fmt::Error only for an output failure.
        // Propagate input errors from Serialize after the formatter returns.
        self.image_error.set(result.err());
        Ok(())
    }
}

fn read_image_chunks(
    mut source: impl Read,
    bytes: u64,
    format: ImageFormat,
    mut consume: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<String> {
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0; IMAGE_CHUNK_BYTES];
    let mut remaining = bytes;
    while remaining > 0 {
        let size = remaining.min(IMAGE_CHUNK_BYTES as u64) as usize;
        let chunk = &mut buffer[..size];
        source.read_exact(chunk)?;
        if remaining == bytes && ImageFormat::sniff(chunk) != Some(format) {
            return Err(io::Error::other("image format changed while reading"));
        }
        hash.update(chunk);
        consume(chunk)?;
        remaining -= size as u64;
    }
    if bytes == 0 || source.read(&mut buffer[..1])? != 0 {
        return Err(io::Error::other("image size changed while reading"));
    }
    Ok(hex(hash.finish().as_ref()))
}

fn check_cancelled(cancel: &CancellationToken) -> io::Result<()> {
    if cancel.is_cancelled() {
        Err(io::Error::other("image preparation cancelled"))
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub enum StoreError {
    UnsupportedFormat,
    Io(io::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormat => {
                f.write_str("not a supported image format (PNG/JPEG/GIF/WebP)")
            }
            Self::Io(error) => fmt::Display::fmt(error, f),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Admit bytes already obtained through the caller's authorization. The root
/// must be the caller's approved canonical workspace. Publication is atomic;
/// successful return means the exact object is present and verifies.
pub fn snapshot_bytes(workspace: &Path, bytes: &[u8]) -> Result<ImageAsset, StoreError> {
    snapshot_bytes_cancellable(workspace, bytes, &CancellationToken::new())
}

pub fn snapshot_bytes_cancellable(
    workspace: &Path,
    bytes: &[u8],
    cancel: &CancellationToken,
) -> Result<ImageAsset, StoreError> {
    check_cancelled(cancel)?;
    let format = ImageFormat::sniff(bytes).ok_or(StoreError::UnsupportedFormat)?;
    snapshot_reader(workspace, bytes, format, bytes.len() as u64, cancel)
}

fn snapshot_reader(
    workspace: &Path,
    source: impl Read,
    format: ImageFormat,
    bytes: u64,
    cancel: &CancellationToken,
) -> Result<ImageAsset, StoreError> {
    let root = crate::pinned_dir::open_canonical(workspace)?;
    let state = crate::pinned_dir::owned_child(&root, ".deep-code")?;
    let images = crate::pinned_dir::owned_child(&state, "images")?;
    let _ = state
        .open_with(
            ".gitignore",
            cap_std::fs::OpenOptions::new().write(true).create_new(true),
        )
        .and_then(|mut file| file.write_all(crate::session_store::SELF_IGNORE_BODY.as_bytes()));
    let temporary = format!(".pending-{}.tmp", uuid::Uuid::new_v4());
    let published = (|| -> io::Result<ImageAsset> {
        let mut options = cap_std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = images.open_with(&temporary, &options)?;
        let digest = read_image_chunks(source, bytes, format, |chunk| {
            check_cancelled(cancel)?;
            file.write_all(chunk)
        })?;
        let asset = ImageAsset {
            workspace: workspace.to_path_buf(),
            digest,
            format,
            bytes,
        };
        check_cancelled(cancel)?;
        file.sync_all()?;
        check_cancelled(cancel)?;
        match images.hard_link(&temporary, &images, asset.file_name()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        sync_directory(&images)?;
        sync_directory(&state)?;
        sync_directory(&root)?;
        Ok(asset)
    })();
    let _ = images.remove_file(&temporary);
    let asset = published?;
    asset
        .verify_cancellable(cancel)
        .map_err(|error| io::Error::other(error.note_about(&asset.to_string())))?;
    Ok(asset)
}

/// Snapshot an explicit user attachment. Tool-selected paths must not use this
/// entry point: their caller supplies authorized bytes to snapshot_bytes.
pub fn snapshot_path(workspace: &Path, path: &Path) -> Result<ImageAsset, String> {
    snapshot_path_cancellable(workspace, path, &CancellationToken::new())
}

pub fn snapshot_path_cancellable(
    workspace: &Path,
    path: &Path,
    cancel: &CancellationToken,
) -> Result<ImageAsset, String> {
    check_cancelled(cancel).map_err(|error| image_io_error(error).note(path))?;
    let mut file = open_user_file(path).map_err(|error| error.note(path))?;
    let attachment = inspect_open_file(&mut file, path).map_err(|error| error.note(path))?;
    file.rewind()
        .map_err(|error| image_io_error(error).note(path))?;
    snapshot_reader(workspace, file, attachment.format, attachment.bytes, cancel).map_err(|error| {
        format!(
            "[图片无法保存 / image snapshot failed: {}: {error}]",
            path.display()
        )
    })
}

/// Store clipboard pixels before attaching them to the draft. The returned
/// path is only a draft handle; submission admits its own verified snapshot.
pub fn store(workspace: &Path, data: &[u8]) -> Result<PathBuf, StoreError> {
    let canonical = crate::paths::canonicalize(workspace)?;
    snapshot_bytes(&canonical, data).map(|asset| workspace.join(IMAGES_DIR).join(asset.file_name()))
}

pub fn store_in_pinned_workspace(workspace: &Path, data: &[u8]) -> Result<PathBuf, StoreError> {
    snapshot_bytes(workspace, data).map(|asset| asset.path())
}

fn open_stored_file(directory: &cap_std::fs::Dir, name: &str) -> io::Result<cap_std::fs::File> {
    use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    directory.open_with(name, &options)
}

fn sync_directory(directory: &cap_std::fs::Dir) -> io::Result<()> {
    #[cfg(unix)]
    {
        // Linux directory capabilities use O_PATH, which cannot be fsynced.
        // Reopen the pinned directory for reading rather than cloning its flags.
        directory.open(".")?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        Ok(())
    }
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

/// One image's inclusion in a particular prepared request. Inclusion is not a
/// claim that an HTTP request succeeded or that the model understood the image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageDelivery {
    /// Persisted observation identity, when the request originated in a session.
    /// Protocol-only request preparation has no domain source to bind.
    pub observation_id: Option<String>,
    pub message_index: usize,
    pub block_index: usize,
    pub call_id: Option<String>,
    pub image: String,
    pub included: bool,
    pub reason: Option<String>,
}

pub(crate) struct PreparedImageRequest {
    pub request: ChatRequest,
    pub images: Vec<ImageDelivery>,
}

/// Prepare the actual request, including tools and model options, so image
/// budgets use the same serialized body that the HTTP client will send.
#[cfg(test)]
pub(crate) fn prepare_request_images(
    request: ChatRequest,
    detail: Option<ImageDetail>,
    accepted: bool,
    submissions: &[usize],
) -> Result<PreparedImageRequest, String> {
    prepare_request_images_cancellable(
        request,
        detail,
        accepted,
        submissions,
        &CancellationToken::new(),
    )
}

pub(crate) fn prepare_request_images_cancellable(
    request: ChatRequest,
    detail: Option<ImageDetail>,
    accepted: bool,
    submissions: &[usize],
    cancel: &CancellationToken,
) -> Result<PreparedImageRequest, String> {
    prepare_request_images_with_limits(
        request,
        detail,
        accepted,
        submissions,
        MAX_TOTAL_BYTES,
        MAX_REQUEST_BYTES,
        cancel,
    )
}

fn prepare_request_images_with_limits(
    mut request: ChatRequest,
    detail: Option<ImageDetail>,
    accepted: bool,
    submissions: &[usize],
    total_limit: u64,
    request_limit: u64,
    cancel: &CancellationToken,
) -> Result<PreparedImageRequest, String> {
    let (messages, mut images) = prepare_messages(
        request.messages,
        detail,
        accepted,
        submissions,
        total_limit,
        cancel,
    )?;
    request.messages = messages;
    check_cancelled(cancel).map_err(|error| error.to_string())?;
    fit_body(&mut request, request_limit, submissions, &mut images);
    images.sort_by_key(|image| (image.message_index, image.block_index));
    Ok(PreparedImageRequest { request, images })
}

fn prepare_messages(
    mut messages: Vec<Message>,
    detail: Option<ImageDetail>,
    accepted: bool,
    submissions: &[usize],
    total_limit: u64,
    cancel: &CancellationToken,
) -> Result<(Vec<Message>, Vec<ImageDelivery>), String> {
    let mut remaining = total_limit;
    let mut deliveries = Vec::new();
    for (message_index, message) in messages.iter_mut().enumerate().rev() {
        let submitted = submissions.contains(&message_index);
        for (block_index, block) in message.content.blocks.iter_mut().enumerate().rev() {
            let ContentBlock::Image { image } = block else {
                continue;
            };
            check_cancelled(cancel).map_err(|error| error.to_string())?;
            let description = image.to_string();
            let bytes = match image {
                ImageRef::Asset(asset) => asset.bytes,
                ImageRef::Prepared { source, .. } => source.asset.bytes,
                ImageRef::Url { url, .. } => inline_bytes(url).unwrap_or(0),
            };
            let outcome = if submitted {
                // Send the user's selected attachments together. A local read
                // failure stops the submission, leaving the draft recoverable.
                Ok(prepare_image(image, detail, submitted, cancel)?)
            } else if !accepted {
                Err(not_accepted_note(&description))
            } else if message.role == Role::Assistant {
                Err(format!(
                    "[图片未发送 / image not sent, assistant messages do not support images: {description}]"
                ))
            } else if bytes > remaining {
                Err(over_total_note(&description, total_limit))
            } else {
                prepare_image(image, detail, submitted, cancel)
            };
            let (included, reason) = match outcome {
                Ok(()) => {
                    remaining = remaining.saturating_sub(bytes);
                    (true, None)
                }
                Err(reason) => {
                    *block = ContentBlock::Text {
                        text: reason.clone(),
                    };
                    (false, Some(reason))
                }
            };
            deliveries.push(ImageDelivery {
                observation_id: None,
                message_index,
                block_index,
                call_id: message.tool_call_id.clone(),
                image: description,
                included,
                reason,
            });
        }
    }
    Ok((messages, deliveries))
}

fn prepare_image(
    image: &mut ImageRef,
    detail: Option<ImageDetail>,
    submitted: bool,
    cancel: &CancellationToken,
) -> Result<(), String> {
    if let ImageRef::Asset(asset) = image {
        let replay = if submitted {
            asset.verify_cancellable(cancel).map(|()| None)
        } else {
            asset
                .read_cancellable(cancel)
                .map(|bytes| Some(Arc::new(bytes)))
        }
        .map_err(|error| error.note_about(&asset.to_string()))?;
        *image = ImageRef::Prepared {
            source: PreparedImage {
                asset: asset.clone(),
                replay,
            },
            detail,
        };
    }
    Ok(())
}

fn inline_bytes(url: &str) -> Option<u64> {
    let (_, encoded) = url.strip_prefix("data:")?.split_once(";base64,")?;
    let padding = encoded
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count() as u64;
    Some(((encoded.len() as u64).saturating_mul(3) / 4).saturating_sub(padding.min(2)))
}

/// Make room by trimming replayed images. The newly submitted images reach the
/// provider together even when they exceed the historical replay budget.
fn fit_body(
    request: &mut ChatRequest,
    limit: u64,
    submissions: &[usize],
    deliveries: &mut [ImageDelivery],
) {
    if !deliveries
        .iter()
        .any(|image| image.included && !submissions.contains(&image.message_index))
    {
        return;
    }
    loop {
        match serialized_size(request) {
            Some(size) if size <= limit => return,
            None => return,
            Some(_) => {}
        }
        let mut removed = None;
        'messages: for (message_index, message) in request.messages.iter_mut().enumerate() {
            if submissions.contains(&message_index) {
                continue;
            }
            for (block_index, block) in message.content.blocks.iter_mut().enumerate() {
                let ContentBlock::Image { image } = block else {
                    continue;
                };
                let delivery = deliveries.iter_mut().find(|delivery| {
                    delivery.message_index == message_index && delivery.block_index == block_index
                });
                let description = delivery
                    .as_ref()
                    .map_or_else(|| image.to_string(), |delivery| delivery.image.clone());
                let reason = over_request_note(&description, limit);
                if let Some(delivery) = delivery {
                    delivery.included = false;
                    delivery.reason = Some(reason.clone());
                }
                *block = ContentBlock::Text { text: reason };
                removed = Some(());
                break 'messages;
            }
        }
        if removed.is_none() {
            return;
        }
    }
}

fn serialized_size(request: &ChatRequest) -> Option<u64> {
    // The data URL alphabet needs no JSON escaping. Count those bytes from
    // the asset sizes, then serialize the surrounding JSON with empty URLs.
    let mut request = request.clone();
    let mut counter = ByteCounter::default();
    for message in &mut request.messages {
        for block in &mut message.content.blocks {
            let ContentBlock::Image { image } = block else {
                continue;
            };
            if let ImageRef::Prepared { source, detail } = image {
                let asset = &source.asset;
                let prefix = "data:;base64,".len() + asset.format.media_type().len();
                counter.bytes = counter.bytes.saturating_add(prefix as u64);
                counter.bytes = counter
                    .bytes
                    .saturating_add(asset.bytes.div_ceil(3).saturating_mul(4));
                *image = ImageRef::Url {
                    url: String::new(),
                    detail: *detail,
                };
            }
        }
    }
    serde_json::to_writer(&mut counter, &request)
        .ok()
        .map(|()| counter.bytes)
}

#[derive(Default)]
struct ByteCounter {
    bytes: u64,
}

impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
