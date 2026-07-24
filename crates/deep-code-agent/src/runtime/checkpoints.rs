use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::checkpoint::{CheckpointId, CheckpointStore};
use crate::runtime::AgentRuntime;
use crate::runtime::event::{RuntimeEvent, emit};
use crate::session_store::CheckpointRecord;
use crate::tool::ToolError;

impl AgentRuntime {
    /// Enable automatic before/after turn snapshots for the given workspace
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
    pub async fn restore_checkpoint(&self, id: CheckpointId) -> Result<(), ToolError> {
        let store = self
            .checkpoints
            .as_ref()
            .ok_or_else(|| ToolError::ExecutionFailed {
                name: "checkpoint".to_string(),
                message: "checkpoints are not enabled on this runtime".to_string(),
            })?;
        store.restore(&id)
    }

    pub(super) async fn snapshot_turn(
        &self,
        label: &str,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        let Some(store) = self.checkpoints.as_ref() else {
            return;
        };
        // Full workspace copy: run it on the blocking pool so a large repo
        // can't stall the async executor for the duration of the copy.
        let store = Arc::clone(store);
        let owned_label = label.to_string();
        let outcome = tokio::task::spawn_blocking(move || store.snapshot(&owned_label)).await;
        match outcome {
            Ok(Ok((id, prune_warnings))) => {
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
            }
            Ok(Err(error)) => {
                emit(
                    tx,
                    RuntimeEvent::Error {
                        turn_id: None,
                        message: crate::tr_with(
                            self.ui_lang(),
                            crate::TextId::CheckpointSnapshotFailed,
                            &[("label", label), ("error", &error.to_string())],
                        ),
                    },
                );
            }
            Err(join_error) => {
                emit(
                    tx,
                    RuntimeEvent::Error {
                        turn_id: None,
                        message: crate::tr_with(
                            self.ui_lang(),
                            crate::TextId::CheckpointSnapshotFailed,
                            &[("label", label), ("error", &join_error.to_string())],
                        ),
                    },
                );
            }
        }
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
            session.touch();
        }
        persistence.actor.request_save();
    }
}
