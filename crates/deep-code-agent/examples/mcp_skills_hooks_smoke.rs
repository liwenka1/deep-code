//! Smoke example for MCP, skills, and hooks (offline, no network).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use deep_code_agent::{
    ApprovalDecision, HookDispatcher, HookEvent, HookSink, HooksConfig, InMemoryMcpClient,
    McpManager, McpServerConfig, McpTransport, RuntimeBootstrap, SkillRegistry, ToolCall,
    ToolRegistry, ToolRunOutcome, attach_runtime_tools, build_system_prompt, qualify_tool_name,
};
use serde_json::json;
use tempfile::TempDir;

#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<serde_json::Value>>,
}

impl HookSink for RecordingSink {
    fn emit(&self, event: &HookEvent) {
        self.events.lock().unwrap().push(event.to_json());
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let workspace = TempDir::new()?;
    let skills_dir = workspace.path().join("skills").join("demo-skill");
    std::fs::create_dir_all(&skills_dir)?;
    std::fs::write(
        skills_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: Demo skill for smoke test\n---\nFollow the demo workflow.\n",
    )?;

    let prompt = build_system_prompt("You are deep-code.", workspace.path());
    anyhow::ensure!(
        prompt.contains("demo-skill"),
        "skills should enter system prompt"
    );

    let client = Arc::new(
        InMemoryMcpClient::new("mock")
            .with_tool("echo", Some("echo"), json!({"message": "hello"}))
            .with_resource("file://demo", Some("demo resource"))
            .with_prompt("greet", Some("greeting"), json!([])),
    );
    let mut manager = McpManager::new();
    manager.register_mock_client(
        McpServerConfig {
            name: "mock".to_string(),
            transport: McpTransport::Stdio,
            command: Some("mock".to_string()),
            args: Vec::new(),
            url: None,
            env: HashMap::new(),
            enabled: true,
        },
        client,
    )?;
    anyhow::ensure!(manager.list_tools().len() == 1);
    anyhow::ensure!(manager.list_resources().len() == 1);
    anyhow::ensure!(manager.list_prompts().len() == 1);

    let hooks_path = workspace.path().join("hooks.jsonl");
    let recording = Arc::new(RecordingSink::default());
    let mut hooks = HookDispatcher::from_config(&HooksConfig {
        stdout: false,
        jsonl: Some(hooks_path.clone()),
    });
    hooks.add_sink(recording.clone());

    let bootstrap = RuntimeBootstrap {
        hooks: Arc::new(hooks),
        mcp: Arc::new(RwLock::new(manager)),
    };

    let mut registry = ToolRegistry::new();
    attach_runtime_tools(&mut registry, &bootstrap);

    let qualified = qualify_tool_name("mock", "echo");
    let call = ToolCall::new("call_1", qualified, json!({}));
    let outcome = registry
        .run_tool_call(call, Some(ApprovalDecision::Approved))
        .await?;
    let ToolRunOutcome::Result { result } = outcome else {
        anyhow::bail!("expected tool result");
    };
    anyhow::ensure!(result.content.contains("hello"));

    let events = recording.events.lock().unwrap();
    anyhow::ensure!(events.len() == 2, "pre/post hooks should fire");
    anyhow::ensure!(events[0]["type"] == "tool_pre");
    anyhow::ensure!(events[1]["type"] == "tool_post");

    let jsonl = std::fs::read_to_string(hooks_path)?;
    anyhow::ensure!(jsonl.contains("tool_post"));

    let registry = SkillRegistry::discover(&workspace.path().join("skills"));
    anyhow::ensure!(registry.len() == 1);

    println!("mcp/skills/hooks smoke passed");
    Ok(())
}
