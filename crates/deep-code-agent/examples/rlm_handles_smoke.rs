//! Offline smoke for handle_read and RLM sessions.
//!
//! Run with:
//!   cargo run -p deep-code-agent --example rlm_handles_smoke

use std::sync::{Arc, RwLock};

use deep_code_agent::{
    ApprovalDecision, HandleStore, RlmServices, ToolCall, ToolRegistry, register_handle_read,
    register_rlm_tools,
};
use serde_json::json;
use tempfile::TempDir;

fn main() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let sample = dir.path().join("log.txt");
    std::fs::write(
        &sample,
        (0..400)
            .map(|index| format!("event-{index}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )?;

    let store = Arc::new(RwLock::new(HandleStore::new()));
    let services = Arc::new(RlmServices::new(Arc::clone(&store), dir.path().to_path_buf()));
    let mut registry = ToolRegistry::new();
    register_rlm_tools(&mut registry, Arc::clone(&services));
    register_handle_read(&mut registry, store);

    let open = ToolCall::new(
        "open",
        "rlm_open",
        json!({"name": "logs", "file_path": "log.txt"}),
    );
    let open_result = run(&registry, open, None)?;
    println!("rlm_open: {open_result}");

    let eval = ToolCall::new(
        "eval",
        "rlm_eval",
        json!({"name": "logs", "code": "stats\nhead 5"}),
    );
    let eval_result = run(&registry, eval, Some(ApprovalDecision::Approved))?;
    println!("rlm_eval: {eval_result}");

    let handle_id = extract_handle_id(&eval_result);
    if let Some(handle_id) = handle_id {
        let read = ToolCall::new(
            "read",
            "handle_read",
            json!({"handle": handle_id, "mode": "head", "lines": 3}),
        );
        let read_result = run(&registry, read, None)?;
        println!("handle_read: {read_result}");
    }

    let close = ToolCall::new("close", "rlm_close", json!({"name": "logs"}));
    let close_result = run(&registry, close, None)?;
    println!("rlm_close: {close_result}");

    Ok(())
}

fn run(
    registry: &ToolRegistry,
    call: ToolCall,
    decision: Option<ApprovalDecision>,
) -> anyhow::Result<String> {
    match registry.run_tool_call(call, decision)? {
        deep_code_agent::ToolRunOutcome::ApprovalRequired { request } => {
            anyhow::bail!("unexpected approval for {}", request.tool_name)
        }
        deep_code_agent::ToolRunOutcome::Result { result } => Ok(result.content),
    }
}

fn extract_handle_id(payload: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    value
        .get("handle")
        .and_then(|handle| handle.get("id").or_else(|| handle.get("name")))
        .and_then(|id| id.as_str())
        .map(str::to_string)
        .or_else(|| value.get("handle_id").and_then(|id| id.as_str()).map(str::to_string))
}
