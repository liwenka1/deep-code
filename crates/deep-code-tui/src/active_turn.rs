use deep_code_agent::{ApprovalRequest, ToolCallId};

use crate::history::{HistoryCell, ToolApprovalState};

/// Bound on the buffered live-output tail per running tool (display only —
/// the agent-side ring buffer keeps the full 128 KiB).
const LIVE_OUTPUT_MAX_CHARS: usize = 4_096;
/// How many trailing output lines the transcript preview shows per tool.
const LIVE_OUTPUT_PREVIEW_LINES: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveOutput(String);

impl LiveOutput {
    pub fn push(&mut self, text: &str) {
        self.0.push_str(text);
        let count = self.0.chars().count();
        if count > LIVE_OUTPUT_MAX_CHARS {
            self.0 = self.0.chars().skip(count - LIVE_OUTPUT_MAX_CHARS).collect();
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Last few complete lines for the transcript preview.
    #[must_use]
    pub fn preview_tail(&self) -> String {
        let lines: Vec<&str> = self.0.lines().collect();
        let start = lines.len().saturating_sub(LIVE_OUTPUT_PREVIEW_LINES);
        lines[start..].join("\n")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveToolCell {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub arguments: String,
    pub approval: ToolApprovalState,
    pub live_output: LiveOutput,
    /// When this call started running, for the "still alive, N seconds in"
    /// readouts (status bar + transcript preview). A minutes-long tool (agent,
    /// a build) with no clock reads as a hang.
    pub started_at: std::time::Instant,
}

/// The turn currently streaming. It carries no turn id: the TUI shows one
/// turn at a time and attributes every event to it, so nothing ever reads the
/// id back — a turn that arrives without a `TurnStarted` (a late delta, an
/// approval) is started the same way as one that does.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ActiveTurn {
    pub assistant_buffer: String,
    pub reasoning_buffer: String,
    pub tools: Vec<ActiveToolCell>,
    pub diagnostics: Vec<HistoryCell>,
    pub pending_approval: Option<ApprovalRequest>,
}

impl ActiveTurn {
    pub fn push_assistant_delta(&mut self, text: &str) {
        self.assistant_buffer.push_str(text);
    }

    pub fn push_reasoning_delta(&mut self, text: &str) {
        self.reasoning_buffer.push_str(text);
    }

    pub fn upsert_tool(&mut self, cell: ActiveToolCell) {
        if let Some(existing) = self
            .tools
            .iter_mut()
            .find(|tool| tool.tool_call_id == cell.tool_call_id)
        {
            // A re-upsert (duplicate ToolCallStarted) must not wipe output
            // that already streamed in, nor restart the clock.
            let live_output = std::mem::take(&mut existing.live_output);
            let started_at = existing.started_at;
            *existing = cell;
            existing.live_output = live_output;
            existing.started_at = started_at;
        } else {
            self.tools.push(cell);
        }
    }

    pub fn append_tool_output(&mut self, tool_call_id: &ToolCallId, text: &str) {
        if let Some(existing) = self
            .tools
            .iter_mut()
            .find(|tool| &tool.tool_call_id == tool_call_id)
        {
            existing.live_output.push(text);
        }
    }

    pub fn append_tool_arguments(&mut self, tool_call_id: &ToolCallId, delta: &str) {
        if let Some(existing) = self
            .tools
            .iter_mut()
            .find(|tool| &tool.tool_call_id == tool_call_id)
        {
            existing.arguments.push_str(delta);
        } else {
            self.tools.push(ActiveToolCell {
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_call_id.as_str().to_string(),
                arguments: delta.to_string(),
                approval: ToolApprovalState::NotRequired,
                live_output: LiveOutput::default(),
                started_at: std::time::Instant::now(),
            });
        }
    }

    pub fn mark_approval_required(&mut self, request: &ApprovalRequest) {
        let tool_call_id = ToolCallId::from(request.call_id.clone());
        if let Some(existing) = self
            .tools
            .iter_mut()
            .find(|tool| tool.tool_call_id == tool_call_id)
        {
            existing.approval = ToolApprovalState::Required;
        } else {
            self.tools.push(ActiveToolCell {
                tool_call_id,
                tool_name: request.tool_name.clone(),
                arguments: request.arguments.to_string(),
                approval: ToolApprovalState::Required,
                live_output: LiveOutput::default(),
                started_at: std::time::Instant::now(),
            });
        }
    }

    pub fn resolve_approval(&mut self, decision: deep_code_agent::ApprovalDecision) {
        let Some(request) = self.pending_approval.take() else {
            return;
        };
        let tool_call_id = ToolCallId::from(request.call_id);
        let approval = match decision {
            deep_code_agent::ApprovalDecision::Approved
            | deep_code_agent::ApprovalDecision::ApprovedForSession => ToolApprovalState::Approved,
            deep_code_agent::ApprovalDecision::Denied => ToolApprovalState::Denied,
        };
        if let Some(existing) = self
            .tools
            .iter_mut()
            .find(|tool| tool.tool_call_id == tool_call_id)
        {
            existing.approval = approval;
        }
    }

    pub fn push_diagnostics(&mut self, summary: String, rendered: String) {
        self.diagnostics
            .push(HistoryCell::Diagnostics { summary, rendered });
    }

    /// Flush only what belongs to one finished tool call: the streamed
    /// text/reasoning so far (once), that tool's cell, and accumulated
    /// diagnostics. Other still-running tool cells stay in the active turn.
    pub fn take_finished_tool_cells(&mut self, tool_call_id: &ToolCallId) -> Vec<HistoryCell> {
        let mut cells = Vec::new();
        if !self.reasoning_buffer.is_empty() {
            cells.push(HistoryCell::Reasoning {
                text: std::mem::take(&mut self.reasoning_buffer),
            });
        }
        if !self.assistant_buffer.is_empty() {
            cells.push(HistoryCell::Assistant {
                text: std::mem::take(&mut self.assistant_buffer),
            });
        }
        if let Some(position) = self
            .tools
            .iter()
            .position(|tool| &tool.tool_call_id == tool_call_id)
        {
            let tool = self.tools.remove(position);
            cells.push(HistoryCell::ToolCall {
                tool_name: tool.tool_name,
                arguments: tool.arguments,
                approval: tool.approval,
                // Finished: the ToolResult line right under it says how it
                // ended; a stale clock would just be noise.
                running_for_secs: None,
            });
        }
        cells.append(&mut self.diagnostics);
        cells
    }

    #[must_use]
    pub fn preview_cells(&self) -> Vec<HistoryCell> {
        let mut cells = Vec::new();
        if !self.reasoning_buffer.is_empty() {
            cells.push(HistoryCell::Reasoning {
                text: self.reasoning_buffer.clone(),
            });
        }
        if !self.assistant_buffer.is_empty() {
            cells.push(HistoryCell::Assistant {
                text: self.assistant_buffer.clone(),
            });
        }
        for tool in &self.tools {
            cells.push(HistoryCell::ToolCall {
                tool_name: tool.tool_name.clone(),
                arguments: tool.arguments.clone(),
                approval: tool.approval,
                // Recomputed on every render tick, so the line reads
                // "agent … · 47s" and visibly counts while the call runs.
                running_for_secs: Some(tool.started_at.elapsed().as_secs()),
            });
            if !tool.live_output.is_empty() {
                cells.push(HistoryCell::ToolStream {
                    text: tool.live_output.preview_tail(),
                });
            }
        }
        cells.extend(self.diagnostics.iter().cloned());
        // The pending approval is shown by the dedicated panel (with the y/a/n
        // choices); don't also duplicate it inline in the transcript preview.
        cells
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_cells_exposes_streaming_reasoning_assistant_and_tool() {
        let mut turn = ActiveTurn::default();
        turn.push_reasoning_delta("thinking");
        turn.push_assistant_delta("answer");
        turn.upsert_tool(ActiveToolCell {
            tool_call_id: ToolCallId("call_1".to_string()),
            tool_name: "mock_echo".to_string(),
            arguments: "{\"message\":\"hi\"}".to_string(),
            approval: ToolApprovalState::NotRequired,
            live_output: LiveOutput::default(),
            started_at: std::time::Instant::now(),
        });

        let cells = turn.preview_cells();
        assert!(matches!(cells[0], HistoryCell::Reasoning { .. }));
        assert!(matches!(cells[1], HistoryCell::Assistant { .. }));
        assert!(matches!(
            &cells[2],
            HistoryCell::ToolCall { tool_name, .. } if tool_name == "mock_echo"
        ));
    }

    #[test]
    fn streamed_tool_output_previews_tail_and_never_reaches_history() {
        let mut turn = ActiveTurn::default();
        let id = ToolCallId("call_1".to_string());
        turn.upsert_tool(ActiveToolCell {
            tool_call_id: id.clone(),
            tool_name: "shell".to_string(),
            arguments: "{\"command\":\"cargo build\"}".to_string(),
            approval: ToolApprovalState::NotRequired,
            live_output: LiveOutput::default(),
            started_at: std::time::Instant::now(),
        });

        for line in 0..10 {
            turn.append_tool_output(&id, &format!("line-{line}\n"));
        }

        let cells = turn.preview_cells();
        let Some(HistoryCell::ToolStream { text }) = cells
            .iter()
            .find(|cell| matches!(cell, HistoryCell::ToolStream { .. }))
        else {
            panic!("expected a live-output preview cell");
        };
        // Only the trailing lines survive the preview cap.
        assert!(text.contains("line-9"));
        assert!(!text.contains("line-0"));

        // A duplicate upsert must not wipe streamed output.
        turn.upsert_tool(ActiveToolCell {
            tool_call_id: id.clone(),
            tool_name: "shell".to_string(),
            arguments: "{\"command\":\"cargo build\"}".to_string(),
            approval: ToolApprovalState::NotRequired,
            live_output: LiveOutput::default(),
            started_at: std::time::Instant::now(),
        });
        assert!(!turn.tools[0].live_output.is_empty());

        // The finished-tool flush drops live output: the final ToolResult
        // summary replaces it in history.
        let flushed = turn.take_finished_tool_cells(&id);
        assert!(
            flushed
                .iter()
                .all(|cell| !matches!(cell, HistoryCell::ToolStream { .. }))
        );
    }

    #[test]
    fn live_output_buffer_keeps_bounded_tail() {
        let mut output = LiveOutput::default();
        output.push(&"a".repeat(5_000));
        output.push("tail-marker");
        let preview = output.preview_tail();
        assert!(preview.contains("tail-marker"));
        assert!(preview.chars().count() <= 4_096);
    }
}
