use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::checkpoint::{CheckpointId, CheckpointStore};
use crate::client::LlmClient;
use crate::runtime::AgentRuntime;
use crate::runtime::event::{RuntimeEvent, emit};
use crate::tool::ToolError;

impl<C: LlmClient + 'static> AgentRuntime<C> {
    /// Enable automatic before/after turn snapshots for the given workspace root.
    ///
    /// If checkpoint storage cannot be created, checkpoints stay disabled and the
    /// runtime is still returned.
    #[must_use]
    pub fn with_checkpoints(mut self, workspace: impl Into<PathBuf>) -> Self {
        match CheckpointStore::new(workspace) {
            Ok(store) => self.checkpoints = Some(Arc::new(store)),
            Err(error) => eprintln!("checkpoints disabled: {error}"),
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

    #[must_use]
    pub fn checkpoints_enabled(&self) -> bool {
        self.checkpoints.is_some()
    }

    pub(super) fn snapshot_turn(&self, label: &str, tx: &mpsc::UnboundedSender<RuntimeEvent>) {
        let Some(store) = self.checkpoints.as_ref() else {
            return;
        };
        match store.snapshot(label) {
            Ok(id) => emit(
                tx,
                RuntimeEvent::CheckpointCreated {
                    id,
                    label: label.to_string(),
                },
            ),
            Err(error) => {
                eprintln!("checkpoint snapshot '{label}' failed: {error}");
            }
        }
    }
}
