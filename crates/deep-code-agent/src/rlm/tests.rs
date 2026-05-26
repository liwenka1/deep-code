use std::sync::{Arc, RwLock};

use serde_json::json;
use tempfile::TempDir;

use crate::handle::{HANDLE_READ_TOOL, HandleStore, register_handle_read};
use crate::rlm::{RlmManager, RlmServices, register_rlm_tools};
use crate::tool::{ApprovalDecision, ToolCall, ToolRegistry};

#[test]
fn rlm_open_eval_and_handle_overflow() {
    let store = Arc::new(RwLock::new(HandleStore::new()));
    let mut manager = RlmManager::new(Arc::clone(&store));
    let body = (0..2000)
        .map(|index| format!("line-{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    manager.open("paper".to_string(), body, "inline").unwrap();
    manager
        .configure("paper", Some(128), None)
        .expect("configure");
    let output = manager
        .eval("paper", "head 100")
        .expect("eval should succeed");
    assert!(output.stored_handle);
    assert!(output.handle_id.is_some());
}

#[test]
fn rlm_tools_roundtrip_with_handle_read() {
    let dir = TempDir::new().unwrap();
    let file_path = dir.path().join("sample.txt");
    std::fs::write(&file_path, "alpha\nbeta\ngamma\n").unwrap();

    let store = Arc::new(RwLock::new(HandleStore::new()));
    let services = Arc::new(RlmServices::new(
        Arc::clone(&store),
        dir.path().to_path_buf(),
    ));
    let mut registry = ToolRegistry::new();
    register_rlm_tools(&mut registry, Arc::clone(&services));
    register_handle_read(&mut registry, store);

    let open = ToolCall::new(
        "c1",
        "rlm_open",
        json!({"name": "ctx", "file_path": "sample.txt"}),
    );
    let open_result = registry
        .run_tool_call(open, None)
        .expect("open")
        .result()
        .expect("open result");
    assert!(open_result.content.contains("ctx"));

    let eval = ToolCall::new(
        "c2",
        "rlm_eval",
        json!({"name": "ctx", "code": "grep alpha"}),
    );
    let eval_outcome = registry.run_tool_call(eval, None).expect("eval plan");
    assert!(eval_outcome.approval_required());
    let eval_result = registry
        .run_tool_call(
            ToolCall::new(
                "c2",
                "rlm_eval",
                json!({"name": "ctx", "code": "grep alpha"}),
            ),
            Some(ApprovalDecision::Approved),
        )
        .expect("eval")
        .result()
        .expect("eval result");
    assert!(eval_result.content.contains("alpha"));

    let close = ToolCall::new("c3", "rlm_close", json!({"name": "ctx"}));
    registry
        .run_tool_call(close, None)
        .expect("close")
        .result()
        .expect("close result");
}

#[test]
fn rlm_overflow_then_handle_read() {
    let dir = TempDir::new().unwrap();
    let body = (0..500)
        .map(|index| format!("event-{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(dir.path().join("log.txt"), &body).unwrap();

    let store = Arc::new(RwLock::new(HandleStore::new()));
    let services = Arc::new(RlmServices::new(
        Arc::clone(&store),
        dir.path().to_path_buf(),
    ));
    let mut registry = ToolRegistry::new();
    register_rlm_tools(&mut registry, Arc::clone(&services));
    register_handle_read(&mut registry, store);

    registry
        .run_tool_call(
            ToolCall::new(
                "open",
                "rlm_open",
                json!({"name": "logs", "file_path": "log.txt"}),
            ),
            None,
        )
        .expect("open")
        .result()
        .expect("open result");

    registry
        .run_tool_call(
            ToolCall::new(
                "cfg",
                "rlm_configure",
                json!({"name": "logs", "max_inline_chars": 64}),
            ),
            None,
        )
        .expect("configure")
        .result()
        .expect("configure result");

    let eval_result = registry
        .run_tool_call(
            ToolCall::new(
                "eval",
                "rlm_eval",
                json!({"name": "logs", "code": "head 50"}),
            ),
            Some(ApprovalDecision::Approved),
        )
        .expect("eval")
        .result()
        .expect("eval result");
    assert!(eval_result.content.contains("\"stored\":true"));

    let handle_id = extract_handle_id(&eval_result.content).expect("handle id");
    let read_result = registry
        .run_tool_call(
            ToolCall::new(
                "read",
                HANDLE_READ_TOOL,
                json!({"handle": handle_id, "mode": "head", "lines": 3}),
            ),
            None,
        )
        .expect("handle_read")
        .result()
        .expect("handle_read result");
    assert!(read_result.content.contains("event-0"));
    assert!(read_result.content.contains("event-1"));
}

fn extract_handle_id(payload: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    value
        .get("handle")
        .and_then(|handle| handle.get("id").or_else(|| handle.get("name")))
        .and_then(|id| id.as_str())
        .map(str::to_string)
        .or_else(|| {
            value
                .get("handle_id")
                .and_then(|id| id.as_str())
                .map(str::to_string)
        })
}

trait ToolOutcomeExt {
    fn result(self) -> Option<crate::tool::ToolResult>;
    fn approval_required(&self) -> bool;
}

impl ToolOutcomeExt for crate::tool::ToolRunOutcome {
    fn result(self) -> Option<crate::tool::ToolResult> {
        match self {
            crate::tool::ToolRunOutcome::Result { result } => Some(result),
            crate::tool::ToolRunOutcome::ApprovalRequired { .. } => None,
        }
    }

    fn approval_required(&self) -> bool {
        matches!(self, crate::tool::ToolRunOutcome::ApprovalRequired { .. })
    }
}
