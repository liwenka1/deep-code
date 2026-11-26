//! Portable session exports contain only the observations referenced by a
//! host-authenticated transcript, never the surrounding workspace or checkpoints.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{JsonSessionStore, SessionId, SessionRecord, SessionStore, SessionStoreError};
use crate::content::{Content, ContentBlock};
use crate::image::ImageAsset;
use crate::session_entry::{EntryKind, SessionEntry};

const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SOURCE_DEPTH: usize = 32;

impl JsonSessionStore {
    /// Export every regular session in this store. Listing filenames here does
    /// not apply the session picker's recent-record window.
    pub fn export_all_bundles(&self, destination: &Path) -> Result<usize, SessionStoreError> {
        let workspace =
            crate::pinned_dir::open_canonical(self.workspace_path()).map_err(io_error)?;
        let sessions = workspace
            .open_dir_nofollow(".deep-code")
            .and_then(|state| state.open_dir_nofollow("sessions"))
            .map_err(io_error)?;
        let mut count = 0;
        for entry in sessions.entries().map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            if !entry.file_type().map_err(io_error)?.is_file() {
                continue;
            }
            let name = PathBuf::from(entry.file_name());
            if name.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            let id = name
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| export_error("invalid session filename"))?;
            self.export_bundle(&SessionId::parse(id)?, destination)?;
            count += 1;
        }
        Ok(count)
    }

    /// Export a verified transcript and its referenced images/output into an
    /// independent workspace. The returned record can be loaded from that
    /// workspace after the original directory has been removed.
    ///
    /// Exported conversations carry no writable-root grants. Sources are read
    /// only from this store's workspace, without following links; a missing or
    /// invalid source fails the export before publishing its session record.
    pub fn export_bundle(
        &self,
        id: &SessionId,
        destination: &Path,
    ) -> Result<SessionRecord, SessionStoreError> {
        let mut record = self.load(id)?;
        if record.workspace != self.workspace_path() {
            return Err(export_error("session workspace differs from its store"));
        }
        let source = crate::pinned_dir::open_canonical(self.workspace_path()).map_err(io_error)?;
        let (destination, directory) = export_destination(destination).map_err(io_error)?;
        if destination == record.workspace {
            return Err(export_error(
                "export destination must differ from the source workspace",
            ));
        }
        // Initialize the state directory's ignore rule before any observation
        // is written, including exports that later fail on another source.
        let target = JsonSessionStore::for_pinned_workspace(&destination)?;
        let mut exporter = Exporter {
            source,
            source_path: record.workspace.clone(),
            destination: directory,
            destination_path: destination.clone(),
            copied: HashMap::new(),
            copied_images: HashSet::new(),
            active: HashSet::new(),
            in_flight_bytes: 0,
        };
        self.visit_archives(&record.entries, &CancellationToken::new(), |mut archive| {
            exporter.entries(&mut archive.entries)?;
            archive.workspace.clone_from(&destination);
            target.save_archive(&mut archive)
        })?;
        exporter.entries(&mut record.entries)?;
        record.workspace = destination;
        record.extra_roots.clear();
        record.extra_roots_mac = None;
        record.checkpoints.clear();
        record.record_mac = None;
        target.save(&mut record)?;
        Ok(record)
    }
}

impl Exporter {
    fn entries(&mut self, entries: &mut [Arc<SessionEntry>]) -> Result<(), SessionStoreError> {
        for entry in entries {
            match &mut Arc::make_mut(entry).kind {
                EntryKind::User { content } => self.content(content, false)?,
                EntryKind::Assistant { exchanges, .. } => {
                    for exchange in exchanges {
                        if let Some(result) = &mut exchange.result {
                            let rebase_notices = result.details.as_ref().is_some_and(|details| {
                                ["stdout", "stderr"].iter().any(|name| {
                                    details
                                        .get(name)
                                        .and_then(|stream| stream.get("spill"))
                                        .and_then(|spill| spill.get("path"))
                                        .is_some_and(Value::is_string)
                                })
                            });
                            if let Some(details) = &mut result.details {
                                self.details(details)?;
                            }
                            self.content(&mut result.content, rebase_notices)?;
                        }
                    }
                }
                EntryKind::Compaction { image_sources, .. } => {
                    for source in image_sources {
                        self.image(&mut source.image)?;
                    }
                }
                EntryKind::System { .. } => {}
            }
        }
        Ok(())
    }
}

struct Exporter {
    source: Dir,
    source_path: PathBuf,
    destination: Dir,
    destination_path: PathBuf,
    copied: HashMap<PathBuf, CopiedSource>,
    copied_images: HashSet<ImageAsset>,
    active: HashSet<PathBuf>,
    in_flight_bytes: u64,
}

struct CopiedSource {
    original_bytes: usize,
    exported_bytes: usize,
    structured: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Details,
    Content,
    Stream,
}

impl Exporter {
    fn content(
        &mut self,
        content: &mut Content,
        rebase_notices: bool,
    ) -> Result<(), SessionStoreError> {
        for block in &mut content.blocks {
            match block {
                ContentBlock::Image { image } => self.image(image)?,
                ContentBlock::Text { text } if rebase_notices => self.rebase_stream_notices(text),
                ContentBlock::Text { .. } => {}
            }
        }
        Ok(())
    }

    fn image(&mut self, image: &mut ImageAsset) -> Result<(), SessionStoreError> {
        if image.workspace != self.source_path {
            return Err(export_error(
                "image workspace differs from the session store",
            ));
        }
        let mut exported = image.clone();
        exported.workspace.clone_from(&self.destination_path);
        if !self.copied_images.contains(image) {
            let source = image.path();
            image
                .validate_identity()
                .map_err(|error| export_error(error.note(&source)))?;
            let relative = source
                .strip_prefix(&self.source_path)
                .map_err(|_| export_error("image is outside the source workspace"))?;
            publish_source(&self.destination, relative, |file| {
                image
                    .copy_verified_to(file)
                    .map_err(|error| io::Error::other(error.note(&source)))
            })
            .map_err(io_error)?;
            exported
                .verify()
                .map_err(|error| export_error(error.note(&exported.path())))?;
            self.copied_images.insert(image.clone());
        }
        *image = exported;
        Ok(())
    }

    fn details(&mut self, details: &mut Value) -> Result<(), SessionStoreError> {
        if let Some(storage) = details.get_mut("details_storage")
            && let Some(path) = storage.get("path").and_then(Value::as_str)
        {
            let path = path.to_string();
            let complete = storage.get("complete").and_then(Value::as_bool) == Some(true);
            let expected = complete
                .then(|| storage.get("original_bytes").and_then(Value::as_u64))
                .flatten();
            let (relative, size) = self.spill(&path, SourceKind::Details, expected)?;
            storage["path"] = path_value(&relative);
            if complete {
                storage["original_bytes"] = size.into();
            }
        }
        if let Some(storage) = details.get_mut("content_storage")
            && let Some(path) = storage.get("path").and_then(Value::as_str)
        {
            let path = path.to_string();
            let expected = (storage.get("complete").and_then(Value::as_bool) == Some(true))
                .then(|| storage.get("original_bytes").and_then(Value::as_u64))
                .flatten();
            let (relative, _) = self.spill(&path, SourceKind::Content, expected)?;
            storage["path"] = path_value(&relative);
        }
        for name in ["stdout", "stderr"] {
            if let Some(spill) = details
                .get_mut(name)
                .and_then(|stream| stream.get_mut("spill"))
                && let Some(path) = spill.get("path").and_then(Value::as_str)
            {
                let path = path.to_string();
                let expected = spill.get("bytes").and_then(Value::as_u64);
                let (relative, _) = self.spill(&path, SourceKind::Stream, expected)?;
                spill["path"] = path_value(&relative);
            }
        }
        Ok(())
    }

    fn rebase_stream_notices(&self, text: &mut String) {
        // Only native, whole-line shell notices are presentation references.
        // A read_file JSON string can contain the same words as source code.
        let mut rendered = String::with_capacity(text.len());
        for line in text.split_inclusive('\n') {
            let native_notice = (line.starts_with("[stdout truncated — ")
                || line.starts_with("[stderr truncated — "))
                && line
                    .trim_end_matches('\n')
                    .ends_with("read that file for the parts not shown]");
            if native_notice {
                let mut line = line.to_string();
                for relative in self.copied.keys() {
                    let absolute = self.source_path.join(relative);
                    line = line.replace(
                        &format!("saved: '{}'", absolute.display()),
                        &format!("saved: '{}'", relative.display()),
                    );
                }
                rendered.push_str(&line);
            } else {
                rendered.push_str(line);
            }
        }
        *text = rendered;
    }

    fn spill(
        &mut self,
        raw: &str,
        kind: SourceKind,
        expected: Option<u64>,
    ) -> Result<(PathBuf, usize), SessionStoreError> {
        let relative = spill_relative(&self.source_path, Path::new(raw))?;
        let structured = kind == SourceKind::Details;
        let exact_size = kind != SourceKind::Stream;
        if let Some(copied) = self.copied.get(&relative) {
            validate_source_size(&relative, exact_size, expected, copied.original_bytes)?;
            if structured && !copied.structured {
                return Err(export_error(
                    "details source was already referenced as raw output",
                ));
            }
            return Ok((relative, copied.exported_bytes));
        }
        if self.active.len() >= MAX_SOURCE_DEPTH || !self.active.insert(relative.clone()) {
            return Err(export_error("cyclic or overly nested details source"));
        }
        let available = super::MAX_SESSION_BODY_BYTES
            .saturating_sub(self.in_flight_bytes)
            .min(MAX_SOURCE_BYTES);
        let mut bytes = read_source(&self.source, &relative, available).map_err(|error| {
            export_error(format!(
                "cannot export source {}: {error}",
                relative.display()
            ))
        })?;
        validate_source_size(&relative, exact_size, expected, bytes.len())?;
        let original_bytes = bytes.len();
        self.in_flight_bytes += original_bytes as u64;
        if structured {
            match serde_json::from_slice::<Value>(&bytes) {
                Ok(mut value) => {
                    self.details(&mut value)?;
                    bytes = serde_json::to_vec(&value)
                        .map_err(|error| export_error(error.to_string()))?;
                }
                Err(error) if expected.is_some() => {
                    return Err(export_error(format!(
                        "complete details source is invalid JSON: {error}"
                    )));
                }
                Err(_) => {}
            }
        }
        write_source(&self.destination, &relative, &bytes).map_err(io_error)?;
        self.in_flight_bytes -= original_bytes as u64;
        self.active.remove(&relative);
        self.copied.insert(
            relative.clone(),
            CopiedSource {
                original_bytes,
                exported_bytes: bytes.len(),
                structured,
            },
        );
        Ok((relative, bytes.len()))
    }
}

fn validate_source_size(
    path: &Path,
    exact: bool,
    expected: Option<u64>,
    actual: usize,
) -> Result<(), SessionStoreError> {
    // A stream snapshot records the prefix captured at that instant; its
    // underlying spill may keep growing until the runtime shuts down.
    if expected.is_some_and(|expected| {
        if exact {
            expected != actual as u64
        } else {
            expected > actual as u64
        }
    }) {
        return Err(export_error(format!(
            "source size changed: {}",
            path.display()
        )));
    }
    Ok(())
}

fn spill_relative(workspace: &Path, path: &Path) -> Result<PathBuf, SessionStoreError> {
    let relative = if path.is_absolute() {
        path.strip_prefix(workspace)
            .map_err(|_| export_error("source is outside its workspace"))?
    } else {
        path
    };
    let parts: Vec<_> = relative.components().collect();
    let [
        Component::Normal(state),
        Component::Normal(spill),
        Component::Normal(home),
        Component::Normal(name),
    ] = parts.as_slice()
    else {
        return Err(export_error("invalid transcript source path"));
    };
    let home = home
        .to_str()
        .ok_or_else(|| export_error("invalid source directory"))?;
    let name = name
        .to_str()
        .ok_or_else(|| export_error("invalid source filename"))?;
    let valid = if home == "tool-results" {
        name.strip_suffix(".txt")
            .is_some_and(|stem| uuid::Uuid::parse_str(stem).is_ok())
    } else {
        home.starts_with("run-")
            && home
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && name.starts_with("job_")
            && (name.ends_with(".stdout.log") || name.ends_with(".stderr.log"))
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    };
    if *state != ".deep-code" || *spill != "spill" || !valid {
        return Err(export_error(
            "source is not a recognized captured-output path",
        ));
    }
    Ok(relative.to_path_buf())
}

fn read_source(root: &Dir, relative: &Path, max_bytes: u64) -> io::Result<Vec<u8>> {
    let mut parent = root.try_clone()?;
    let names: Vec<_> = relative.components().collect();
    for component in &names[..names.len() - 1] {
        parent = parent.open_dir_nofollow(component.as_os_str())?;
    }
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = parent.open_with(names.last().unwrap().as_os_str(), &options)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("source is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(io::Error::other(
            "captured source exceeds the per-file or in-flight export limit",
        ));
    }
    Ok(bytes)
}

fn write_source(root: &Dir, relative: &Path, bytes: &[u8]) -> io::Result<()> {
    publish_source(root, relative, |file| file.write_all(bytes))?;
    if read_source(root, relative, MAX_SOURCE_BYTES)? != bytes {
        return Err(io::Error::other(
            "export destination source already has different contents",
        ));
    }
    Ok(())
}

fn publish_source(
    root: &Dir,
    relative: &Path,
    write: impl FnOnce(&mut cap_std::fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let mut parent = root.try_clone()?;
    let names: Vec<_> = relative.components().collect();
    for component in &names[..names.len() - 1] {
        parent = crate::pinned_dir::owned_child(
            &parent,
            component
                .as_os_str()
                .to_str()
                .ok_or_else(|| io::Error::other("invalid source directory"))?,
        )?;
    }
    let temporary = format!(".export-{}.tmp", uuid::Uuid::new_v4());
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = parent.open_with(&temporary, &options)?;
        write(&mut file)?;
        file.sync_all()?;
        drop(file);
        match parent.hard_link(&temporary, &parent, names.last().unwrap().as_os_str()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        #[cfg(unix)]
        parent.open(".")?.sync_all()?;
        Ok(())
    })();
    let _ = parent.remove_file(&temporary);
    result
}

fn export_destination(path: &Path) -> io::Result<(PathBuf, Dir)> {
    let leaf = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("invalid export destination"))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let parent_path = crate::paths::canonicalize(parent)?;
    let parent = crate::pinned_dir::open_canonical(&parent_path)?;
    let directory = crate::pinned_dir::owned_child(&parent, leaf)?;
    Ok((parent_path.join(leaf), directory))
}

fn path_value(path: &Path) -> Value {
    Value::String(path.to_string_lossy().into_owned())
}

fn io_error(error: io::Error) -> SessionStoreError {
    export_error(error.to_string())
}

fn export_error(message: impl Into<String>) -> SessionStoreError {
    SessionStoreError::Io {
        message: format!("transcript export failed: {}", message.into()),
    }
}

#[cfg(test)]
mod tests;
