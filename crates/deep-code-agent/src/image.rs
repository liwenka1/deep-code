//! Image admission, content-addressed observations, and request projection.
//!
//! User-selected paths are input intentions. Persisted observations reference
//! exact stored bytes; they never reopen those source paths. Tool readers must
//! obtain bytes through their own filesystem authorization before admission.
//! Request preparation resolves references and reports every omitted image.

use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use base64::Engine as _;
use cap_fs_ext::DirExt;
use serde::{Deserialize, Serialize};

use crate::content::ContentBlock;
use crate::message::Message;
use crate::message::Role;
use crate::model::ChatRequest;

const IMAGES_DIR: &str = ".deep-code/images";

pub const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_REQUEST_BYTES: u64 = 48 * 1024 * 1024;
pub const MAX_TOKENS_PER_IMAGE: u32 = 1024;
/// Product admission limit, independent of the provider's larger ceiling.
pub const MAX_IMAGES_PER_MESSAGE: usize = 4;

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

    /// Resolve only inside the recorded canonical store. Every component is
    /// opened without following links, then bytes, format and digest are checked.
    pub fn read(&self) -> Result<Vec<u8>, ImageError> {
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
        if self.bytes > MAX_IMAGE_BYTES {
            return Err(ImageError::TooLarge { bytes: self.bytes });
        }
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
        let mut bytes = Vec::new();
        file.take(self.bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(image_io_error)?;
        if bytes.len() as u64 != self.bytes
            || digest_of(&bytes) != self.digest
            || ImageFormat::sniff(&bytes) != Some(self.format)
        {
            return Err(ImageError::Unreadable("image asset content changed".into()));
        }
        Ok(bytes)
    }

    pub fn data_url(&self) -> Result<String, ImageError> {
        self.read()
            .map(|bytes| encode_data_url(self.format, &bytes))
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

/// Request-only image representation. Persistent content holds ImageAsset,
/// while preparation replaces Asset with the provider's URL representation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ImageRef {
    Asset(ImageAsset),
    Url {
        url: String,
        detail: Option<ImageDetail>,
    },
}

impl ImageRef {
    #[must_use]
    pub fn asset(&self) -> Option<&ImageAsset> {
        match self {
            Self::Asset(asset) => Some(asset),
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
    TooLarge { bytes: u64 },
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
            Self::TooLarge { bytes } => format!(
                "[图片过大 / image too large ({} > {}): {image}]",
                size_label(*bytes),
                size_label(MAX_IMAGE_BYTES),
            ),
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

fn over_count_note(image: &str) -> String {
    format!("[图片已省略 / image omitted, at most {MAX_IMAGES_PER_MESSAGE} per message: {image}]")
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
    let metadata = file.metadata().map_err(image_io_error)?;
    check_source_metadata(&metadata)?;
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
    options.open(path).map_err(image_io_error)
}

fn check_source_metadata(metadata: &std::fs::Metadata) -> Result<(), ImageError> {
    if !metadata.is_file() {
        return Err(ImageError::NotAFile);
    }
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(ImageError::TooLarge {
            bytes: metadata.len(),
        });
    }
    Ok(())
}

fn read_user_image(path: &Path) -> Result<Vec<u8>, ImageError> {
    let file = open_user_file(path)?;
    check_source_metadata(&file.metadata().map_err(image_io_error)?)?;
    let mut bytes = Vec::new();
    file.take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(image_io_error)?;
    validate_bytes(&bytes)?;
    Ok(bytes)
}

fn validate_bytes(bytes: &[u8]) -> Result<ImageFormat, ImageError> {
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(ImageError::TooLarge {
            bytes: bytes.len() as u64,
        });
    }
    ImageFormat::sniff(bytes).ok_or(ImageError::UnsupportedFormat)
}

/// Encode a user-selected file. Persisted request preparation uses verified
/// ImageAsset reads instead of reopening arbitrary paths.
pub fn data_url(path: &Path) -> Result<String, ImageError> {
    let bytes = read_user_image(path)?;
    let format = validate_bytes(&bytes)?;
    Ok(encode_data_url(format, &bytes))
}

fn encode_data_url(format: ImageFormat, bytes: &[u8]) -> String {
    format!(
        "data:{};base64,{}",
        format.media_type(),
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

#[derive(Debug)]
pub enum StoreError {
    UnsupportedFormat,
    TooLarge { bytes: u64 },
    Io(io::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedFormat => {
                f.write_str("not a supported image format (PNG/JPEG/GIF/WebP)")
            }
            Self::TooLarge { bytes } => write!(
                f,
                "image too large ({} > {})",
                size_label(*bytes),
                size_label(MAX_IMAGE_BYTES)
            ),
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
    let format = validate_bytes(bytes).map_err(|error| match error {
        ImageError::TooLarge { bytes } => StoreError::TooLarge { bytes },
        _ => StoreError::UnsupportedFormat,
    })?;
    let asset = ImageAsset {
        workspace: workspace.to_path_buf(),
        digest: digest_of(bytes),
        format,
        bytes: bytes.len() as u64,
    };
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
    let published = (|| -> io::Result<()> {
        let mut options = cap_std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = images.open_with(&temporary, &options)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        match images.hard_link(&temporary, &images, asset.file_name()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        sync_directory(&images)?;
        sync_directory(&state)?;
        sync_directory(&root)?;
        Ok(())
    })();
    let _ = images.remove_file(&temporary);
    published?;
    let stored = asset
        .read()
        .map_err(|error| io::Error::other(error.note_about(&asset.to_string())))?;
    if stored != bytes {
        return Err(io::Error::other("image asset does not match admitted bytes").into());
    }
    Ok(asset)
}

/// Snapshot explicit user attachments. Tool-selected paths must not use this
/// entry point: their caller supplies authorized bytes to snapshot_bytes.
pub fn snapshot_paths(workspace: &Path, paths: &[PathBuf]) -> (Vec<ImageAsset>, Vec<String>) {
    let mut assets = Vec::new();
    let mut notes = Vec::new();
    let mut remaining = MAX_TOTAL_BYTES;
    for path in paths {
        if assets.len() >= MAX_IMAGES_PER_MESSAGE {
            notes.push(over_count_note(&path.display().to_string()));
            continue;
        }
        match read_user_image(path) {
            Ok(bytes) if bytes.len() as u64 > remaining => notes.push(over_total_note(
                &path.display().to_string(),
                MAX_TOTAL_BYTES,
            )),
            Ok(bytes) => match snapshot_bytes(workspace, &bytes) {
                Ok(asset) => {
                    remaining -= asset.bytes;
                    assets.push(asset);
                }
                Err(error) => notes.push(format!(
                    "[图片无法保存 / image snapshot failed: {}: {error}]",
                    path.display()
                )),
            },
            Err(error) => notes.push(error.note(path)),
        }
    }
    (assets, notes)
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

fn digest_of(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&ring::digest::SHA256, bytes).as_ref())
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
#[must_use]
pub(crate) fn prepare_request_images(
    request: ChatRequest,
    detail: Option<ImageDetail>,
    accepted: bool,
) -> PreparedImageRequest {
    prepare_request_images_with_limits(
        request,
        detail,
        accepted,
        MAX_TOTAL_BYTES,
        MAX_REQUEST_BYTES,
    )
}

fn prepare_request_images_with_limits(
    mut request: ChatRequest,
    detail: Option<ImageDetail>,
    accepted: bool,
    total_limit: u64,
    request_limit: u64,
) -> PreparedImageRequest {
    let (messages, mut images) = prepare_messages(request.messages, detail, accepted, total_limit);
    request.messages = messages;
    fit_body(&mut request, request_limit, &mut images);
    images.sort_by_key(|image| (image.message_index, image.block_index));
    PreparedImageRequest { request, images }
}

fn prepare_messages(
    mut messages: Vec<Message>,
    detail: Option<ImageDetail>,
    accepted: bool,
    total_limit: u64,
) -> (Vec<Message>, Vec<ImageDelivery>) {
    let mut remaining = total_limit;
    let mut deliveries = Vec::new();
    for (message_index, message) in messages.iter_mut().enumerate().rev() {
        let mut retained = 0usize;
        for (block_index, block) in message.content.blocks.iter_mut().enumerate().rev() {
            let ContentBlock::Image { image } = block else {
                continue;
            };
            let description = image.to_string();
            let outcome = if !accepted {
                Err(not_accepted_note(&description))
            } else if message.role == Role::Assistant {
                Err(format!(
                    "[图片未发送 / image not sent, assistant messages do not support images: {description}]"
                ))
            } else if retained >= MAX_IMAGES_PER_MESSAGE {
                Err(over_count_note(&description))
            } else {
                resolve_image(image, detail, remaining, total_limit)
            };
            let (included, reason) = match outcome {
                Ok((resolved, bytes)) => {
                    remaining -= bytes;
                    retained += 1;
                    *image = resolved;
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
    (messages, deliveries)
}

fn resolve_image(
    image: &ImageRef,
    detail: Option<ImageDetail>,
    remaining: u64,
    total_limit: u64,
) -> Result<(ImageRef, u64), String> {
    let bytes = match image {
        ImageRef::Asset(asset) => asset.bytes,
        ImageRef::Url { url, .. } => inline_bytes(url).unwrap_or(0),
    };
    if bytes > MAX_IMAGE_BYTES {
        return Err(ImageError::TooLarge { bytes }.note_about(&image.to_string()));
    }
    if bytes > remaining {
        return Err(over_total_note(&image.to_string(), total_limit));
    }
    match image {
        ImageRef::Asset(asset) => asset
            .data_url()
            .map(|url| (ImageRef::Url { url, detail }, bytes))
            .map_err(|error| error.note_about(&asset.to_string())),
        ImageRef::Url { .. } => Ok((image.clone(), bytes)),
    }
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

fn fit_body(request: &mut ChatRequest, limit: u64, deliveries: &mut [ImageDelivery]) {
    loop {
        match serialized_size(request) {
            Some(size) if size <= limit => return,
            None => return,
            Some(_) => {}
        }
        let mut removed = None;
        'messages: for (message_index, message) in request.messages.iter_mut().enumerate() {
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
    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, request)
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
