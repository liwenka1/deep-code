//! Immutable, authenticated transcript segments written before compaction.

use std::collections::HashSet;

use cap_fs_ext::DirExt;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::session_entry::{EntryKind, SessionEntry};

const ARCHIVES_DIR: &str = "archives";

impl JsonSessionStore {
    pub(crate) fn save_compaction_archive(
        &self,
        summary_id: &str,
        entries: Vec<Arc<SessionEntry>>,
    ) -> Result<(), SessionStoreError> {
        let mut record = SessionRecord::new(self.workspace.clone(), "");
        record.id = SessionId::parse(summary_id)?;
        record.entries = entries;
        self.save_archive(&mut record)
    }

    pub(crate) fn discard_compaction_archive(&self, summary_id: &str) {
        if let (Ok(store), Ok(id)) = (self.archive_store(false), SessionId::parse(summary_id)) {
            let _ = store.delete(&id);
        }
    }

    pub(in crate::session_store) fn save_archive(
        &self,
        record: &mut SessionRecord,
    ) -> Result<(), SessionStoreError> {
        self.archive_store(true)?.save(record)
    }

    fn archive_store(&self, create: bool) -> Result<Self, SessionStoreError> {
        let directory = if create {
            crate::pinned_dir::owned_child(&self.directory, ARCHIVES_DIR)
        } else {
            self.directory.open_dir_nofollow(ARCHIVES_DIR)
        }
        .map_err(store_io)?;
        Ok(Self {
            root: self.root.join(ARCHIVES_DIR),
            directory: Arc::new(directory),
            ..self.clone()
        })
    }

    /// Walk newest to oldest. A segment can carry the summary of its
    /// predecessor, so each compaction saves only the newly folded entries.
    pub(in crate::session_store) fn visit_archives(
        &self,
        entries: &[Arc<SessionEntry>],
        cancel: &CancellationToken,
        mut visit: impl FnMut(SessionRecord) -> Result<(), SessionStoreError>,
    ) -> Result<(), SessionStoreError> {
        let Some(mut next) = archive_id(entries)? else {
            return Ok(());
        };
        let store = self.archive_store(false)?;
        let mut seen = HashSet::new();
        loop {
            check_scan_cancelled(cancel)?;
            if !seen.insert(next.clone()) {
                return Err(store_io(std::io::Error::other("cyclic transcript archive")));
            }
            let path = store.path_for(&next)?;
            // Read the signed id directly. The normal resume loader permits
            // rebinding a copied session file to its filename.
            let (record, _) = store.read_sized(&path, Some(cancel))?;
            if record.id != next || record.workspace != self.workspace {
                return Err(SessionStoreError::AuthenticationFailed {
                    path: path.display().to_string(),
                });
            }
            let parent = archive_id(&record.entries)?;
            visit(record)?;
            let Some(parent) = parent else {
                return Ok(());
            };
            next = parent;
        }
    }

    /// Expand the transcript of a loaded session for display. A summary from
    /// before transcript archiving remains in place.
    pub fn transcript_entries(
        &self,
        record: &SessionRecord,
        cancel: &CancellationToken,
    ) -> Result<Vec<Arc<SessionEntry>>, SessionStoreError> {
        let mut segments = Vec::new();
        self.visit_archives(&record.entries, cancel, |archive| {
            segments.push(archive.entries);
            Ok(())
        })?;
        // The first summary sits after the system prompt. Replace it at that
        // position, preserving the system prompt and retained tail.
        let summary = record.entries.iter().position(has_archive);
        let Some(summary) = summary else {
            return Ok(record.entries.clone());
        };
        let mut transcript = record.entries[..summary].to_vec();
        for segment in segments.into_iter().rev() {
            transcript.extend(segment.into_iter().filter(|entry| !has_archive(entry)));
        }
        transcript.extend_from_slice(&record.entries[summary + 1..]);
        Ok(transcript)
    }
}

fn archive_id(entries: &[Arc<SessionEntry>]) -> Result<Option<SessionId>, SessionStoreError> {
    entries
        .iter()
        .find(|entry| has_archive(entry))
        .map(|entry| SessionId::parse(&entry.id))
        .transpose()
}

fn has_archive(entry: &Arc<SessionEntry>) -> bool {
    matches!(
        entry.kind,
        EntryKind::Compaction {
            has_archive: true,
            ..
        }
    )
}
