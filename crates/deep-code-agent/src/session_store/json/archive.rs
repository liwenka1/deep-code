//! Immutable, authenticated transcript segments written before compaction.

use std::collections::HashSet;
use std::ops::ControlFlow;

use cap_fs_ext::DirExt;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::runtime::TurnId;
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
        if let Ok(id) = SessionId::parse(summary_id) {
            let _ = self.remove_archives([id]);
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

    /// The caller holds the primary store's exclusive lock. Consider only
    /// segments reachable from the deleted record. A compaction may already
    /// have written a segment whose new summary is still waiting to save.
    pub(super) fn reclaim_deleted_archive(
        &self,
        deleted: &SessionRecord,
    ) -> Result<(), SessionStoreError> {
        let cancel = CancellationToken::new();
        let mut unused = HashSet::new();
        self.visit_archives(&deleted.entries, &cancel, |archive| {
            unused.insert(archive.id);
            Ok(ControlFlow::Continue(()))
        })?;
        if unused.is_empty() {
            return Ok(());
        }
        for entry in self.directory.entries().map_err(store_io)? {
            let name = PathBuf::from(entry.map_err(store_io)?.file_name());
            let Some(id) = name.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if name.extension().is_none_or(|extension| extension != "json")
                || validate_session_id(id).is_err()
            {
                continue;
            }
            let (record, _) = self.read_while_locked(&self.root.join(name), None)?;
            self.visit_archives(&record.entries, &cancel, |archive| {
                unused.remove(&archive.id);
                Ok(if unused.is_empty() {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                })
            })?;
            if unused.is_empty() {
                return Ok(());
            }
        }
        self.remove_archives(unused)
    }

    fn remove_archives(
        &self,
        archives: impl IntoIterator<Item = SessionId>,
    ) -> Result<(), SessionStoreError> {
        let store = self.archive_store(false)?;
        let _guard = StoreLock::acquire(&store.directory, true, None)?;
        for id in archives {
            match store.directory.remove_file(format!("{}.json", id.as_str())) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(store_io(error)),
            }
        }
        #[cfg(unix)]
        store
            .directory
            .open(".")
            .and_then(|file| file.sync_all())
            .map_err(store_io)?;
        store.reclaim_chunks();
        Ok(())
    }

    /// Walk newest to oldest. A segment can carry the summary of its
    /// predecessor, so each compaction saves only the newly folded entries.
    pub(in crate::session_store) fn visit_archives(
        &self,
        entries: &[Arc<SessionEntry>],
        cancel: &CancellationToken,
        mut visit: impl FnMut(SessionRecord) -> Result<ControlFlow<()>, SessionStoreError>,
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
            if visit(record)?.is_break() {
                return Ok(());
            }
            let Some(parent) = parent else {
                return Ok(());
            };
            next = parent;
        }
    }

    /// A turn can have a live rejected steer and an archived opening prompt.
    /// Keep only candidates with no confirmed user entry in either place.
    pub(crate) fn retain_unconfirmed_turns(
        &self,
        entries: &[Arc<SessionEntry>],
        turns: &mut HashSet<TurnId>,
        cancel: &CancellationToken,
    ) -> Result<(), SessionStoreError> {
        fn remove_confirmed(turns: &mut HashSet<TurnId>, entries: &[Arc<SessionEntry>]) {
            for entry in entries {
                if entry.unsent.is_none()
                    && matches!(entry.kind, EntryKind::User { .. })
                    && let Some(id) = &entry.turn_id
                {
                    turns.remove(id);
                }
            }
        }

        if !turns.is_empty() {
            remove_confirmed(turns, entries);
        }
        if !turns.is_empty() {
            self.visit_archives(entries, cancel, |archive| {
                remove_confirmed(turns, &archive.entries);
                Ok(if turns.is_empty() {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                })
            })?;
        }
        Ok(())
    }

    /// Expand the transcript of a loaded session for display. A summary from
    /// before transcript archiving remains in place.
    pub fn transcript_entries(
        &self,
        record: &SessionRecord,
        cancel: &CancellationToken,
    ) -> Result<Vec<Arc<SessionEntry>>, SessionStoreError> {
        Ok(self
            .collect_transcript(record, cancel, |_| true)?
            .expect("all transcript segments were accepted"))
    }

    pub(in crate::session_store) fn transcript_for_resume(
        &self,
        record: &SessionRecord,
        cancel: &CancellationToken,
    ) -> Result<Option<Vec<Arc<SessionEntry>>>, SessionStoreError> {
        let mut remaining_entries = MAX_SESSION_ENTRIES.saturating_sub(record.entries.len());
        let mut remaining_bytes = MAX_SESSION_BODY_BYTES;
        if !crate::session_store::charge_json(record, &mut remaining_bytes) {
            return Ok(None);
        }
        self.collect_transcript(record, cancel, |entries| {
            let Some(remaining) = remaining_entries.checked_sub(entries.len()) else {
                return false;
            };
            remaining_entries = remaining;
            crate::session_store::charge_json(&entries, &mut remaining_bytes)
        })
    }

    fn collect_transcript(
        &self,
        record: &SessionRecord,
        cancel: &CancellationToken,
        mut keep: impl FnMut(&[Arc<SessionEntry>]) -> bool,
    ) -> Result<Option<Vec<Arc<SessionEntry>>>, SessionStoreError> {
        let mut segments = Vec::new();
        let mut fits = true;
        self.visit_archives(&record.entries, cancel, |archive| {
            if !keep(&archive.entries) {
                fits = false;
                return Ok(ControlFlow::Break(()));
            }
            segments.push(archive.entries);
            Ok(ControlFlow::Continue(()))
        })?;
        if !fits {
            return Ok(None);
        }
        // The first summary sits after the system prompt. Replace it at that
        // position, preserving the system prompt and retained tail.
        let summary = record.entries.iter().position(has_archive);
        let Some(summary) = summary else {
            return Ok(Some(record.entries.clone()));
        };
        let mut transcript = record.entries[..summary].to_vec();
        for segment in segments.into_iter().rev() {
            transcript.extend(segment.into_iter().filter(|entry| !has_archive(entry)));
        }
        transcript.extend_from_slice(&record.entries[summary + 1..]);
        Ok(Some(transcript))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(store: &JsonSessionStore, entries: Vec<Arc<SessionEntry>>) -> Arc<SessionEntry> {
        let mut summary = SessionEntry::compaction("summary", entries.len());
        let EntryKind::Compaction { has_archive, .. } = &mut summary.kind else {
            panic!("expected compaction");
        };
        *has_archive = true;
        store.save_compaction_archive(&summary.id, entries).unwrap();
        Arc::new(summary)
    }

    #[test]
    fn deleting_sessions_reclaims_their_archives_after_the_last_reference() {
        let workspace = tempfile::tempdir().unwrap();
        let root = crate::canonicalize(workspace.path()).unwrap();
        let store = JsonSessionStore::for_pinned_workspace(&root).unwrap();
        let old = archive(&store, vec![Arc::new(SessionEntry::user("shared history"))]);
        let recent = archive(
            &store,
            vec![old.clone(), Arc::new(SessionEntry::user("newer history"))],
        );
        let pending = archive(&store, vec![Arc::new(SessionEntry::user("unpublished"))]);
        let archive_store = store.archive_store(false).unwrap();
        let present = |entry: &SessionEntry| {
            archive_store
                .load(&SessionId::parse(&entry.id).unwrap())
                .is_ok()
        };
        let mut original = SessionRecord::new(root.clone(), "system");
        original.entries.push(recent.clone());
        store.save(&mut original).unwrap();
        let mut sibling = SessionRecord::new(root, "system");
        sibling.entries.push(old.clone());
        store.save(&mut sibling).unwrap();

        store.delete(&original.id).unwrap();
        assert!(present(&old));
        assert!(!present(&recent));
        assert!(present(&pending));
        let entries = store
            .transcript_entries(&store.load(&sibling.id).unwrap(), &CancellationToken::new())
            .unwrap();
        assert!(
            matches!(&entries[1].kind, EntryKind::User { content } if content.text() == "shared history")
        );
        store.delete(&sibling.id).unwrap();
        assert!(!present(&old));
        assert!(present(&pending));
        assert!(store.take_diagnostics().is_empty());
    }
}
