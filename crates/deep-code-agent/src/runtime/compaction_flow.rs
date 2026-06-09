use tokio::sync::mpsc;

use crate::client::LlmClient;
use crate::compaction::{compact_messages, should_compact};
use crate::runtime::AgentRuntime;
use crate::runtime::event::{RuntimeEvent, emit};

impl<C: LlmClient + 'static> AgentRuntime<C> {
    pub(super) async fn maybe_compact(
        &self,
        model: &str,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) -> bool {
        let messages = self.state.lock().await.session.messages().to_vec();
        if !should_compact(model, &messages, self.config.compaction_threshold) {
            return false;
        }
        let result = compact_messages(&messages);
        if result.archived_count == 0 {
            return false;
        }
        let compacted = {
            let mut state = self.state.lock().await;
            state.session.replace_messages(result.messages.clone());
            state.last_prefix_hash = None;
            state.session.messages().to_vec()
        };
        if let Some(persistence) = self.persistence.as_ref() {
            {
                let mut record = persistence.record.lock().await;
                record.messages = compacted;
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
