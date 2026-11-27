use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::{
    CheckpointRecord, JsonSessionStore, SessionId, SessionRecord, SessionStoreError, TurnRecord,
};
use crate::checkpoint::{CheckpointId, CheckpointStore};
use crate::runtime::TurnId;
use crate::session_entry::{EntryKind, SessionEntry};

/// A saved workspace boundary identified by its conversation turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePoint {
    pub id: CheckpointId,
    pub round: usize,
    pub before_turn: bool,
}

/// A saved conversation round, including those without a workspace snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreRound {
    pub round: usize,
    pub prompt: Option<String>,
    pub image_count: usize,
    pub checkpoints: Vec<RestorePoint>,
}

/// A saved branch whose private workspace is removed if the caller abandons it.
pub struct PreparedRestore {
    pub point: RestorePoint,
    record: SessionRecord,
    directory: tempfile::TempDir,
}

impl PreparedRestore {
    #[must_use]
    pub fn record(&self) -> &SessionRecord {
        &self.record
    }

    /// Keep the independent workspace once the caller is ready to use it.
    pub fn commit(self) -> SessionRecord {
        let _ = self.directory.keep();
        self.record
    }
}

impl JsonSessionStore {
    /// Prepare an independent workspace and a session starting at the
    /// selected restore point. Call on a worker because it copies files.
    pub fn prepare_restore(
        &self,
        session: &SessionId,
        selector: &str,
        cancel: &CancellationToken,
    ) -> Result<PreparedRestore, SessionStoreError> {
        let (point, mut record) = self.session_at_checkpoint(session, selector, cancel)?;
        // Restore the original messages to the live context when they fit the
        // session store. Longer branches keep the summary and linked originals.
        if record.summary.is_some()
            && let Some(originals) = self.transcript_for_resume(&record, cancel)?
        {
            require_original_history(&originals)?;
            record.entries = originals;
            record.summary = None;
        }
        let parent = self
            .workspace_path()
            .parent()
            .ok_or_else(|| restore_error("the workspace has no parent directory for a copy"))?;
        let directory = tempfile::Builder::new()
            .prefix("deep-code-restore-")
            .tempdir_in(parent)
            .map_err(restore_error)?;
        let destination = crate::paths::canonicalize(directory.path()).map_err(restore_error)?;
        let store = CheckpointStore::new(self.workspace_path()).map_err(restore_error)?;
        let copied = store.copy_for_fork(&point.id, &destination, cancel);
        if cancel.is_cancelled() {
            return Err(SessionStoreError::Cancelled);
        }
        copied.map_err(restore_error)?;
        let copied = self.export_record(record, &destination, cancel, true);
        if cancel.is_cancelled() {
            return Err(SessionStoreError::Cancelled);
        }
        let record = copied?;
        Ok(PreparedRestore {
            point,
            record,
            directory,
        })
    }

    /// List the restore points still on disk for the requested session.
    pub fn restore_points(
        &self,
        session: &SessionId,
        cancel: &CancellationToken,
    ) -> Result<Vec<RestorePoint>, SessionStoreError> {
        let record = self.restore_source(session, cancel)?;
        Ok(self
            .available_points(&record)?
            .into_iter()
            .map(|(_, point)| point)
            .collect())
    }

    /// List rounds with prompt previews from the original transcript. Read one
    /// archive at a time to keep the picker independent of transcript length.
    pub fn restore_rounds(
        &self,
        session: &SessionId,
        cancel: &CancellationToken,
    ) -> Result<Vec<RestoreRound>, SessionStoreError> {
        let record = self.restore_source(session, cancel)?;
        let mut numbers = HashMap::new();
        let mut rounds: Vec<_> = numbered_turns(&record)
            .map(|(round, turn)| {
                numbers.insert(&turn.id, round);
                RestoreRound {
                    round,
                    prompt: None,
                    image_count: 0,
                    checkpoints: Vec::new(),
                }
            })
            .collect();
        for (_, point) in self.available_points(&record)? {
            rounds[point.round - 1].checkpoints.push(point);
        }
        // Read newest to oldest, so if a turn has steering messages in
        // different segments the opening prompt supplies the final preview.
        let mut preview = |entries: &[Arc<SessionEntry>]| {
            for entry in entries.iter().rev().filter(|entry| entry.unsent.is_none()) {
                let Some(round) = entry.turn_id.as_ref().and_then(|id| numbers.get(id)) else {
                    continue;
                };
                if let EntryKind::User { content } = &entry.kind {
                    let round = &mut rounds[round - 1];
                    let mut chars = content.text_blocks().flat_map(str::chars);
                    let mut text: String = chars.by_ref().take(120).collect();
                    if chars.next().is_some() {
                        text.push('…');
                    }
                    round.prompt = Some(text.split_whitespace().collect::<Vec<_>>().join(" "));
                    round.image_count += content.image_count();
                }
            }
        };
        preview(&record.entries);
        self.visit_archives(&record.entries, cancel, |archive| {
            preview(&archive.entries);
            Ok(ControlFlow::Continue(()))
        })?;
        Ok(rounds)
    }

    /// Build the conversation up to a restore point under a new session id.
    /// The selector is a round number for completed turns or a checkpoint id.
    pub fn session_at_checkpoint(
        &self,
        session: &SessionId,
        selector: &str,
        cancel: &CancellationToken,
    ) -> Result<(RestorePoint, SessionRecord), SessionStoreError> {
        let source = self.restore_source(session, cancel)?;
        let round = selector.parse::<usize>().ok();
        let (checkpoint, point) = self
            .available_points(&source)?
            .into_iter()
            .find(|(_, point)| {
                point.id.0 == selector || (!point.before_turn && round == Some(point.round))
            })
            .ok_or_else(|| {
                restore_error(format!(
                    "restore point '{selector}' is unavailable in this conversation"
                ))
            })?;
        let mut entries =
            self.checkpoint_entries(&source, checkpoint, point.before_turn, cancel)?;
        entries.retain(|entry| entry.unsent.is_none());
        require_original_history(&entries)?;
        // A summary in this prefix only covers messages preceding the selected
        // boundary. The originals remain accessible through its archive.
        let summary = entries.iter().find_map(|entry| match &entry.kind {
            EntryKind::Compaction { summary, .. } => Some(summary.clone()),
            _ => None,
        });
        let drafts = draft_turns(&source.entries);
        let turn_count = source
            .turns
            .iter()
            .position(|turn| turn.id == checkpoint.turn_id)
            .expect("available points have a recorded turn")
            + usize::from(!point.before_turn);
        let mut record = SessionRecord::new(source.workspace.clone(), "");
        record.entries = entries;
        record.summary = summary;
        record.turns = source
            .turns
            .into_iter()
            .take(turn_count)
            .filter(|turn| !drafts.contains(&turn.id))
            .collect();
        Ok((point, record))
    }

    fn restore_source(
        &self,
        session: &SessionId,
        cancel: &CancellationToken,
    ) -> Result<SessionRecord, SessionStoreError> {
        let source = self.load_cancellable(session, cancel)?;
        if source.workspace != self.workspace_path() {
            return Err(restore_error("session workspace differs from its store"));
        }
        Ok(source)
    }

    fn available_points<'a>(
        &self,
        record: &'a SessionRecord,
    ) -> Result<Vec<(&'a CheckpointRecord, RestorePoint)>, SessionStoreError> {
        if record.checkpoints.is_empty() {
            return Ok(Vec::new());
        }
        let store = CheckpointStore::new(self.workspace_path()).map_err(restore_error)?;
        let saved = store.list().map_err(restore_error)?;
        let rounds: HashMap<_, _> = numbered_turns(record)
            .map(|(round, turn)| (&turn.id, round))
            .collect();
        Ok(record
            .checkpoints
            .iter()
            .filter_map(|checkpoint| {
                let before_turn = match checkpoint.label.as_str() {
                    "after_turn" => false,
                    "before_turn" => true,
                    _ => return None,
                };
                let &round = rounds.get(&checkpoint.turn_id)?;
                saved.binary_search(&checkpoint.id).is_ok().then(|| {
                    (
                        checkpoint,
                        RestorePoint {
                            id: checkpoint.id.clone(),
                            round,
                            before_turn,
                        },
                    )
                })
            })
            .collect())
    }

    fn checkpoint_entries(
        &self,
        source: &SessionRecord,
        checkpoint: &CheckpointRecord,
        before_turn: bool,
        cancel: &CancellationToken,
    ) -> Result<Vec<Arc<SessionEntry>>, SessionStoreError> {
        let cut = |entries: &[Arc<SessionEntry>]| {
            let belongs_to_turn = |entry: &Arc<SessionEntry>| {
                entry.turn_id.as_ref() == Some(&checkpoint.turn_id) && entry.unsent.is_none()
            };
            if before_turn {
                entries.iter().position(|entry| {
                    belongs_to_turn(entry) && matches!(entry.kind, EntryKind::User { .. })
                })
            } else {
                entries.iter().rposition(|entry| {
                    belongs_to_turn(entry)
                        && matches!(&entry.kind, EntryKind::Assistant { exchanges, .. } if exchanges.is_empty())
                }).map(|end| end + 1)
            }
        };
        let mut prefix = cut(&source.entries).map(|end| source.entries[..end].to_vec());
        // A legacy before-turn point may precede a steering message that's
        // still live even though the opening user message was folded.
        if before_turn || prefix.is_none() {
            self.visit_archives(&source.entries, cancel, |mut archive| {
                if let Some(end) = cut(&archive.entries) {
                    archive.entries.truncate(end);
                    if let Some(system) = source
                        .entries
                        .first()
                        .filter(|entry| matches!(entry.kind, EntryKind::System { .. }))
                    {
                        archive.entries.insert(0, Arc::clone(system));
                    }
                    prefix = Some(archive.entries);
                    if !before_turn {
                        return Ok(ControlFlow::Break(()));
                    }
                }
                Ok(ControlFlow::Continue(()))
            })?;
        }
        prefix.ok_or_else(|| restore_error(format!(
            "the transcript for checkpoint '{}' is missing or was compacted before original entries were archived",
            checkpoint.id.0
        )))
    }
}

pub(super) fn require_original_history(
    entries: &[Arc<SessionEntry>],
) -> Result<(), SessionStoreError> {
    if entries.iter().any(|entry| {
        matches!(
            entry.kind,
            EntryKind::Compaction {
                has_archive: false,
                ..
            }
        )
    }) {
        return Err(restore_error(
            "a legacy compaction kept only a summary of this history",
        ));
    }
    Ok(())
}

fn draft_turns(entries: &[Arc<SessionEntry>]) -> HashSet<&TurnId> {
    entries
        .iter()
        .filter(|entry| entry.unsent.is_some())
        .filter_map(|entry| entry.turn_id.as_ref())
        .collect()
}

fn numbered_turns(record: &SessionRecord) -> impl Iterator<Item = (usize, &TurnRecord)> {
    let drafts = draft_turns(&record.entries);
    record
        .turns
        .iter()
        .filter(move |turn| !drafts.contains(&turn.id))
        .enumerate()
        .map(|(index, turn)| (index + 1, turn))
}

fn restore_error(error: impl std::fmt::Display) -> SessionStoreError {
    SessionStoreError::Io {
        message: format!("restore: {error}"),
    }
}

#[cfg(test)]
mod tests;
