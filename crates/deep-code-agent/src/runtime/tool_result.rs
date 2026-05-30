use tokio::sync::mpsc;

use crate::client::LlmClient;
use crate::lsp::{is_edit_tool, render_blocks, summarize_blocks};
use crate::model::{ToolCallFunctionPayload, ToolCallPayload};
use crate::runtime::AgentRuntime;
use crate::runtime::diagnostics::append_diagnostics;
use crate::runtime::event::{RuntimeEvent, emit};
use crate::runtime::state::PendingToolCall;
use crate::tool::{
    ApprovalDecision, ToolCall, ToolError, ToolResult, ToolResultStatus, ToolRunOutcome,
};

impl<C: LlmClient + 'static> AgentRuntime<C> {
    pub(super) async fn handle_approval(
        &self,
        pending: PendingToolCall,
        decision: ApprovalDecision,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        let outcome = self
            .tools
            .run_tool_call(pending.call.clone(), Some(decision));
        match outcome {
            Ok(ToolRunOutcome::Result { result }) => {
                self.record_tool_result(&pending.call, result, tx).await;
            }
            Ok(ToolRunOutcome::ApprovalRequired { request }) => {
                {
                    let mut state = self.state.lock().await;
                    state.pending = Some(pending);
                }
                emit(tx, RuntimeEvent::ApprovalRequired { request });
                return;
            }
            Err(error) => {
                emit(tx, runtime_error_from_tool_error(error));
                self.abort_turn().await;
                return;
            }
        }

        // Approved (or denied) call recorded; resume the loop to feed the
        // tool result into the next chat turn.
        self.run_loop(tx).await;
    }

    pub(super) async fn record_tool_result(
        &self,
        call: &ToolCall,
        mut result: ToolResult,
        tx: &mpsc::UnboundedSender<RuntimeEvent>,
    ) {
        if result.status == ToolResultStatus::Success
            && is_edit_tool(&call.name)
            && let Some(lsp) = self.lsp.as_ref()
        {
            let blocks = lsp.collect_for_edit(&call.name, &call.arguments).await;
            if !blocks.is_empty() {
                let rendered = render_blocks(&blocks);
                let summary = summarize_blocks(&blocks);
                result.content = append_diagnostics(&result.content, &rendered);
                emit(
                    tx,
                    RuntimeEvent::DiagnosticsUpdated {
                        summary: summary.clone(),
                        rendered,
                    },
                );
            }
        }

        {
            let mut state = self.state.lock().await;
            state.session.push(result.to_message());
            if let Some(turn) = state.current_turn.as_mut() {
                turn.tool_results.push(result.clone());
            }
        }
        self.persist().await;
        emit(tx, RuntimeEvent::ToolResult { result });
    }
}

pub(super) fn tool_call_payload(call: &ToolCall) -> ToolCallPayload {
    // Compact form keeps history small and matches typical OpenAI-style
    // assistant payloads. We don't try to preserve the exact bytes the model
    // produced because we already parsed them through `ToolCallAccumulator`.
    let arguments = serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_string());
    ToolCallPayload {
        id: call.id.clone(),
        call_type: "function".to_string(),
        function: ToolCallFunctionPayload {
            name: call.name.clone(),
            arguments,
        },
    }
}

pub(super) fn runtime_error_from_tool_error(error: ToolError) -> RuntimeEvent {
    RuntimeEvent::Error {
        message: error.to_string(),
    }
}
