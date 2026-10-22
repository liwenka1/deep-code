//! `todo_write`: the model's own task list for multi-step work.
//!
//! Stateless on purpose. Every call carries the complete list and replaces
//! the previous one, so there is nothing to reconcile, nothing to persist and
//! nothing a resumed session has to rebuild: the newest call is the list. The
//! model-facing result is a one-line tally (it just sent the list, and
//! echoing it back would spend context on what it already holds); the full
//! list rides in `ToolOutput::details` for the UI, which draws it as a
//! checklist. No gate: the tool touches nothing outside the conversation.
//!
//! Why it exists at all: a long turn is otherwise a stream of tool calls with
//! no statement of intent, and the user cannot tell "step 3 of 7" from
//! "wandering". The list is that statement, kept current by the model.

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::tool::{Tool, ToolCx, ToolError, ToolOutput};

/// Items per list. A plan longer than this is a plan that should be split.
const MAX_ITEMS: usize = 50;
/// Characters per item: a line, not a paragraph.
const MAX_ITEM_CHARS: usize = 200;

#[derive(Debug, Clone, Copy)]
pub struct TodoWriteTool;

impl TodoWriteTool {
    pub const NAME: &'static str = "todo_write";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoItem {
    /// What has to happen, as one line.
    content: String,
    /// pending | in_progress | done
    status: TodoStatus,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoWriteParams {
    /// The complete task list, in order. Replaces the previous list.
    items: Vec<TodoItem>,
}

#[async_trait]
impl Tool for TodoWriteTool {
    type Params = TodoWriteParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Keep the task list for multi-step work (three or more steps). Send the COMPLETE list every \
         time — it replaces the previous one — with each item's status: pending, in_progress or \
         done. Mark a step in_progress when you start it and done when it is finished, so the user \
         can follow progress; one line per item, at most 50 items."
    }

    async fn run(&self, params: TodoWriteParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let invalid = |message: String| ToolError::InvalidArguments {
            name: Self::NAME.to_string(),
            message,
        };
        if params.items.len() > MAX_ITEMS {
            return Err(invalid(format!(
                "at most {MAX_ITEMS} items (got {})",
                params.items.len()
            )));
        }
        let mut items = Vec::with_capacity(params.items.len());
        for (index, item) in params.items.into_iter().enumerate() {
            let content = item.content.trim();
            if content.is_empty() {
                return Err(invalid(format!("item {} has empty content", index + 1)));
            }
            if content.chars().count() > MAX_ITEM_CHARS {
                return Err(invalid(format!(
                    "item {} is longer than {MAX_ITEM_CHARS} characters",
                    index + 1
                )));
            }
            items.push(TodoItem {
                content: content.to_string(),
                status: item.status,
            });
        }
        let count = |status: TodoStatus| items.iter().filter(|item| item.status == status).count();
        let (done, in_progress, pending) = (
            count(TodoStatus::Done),
            count(TodoStatus::InProgress),
            count(TodoStatus::Pending),
        );
        let content = format!(
            "todo: {done}/{} done, {in_progress} in progress, {pending} pending",
            items.len()
        );
        Ok(ToolOutput::text(content).with_details(json!({ "todos": items })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    use crate::tool::{ErasedTool, ToolCall, ToolResultStatus};

    async fn run(arguments: Value) -> Result<crate::tool::ToolResult, ToolError> {
        TodoWriteTool
            .execute(
                &ToolCall::new("call_1", TodoWriteTool::NAME, arguments),
                &ToolCx::default(),
            )
            .await
    }

    /// The model gets a tally; the UI gets the trimmed list, in order, with
    /// the statuses it sent.
    #[tokio::test]
    async fn the_tally_goes_to_the_model_and_the_list_to_the_ui() {
        let result = run(json!({ "items": [
            { "content": "  read the parser ", "status": "done" },
            { "content": "write the fix", "status": "in_progress" },
            { "content": "run the tests", "status": "pending" },
            { "content": "update docs", "status": "pending" }
        ] }))
        .await
        .unwrap();
        assert_eq!(result.status, ToolResultStatus::Success);
        assert_eq!(result.content, "todo: 1/4 done, 1 in progress, 2 pending");
        let todos = result.details.unwrap()["todos"].clone();
        assert_eq!(todos[0]["content"], "read the parser", "trimmed");
        assert_eq!(todos[0]["status"], "done");
        assert_eq!(todos[1]["status"], "in_progress");
        assert_eq!(todos.as_array().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn bad_lists_are_argument_errors() {
        for (arguments, expect) in [
            (
                json!({ "items": [{ "content": "   ", "status": "pending" }] }),
                "empty",
            ),
            (
                json!({ "items": [{ "content": "x".repeat(201), "status": "pending" }] }),
                "200",
            ),
            (
                json!({ "items": (0..51).map(|i| json!({ "content": format!("s{i}"), "status": "pending" })).collect::<Vec<_>>() }),
                "50",
            ),
            (
                json!({ "items": [{ "content": "x", "status": "blocked" }] }),
                "blocked",
            ),
            (
                json!({ "items": [{ "content": "x", "status": "done", "extra": 1 }] }),
                "extra",
            ),
        ] {
            let error = run(arguments).await.unwrap_err();
            assert!(
                matches!(error, ToolError::InvalidArguments { .. }),
                "{error}"
            );
            assert!(error.to_string().contains(expect), "{error}");
        }
        let empty = run(json!({ "items": [] })).await.unwrap();
        assert_eq!(empty.content, "todo: 0/0 done, 0 in progress, 0 pending");
    }

    #[test]
    fn the_tool_is_ungated() {
        use crate::execution_policy::{ExecPolicy, PolicyVerdict, ToolKind};
        assert_eq!(
            ExecPolicy::classify_tool(TodoWriteTool::NAME),
            ToolKind::ReadOnlyFile
        );
        let plan = ExecPolicy::new().evaluate_tool(TodoWriteTool::NAME, &json!({}));
        assert!(matches!(plan.verdict, PolicyVerdict::Allow));
        assert!(!plan.requires_approval);
    }
}
