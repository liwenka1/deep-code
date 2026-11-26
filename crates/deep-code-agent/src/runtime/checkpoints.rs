use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::checkpoint::{CheckpointId, CheckpointStore, SnapshotOutcome, SnapshotSkip};
use crate::runtime::AgentRuntime;
use crate::runtime::event::{RuntimeEvent, TurnId, emit};
use crate::session_store::CheckpointRecord;
use crate::tool::ToolError;

impl AgentRuntime {
    /// Enable an automatic end-of-turn snapshot for the given workspace
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
        let state = self.state.clone().lock_owned().await;
        if state.current_turn_id.is_some() || state.pending.is_some() {
            return Err(ToolError::exec_failed(
                "checkpoint",
                "a turn is in progress — cancel it (Esc) before restoring, so the rollback \
                     is not overwritten by the turn's remaining file writes",
            ));
        }
        let jobs = match &self.jobs {
            Some(jobs) => Some(jobs.idle_workspace().await?),
            None => None,
        };
        let store = Arc::clone(store);
        tokio::task::spawn_blocking(move || {
            // Keep both guards in the blocking task even if its awaiting
            // caller is cancelled. No turn or process may start mid-restore.
            let _state = state;
            let _jobs = jobs;
            store.restore(&id)
        })
        .await
        .map_err(|error| ToolError::exec_failed("checkpoint", error.to_string()))?
    }

    pub(super) async fn snapshot_completed_turn(
        &self,
        turn_id: &TurnId,
        cancel: &CancellationToken,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        let Some(store) = self.checkpoints.as_ref() else {
            return;
        };
        let label = "after_turn";
        // Full workspace copy: run it on the blocking pool so a large repo
        // can't stall the async executor for the duration of the copy. The
        // token lets Esc/quit abort the copy promptly (see `CheckpointStore::
        // snapshot`) rather than after the whole tree has been copied.
        let store = Arc::clone(store);
        let cancel = cancel.clone();
        let idle_jobs = match &self.jobs {
            Some(jobs) => jobs.idle_workspace().await.map(Some),
            None => Ok(None),
        };
        let outcome = match idle_jobs {
            Ok(idle_jobs) => tokio::task::spawn_blocking(move || {
                let _idle_jobs = idle_jobs;
                store
                    .snapshot(label, &cancel)
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result),
            Err(error) => Err(error.to_string()),
        };
        let failure = match outcome {
            Ok(SnapshotOutcome::Created { id, prune_warnings }) => {
                for message in prune_warnings {
                    emit(tx, RuntimeEvent::Warning { message });
                }
                self.record_checkpoint(id.clone(), turn_id.clone(), label)
                    .await;
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
            Ok(SnapshotOutcome::Skipped(SnapshotSkip::Cancelled)) => return,
            // Over the entry budget: warn once (the turn it trips), then the
            // session latch keeps every later turn silent.
            Ok(SnapshotOutcome::Skipped(SnapshotSkip::TooLarge { first })) => {
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
            Err(error) => error,
        };
        // A checkpoint failure leaves the completed response usable.
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

    async fn record_checkpoint(&self, id: CheckpointId, turn_id: TurnId, label: &str) {
        if self.persistence.is_none() {
            return;
        }
        let checkpoint = CheckpointRecord::new(id, turn_id, label);
        // Save the checkpoint and the final assistant entry in one revision.
        self.persist_with(|record| {
            record.checkpoints.push(checkpoint);
            let cap = self.config.checkpoint_max_snapshots;
            if cap > 0 && record.checkpoints.len() > cap {
                let excess = record.checkpoints.len() - cap;
                record.checkpoints.drain(..excess);
            }
        })
        .await;
    }
}
