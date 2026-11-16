//! Physical storage only: the authenticated logical record is reconstructed
//! unchanged before deserialization and integrity verification.

use cap_fs_ext::DirExt;
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};

use super::*;

const CHUNK_BYTES: usize = 1024 * 1024;
const CHUNKS_DIR: &str = "chunks";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    storage: Storage,
    bytes: u64,
    sha256: String,
    mac: String,
    chunks: Vec<String>,
}

#[derive(Serialize, Deserialize)]
enum Storage {
    #[serde(rename = "chunks_v1")]
    Chunks,
}

fn hash(bytes: &[u8]) -> String {
    digest(&SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn invalid(path: &Path, reason: impl std::fmt::Display) -> SessionStoreError {
    SessionStoreError::Serialization {
        message: format!("invalid chunked session {}: {reason}", path.display()),
    }
}

fn read_chunk(directory: &Dir, hash: &str, bytes: usize) -> std::io::Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = directory.open_with(format!("{hash}.bin"), &options)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("session chunk is not a regular file"));
    }
    let mut contents = Vec::new();
    file.take(bytes as u64 + 1).read_to_end(&mut contents)?;
    if contents.len() != bytes {
        return Err(std::io::Error::other("session chunk size mismatch"));
    }
    Ok(contents)
}

pub(super) fn write(directory: &Dir, body: &[u8]) -> Result<Vec<u8>, SessionStoreError> {
    let body_hash = hash(body);
    let chunks = crate::pinned_dir::owned_child(directory, CHUNKS_DIR).map_err(store_io)?;
    let mut hashes = Vec::new();
    for bytes in body.chunks(CHUNK_BYTES) {
        let hash = hash(bytes);
        match read_chunk(&chunks, &hash, bytes.len()) {
            Ok(existing) if existing == bytes => {}
            Ok(_) => {
                return Err(store_io(std::io::Error::other(
                    "session chunk digest mismatch",
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                write_atomic_in(&chunks, std::ffi::OsStr::new(&format!("{hash}.bin")), bytes)
                    .map_err(store_io)?;
            }
            Err(error) => return Err(store_io(error)),
        }
        hashes.push(hash);
    }
    let mac = crate::session_integrity::sign_storage(&body_hash, body.len() as u64, &hashes)
        .ok_or_else(|| SessionStoreError::KeyUnavailable {
            detail: "session storage signing key is unavailable".into(),
        })?;
    // Publish the manifest only after every immutable chunk is durable.
    #[cfg(unix)]
    directory
        .open(".")
        .and_then(|file| file.sync_all())
        .map_err(store_io)?;
    serde_json::to_vec_pretty(&Manifest {
        storage: Storage::Chunks,
        bytes: body.len() as u64,
        sha256: body_hash,
        mac,
        chunks: hashes,
    })
    .map_err(|error| invalid(Path::new("<prepared>"), error))
}

pub(super) fn read(
    directory: &Dir,
    value: serde_json::Value,
    path: &Path,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<Vec<u8>, SessionStoreError> {
    let manifest = authenticated_manifest(value, path)?;
    let length = manifest.bytes as usize;
    let chunks = directory.open_dir_nofollow(CHUNKS_DIR).map_err(store_io)?;
    let mut body = Vec::with_capacity(length);
    for hash in &manifest.chunks {
        if let Some(cancel) = cancel {
            check_scan_cancelled(cancel)?;
        }
        let bytes = CHUNK_BYTES.min(length - body.len());
        let part = read_chunk(&chunks, hash, bytes).map_err(store_io)?;
        if self::hash(&part) != *hash {
            return Err(invalid(path, "chunk digest mismatch"));
        }
        body.extend_from_slice(&part);
    }
    if hash(&body) != manifest.sha256 {
        return Err(invalid(path, "record digest mismatch"));
    }
    Ok(body)
}

fn authenticated_manifest(
    value: serde_json::Value,
    path: &Path,
) -> Result<Manifest, SessionStoreError> {
    let manifest: Manifest = serde_json::from_value(value).map_err(|error| invalid(path, error))?;
    if manifest.bytes == 0 || manifest.bytes > MAX_SESSION_BODY_BYTES {
        return Err(invalid(path, "reconstructed record exceeds its byte limit"));
    }
    let length = usize::try_from(manifest.bytes).map_err(|error| invalid(path, error))?;
    if manifest.chunks.len() != length.div_ceil(CHUNK_BYTES)
        || !valid_hash(&manifest.sha256)
        || !manifest.chunks.iter().all(|hash| valid_hash(hash))
    {
        return Err(invalid(path, "invalid chunk count or digest"));
    }
    crate::session_integrity::ensure_key().map_err(|reason| SessionStoreError::KeyUnavailable {
        detail: reason.to_string(),
    })?;
    if !crate::session_integrity::verify_storage(
        &manifest.sha256,
        manifest.bytes,
        &manifest.chunks,
        &manifest.mac,
    ) {
        return Err(SessionStoreError::AuthenticationFailed {
            path: path.display().to_string(),
        });
    }
    Ok(manifest)
}

/// Called with the store's exclusive lock. Uncertain indexes leave all chunks
/// untouched; only a complete authenticated mark pass permits deletion.
pub(super) fn collect(directory: &Dir) -> Result<(), SessionStoreError> {
    use std::collections::{BTreeSet, HashSet};
    let chunks = match directory.open_dir_nofollow(CHUNKS_DIR) {
        Ok(chunks) => chunks,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(store_io(error)),
    };
    let mut previous = None;
    loop {
        // Bound scratch memory by candidates, not by total valid history.
        // Large stores must not permanently stop reclaiming their old blocks.
        let mut page = BTreeSet::new();
        // Finish enumeration before deleting anything. Resume by hash rather
        // than a directory iterator whose entries change during collection.
        for entry in chunks.entries().map_err(store_io)? {
            let entry = entry.map_err(store_io)?;
            let name = PathBuf::from(entry.file_name());
            let Some(hash) = name.file_stem().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.extension().is_some_and(|extension| extension == "bin")
                && valid_hash(hash)
                && previous
                    .as_ref()
                    .is_none_or(|previous: &String| hash > previous.as_str())
                && entry.file_type().map_err(store_io)?.is_file()
            {
                page.insert(hash.to_string());
                if page.len() > 1024 {
                    page.pop_last();
                }
            }
        }
        let Some(last) = page.last().cloned() else {
            break;
        };
        previous = Some(last);
        let mut candidates: HashSet<_> = page.into_iter().collect();
        remove_referenced(directory, &mut candidates)?;
        if candidates.is_empty() {
            continue;
        }
        // Commit the index state used for marking before reclaiming old blocks.
        #[cfg(unix)]
        directory
            .open(".")
            .and_then(|file| file.sync_all())
            .map_err(store_io)?;
        for hash in candidates {
            match chunks.remove_file(format!("{hash}.bin")) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(store_io(error)),
            }
        }
    }
    #[cfg(unix)]
    chunks
        .open(".")
        .and_then(|file| file.sync_all())
        .map_err(store_io)?;
    Ok(())
}

fn remove_referenced(
    directory: &Dir,
    candidates: &mut std::collections::HashSet<String>,
) -> Result<(), SessionStoreError> {
    for entry in directory.entries().map_err(store_io)? {
        let entry = entry.map_err(store_io)?;
        let name = PathBuf::from(entry.file_name());
        if name.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Some(stem) = name.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if validate_session_id(stem).is_err() {
            continue;
        }
        let (raw, too_large) = read_index(directory, name.as_os_str()).map_err(store_io)?;
        if too_large {
            return Err(invalid(&name, "index exceeds its byte limit"));
        }
        let value: serde_json::Value =
            serde_json::from_str(&raw).map_err(|error| invalid(&name, error))?;
        if value.get("storage").is_some() {
            let manifest = authenticated_manifest(value, &name)?;
            for hash in manifest.chunks {
                candidates.remove(&hash);
            }
            if candidates.is_empty() {
                return Ok(());
            }
        } else {
            let mut record: SessionRecord =
                serde_json::from_value(value).map_err(|error| invalid(&name, error))?;
            if !crate::session_integrity::verify_record(&mut record) {
                return Err(SessionStoreError::AuthenticationFailed {
                    path: name.display().to_string(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod gc_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionEntry;

    fn large_record(workspace: &Path) -> SessionRecord {
        let mut record = SessionRecord::new(workspace.to_path_buf(), "system");
        record
            .entries
            .push(Arc::new(SessionEntry::user("界".repeat(3 * 1024 * 1024))));
        record
    }

    fn manifest(store: &JsonSessionStore, id: &SessionId) -> Manifest {
        serde_json::from_slice(&std::fs::read(store.path_for(id).unwrap()).unwrap()).unwrap()
    }

    #[test]
    fn a_large_record_round_trips_without_losing_content_and_reuses_immutable_chunks() {
        let workspace = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let mut record = large_record(workspace.path());
        store.save(&mut record).unwrap();
        let index = manifest(&store, &record.id);
        assert!(index.bytes > MAX_SESSION_FILE_BYTES);
        assert!(
            std::fs::metadata(store.path_for(&record.id).unwrap())
                .unwrap()
                .len()
                < 16 * 1024
        );
        let chunks = store.root.join(CHUNKS_DIR);
        for name in &index.chunks {
            assert!(
                std::fs::metadata(chunks.join(format!("{name}.bin")))
                    .unwrap()
                    .len()
                    <= CHUNK_BYTES as u64
            );
        }
        let count = std::fs::read_dir(&chunks).unwrap().count();
        store.save(&mut record).unwrap();
        assert_eq!(std::fs::read_dir(&chunks).unwrap().count(), count);
        assert_eq!(store.load(&record.id).unwrap(), record);
        assert_eq!(store.list().unwrap(), vec![record.clone()]);
        let fork = SessionId("large_fork".into());
        std::fs::copy(
            store.path_for(&record.id).unwrap(),
            store.path_for(&fork).unwrap(),
        )
        .unwrap();
        let forked = store.load(&fork).unwrap();
        assert_eq!(forked.id, fork);
        assert_eq!(forked.entries, record.entries);
        store.delete(&record.id).unwrap();
        assert_eq!(store.load(&fork).unwrap().entries, record.entries);
    }

    #[test]
    fn missing_corrupt_and_symlinked_chunks_are_refused() {
        let workspace = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let mut record = large_record(workspace.path());
        store.save(&mut record).unwrap();
        let index = manifest(&store, &record.id);
        let path = store
            .root
            .join(CHUNKS_DIR)
            .join(format!("{}.bin", index.chunks[0]));
        let original = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(store.load(&record.id).is_err());
        let mut corrupted = original.clone();
        corrupted[0] ^= 1;
        std::fs::write(&path, corrupted).unwrap();
        assert!(store.load(&record.id).is_err());
        std::fs::remove_file(&path).unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("chunk");
        std::fs::write(&target, original).unwrap();
        if crate::test_symlinks::symlink_file_for_test(&target, &path) {
            assert!(store.load(&record.id).is_err());
        }
    }

    #[test]
    fn invalid_manifest_limits_and_paths_are_rejected_before_reading_chunks() {
        let workspace = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let id = SessionId("invalid_manifest".into());
        for (bytes, hashes) in [
            (MAX_SESSION_BODY_BYTES + 1, vec!["a".repeat(64)]),
            (1, vec!["../outside".into()]),
            (1, vec!["a".repeat(64); 2]),
        ] {
            let index = Manifest {
                storage: Storage::Chunks,
                bytes,
                sha256: "a".repeat(64),
                mac: "invalid".into(),
                chunks: hashes,
            };
            std::fs::write(
                store.path_for(&id).unwrap(),
                serde_json::to_vec(&index).unwrap(),
            )
            .unwrap();
            assert!(store.load(&id).is_err());
        }
    }

    #[test]
    fn a_failed_chunk_write_preserves_the_last_complete_snapshot() {
        let workspace = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let mut record = SessionRecord::new(workspace.path().to_path_buf(), "system");
        store.save(&mut record).unwrap();
        let previous = record.clone();
        std::fs::write(store.root.join(CHUNKS_DIR), "not a directory").unwrap();
        record
            .entries
            .push(Arc::new(SessionEntry::user("x".repeat(9 * 1024 * 1024))));
        assert!(store.save(&mut record).is_err());
        assert_eq!(store.load(&record.id).unwrap(), previous);
    }

    #[test]
    fn an_unauthenticated_manifest_is_rejected_before_opening_its_chunks() {
        let workspace = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let id = SessionId("unsigned_manifest".into());
        let index = Manifest {
            storage: Storage::Chunks,
            bytes: 1,
            sha256: "a".repeat(64),
            mac: "0".repeat(64),
            chunks: vec!["a".repeat(64)],
        };
        std::fs::write(
            store.path_for(&id).unwrap(),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            store.load(&id),
            Err(SessionStoreError::AuthenticationFailed { .. })
        ));
        assert!(!store.root.join(CHUNKS_DIR).exists());
    }

    #[test]
    fn an_uncertain_chunk_reference_list_prevents_collection_without_failing_a_durable_save() {
        let workspace = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let mut record = large_record(workspace.path());
        store.save(&mut record).unwrap();
        let path = store.path_for(&record.id).unwrap();
        let original = std::fs::read(&path).unwrap();
        let mut index = manifest(&store, &record.id);
        let referenced = index.chunks[0].clone();
        index.chunks[0] = "a".repeat(64);
        std::fs::write(&path, serde_json::to_vec(&index).unwrap()).unwrap();
        assert!(matches!(
            store.load(&record.id),
            Err(SessionStoreError::AuthenticationFailed { .. })
        ));
        let mut other = SessionRecord::new(workspace.path().to_path_buf(), "other");
        store.save(&mut other).unwrap();
        assert_eq!(store.load(&other.id).unwrap(), other);
        assert!(
            store
                .root
                .join(CHUNKS_DIR)
                .join(format!("{referenced}.bin"))
                .is_file()
        );
        assert!(
            store
                .take_diagnostics()
                .iter()
                .any(|message| message.contains("chunk cleanup skipped"))
        );
        std::fs::write(path, original).unwrap();
        assert_eq!(store.load(&record.id).unwrap(), record);
    }

    #[test]
    fn legitimate_inline_history_above_128_mib_does_not_disable_collection() {
        let workspace = tempfile::tempdir().unwrap();
        let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let mut large = large_record(workspace.path());
        store.save(&mut large).unwrap();
        let mut inline = SessionRecord::new(workspace.path().to_path_buf(), "small");
        let mut padded = super::super::super::prepare_record(&mut inline)
            .unwrap()
            .json()
            .to_string();
        padded.push_str(&" ".repeat(7 * 1024 * 1024));
        let source = store.root.join("padded_source");
        std::fs::write(&source, padded).unwrap();
        for index in 0..20 {
            std::fs::hard_link(&source, store.root.join(format!("padded_{index}.json"))).unwrap();
        }
        let bytes = b"unreferenced bytes";
        let orphan = store
            .root
            .join(CHUNKS_DIR)
            .join(format!("{}.bin", hash(bytes)));
        std::fs::write(&orphan, bytes).unwrap();
        let mut other = SessionRecord::new(workspace.path().to_path_buf(), "new");
        store.save(&mut other).unwrap();
        assert!(!orphan.exists());
        assert_eq!(store.load(&large.id).unwrap(), large);
        assert!(store.take_diagnostics().is_empty());
    }

    #[test]
    fn listing_large_records_keeps_the_same_recent_prefix_in_every_scan_order() {
        let mut records: Vec<_> = (1..=3)
            .map(|time| {
                let mut record = SessionRecord::new(PathBuf::from("/workspace"), "system");
                record.updated_at_ms = time;
                record
            })
            .collect();
        let oldest = records.remove(0);
        let middle = records.remove(0);
        let latest = records.remove(0);
        let candidates = [(oldest, 30), (middle, 100), (latest.clone(), 40)];
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let mut window = SessionWindow::default();
            for index in order {
                let (record, mib) = &candidates[index];
                window.push(record.clone(), mib * 1024 * 1024);
            }
            assert_eq!(window.records.len(), 1);
            assert_eq!(window.records[0].0, latest);
        }
    }
}
