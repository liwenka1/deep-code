use deep_code_agent::{ApprovalRequest, ToolCallId, TurnId};

use crate::history::{HistoryCell, ToolApprovalState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveToolCell {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub arguments: String,
    pub risk_level: Option<String>,
    pub requires_sandbox: Option<bool>,
    pub approval: ToolApprovalState,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveTurn {
    pub turn_id: TurnId,
    pub assistant_buffer: String,
    pub reasoning_buffer: String,
    pub saw_structured_assistant_delta: bool,
    pub saw_structured_reasoning_delta: bool,
    pub tools: Vec<ActiveToolCell>,
    pub diagnostics: Vec<HistoryCell>,
    pub pending_approval: Option<ApprovalRequest>,
}

impl ActiveTurn {
    #[must_use]
    pub fn new(turn_id: TurnId) -> Self {
        Self {
            turn_id,
            assistant_buffer: String::new(),
            reasoning_buffer: String::new(),
            saw_structured_assistant_delta: false,
            saw_structured_reasoning_delta: false,
            tools: Vec::new(),
            diagnostics: Vec::new(),
            pending_approval: None,
        }
    }

    pub fn push_structured_assistant(&mut self, text: &str) {
        self.saw_structured_assistant_delta = true;
        self.assistant_buffer.push_str(text);
    }

    pub fn push_provider_assistant(&mut self, text: &str) {
        if !self.saw_structured_assistant_delta {
            self.assistant_buffer.push_str(text);
        }
    }

    pub fn push_structured_reasoning(&mut self, text: &str) {
        self.saw_structured_reasoning_delta = true;
        self.reasoning_buffer.push_str(text);
    }

    pub fn push_provider_reasoning(&mut self, text: &str) {
        if !self.saw_structured_reasoning_delta {
            self.reasoning_buffer.push_str(text);
        }
    }

    pub fn upsert_tool(&mut self, cell: ActiveToolCell) {
        if let Some(existing) = self
            .tools
            .iter_mut()
            .find(|tool| tool.tool_call_id == cell.tool_call_id)
        {
            *existing = cell;
        } else {
            self.tools.push(cell);
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
                risk_level: None,
                requires_sandbox: None,
                approval: ToolApprovalState::NotRequired,
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
            existing.risk_level = Some(format!("{:?}", request.risk_level));
            existing.requires_sandbox = Some(request.requires_sandbox);
            existing.approval = ToolApprovalState::Required;
        } else {
            self.tools.push(ActiveToolCell {
                tool_call_id,
                tool_name: request.tool_name.clone(),
                arguments: request.arguments.to_string(),
                risk_level: Some(format!("{:?}", request.risk_level)),
                requires_sandbox: Some(request.requires_sandbox),
                approval: ToolApprovalState::Required,
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
                risk_level: tool.risk_level,
                requires_sandbox: tool.requires_sandbox,
                approval: tool.approval,
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
        cells.extend(self.tools.iter().map(|tool| HistoryCell::ToolCall {
            tool_name: tool.tool_name.clone(),
            arguments: tool.arguments.clone(),
            risk_level: tool.risk_level.clone(),
            requires_sandbox: tool.requires_sandbox,
            approval: tool.approval,
        }));
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
        let mut turn = ActiveTurn::new(TurnId("turn_1".to_string()));
        turn.push_structured_reasoning("thinking");
        turn.push_structured_assistant("answer");
        turn.upsert_tool(ActiveToolCell {
            tool_call_id: ToolCallId("call_1".to_string()),
            tool_name: "mock_echo".to_string(),
            arguments: "{\"message\":\"hi\"}".to_string(),
            risk_level: None,
            requires_sandbox: None,
            approval: ToolApprovalState::NotRequired,
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
    fn provider_deltas_are_compatibility_fallback_only() {
        let mut turn = ActiveTurn::new(TurnId("turn_1".to_string()));
        turn.push_structured_assistant("hello");
        turn.push_provider_assistant("hello");
        turn.push_provider_reasoning("legacy thinking");

        let cells = turn.preview_cells();
        assert!(matches!(
            &cells[0],
            HistoryCell::Reasoning { text } if text == "legacy thinking"
        ));
        assert!(matches!(
            &cells[1],
            HistoryCell::Assistant { text } if text == "hello"
        ));
    }
}
