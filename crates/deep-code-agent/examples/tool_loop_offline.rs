//! Offline smoke for the agent runtime + tool loop.
//!
//! Drives an `AgentRuntime` against a tiny scripted in-process client that
//! emits a `mock_echo` tool call, then a final text response after the tool
//! result is fed back. No network or API key needed.
//!
//! Run with:
//!   cargo run -p deep-code-agent --example tool_loop_offline

use std::pin::Pin;
use std::sync::Mutex;

use async_stream::try_stream;
use deep_code_agent::{
    AgentConfig, AgentEvent, AgentEventStream, AgentResult, AgentRuntime, ApprovalDecision,
    ChatRequest, FunctionCallDelta, LlmClient, MockEchoTool, RuntimeEvent, ToolCallDelta,
    ToolRegistry,
};
use futures_core::Stream;

struct ScriptedClient {
    scripts: Mutex<Vec<Vec<AgentEvent>>>,
}

impl ScriptedClient {
    fn new(scripts: Vec<Vec<AgentEvent>>) -> Self {
        Self {
            scripts: Mutex::new(scripts),
        }
    }
}

impl LlmClient for ScriptedClient {
    fn provider_name(&self) -> &'static str {
        "scripted"
    }

    fn model(&self) -> &str {
        "scripted-offline"
    }

    async fn stream_chat(&self, _request: ChatRequest) -> AgentResult<AgentEventStream> {
        let events = {
            let mut scripts = self.scripts.lock().unwrap();
            if scripts.is_empty() {
                Vec::new()
            } else {
                scripts.remove(0)
            }
        };
        let stream = try_stream! {
            for event in events {
                yield event;
            }
        };
        let stream: Pin<Box<dyn Stream<Item = AgentResult<AgentEvent>> + Send>> = Box::pin(stream);
        Ok(stream)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: ToolCallDelta {
                    index: Some(0),
                    id: Some("call_1".to_string()),
                    call_type: Some("function".to_string()),
                    function: Some(FunctionCallDelta {
                        name: Some(MockEchoTool::NAME.to_string()),
                        arguments: Some(r#"{"message":"hi from offline"}"#.to_string()),
                    }),
                },
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "All done.".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);

    let runtime = AgentRuntime::with_system_prompt(
        client,
        ToolRegistry::with_mock_tools(),
        "You are a smoke-test assistant.",
        AgentConfig::default(),
        false,
    );

    let mut events = runtime.submit_user("please run the mock tool").await;
    println!("--- turn 1 (until approval) ---");
    while let Some(event) = events.recv().await {
        print_event(&event);
        if matches!(
            event,
            RuntimeEvent::ApprovalRequired { .. }
                | RuntimeEvent::TurnFinished { .. }
                | RuntimeEvent::Error { .. }
        ) {
            break;
        }
    }

    println!("--- approving and resuming ---");
    let mut events = runtime.submit_approval(ApprovalDecision::Approved).await;
    while let Some(event) = events.recv().await {
        print_event(&event);
        if matches!(
            event,
            RuntimeEvent::TurnFinished { .. } | RuntimeEvent::Error { .. }
        ) {
            break;
        }
    }

    println!("--- final session ---");
    for (idx, message) in runtime.session_messages().await.iter().enumerate() {
        println!(
            "  [{idx}] role={:?} content={:?} tool_call_id={:?} tool_calls={}",
            message.role,
            message.content,
            message.tool_call_id,
            message.tool_calls.len()
        );
    }

    Ok(())
}

fn print_event(event: &RuntimeEvent) {
    match event {
        RuntimeEvent::Provider(AgentEvent::TextDelta { text }) => {
            println!("text: {text}");
        }
        RuntimeEvent::Provider(AgentEvent::ToolCallDelta { delta }) => {
            println!("tool-call-delta: {delta:?}");
        }
        RuntimeEvent::ApprovalRequired { request, .. } => {
            println!(
                "approval required: tool={} args={}",
                request.tool_name, request.arguments
            );
        }
        RuntimeEvent::ToolResult { result } => {
            println!(
                "tool result: status={:?} content={:?}",
                result.status, result.content
            );
        }
        RuntimeEvent::TurnFinished { usage, .. } => {
            println!("turn finished: usage={usage:?}");
        }
        RuntimeEvent::Error { message, .. } => {
            println!("error: {message}");
        }
        other => println!("event: {other:?}"),
    }
}
