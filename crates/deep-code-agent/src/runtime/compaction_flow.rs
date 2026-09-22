use tokio::sync::mpsc;

use crate::compaction::{compact_entries, should_compact};
use crate::runtime::AgentRuntime;
use crate::runtime::event::{RuntimeEvent, emit};

impl AgentRuntime {
    pub(super) async fn maybe_compact(
        &self,
        model: &str,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) -> bool {
        let (wire, entries) = {
            let state = self.state.lock().await;
            (
                state.session.wire_messages(),
                state.session.entries().to_vec(),
            )
        };
        if !should_compact(model, &wire, self.config.compaction_threshold) {
            return false;
        }
        let result = compact_entries(&entries);
        if result.archived_count == 0 {
            return false;
        }
        let compacted = {
            let mut state = self.state.lock().await;
            state.session.replace_entries(result.entries.clone());
            // No prefix reset here. Compaction replaces the history with a
            // shorter, different one, so `telemetry::probe_prefix` reports
            // `Changed` on its own — which is also the accurate label, where
            // the reset used to make the turn after a compaction read
            // `FirstTurn`.
            state.session.entries().to_vec()
        };
        if let Some(persistence) = self.persistence.as_ref() {
            {
                let mut record = persistence.record.lock().await;
                record.entries = compacted;
                record.summary = Some(result.summary.clone());
                record.compaction = Some(format!("archived={}", result.archived_count));
                record.touch();
            }
            persistence.actor.request_save();
        }
        emit(
            tx,
            RuntimeEvent::CompactionApplied {
                archived_count: result.archived_count,
                summary: result.summary.clone(),
            },
        );
        true
    }
}
