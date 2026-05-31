use deep_code_agent::{ApprovalRequest, ToolCallId, TurnId};

use crate::history::HistoryCell;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveToolCell {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveTurn {
    pub turn_id: TurnId,
    pub assistant_buffer: String,
    pub reasoning_buffer: String,
    pub saw_structured_assistant_delta: bool,
    pub saw_structured_reasoning_delta: bool,
    pub tools: Vec<ActiveToolCell>,
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
            });
        }
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
        }));
        if let Some(request) = &self.pending_approval {
            cells.push(HistoryCell::Approval {
                tool_name: request.tool_name.clone(),
                description: request.description.clone(),
                risk_level: format!("{:?}", request.risk_level),
                requires_sandbox: request.requires_sandbox,
                matched_rule: request.matched_rule.clone(),
                arguments: request.arguments.to_string(),
            });
        }
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
