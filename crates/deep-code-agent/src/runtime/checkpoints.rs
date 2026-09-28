use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::checkpoint::{CheckpointId, CheckpointStore, SnapshotOutcome, SnapshotSkip};
use crate::runtime::AgentRuntime;
use crate::runtime::event::{RuntimeEvent, emit};
use crate::session_store::CheckpointRecord;
use crate::tool::ToolError;

impl AgentRuntime {
    /// Enable an automatic before-turn snapshot for the given workspace
    /// root.
    ///
    /// If checkpoint storage cannot be created, checkpoints stay disabled, the
    /// reason is pushed to `warnings`, and the runtime is still returned.
    #[must_use]
    pub fn with_checkpoints(
        mut self,
        workspace: impl Into<PathBuf>,
        warnings: &mut Vec<String>,
    ) -> Self {
        match CheckpointStore::new(workspace) {
            Ok(store) => {
                self.checkpoints = Some(Arc::new(
                    store.with_max_snapshots(self.config.checkpoint_max_snapshots),
                ));
            }
            Err(error) => warnings.push(format!("checkpoints disabled: {error}")),
        }
        self
    }

    /// Restore workspace files from a checkpoint id.
    /// Restore, returning the paths `clear` deliberately kept (see
    /// [`CheckpointStore::restore`]) so the caller can say so instead of
    /// reporting a flat success.
    pub async fn restore_checkpoint(&self, id: CheckpointId) -> Result<Vec<String>, ToolError> {
        let store = self.checkpoints.as_ref().ok_or_else(|| {
            ToolError::exec_failed("checkpoint", "checkpoints are not enabled on this runtime")
        })?;
        // Refuse while a turn is live or parked on an approval. Restore clears
        // and re-copies the whole workspace; a turn running concurrently keeps
        // writing files (its tool calls run on other tasks) on top of — or into
        // a directory being cleared by — the restore, so the workspace ends up a
        // mix of the rewound tree and the live turn's later writes, with the
        // model unaware and a "restored" report either way. Cancel the turn
        // first (Esc), then restore.
        {
            let state = self.state.lock().await;
            if state.current_turn_id.is_some() || state.pending.is_some() {
                return Err(ToolError::exec_failed(
                    "checkpoint",
                    "a turn is in progress — cancel it (Esc) before restoring, so the rollback \
                     is not overwritten by the turn's remaining file writes",
                ));
            }
        }
        store.restore(&id)
    }

    pub(super) async fn snapshot_turn(
        &self,
        label: &str,
        cancel: &CancellationToken,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        let Some(store) = self.checkpoints.as_ref() else {
            return;
        };
        // Full workspace copy: run it on the blocking pool so a large repo
        // can't stall the async executor for the duration of the copy. The
        // token lets Esc/quit abort the copy promptly (see `CheckpointStore::
        // snapshot`) rather than after the whole tree has been copied.
        let store = Arc::clone(store);
        let owned_label = label.to_string();
        let cancel = cancel.clone();
        let outcome =
            tokio::task::spawn_blocking(move || store.snapshot(&owned_label, &cancel)).await;
        let failure = match outcome {
            Ok(Ok(SnapshotOutcome::Created { id, prune_warnings })) => {
                for message in prune_warnings {
                    emit(tx, RuntimeEvent::Warning { message });
                }
                self.record_checkpoint(id.clone(), label).await;
                emit(
                    tx,
                    RuntimeEvent::CheckpointCreated {
                        id,
                        label: label.to_string(),
                    },
                );
                return;
            }
            // Cancelled: the turn is being torn down anyway, so say nothing.
            Ok(Ok(SnapshotOutcome::Skipped(SnapshotSkip::Cancelled))) => return,
            // Over the entry budget: warn once (the turn it trips), then the
            // session latch keeps every later turn silent.
            Ok(Ok(SnapshotOutcome::Skipped(SnapshotSkip::TooLarge { first }))) => {
                if first {
                    emit(
                        tx,
                        RuntimeEvent::Warning {
                            message: crate::tr_with(
                                self.ui_lang(),
                                crate::TextId::CheckpointDisabledTooLarge,
                                &[(
                                    "limit",
                                    &crate::checkpoint::MAX_SNAPSHOT_ENTRIES.to_string(),
                                )],
                            ),
                        },
                    );
                }
                return;
            }
            Ok(Err(error)) => error.to_string(),
            Err(join_error) => join_error.to_string(),
        };
        // The turn goes on without its restore point — `drive_turn` spawns the
        // loop right after this call regardless — so this is a degradation to
        // surface, not a terminal `Error`. Every consumer treats `Error` as the
        // end of the turn (the TUI stops observing the stream, headless stops
        // the run), and emitting it here left the loop running unobserved:
        // tools executed and cost accrued with nothing on screen, and an
        // approval request parked with nobody to answer it. An unreadable
        // subtree in the workspace made that happen on every turn.
        emit(
            tx,
            RuntimeEvent::Warning {
                message: crate::tr_with(
                    self.ui_lang(),
                    crate::TextId::CheckpointSnapshotFailed,
                    &[("label", label), ("error", &failure)],
                ),
            },
        );
    }

    async fn record_checkpoint(&self, id: CheckpointId, label: &str) {
        let Some(persistence) = self.persistence.as_ref() else {
            return;
        };
        let record = CheckpointRecord::new(id, label);
        {
            // A plain lock (not try_lock): losing the race here used to drop
            // the checkpoint from session metadata, leaving a snapshot on disk
            // that `/restore` could not list.
            let mut session = persistence.record.lock().await;
            session.checkpoints.push(record);
            // Same cap the disk store prunes to, in the same direction.
            // `CheckpointStore::prune_old_snapshots` deletes the oldest
            // snapshot directories beyond `max_snapshots`; this list did not,
            // so a session longer than the cap accumulated metadata for
            // snapshots that no longer exist — and `hydrate_history` renders
            // every one of them on resume with a `/restore <id>` hint that can
            // only fail. Push-only and id-timestamped, so creation order is
            // index order and the oldest are at the front. `0` means "keep
            // everything" on both sides.
            let cap = self.config.checkpoint_max_snapshots;
            if cap > 0 && session.checkpoints.len() > cap {
                let excess = session.checkpoints.len() - cap;
                session.checkpoints.drain(..excess);
            }
            session.touch();
        }
        persistence.actor.request_save();
    }
}
