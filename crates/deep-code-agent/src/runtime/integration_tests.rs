use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_stream::try_stream;
use futures_core::Stream;

use super::*;
use crate::client::AgentEventStream;
use crate::error::{AgentError, AgentResult};
use crate::event::AgentEvent;
use crate::model::{ChatRequest, FunctionCallDelta, ToolCallDelta};
use crate::runtime::diagnostics::append_diagnostics;
use crate::session_store::SessionStore;
use crate::tool::{MockEchoTool, Tool, ToolError, ToolRegistry, ToolResultStatus};

#[test]
fn append_diagnostics_joins_blocks() {
    assert_eq!(
        append_diagnostics(
            "{\"path\":\"a.rs\"}",
            "<diagnostics file=\"a.rs\">\n</diagnostics>"
        ),
        "{\"path\":\"a.rs\"}\n\n<diagnostics file=\"a.rs\">\n</diagnostics>"
    );
}

/// Scripted client: replays a pre-recorded sequence of provider events for
/// each successive call to `stream_chat`.
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

#[async_trait::async_trait]
impl LlmClient for ScriptedClient {
    fn provider_name(&self) -> &'static str {
        "scripted"
    }

    fn model(&self) -> &str {
        "scripted-model"
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

fn tool_call_delta(id: &str, name: &str, arguments: &str) -> ToolCallDelta {
    indexed_tool_call_delta(0, id, name, arguments)
}

fn indexed_tool_call_delta(index: u32, id: &str, name: &str, arguments: &str) -> ToolCallDelta {
    ToolCallDelta {
        index: Some(index),
        id: Some(id.to_string()),
        call_type: Some("function".to_string()),
        function: Some(FunctionCallDelta {
            name: Some(name.to_string()),
            arguments: Some(arguments.to_string()),
        }),
    }
}

/// Auto-approved echo tool: borrows the exact whitelisted read-only name
/// `read_file` (the policy classifies by exact tool name), and the spec
/// itself does not require approval. The fake registry in these tests never
/// registers the real workspace tools, so the name cannot collide.
#[derive(Debug, Clone, Copy)]
struct AutoEchoTool;

impl AutoEchoTool {
    const NAME: &'static str = "read_file";
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AutoEchoParams {
    message: Option<String>,
}

#[async_trait::async_trait]
impl Tool for AutoEchoTool {
    type Params = AutoEchoParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Echoes a message without approval."
    }

    async fn run(
        &self,
        params: AutoEchoParams,
        _cx: &crate::tool::ToolCx,
    ) -> Result<crate::tool::ToolOutput, ToolError> {
        let message = params.message.unwrap_or_default();
        Ok(crate::tool::ToolOutput::text(format!(
            "read_file: {message}"
        )))
    }
}

fn registry_with_auto_and_mock() -> ToolRegistry {
    let mut registry = ToolRegistry::with_mock_tools();
    registry.register(AutoEchoTool);
    registry
}

/// A tool that always returns an error result, for exercising the cascade
/// struggle signal (repeated tool-call execution failures).
#[derive(Debug, Clone, Copy)]
struct FailingTool;

impl FailingTool {
    const NAME: &'static str = "fail_probe";
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FailingParams {}

#[async_trait::async_trait]
impl Tool for FailingTool {
    type Params = FailingParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Always fails."
    }

    async fn run(
        &self,
        _params: FailingParams,
        _cx: &crate::tool::ToolCx,
    ) -> Result<crate::tool::ToolOutput, ToolError> {
        Ok(crate::tool::ToolOutput::soft_error("boom"))
    }
}

/// A tool that reports out-of-band spend (the sub-agent shape). Borrows the
/// whitelisted read-only name so the call runs without approval (same trick
/// as `AutoEchoTool`, which is not registered alongside it).
#[derive(Debug, Clone, Copy)]
struct SpendReportingTool;

impl SpendReportingTool {
    const NAME: &'static str = "read_file";
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SpendParams {}

#[async_trait::async_trait]
impl Tool for SpendReportingTool {
    type Params = SpendParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Reports out-of-band spend."
    }

    async fn run(
        &self,
        _params: SpendParams,
        cx: &crate::tool::ToolCx,
    ) -> Result<crate::tool::ToolOutput, ToolError> {
        cx.report_spend(crate::tool::ToolSpend {
            cost: crate::pricing::CostEstimate { usd: 0.5, cny: 3.5 },
            cache_hit_tokens: 300,
            cache_miss_tokens: 100,
            cache_savings: crate::pricing::CostEstimate { usd: 0.2, cny: 1.4 },
        });
        Ok(crate::tool::ToolOutput::text("spent"))
    }
}

/// A tool that signals it has started and then parks on the cancel token (the
/// long-running foreground-command shape). Borrows the whitelisted read-only
/// name so the call runs without approval (same trick as `SpendReportingTool`).
#[derive(Debug)]
struct ParkUntilCancelledTool {
    started: Arc<tokio::sync::Notify>,
}

impl ParkUntilCancelledTool {
    const NAME: &'static str = "read_file";
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ParkParams {}

#[async_trait::async_trait]
impl Tool for ParkUntilCancelledTool {
    type Params = ParkParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Parks until cancelled."
    }

    async fn run(
        &self,
        _params: ParkParams,
        cx: &crate::tool::ToolCx,
    ) -> Result<crate::tool::ToolOutput, ToolError> {
        self.started.notify_one();
        cx.cancel_token().cancelled().await;
        Ok(crate::tool::ToolOutput::text("cancelled"))
    }
}

/// A stand-in registered under the real `shell` name so the execution policy
/// classifies and gates it exactly like shell — without spawning anything. It
/// echoes whether the sandbox grant carried network, so tests can assert the
/// end-to-end plumbing (declaration → gate → approval → sandbox policy).
#[derive(Debug, Clone, Copy)]
struct FakeShellTool;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FakeShellParams {
    command: String,
    #[allow(dead_code)] // consumed by the execution policy from raw arguments
    network: Option<bool>,
}

#[async_trait::async_trait]
impl Tool for FakeShellTool {
    type Params = FakeShellParams;

    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Fake shell."
    }

    async fn run(
        &self,
        params: FakeShellParams,
        cx: &crate::tool::ToolCx,
    ) -> Result<crate::tool::ToolOutput, ToolError> {
        Ok(crate::tool::ToolOutput::text(format!(
            "ran `{}` net={}",
            params.command,
            cx.sandbox_policy().has_network_access()
        )))
    }
}

fn started_ids(events: &[RuntimeEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ToolCallStarted { tool_call_id, .. } => {
                Some(tool_call_id.as_str().to_string())
            }
            _ => None,
        })
        .collect()
}

fn finished_ids(events: &[RuntimeEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ToolCallFinished { tool_call_id, .. } => {
                Some(tool_call_id.as_str().to_string())
            }
            _ => None,
        })
        .collect()
}

async fn drain(rx: &mut RuntimeEventReceiver) -> Vec<RuntimeEvent> {
    let mut out = Vec::new();
    while let Some(event) = rx.recv().await {
        out.push(event);
    }
    out
}

#[tokio::test]
async fn one_provider_delta_maps_to_exactly_one_runtime_event() {
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::TextDelta {
            text: "hel".to_string(),
        },
        AgentEvent::TextDelta {
            text: "lo".to_string(),
        },
        AgentEvent::ReasoningDelta {
            text: "think".to_string(),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let runtime = AgentRuntime::new(client, ToolRegistry::default());

    let mut rx = runtime.submit_user("hi").await;
    let events = drain(&mut rx).await;

    let assistant_deltas = events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::AssistantDelta { .. }))
        .count();
    let reasoning_deltas = events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::ReasoningDelta { .. }))
        .count();
    assert_eq!(assistant_deltas, 2, "no duplicate assistant delta events");
    assert_eq!(reasoning_deltas, 1, "no duplicate reasoning delta events");
}

#[tokio::test]
async fn approve_path_feeds_tool_result_into_next_turn() {
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "thanks".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools());

    let mut rx = runtime.submit_user("please echo").await;
    let first = drain(&mut rx).await;
    assert!(matches!(
        first.last(),
        Some(RuntimeEvent::ApprovalRequired { .. })
    ));

    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    let second = drain(&mut rx).await;

    let mut saw_tool_result = false;
    let mut saw_finish = false;
    for event in &second {
        match event {
            RuntimeEvent::ToolCallFinished { result, .. } => {
                assert_eq!(result.status, ToolResultStatus::Success);
                assert_eq!(result.content, "mock_echo: hi");
                saw_tool_result = true;
            }
            RuntimeEvent::TurnFinished { .. } => saw_finish = true,
            _ => {}
        }
    }
    assert!(saw_tool_result, "expected ToolResult event after approval");
    assert!(saw_finish, "expected TurnFinished after second turn");

    let messages = runtime.session_messages().await;
    // Expect: user, assistant(tool_calls), tool, assistant("thanks").
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1].tool_calls.len(), 1);
    assert_eq!(messages[1].tool_calls[0].id, "call_1");
    assert_eq!(messages[2].role, crate::message::Role::Tool);
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(messages[3].content, "thanks");
}

#[tokio::test]
async fn yolo_mode_auto_approves_gated_tool_without_parking() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools())
        .with_permission_mode(SharedPermissionMode::new(PermissionMode::Yolo));

    // A single drive: Yolo auto-approves the gated call and the turn runs to
    // completion without ever parking on approval.
    let mut rx = runtime.submit_user("please echo").await;
    let events = drain(&mut rx).await;

    assert!(
        events
            .iter()
            .all(|event| !matches!(event, RuntimeEvent::ApprovalRequired { .. })),
        "yolo must not park on approval"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallFinished { result, .. } if result.content == "mock_echo: hi"
    )));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnFinished { .. }))
    );
}

#[tokio::test]
async fn default_mode_still_parks_gated_tool() {
    // Same gated call, but Default mode (the runtime's default) parks it.
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::ToolCallDelta {
            delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools());
    let mut rx = runtime.submit_user("please echo").await;
    let events = drain(&mut rx).await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::ApprovalRequired { .. })
    ));
}

#[tokio::test]
async fn auto_mode_runs_when_classifier_approves() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};
    // Script serves one response per stream_chat call: (1) the turn emits the
    // gated call, (2) the classifier's judge call approves, (3) the turn
    // continues to completion.
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: r#"{"approve": true, "reason": "safe mock"}"#.to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools())
        .with_permission_mode(SharedPermissionMode::new(PermissionMode::Auto));

    let mut rx = runtime.submit_user("please echo").await;
    let events = drain(&mut rx).await;

    assert!(
        events
            .iter()
            .all(|event| !matches!(event, RuntimeEvent::ApprovalRequired { .. })),
        "classifier approved → no parking"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallFinished { result, .. } if result.content == "mock_echo: hi"
    )));
}

#[tokio::test]
async fn auto_mode_asks_when_classifier_denies() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};
    // (1) gated call, (2) judge denies → the batch parks on approval.
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: r#"{"approve": false, "reason": "unclear"}"#.to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools())
        .with_permission_mode(SharedPermissionMode::new(PermissionMode::Auto));

    let mut rx = runtime.submit_user("please echo").await;
    let events = drain(&mut rx).await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::ApprovalRequired { .. })),
        "classifier denied → ask the human"
    );
}

/// A mock network tool that borrows the exact name `fetch_url` (the policy
/// classifies by name). Registered so an Auto-mode call can dispatch; if the
/// egress floor holds, it parks and this body never runs.
#[derive(Debug, Clone, Copy)]
struct NetworkMockTool;

impl NetworkMockTool {
    const NAME: &'static str = "fetch_url";
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct NetworkMockParams {
    #[allow(dead_code)]
    url: Option<String>,
}

#[async_trait::async_trait]
impl Tool for NetworkMockTool {
    type Params = NetworkMockParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Mock network tool."
    }

    async fn run(
        &self,
        _params: NetworkMockParams,
        _cx: &crate::tool::ToolCx,
    ) -> Result<crate::tool::ToolOutput, ToolError> {
        Ok(crate::tool::ToolOutput::text("fetched"))
    }
}

/// The auto-mode egress floor covers the network-native tools (`fetch_url`/
/// `web_search`), not just calls carrying a declared `network: true`. The judge
/// is scripted to APPROVE, so the only way this parks is the floor firing
/// *before* the judge — the bug where `fetch_url` was decided by the classifier
/// in Auto mode and could exfiltrate over a GET query string.
#[tokio::test]
async fn auto_mode_floors_network_tools_above_the_judge() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta(
                    "call_1",
                    NetworkMockTool::NAME,
                    r#"{"url":"http://example.com/"}"#,
                ),
            },
            AgentEvent::Done { usage: None },
        ],
        // A judge verdict that WOULD approve — it must go unused because the
        // floor parks the call before any judge request is made.
        vec![
            AgentEvent::TextDelta {
                text: r#"{"approve": true, "reason": "looks fine"}"#.to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let mut registry = ToolRegistry::with_mock_tools();
    registry.register(NetworkMockTool);
    let runtime = AgentRuntime::new(client, registry)
        .with_permission_mode(SharedPermissionMode::new(PermissionMode::Auto));

    let mut rx = runtime.submit_user("fetch it").await;
    let events = drain(&mut rx).await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::ApprovalRequired { .. })),
        "a network-native tool must be floored to the human in Auto mode, even with an approving judge"
    );
    assert!(
        events.iter().all(|event| !matches!(
            event,
            RuntimeEvent::ToolCallFinished { result, .. } if result.content == "fetched"
        )),
        "the network tool must not have run"
    );
}

/// Mode monotonicity: Auto sits above AcceptEdits in the cycle, so it must
/// auto-approve everything AcceptEdits does. A bounded fs-edit (`mkdir sub`)
/// defaults to the High risk tier, which Auto's judge floor would otherwise
/// park on — but the AcceptEdits inheritance short-circuits before the judge,
/// so the call runs without ever emitting an approval prompt. The script has
/// NO judge response, proving the judge was never consulted.
#[tokio::test]
async fn auto_mode_inherits_accept_edits_fs_grant_without_asking() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};
    let workspace = tempfile::tempdir().unwrap();
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_1", "shell", r#"{"command": "mkdir sub"}"#),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let registry = crate::shell_tools::ShellTools::new(workspace.path())
        .unwrap()
        .with_sandbox(crate::sandbox::SandboxManager::new().force_sandbox(Some(false)))
        .into_registry();
    let runtime = AgentRuntime::with_new_session(
        client,
        registry,
        "system",
        workspace.path(),
        &crate::config::AgentConfig::builtin(),
    )
    .unwrap()
    .with_permission_mode(SharedPermissionMode::new(PermissionMode::Auto));

    let mut rx = runtime.submit_user("make a dir").await;
    let events = drain(&mut rx).await;
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, RuntimeEvent::ApprovalRequired { .. })),
        "Auto must inherit AcceptEdits's bounded fs-edit grant, not park on it"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ToolCallFinished { .. })),
        "the mkdir should have run to completion"
    );
    assert!(workspace.path().join("sub").is_dir(), "mkdir actually ran");
}

/// A parallel-safe tool (name `agent` → `ToolKind::SubAgent`) that blocks on a
/// shared size-2 barrier: `run` only returns once two calls are in flight at
/// once. Sequential batch execution deadlocks on it; concurrent execution
/// passes.
#[derive(Clone)]
struct BarrierAgentTool {
    barrier: Arc<tokio::sync::Barrier>,
    ran: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BarrierParams {}

#[async_trait::async_trait]
impl Tool for BarrierAgentTool {
    type Params = BarrierParams;

    fn name(&self) -> &str {
        "agent"
    }

    fn description(&self) -> &str {
        "Barrier-synchronized stand-in for the agent tool."
    }

    async fn run(
        &self,
        _params: BarrierParams,
        _cx: &crate::tool::ToolCx,
    ) -> Result<crate::tool::ToolOutput, ToolError> {
        self.ran.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.barrier.wait().await;
        Ok(crate::tool::ToolOutput::text("done"))
    }
}

#[tokio::test]
async fn batch_runs_parallel_safe_agent_calls_concurrently() {
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: indexed_tool_call_delta(0, "call_1", "agent", "{}"),
            },
            AgentEvent::ToolCallDelta {
                delta: indexed_tool_call_delta(1, "call_2", "agent", "{}"),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "synthesized".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(BarrierAgentTool {
        barrier: Arc::new(tokio::sync::Barrier::new(2)),
        ran: Arc::clone(&ran),
    });
    let runtime = AgentRuntime::new(client, registry);

    let mut rx = runtime.submit_user("fan out").await;
    // A serial batch would wedge here: the first call blocks on the size-2
    // barrier forever and the second never starts. Only concurrent execution
    // drains within the timeout.
    let events = tokio::time::timeout(std::time::Duration::from_secs(5), drain(&mut rx))
        .await
        .expect("same-batch agent calls must run concurrently, not serially");

    assert_eq!(
        ran.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "both agent calls must execute"
    );
    assert_eq!(
        finished_ids(&events),
        vec!["call_1".to_string(), "call_2".to_string()],
        "results are recorded in issue order regardless of completion order"
    );
}

#[tokio::test]
async fn deny_path_records_denied_tool_message_and_continues() {
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "ok".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools());

    let mut rx = runtime.submit_user("please echo").await;
    drain(&mut rx).await;

    let mut rx = runtime.submit_approval(ApprovalDecision::Denied).await;
    let events = drain(&mut rx).await;

    let denied = events.iter().find_map(|event| match event {
        RuntimeEvent::ToolCallFinished { result, .. } => Some(result),
        _ => None,
    });
    let denied = denied.expect("expected ToolResult on deny path");
    assert_eq!(denied.status, ToolResultStatus::Denied);

    let messages = runtime.session_messages().await;
    assert!(
        messages
            .iter()
            .any(|m| matches!(m.role, crate::message::Role::Tool) && m.content.contains("denied"))
    );
}

/// An unattended denial (the sub-agent auto-decider) carries its real reason,
/// which must replace the stock "denied by user" text end to end: no user saw
/// the prompt, and a child told that a human refused stops asking for things
/// the parent could re-dispatch.
#[tokio::test]
async fn unattended_denial_note_replaces_the_denied_by_user_text() {
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "ok".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools());

    let mut rx = runtime.submit_user("please echo").await;
    drain(&mut rx).await;

    let note = "Denied by sub-agent policy (no user saw this request): test note.";
    let mut rx = runtime
        .submit_approval_with_denial_note(ApprovalDecision::Denied, Some(note.to_string()))
        .await;
    let events = drain(&mut rx).await;

    let denied = events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ToolCallFinished { result, .. } => Some(result),
            _ => None,
        })
        .expect("expected ToolResult on deny path");
    assert_eq!(denied.status, ToolResultStatus::Denied);
    assert_eq!(
        denied.content, note,
        "the note must replace the stock text verbatim"
    );

    let messages = runtime.session_messages().await;
    assert!(
        messages
            .iter()
            .any(|m| matches!(m.role, crate::message::Role::Tool) && m.content == note),
        "the recorded tool message must carry the note, not 'denied by user'"
    );
}

#[tokio::test]
async fn plain_response_yields_assistant_message_and_finish() {
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::TextDelta {
            text: "hello".to_string(),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let runtime = AgentRuntime::new(client, ToolRegistry::default());

    let mut rx = runtime.submit_user("hi").await;
    let events = drain(&mut rx).await;

    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::TurnFinished { .. })
    ));
    let messages = runtime.session_messages().await;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].content, "hello");
    assert!(messages[1].tool_calls.is_empty());
}

#[tokio::test]
async fn plain_response_emits_structured_lifecycle_events() {
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::ReasoningDelta {
            text: "thinking".to_string(),
        },
        AgentEvent::TextDelta {
            text: "hello".to_string(),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let runtime = AgentRuntime::new(client, ToolRegistry::default());

    let mut rx = runtime.submit_user("hi").await;
    let events = drain(&mut rx).await;

    let turn_id = events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::TurnStarted { turn_id, prompt } => {
                assert_eq!(prompt, "hi");
                Some(turn_id.clone())
            }
            _ => None,
        })
        .expect("turn started event");
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::SessionUpdated {
            message_count: 1,
            current_turn_id: Some(id),
            ..
        } if id == &turn_id
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ReasoningDelta { turn_id: id, text }
            if id == &turn_id && text == "thinking"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::AssistantDelta { turn_id: id, text }
            if id == &turn_id && text == "hello"
    )));
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::TurnFinished { turn_id: id, .. }) if id == &turn_id
    ));
}

#[tokio::test]
async fn tool_turn_emits_structured_tool_and_approval_events() {
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools());

    let mut rx = runtime.submit_user("please echo").await;
    let first = drain(&mut rx).await;
    let turn_id = first
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::TurnStarted { turn_id, .. } => Some(turn_id.clone()),
            _ => None,
        })
        .expect("turn started");

    assert!(first.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallUpdated {
            turn_id: id,
            tool_call_id,
            arguments_delta: Some(delta),
        } if id == &turn_id && tool_call_id.as_str() == "call_1" && delta.contains("hi")
    )));
    assert!(first.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallStarted {
            turn_id: id,
            tool_call_id,
            tool_name,
            ..
        } if id == &turn_id && tool_call_id.as_str() == "call_1" && tool_name == MockEchoTool::NAME
    )));
    assert!(first.iter().any(|event| matches!(
        event,
        RuntimeEvent::ApprovalRequired {
            turn_id: Some(id),
            tool_call_id: Some(tool_id),
            ..
        } if id == &turn_id && tool_id.as_str() == "call_1"
    )));

    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    let second = drain(&mut rx).await;
    assert!(matches!(
        second.first(),
        Some(RuntimeEvent::ApprovalResolved {
            turn_id: Some(id),
            tool_call_id,
            decision: ApprovalDecision::Approved,
        }) if id == &turn_id && tool_call_id.as_str() == "call_1"
    ));
    assert!(second.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallFinished {
            turn_id: Some(id),
            tool_call_id,
            result,
        } if id == &turn_id
            && tool_call_id.as_str() == "call_1"
            && result.status == ToolResultStatus::Success
    )));
}

#[tokio::test]
async fn tool_call_updates_reuse_stable_id_for_delta_fragments_without_id() {
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::ToolCallDelta {
            delta: ToolCallDelta {
                index: Some(0),
                id: Some("call_1".to_string()),
                call_type: Some("function".to_string()),
                function: Some(FunctionCallDelta {
                    name: Some(MockEchoTool::NAME.to_string()),
                    arguments: Some(r#"{"message":"hel"#.to_string()),
                }),
            },
        },
        AgentEvent::ToolCallDelta {
            delta: ToolCallDelta {
                index: Some(0),
                id: None,
                call_type: None,
                function: Some(FunctionCallDelta {
                    name: None,
                    arguments: Some(r#"lo"}"#.to_string()),
                }),
            },
        },
        AgentEvent::Done { usage: None },
    ]]);
    let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools());

    let mut rx = runtime.submit_user("please echo").await;
    let events = drain(&mut rx).await;
    let updated_ids = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ToolCallUpdated { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(updated_ids, vec!["call_1", "call_1"]);
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::ApprovalRequired { .. })
    ));
}

#[tokio::test]
async fn invalid_tool_arguments_error_keeps_turn_id() {
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::ToolCallDelta {
            delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"unterminated""#),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools());

    let mut rx = runtime.submit_user("please echo").await;
    let events = drain(&mut rx).await;
    let turn_id = events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::TurnStarted { turn_id, .. } => Some(turn_id.clone()),
            _ => None,
        })
        .expect("turn started");

    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::Error {
            turn_id: Some(id),
            ..
        } if id == &turn_id
    )));
}

#[test]
fn runtime_event_serializes_structured_turn_started() {
    let event = RuntimeEvent::TurnStarted {
        turn_id: TurnId("turn_test".to_string()),
        prompt: "hello".to_string(),
    };
    let json = serde_json::to_value(event).unwrap();
    assert_eq!(json["type"], "turn_started");
    assert_eq!(json["turn_id"], "turn_test");
    assert_eq!(json["prompt"], "hello");
}

#[tokio::test]
async fn turn_snapshots_emit_checkpoint_events() {
    let workspace = tempfile::tempdir().unwrap();
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::TextDelta {
            text: "done".to_string(),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let runtime = AgentRuntime::new(client, ToolRegistry::default())
        .with_checkpoints(workspace.path(), &mut Vec::new());

    let mut rx = runtime.submit_user("hi").await;
    let events = drain(&mut rx).await;

    let checkpoints: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::CheckpointCreated { label, .. } => Some(label.as_str()),
            _ => None,
        })
        .collect();
    // Exactly one snapshot per turn: before_turn is what rewind/restore key
    // off; an end-of-turn copy would duplicate the next turn's before_turn.
    assert_eq!(checkpoints, vec!["before_turn"]);
}

#[tokio::test]
async fn persistent_runtime_records_checkpoint_metadata() {
    let workspace = tempfile::tempdir().unwrap();
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::TextDelta {
            text: "done".to_string(),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let runtime = AgentRuntime::with_new_session(
        client,
        ToolRegistry::default(),
        "system",
        workspace.path(),
        &crate::config::AgentConfig::builtin(),
    )
    .unwrap()
    .with_checkpoints(workspace.path(), &mut Vec::new());
    let session_id = runtime.session_id().await.expect("session id");

    let mut rx = runtime.submit_user("hi").await;
    drain(&mut rx).await;
    runtime.shutdown().await;

    let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let record = store.load(&session_id).unwrap();
    assert!(
        record
            .checkpoints
            .iter()
            .any(|checkpoint| checkpoint.label == "before_turn")
    );
    assert!(
        record
            .checkpoints
            .iter()
            .all(|checkpoint| checkpoint.label != "after_turn"),
        "end-of-turn snapshots were removed as redundant with the next before_turn"
    );
}

#[tokio::test]
async fn submit_approval_without_pending_emits_error() {
    let client = ScriptedClient::new(vec![]);
    let runtime = AgentRuntime::new(client, ToolRegistry::default());

    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    let events = drain(&mut rx).await;
    assert!(matches!(events.first(), Some(RuntimeEvent::Error { .. })));
}

#[test]
fn truncate_tool_output_keeps_head_and_tail() {
    use crate::runtime::tool_result::truncate_tool_output;

    let small = "short output";
    assert_eq!(truncate_tool_output(small), small, "small output unchanged");

    let big = "A".repeat(5_000) + &"B".repeat(20_000) + &"C".repeat(5_000);
    let out = truncate_tool_output(&big);
    assert!(out.chars().count() < big.chars().count());
    assert!(out.starts_with("AAAA"));
    assert!(out.ends_with("CCCC"));
    assert!(out.contains("truncated"));
    // Head + tail + marker, far below the original 30k.
    assert!(out.chars().count() < 9_000);
}

#[test]
fn session_allow_excludes_shell_class_tools() {
    use crate::runtime::approval_flow::session_allowable;
    assert!(session_allowable("mock_echo"));
    assert!(session_allowable("write_file"));
    assert!(
        session_allowable("web_search") && session_allowable("fetch_url"),
        "network tools are the prime session-allow use case"
    );
    assert!(!session_allowable("shell"), "shell risk is per-argument");
    assert!(
        !session_allowable("job"),
        "a cancel-time consent must not blanket-approve action=start"
    );
}

#[tokio::test]
async fn session_approval_skips_future_prompts_for_same_tool() {
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"one"}"#),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_2", MockEchoTool::NAME, r#"{"message":"two"}"#),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools());

    let mut rx = runtime.submit_user("echo twice").await;
    let first = drain(&mut rx).await;
    assert!(matches!(
        first.last(),
        Some(RuntimeEvent::ApprovalRequired { .. })
    ));

    let mut rx = runtime
        .submit_approval(ApprovalDecision::ApprovedForSession)
        .await;
    let second = drain(&mut rx).await;

    // The second gated call of the same tool runs without prompting again,
    // leaving an ApprovalResolved audit event instead.
    assert!(
        second
            .iter()
            .all(|event| !matches!(event, RuntimeEvent::ApprovalRequired { .. })),
        "session approval must suppress further prompts for the tool"
    );
    assert_eq!(finished_ids(&second), vec!["call_1", "call_2"]);
    assert!(second.iter().any(|event| matches!(
        event,
        RuntimeEvent::ApprovalResolved {
            tool_call_id,
            decision: ApprovalDecision::Approved,
            ..
        } if tool_call_id.as_str() == "call_2"
    )));
    assert!(matches!(
        second.last(),
        Some(RuntimeEvent::TurnFinished { .. })
    ));
    assert_eq!(
        runtime.session_messages().await.last().unwrap().content,
        "done"
    );
}

/// Auto mode must park a network declaration for the human WITHOUT consulting
/// the judge: the scripted judge slot (which would approve) stays unconsumed,
/// so the turn ends at ApprovalRequired carrying the network badge.
#[tokio::test]
async fn auto_mode_parks_network_declaration_without_judging() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta(
                    "call_1",
                    "shell",
                    r#"{"command":"git push origin main","network":true}"#,
                ),
            },
            AgentEvent::Done { usage: None },
        ],
        // Judge bait: consumed (and approving) only if the network floor leaks
        // through to the classifier — the test then fails on the last event.
        vec![
            AgentEvent::TextDelta {
                text: r#"{"approve": true, "reason": "leaked"}"#.to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let mut registry = ToolRegistry::default();
    registry.register(FakeShellTool);
    let runtime = AgentRuntime::new(client, registry)
        .with_permission_mode(SharedPermissionMode::new(PermissionMode::Auto));

    let mut rx = runtime.submit_user("push it").await;
    let events = drain(&mut rx).await;
    let Some(RuntimeEvent::ApprovalRequired { request, .. }) = events.last() else {
        panic!("network declaration must park for the human, got {events:?}");
    };
    assert!(request.network, "the approval must carry the network badge");
}

/// Yolo grants a network declaration like everything else, and the grant
/// really reaches the sandbox policy of the executed call.
#[tokio::test]
async fn yolo_mode_auto_approves_network_declaration_with_grant() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta(
                    "call_1",
                    "shell",
                    r#"{"command":"git push origin main","network":true}"#,
                ),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let mut registry = ToolRegistry::default();
    registry.register(FakeShellTool);
    let runtime = AgentRuntime::new(client, registry)
        .with_permission_mode(SharedPermissionMode::new(PermissionMode::Yolo));

    let mut rx = runtime.submit_user("push it").await;
    let events = drain(&mut rx).await;
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, RuntimeEvent::ApprovalRequired { .. })),
        "yolo must not park"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { result, .. }
                if result.content.contains("net=true")
        )),
        "the sandbox policy of the executed call must carry the grant"
    );
}

/// "Approve for session" on a network command remembers the command identity:
/// the next identical declaration runs without prompting again, grant intact —
/// this is the "converse once, then git push stops asking" UX.
#[tokio::test]
async fn session_approval_remembers_network_command_identity() {
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta(
                    "call_1",
                    "shell",
                    r#"{"command":"git push origin main","network":true}"#,
                ),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta(
                    "call_2",
                    "shell",
                    r#"{"command":"git push origin main --tags","network":true}"#,
                ),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let mut registry = ToolRegistry::default();
    registry.register(FakeShellTool);
    let runtime = AgentRuntime::new(client, registry);

    let mut rx = runtime.submit_user("push twice").await;
    let first = drain(&mut rx).await;
    let Some(RuntimeEvent::ApprovalRequired { request, .. }) = first.last() else {
        panic!("first declaration must prompt");
    };
    assert!(request.network);

    let mut rx = runtime
        .submit_approval(ApprovalDecision::ApprovedForSession)
        .await;
    let second = drain(&mut rx).await;
    assert!(
        second
            .iter()
            .all(|event| !matches!(event, RuntimeEvent::ApprovalRequired { .. })),
        "the remembered identity must suppress the second prompt"
    );
    // Both runs executed with the grant (flags vary, identity `git push` matches).
    let granted = second
        .iter()
        .filter(|event| {
            matches!(
                event,
                RuntimeEvent::ToolCallFinished { result, .. } if result.content.contains("net=true")
            )
        })
        .count();
    assert_eq!(granted, 2, "both pushes must run with network: {second:?}");
    assert!(matches!(
        second.last(),
        Some(RuntimeEvent::TurnFinished { .. })
    ));
}

#[tokio::test]
async fn config_auto_allow_exact_name_runs_gated_tool_without_prompt() {
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let config = AgentConfig {
        approval_auto_allow: vec![MockEchoTool::NAME.to_string()],
        ..AgentConfig::builtin()
    };
    let runtime = AgentRuntime::with_system_prompt(
        client,
        ToolRegistry::with_mock_tools(),
        "system",
        config,
        false,
    );

    let mut rx = runtime.submit_user("echo").await;
    let events = drain(&mut rx).await;

    assert!(
        events
            .iter()
            .all(|event| !matches!(event, RuntimeEvent::ApprovalRequired { .. })),
        "auto_allow name must pre-approve the gated call"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ApprovalResolved {
            decision: ApprovalDecision::Approved,
            ..
        }
    )));
    assert_eq!(finished_ids(&events), vec!["call_1"]);
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::TurnFinished { .. })
    ));
}

/// The complement: an entry that is merely a PREFIX of the tool's name grants
/// nothing. Standing consent is an exact-name match — `"mock_"` (or `"s"`)
/// must not stretch over whatever tools happen to share the spelling, so the
/// call parks for a human instead of running.
#[tokio::test]
async fn config_auto_allow_prefix_of_a_name_does_not_pre_approve() {
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::ToolCallDelta {
            delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let config = AgentConfig {
        approval_auto_allow: vec!["mock_".to_string()],
        ..AgentConfig::builtin()
    };
    let runtime = AgentRuntime::with_system_prompt(
        client,
        ToolRegistry::with_mock_tools(),
        "system",
        config,
        false,
    );

    let mut rx = runtime.submit_user("echo").await;
    let events = drain(&mut rx).await;

    assert!(
        matches!(events.last(), Some(RuntimeEvent::ApprovalRequired { .. })),
        "a prefix of a tool name must not pre-approve it: {events:?}"
    );
}

#[tokio::test]
async fn late_approval_after_cancel_is_silent() {
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::ToolCallDelta {
            delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"a"}"#),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools());

    let mut rx = runtime.submit_user("run").await;
    drain(&mut rx).await;

    // Cancel wins the race for the parked batch; the user's keypress lands
    // afterwards and must not surface a red error.
    let mut rx = runtime.cancel_turn().await;
    drain(&mut rx).await;
    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    let events = drain(&mut rx).await;
    assert!(events.is_empty(), "late approval after cancel is benign");
}

#[tokio::test]
async fn write_file_appends_lsp_diagnostics_to_session() {
    use std::sync::Arc;

    use async_trait::async_trait;

    use crate::lsp::{Diagnostic, DiagnosticRange, Language, LspManager, LspTransport, Severity};
    use crate::workspace_tools::workspace_tool_registry;

    struct FakeTransport {
        items: Vec<Diagnostic>,
    }

    #[async_trait]
    impl LspTransport for FakeTransport {
        async fn diagnostics_for(
            &self,
            _path: &std::path::Path,
            _text: &str,
            _wait: std::time::Duration,
        ) -> anyhow::Result<Vec<Diagnostic>> {
            Ok(self.items.clone())
        }

        async fn shutdown(&self) {}
    }

    let dir = tempfile::tempdir().unwrap();
    let manager = LspManager::new(dir.path().to_path_buf());
    manager
        .install_test_transport(
            Language::Rust,
            Arc::new(FakeTransport {
                items: vec![Diagnostic {
                    file: dir.path().join("broken.rs"),
                    range: DiagnosticRange {
                        start_line: 1,
                        start_column: 1,
                        end_line: 1,
                        end_column: 2,
                    },
                    severity: Severity::Error,
                    message: "syntax error".to_string(),
                    source: None,
                    code: None,
                }],
            }),
        )
        .await;

    let args = r#"{"path":"broken.rs","content":"fn main() {"}"#;
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_1", "write_file", args),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "fixed".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::new(client, workspace_tool_registry(dir.path()).unwrap())
        .with_lsp_manager(dir.path().to_path_buf(), manager);

    let mut rx = runtime.submit_user("write broken rust").await;
    drain(&mut rx).await;

    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    let events = drain(&mut rx).await;

    let tool_result = events.iter().find_map(|event| match event {
        RuntimeEvent::ToolCallFinished { result, .. } => Some(result),
        _ => None,
    });
    let tool_result = tool_result.expect("tool result after approval");
    assert!(tool_result.content.contains("<diagnostics file="));
    assert!(tool_result.content.contains("syntax error"));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::DiagnosticsUpdated { .. }))
    );

    let messages = runtime.session_messages().await;
    let tool_message = messages
        .iter()
        .find(|message| matches!(message.role, crate::message::Role::Tool))
        .expect("tool message");
    assert!(tool_message.content.contains("<diagnostics file="));
}

#[tokio::test]
async fn persistence_saves_messages_and_turns() {
    let workspace = tempfile::tempdir().unwrap();
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::TextDelta {
            text: "hello".to_string(),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let runtime = AgentRuntime::with_new_session(
        client,
        ToolRegistry::default(),
        "system",
        workspace.path(),
        &crate::config::AgentConfig::builtin(),
    )
    .unwrap();

    let session_id = runtime.session_id().await.expect("session id");
    let mut rx = runtime.submit_user("hi").await;
    drain(&mut rx).await;
    runtime.shutdown().await;

    let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let record = store.load(&session_id).unwrap();
    assert_eq!(record.message_count(), 3);
    assert_eq!(record.turns.len(), 1);
}

#[tokio::test]
async fn persistence_saves_reasoning_content() {
    let workspace = tempfile::tempdir().unwrap();
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::ReasoningDelta {
            text: "thinking".to_string(),
        },
        AgentEvent::TextDelta {
            text: "hello".to_string(),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let runtime = AgentRuntime::with_new_session(
        client,
        ToolRegistry::default(),
        "system",
        workspace.path(),
        &crate::config::AgentConfig::builtin(),
    )
    .unwrap();

    let session_id = runtime.session_id().await.expect("session id");
    let mut rx = runtime.submit_user("hi").await;
    drain(&mut rx).await;
    runtime.shutdown().await;

    let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let record = store.load(&session_id).unwrap();
    let (content, reasoning) = record
        .entries
        .iter()
        .find_map(|entry| match &entry.kind {
            crate::session_entry::EntryKind::Assistant {
                content, reasoning, ..
            } => Some((content.clone(), reasoning.clone())),
            _ => None,
        })
        .expect("assistant entry");
    assert_eq!(content, "hello");
    assert_eq!(reasoning.as_deref(), Some("thinking"));
}

#[tokio::test]
async fn session_updated_reports_authoritative_metadata() {
    let workspace = tempfile::tempdir().unwrap();
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::TextDelta {
            text: "hello".to_string(),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let runtime = AgentRuntime::with_new_session(
        client,
        ToolRegistry::default(),
        "system",
        workspace.path(),
        &crate::config::AgentConfig::builtin(),
    )
    .unwrap();
    let session_id = runtime.session_id().await.expect("session id");

    let mut rx = runtime.submit_user("hi").await;
    let events = drain(&mut rx).await;

    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::SessionUpdated {
            session_id: Some(id),
            current_turn_id: Some(_),
            message_count,
            turn_count,
            ..
        } if id == &session_id && *message_count >= 2 && *turn_count == 0
    )));
}

#[tokio::test]
async fn stream_error_finalizes_open_turn() {
    let workspace = tempfile::tempdir().unwrap();
    let client = ScriptedClient::new(vec![vec![AgentEvent::Error {
        message: "boom".to_string(),
    }]]);
    let runtime = AgentRuntime::with_new_session(
        client,
        ToolRegistry::default(),
        "system",
        workspace.path(),
        &crate::config::AgentConfig::builtin(),
    )
    .unwrap();

    let session_id = runtime.session_id().await.expect("session id");
    let mut rx = runtime.submit_user("hi").await;
    drain(&mut rx).await;
    runtime.shutdown().await;

    let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let record = store.load(&session_id).unwrap();
    assert_eq!(record.turns.len(), 1);
}

#[tokio::test]
async fn persistence_saves_tool_exchange_results() {
    let workspace = tempfile::tempdir().unwrap();
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "thanks".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::with_new_session(
        client,
        ToolRegistry::with_mock_tools(),
        "system",
        workspace.path(),
        &crate::config::AgentConfig::builtin(),
    )
    .unwrap();

    let session_id = runtime.session_id().await.expect("session id");
    let mut rx = runtime.submit_user("please echo").await;
    drain(&mut rx).await;
    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    drain(&mut rx).await;
    runtime.shutdown().await;

    let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let record = store.load(&session_id).unwrap();
    assert_eq!(record.turns.len(), 1);
    // Tool outputs persist through the entries' exchanges (single copy).
    let exchange = record
        .entries
        .iter()
        .find_map(|entry| match &entry.kind {
            crate::session_entry::EntryKind::Assistant { exchanges, .. } => exchanges.first(),
            _ => None,
        })
        .expect("assistant entry with an exchange");
    assert_eq!(exchange.call.function.name, MockEchoTool::NAME);
    assert_eq!(
        exchange.result.as_ref().expect("recorded result").content,
        "mock_echo: hi"
    );
}

#[tokio::test]
async fn resumed_runtime_continues_conversation() {
    let workspace = tempfile::tempdir().unwrap();
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::TextDelta {
                text: "hello".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "world".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::with_new_session(
        client,
        ToolRegistry::default(),
        "system",
        workspace.path(),
        &crate::config::AgentConfig::builtin(),
    )
    .unwrap();

    let session_id = runtime.session_id().await.expect("session id");
    let mut rx = runtime.submit_user("first").await;
    drain(&mut rx).await;
    runtime.shutdown().await;

    let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let record = store.load(&session_id).unwrap();
    assert_eq!(record.message_count(), 3);

    let resumed = AgentRuntime::from_session_record(
        ScriptedClient::new(vec![vec![
            AgentEvent::TextDelta {
                text: "world".to_string(),
            },
            AgentEvent::Done { usage: None },
        ]]),
        ToolRegistry::default(),
        record,
        store,
        AgentConfig::builtin(),
    );
    let mut rx = resumed.submit_user("second").await;
    drain(&mut rx).await;
    let messages = resumed.session_messages().await;
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[3].content, "second");
    assert_eq!(messages[4].content, "world");
}

/// Lifetime session cost must survive resume: it's persisted into the record
/// and restored, so a resumed session's total continues instead of resetting
/// to zero (the bug was `from_session_record` starting from `Default`).
#[tokio::test]
async fn session_cost_persists_across_resume() {
    use crate::model::Usage;

    let workspace = tempfile::tempdir().unwrap();
    let runtime = AgentRuntime::with_new_session(
        ScriptedClient::new(vec![]),
        ToolRegistry::default(),
        "system",
        workspace.path(),
        &crate::config::AgentConfig::builtin(),
    )
    .unwrap();
    let session_id = runtime.session_id().await.expect("session id");

    let usage = Usage {
        prompt_tokens: Some(1_000),
        completion_tokens: Some(500),
        total_tokens: Some(1_500),
        reasoning_tokens: None,
        prompt_cache_hit_tokens: Some(400),
        prompt_cache_miss_tokens: Some(600),
    };
    let model = crate::config::AgentConfig::builtin().model;
    runtime.accumulate_request_usage(&model, &usage).await;
    let live_cost = runtime.state.lock().await.session_cost;
    runtime.persist().await;
    runtime.shutdown().await;

    let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let record = store.load(&session_id).unwrap();
    assert!(
        (record.session_cost.cny - live_cost.cny).abs() < 1e-9
            && (record.session_cost.usd - live_cost.usd).abs() < 1e-9,
        "persisted record must carry the lifetime cost"
    );
    assert_eq!(record.session_cache_hit_tokens, 400);
    assert_eq!(record.session_cache_miss_tokens, 600);

    let resumed = AgentRuntime::from_session_record(
        ScriptedClient::new(vec![]),
        ToolRegistry::default(),
        record,
        store,
        crate::config::AgentConfig::builtin(),
    );
    let restored = resumed.state.lock().await;
    assert!(
        (restored.session_cost.cny - live_cost.cny).abs() < 1e-9,
        "resume must restore the lifetime cost, not reset to zero"
    );
    assert_eq!(restored.session_cache_hit_tokens, 400);
    assert_eq!(restored.session_cache_miss_tokens, 600);
}

/// Returns a retriable API error on the first `stream_chat`, then succeeds.
#[derive(Clone)]
struct FallbackTestClient {
    inner: Arc<FallbackTestClientInner>,
}

struct FallbackTestClientInner {
    models: Mutex<Vec<String>>,
    attempts: Mutex<u32>,
}

impl FallbackTestClient {
    fn new() -> Self {
        Self {
            inner: Arc::new(FallbackTestClientInner {
                models: Mutex::new(Vec::new()),
                attempts: Mutex::new(0),
            }),
        }
    }

    fn models_used(&self) -> Vec<String> {
        self.inner.models.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl LlmClient for FallbackTestClient {
    fn provider_name(&self) -> &'static str {
        "fallback-test"
    }

    fn model(&self) -> &str {
        "fallback-test"
    }

    async fn stream_chat(&self, request: ChatRequest) -> AgentResult<AgentEventStream> {
        self.inner
            .models
            .lock()
            .unwrap()
            .push(request.model.clone());
        let attempt = {
            let mut attempts = self.inner.attempts.lock().unwrap();
            *attempts += 1;
            *attempts
        };
        if attempt == 1 {
            return Err(AgentError::Api {
                status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                message: "model overloaded".to_string(),
            });
        }

        let stream = try_stream! {
            yield AgentEvent::TextDelta { text: "recovered".to_string() };
            yield AgentEvent::Done { usage: None };
        };
        Ok(Box::pin(stream))
    }
}

#[tokio::test]
async fn auto_pro_retries_with_flash_after_retriable_api_error() {
    use crate::model_registry::{AUTO_MODEL, DEEPSEEK_V4_FLASH, DEEPSEEK_V4_PRO};

    let client = FallbackTestClient::new();
    // Pin zh so the localized fallback-reason assertion is deterministic.
    let config = AgentConfig {
        model: AUTO_MODEL.to_string(),
        language: "zh".to_string(),
        ..AgentConfig::builtin()
    };
    let runtime = AgentRuntime::with_system_prompt(
        client.clone(),
        ToolRegistry::default(),
        "system",
        config,
        false,
    );

    let mut rx = runtime.submit_user("debug this crash").await;
    let events = drain(&mut rx).await;

    assert_eq!(
        client.models_used(),
        vec![DEEPSEEK_V4_PRO.to_string(), DEEPSEEK_V4_FLASH.to_string()]
    );
    let telemetry = events.iter().find_map(|event| match event {
        RuntimeEvent::TurnFinished { telemetry, .. } => telemetry.as_ref(),
        _ => None,
    });
    let telemetry = telemetry.expect("turn finished with telemetry");
    assert!(telemetry.used_model_fallback);
    assert_eq!(telemetry.effective_model, DEEPSEEK_V4_FLASH);
    assert!(telemetry.route_label.contains("fallback→flash"));
    assert!(telemetry.route_reason.contains("debug"));
    assert!(
        telemetry
            .fallback_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("降级"))
    );
    assert_eq!(
        runtime.session_messages().await.last().unwrap().content,
        "recovered"
    );
}

#[tokio::test]
async fn unauthorized_api_error_does_not_fallback() {
    use crate::model_registry::AUTO_MODEL;

    #[derive(Clone)]
    struct AuthFailClient {
        calls: Arc<Mutex<u32>>,
    }

    #[async_trait::async_trait]
    impl LlmClient for AuthFailClient {
        fn provider_name(&self) -> &'static str {
            "auth-fail"
        }

        fn model(&self) -> &str {
            "auth-fail"
        }

        async fn stream_chat(&self, _request: ChatRequest) -> AgentResult<AgentEventStream> {
            *self.calls.lock().unwrap() += 1;
            Err(AgentError::Api {
                status: reqwest::StatusCode::UNAUTHORIZED,
                message: "invalid key".to_string(),
            })
        }
    }

    let client = AuthFailClient {
        calls: Arc::new(Mutex::new(0)),
    };
    // Pin the language so the localized error assertion is deterministic
    // regardless of the test machine's LANG.
    let config = AgentConfig {
        model: AUTO_MODEL.to_string(),
        language: "zh".to_string(),
        ..AgentConfig::builtin()
    };
    let runtime = AgentRuntime::with_system_prompt(
        client.clone(),
        ToolRegistry::default(),
        "system",
        config,
        false,
    );

    let mut rx = runtime.submit_user("debug this").await;
    let events = drain(&mut rx).await;

    assert_eq!(*client.calls.lock().unwrap(), 1);
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Error { message, .. }) if message.contains("鉴权失败")
    ));
}

#[tokio::test]
async fn multi_tool_turn_executes_all_auto_calls_in_order_and_persists() {
    let workspace = tempfile::tempdir().unwrap();
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: indexed_tool_call_delta(
                    0,
                    "call_1",
                    AutoEchoTool::NAME,
                    r#"{"message":"one"}"#,
                ),
            },
            AgentEvent::ToolCallDelta {
                delta: indexed_tool_call_delta(
                    1,
                    "call_2",
                    AutoEchoTool::NAME,
                    r#"{"message":"two"}"#,
                ),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::with_new_session(
        client,
        registry_with_auto_and_mock(),
        "system",
        workspace.path(),
        &crate::config::AgentConfig::builtin(),
    )
    .unwrap();
    let session_id = runtime.session_id().await.expect("session id");

    let mut rx = runtime.submit_user("run both").await;
    let events = drain(&mut rx).await;

    assert_eq!(started_ids(&events), vec!["call_1", "call_2"]);
    assert_eq!(finished_ids(&events), vec!["call_1", "call_2"]);
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::TurnFinished { .. })
    ));

    // Persistence announcements are batched: no SessionUpdated may appear
    // between the two ToolCallFinished events of one batch.
    let finished_positions = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| {
            matches!(event, RuntimeEvent::ToolCallFinished { .. }).then_some(index)
        })
        .collect::<Vec<_>>();
    assert!(
        events[finished_positions[0]..finished_positions[1]]
            .iter()
            .all(|event| !matches!(event, RuntimeEvent::SessionUpdated { .. })),
        "per-call SessionUpdated noise inside a batch"
    );

    let messages = runtime.session_messages().await;
    // system, user, assistant(2 tool_calls), tool(call_1), tool(call_2), assistant("done")
    assert_eq!(messages.len(), 6);
    assert_eq!(messages[2].tool_calls.len(), 2);
    assert_eq!(messages[2].tool_calls[0].id, "call_1");
    assert_eq!(messages[2].tool_calls[1].id, "call_2");
    assert_eq!(messages[3].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(messages[3].content, "read_file: one");
    assert_eq!(messages[4].tool_call_id.as_deref(), Some("call_2"));
    assert_eq!(messages[4].content, "read_file: two");
    assert_eq!(messages[5].content, "done");

    runtime.shutdown().await;
    let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let record = store.load(&session_id).unwrap();
    assert_eq!(record.message_count(), 6);
    assert_eq!(record.turns.len(), 1);
}

#[tokio::test]
async fn multi_tool_turn_mixes_auto_and_approval_calls() {
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: indexed_tool_call_delta(
                    0,
                    "call_1",
                    AutoEchoTool::NAME,
                    r#"{"message":"auto"}"#,
                ),
            },
            AgentEvent::ToolCallDelta {
                delta: indexed_tool_call_delta(
                    1,
                    "call_2",
                    MockEchoTool::NAME,
                    r#"{"message":"gated"}"#,
                ),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::new(client, registry_with_auto_and_mock());

    let mut rx = runtime.submit_user("run both").await;
    let first = drain(&mut rx).await;

    // The auto call completes before the gated call asks for approval.
    assert_eq!(finished_ids(&first), vec!["call_1"]);
    assert!(matches!(
        first.last(),
        Some(RuntimeEvent::ApprovalRequired { tool_call_id: Some(id), .. })
            if id.as_str() == "call_2"
    ));

    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    let second = drain(&mut rx).await;
    assert_eq!(finished_ids(&second), vec!["call_2"]);
    assert!(matches!(
        second.last(),
        Some(RuntimeEvent::TurnFinished { .. })
    ));

    let messages = runtime.session_messages().await;
    // user, assistant(2 tool_calls), tool(call_1), tool(call_2), assistant("done")
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[1].tool_calls.len(), 2);
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(messages[3].tool_call_id.as_deref(), Some("call_2"));
    assert_eq!(messages[3].content, "mock_echo: gated");
}

#[tokio::test]
async fn multi_tool_turn_serializes_two_approvals() {
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: indexed_tool_call_delta(
                    0,
                    "call_1",
                    MockEchoTool::NAME,
                    r#"{"message":"first"}"#,
                ),
            },
            AgentEvent::ToolCallDelta {
                delta: indexed_tool_call_delta(
                    1,
                    "call_2",
                    MockEchoTool::NAME,
                    r#"{"message":"second"}"#,
                ),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::new(client, registry_with_auto_and_mock());

    let mut rx = runtime.submit_user("run both").await;
    let first = drain(&mut rx).await;
    assert!(finished_ids(&first).is_empty());
    assert!(matches!(
        first.last(),
        Some(RuntimeEvent::ApprovalRequired { tool_call_id: Some(id), .. })
            if id.as_str() == "call_1"
    ));

    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    let second = drain(&mut rx).await;
    assert_eq!(finished_ids(&second), vec!["call_1"]);
    assert!(matches!(
        second.last(),
        Some(RuntimeEvent::ApprovalRequired { tool_call_id: Some(id), .. })
            if id.as_str() == "call_2"
    ));

    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    let third = drain(&mut rx).await;
    assert_eq!(finished_ids(&third), vec!["call_2"]);
    assert!(matches!(
        third.last(),
        Some(RuntimeEvent::TurnFinished { .. })
    ));

    let messages = runtime.session_messages().await;
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[2].content, "mock_echo: first");
    assert_eq!(messages[3].content, "mock_echo: second");
}

#[tokio::test]
async fn multi_tool_turn_denying_one_call_keeps_batch_running() {
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: indexed_tool_call_delta(
                    0,
                    "call_1",
                    MockEchoTool::NAME,
                    r#"{"message":"first"}"#,
                ),
            },
            AgentEvent::ToolCallDelta {
                delta: indexed_tool_call_delta(
                    1,
                    "call_2",
                    MockEchoTool::NAME,
                    r#"{"message":"second"}"#,
                ),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::new(client, registry_with_auto_and_mock());

    let mut rx = runtime.submit_user("run both").await;
    drain(&mut rx).await;

    let mut rx = runtime.submit_approval(ApprovalDecision::Denied).await;
    let second = drain(&mut rx).await;
    let denied = second
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ToolCallFinished {
                tool_call_id,
                result,
                ..
            } if tool_call_id.as_str() == "call_1" => Some(result.status),
            _ => None,
        })
        .expect("denied call_1 still records a result");
    assert_eq!(denied, ToolResultStatus::Denied);
    assert!(matches!(
        second.last(),
        Some(RuntimeEvent::ApprovalRequired { tool_call_id: Some(id), .. })
            if id.as_str() == "call_2"
    ));

    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    let third = drain(&mut rx).await;
    assert!(matches!(
        third.last(),
        Some(RuntimeEvent::TurnFinished { .. })
    ));

    let messages = runtime.session_messages().await;
    // user, assistant(2 tool_calls), tool(denied), tool(success), assistant("done")
    assert_eq!(messages.len(), 5);
    assert!(messages[2].content.contains("denied"));
    assert_eq!(messages[3].content, "mock_echo: second");
}

/// First call: yields one text delta then hangs forever; later calls stream
/// normally. Used to exercise mid-stream cancellation and token rotation.
struct HangThenRecoverClient {
    attempts: Arc<Mutex<u32>>,
}

impl HangThenRecoverClient {
    fn new() -> Self {
        Self {
            attempts: Arc::new(Mutex::new(0)),
        }
    }
}

#[async_trait::async_trait]
impl LlmClient for HangThenRecoverClient {
    fn provider_name(&self) -> &'static str {
        "hang-then-recover"
    }

    fn model(&self) -> &str {
        "hang-then-recover"
    }

    async fn stream_chat(&self, _request: ChatRequest) -> AgentResult<AgentEventStream> {
        let attempt = {
            let mut attempts = self.attempts.lock().unwrap();
            *attempts += 1;
            *attempts
        };
        let stream = try_stream! {
            if attempt == 1 {
                yield AgentEvent::TextDelta { text: "partial".to_string() };
                futures_util::future::pending::<()>().await;
            } else {
                yield AgentEvent::TextDelta { text: "recovered".to_string() };
                yield AgentEvent::Done { usage: None };
            }
        };
        let stream: Pin<Box<dyn Stream<Item = AgentResult<AgentEvent>> + Send>> = Box::pin(stream);
        Ok(stream)
    }
}

#[tokio::test]
async fn cancel_during_stream_keeps_partial_text_and_allows_next_turn() {
    let runtime = AgentRuntime::new(HangThenRecoverClient::new(), ToolRegistry::default());

    let mut rx = runtime.submit_user("hang please").await;
    while let Some(event) = rx.recv().await {
        if matches!(event, RuntimeEvent::AssistantDelta { .. }) {
            break;
        }
    }

    let _ = runtime.cancel_turn().await;

    let mut saw_cancelled = false;
    while let Some(event) = rx.recv().await {
        if matches!(event, RuntimeEvent::TurnCancelled { .. }) {
            saw_cancelled = true;
        }
    }
    assert!(saw_cancelled, "expected TurnCancelled on the live channel");

    let messages = runtime.session_messages().await;
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].content, "partial");
    assert!(messages[1].tool_calls.is_empty());

    // Token was rotated: a fresh turn streams to completion.
    let mut rx = runtime.submit_user("again").await;
    let events = drain(&mut rx).await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::TurnFinished { .. })
    ));
    assert_eq!(
        runtime.session_messages().await.last().unwrap().content,
        "recovered"
    );
}

#[tokio::test]
async fn cancel_while_waiting_approval_synthesizes_results_for_batch() {
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::ToolCallDelta {
            delta: indexed_tool_call_delta(0, "call_1", MockEchoTool::NAME, r#"{"message":"a"}"#),
        },
        AgentEvent::ToolCallDelta {
            delta: indexed_tool_call_delta(1, "call_2", MockEchoTool::NAME, r#"{"message":"b"}"#),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools());

    let mut rx = runtime.submit_user("run both").await;
    let first = drain(&mut rx).await;
    assert!(matches!(
        first.last(),
        Some(RuntimeEvent::ApprovalRequired { .. })
    ));

    let mut rx = runtime.cancel_turn().await;
    let events = drain(&mut rx).await;

    assert_eq!(finished_ids(&events), vec!["call_1", "call_2"]);
    assert!(events.iter().all(|event| match event {
        RuntimeEvent::ToolCallFinished { result, .. } => {
            result.status == ToolResultStatus::Error && result.content.contains("取消")
        }
        _ => true,
    }));
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::TurnCancelled { .. })
    ));

    // Every tool_call keeps a paired tool message: no dangling calls.
    let messages = runtime.session_messages().await;
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1].tool_calls.len(), 2);
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(messages[3].tool_call_id.as_deref(), Some("call_2"));
}

/// Per-attempt scripted behaviors for stream-robustness tests. Each
/// `stream_chat` call consumes the next behavior and records the model used.
#[derive(Debug, Clone)]
enum AttemptBehavior {
    /// `stream_chat` itself fails with this API status.
    ConnectFail(u16),
    /// Stream errors immediately, before any content.
    StreamErr,
    /// Stream yields text, then errors.
    TextThenErr(String),
    /// Stream yields nothing and pends forever.
    Hang,
    /// Stream yields this text and finishes cleanly.
    Text(String),
}

#[derive(Clone)]
struct AttemptScriptClient {
    inner: Arc<AttemptScriptInner>,
}

struct AttemptScriptInner {
    behaviors: Mutex<Vec<AttemptBehavior>>,
    models: Mutex<Vec<String>>,
}

impl AttemptScriptClient {
    fn new(behaviors: Vec<AttemptBehavior>) -> Self {
        Self {
            inner: Arc::new(AttemptScriptInner {
                behaviors: Mutex::new(behaviors),
                models: Mutex::new(Vec::new()),
            }),
        }
    }

    fn models_used(&self) -> Vec<String> {
        self.inner.models.lock().unwrap().clone()
    }

    fn attempts(&self) -> usize {
        self.inner.models.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl LlmClient for AttemptScriptClient {
    fn provider_name(&self) -> &'static str {
        "attempt-script"
    }

    fn model(&self) -> &str {
        "attempt-script"
    }

    async fn stream_chat(&self, request: ChatRequest) -> AgentResult<AgentEventStream> {
        self.inner
            .models
            .lock()
            .unwrap()
            .push(request.model.clone());
        let behavior = {
            let mut behaviors = self.inner.behaviors.lock().unwrap();
            if behaviors.is_empty() {
                AttemptBehavior::Text("default".to_string())
            } else {
                behaviors.remove(0)
            }
        };
        if let AttemptBehavior::ConnectFail(status) = behavior {
            return Err(AgentError::Api {
                status: reqwest::StatusCode::from_u16(status).unwrap(),
                message: "scripted failure".to_string(),
            });
        }
        let stream = try_stream! {
            match behavior {
                AttemptBehavior::ConnectFail(_) => unreachable!("handled above"),
                AttemptBehavior::StreamErr => {
                    Err(AgentError::Parse("connection reset".to_string()))?;
                }
                AttemptBehavior::TextThenErr(text) => {
                    yield AgentEvent::TextDelta { text };
                    Err(AgentError::Parse("broken mid-stream".to_string()))?;
                }
                AttemptBehavior::Hang => {
                    futures_util::future::pending::<()>().await;
                }
                AttemptBehavior::Text(text) => {
                    yield AgentEvent::TextDelta { text };
                    yield AgentEvent::Done { usage: None };
                }
            }
        };
        let stream: Pin<Box<dyn Stream<Item = AgentResult<AgentEvent>> + Send>> = Box::pin(stream);
        Ok(stream)
    }
}

fn turn_finished_telemetry(events: &[RuntimeEvent]) -> Option<super::telemetry::TurnTelemetry> {
    events.iter().find_map(|event| match event {
        RuntimeEvent::TurnFinished { telemetry, .. } => telemetry.clone(),
        _ => None,
    })
}

#[tokio::test(start_paused = true)]
async fn stream_error_before_content_retries_transparently() {
    let client = AttemptScriptClient::new(vec![
        AttemptBehavior::StreamErr,
        AttemptBehavior::Text("recovered".to_string()),
    ]);
    let runtime = AgentRuntime::new(client.clone(), ToolRegistry::default());

    let mut rx = runtime.submit_user("hi").await;
    let events = drain(&mut rx).await;

    assert!(
        events
            .iter()
            .all(|event| !matches!(event, RuntimeEvent::Error { .. })),
        "transparent retry must not surface an error"
    );
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::TurnFinished { .. })
    ));
    assert_eq!(client.attempts(), 2);
    let telemetry = turn_finished_telemetry(&events).expect("telemetry");
    assert_eq!(telemetry.stream_retries, 1);
    assert_eq!(
        runtime.session_messages().await.last().unwrap().content,
        "recovered"
    );
}

#[tokio::test(start_paused = true)]
async fn stream_error_after_content_is_not_retried() {
    let client = AttemptScriptClient::new(vec![AttemptBehavior::TextThenErr("part".to_string())]);
    let runtime = AgentRuntime::new(client.clone(), ToolRegistry::default());

    let mut rx = runtime.submit_user("hi").await;
    let events = drain(&mut rx).await;

    assert_eq!(client.attempts(), 1, "billed content must never be retried");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Error { .. }))
    );
}

#[tokio::test(start_paused = true)]
async fn stalled_stream_times_out_with_chinese_error() {
    let client = AttemptScriptClient::new(vec![AttemptBehavior::Hang]);
    let config = AgentConfig {
        stream_chunk_timeout: std::time::Duration::from_secs(5),
        language: "zh".to_string(),
        ..AgentConfig::builtin()
    };
    let runtime =
        AgentRuntime::with_system_prompt(client, ToolRegistry::default(), "system", config, false);

    let mut rx = runtime.submit_user("hi").await;
    let events = drain(&mut rx).await;

    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::Error { message, .. } if message.contains("卡顿")
    )));
}

#[tokio::test(start_paused = true)]
async fn oversized_stream_is_cut_off() {
    let client = AttemptScriptClient::new(vec![AttemptBehavior::Text("x".repeat(64))]);
    let config = AgentConfig {
        stream_max_bytes: 10,
        language: "zh".to_string(),
        ..AgentConfig::builtin()
    };
    let runtime =
        AgentRuntime::with_system_prompt(client, ToolRegistry::default(), "system", config, false);

    let mut rx = runtime.submit_user("hi").await;
    let events = drain(&mut rx).await;

    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::Error { message, .. } if message.contains("过大")
    )));
}

#[tokio::test]
async fn cancel_during_open_backoff_finalizes_as_cancelled() {
    let client = AttemptScriptClient::new(vec![
        AttemptBehavior::ConnectFail(503),
        AttemptBehavior::ConnectFail(503),
        AttemptBehavior::ConnectFail(503),
        AttemptBehavior::ConnectFail(503),
    ]);
    let runtime = AgentRuntime::new(client.clone(), ToolRegistry::default());

    let mut rx = runtime.submit_user("hi").await;
    // Give the loop time to fail the first attempt and enter backoff sleep.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let _ = runtime.cancel_turn().await;
    let events = drain(&mut rx).await;

    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::TurnCancelled { .. })
    ));
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, RuntimeEvent::Error { .. }))
    );
    assert!(client.attempts() <= 2, "cancel must abort the backoff");
}

#[tokio::test(start_paused = true)]
async fn open_retry_runs_after_fallback_exhausted_and_is_counted() {
    use crate::model_registry::{AUTO_MODEL, DEEPSEEK_V4_FLASH, DEEPSEEK_V4_PRO};

    let client = AttemptScriptClient::new(vec![
        AttemptBehavior::ConnectFail(503),
        AttemptBehavior::ConnectFail(503),
        AttemptBehavior::Text("recovered".to_string()),
    ]);
    let config = AgentConfig {
        model: AUTO_MODEL.to_string(),
        ..AgentConfig::builtin()
    };
    let runtime = AgentRuntime::with_system_prompt(
        client.clone(),
        ToolRegistry::default(),
        "system",
        config,
        false,
    );

    let mut rx = runtime.submit_user("debug this crash").await;
    let events = drain(&mut rx).await;

    // Pinned order: Pro fails -> immediate Flash fallback fails -> backoff
    // retry stays on the downgraded Flash model and succeeds.
    assert_eq!(
        client.models_used(),
        vec![
            DEEPSEEK_V4_PRO.to_string(),
            DEEPSEEK_V4_FLASH.to_string(),
            DEEPSEEK_V4_FLASH.to_string()
        ]
    );
    let telemetry = turn_finished_telemetry(&events).expect("telemetry");
    assert_eq!(telemetry.stream_retries, 1);
    assert!(telemetry.used_model_fallback);
    assert_eq!(
        runtime.session_messages().await.last().unwrap().content,
        "recovered"
    );
}

#[tokio::test]
async fn stream_error_keeps_partial_assistant_text() {
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::TextDelta {
            text: "partial answer".to_string(),
        },
        AgentEvent::Error {
            message: "boom".to_string(),
        },
    ]]);
    let runtime = AgentRuntime::new(client, ToolRegistry::default());

    let mut rx = runtime.submit_user("hi").await;
    let events = drain(&mut rx).await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Error { .. }))
    );
    let messages = runtime.session_messages().await;
    // Same semantics as cancellation: the streamed partial text survives.
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].content, "partial answer");
    assert!(messages[1].tool_calls.is_empty());
}

#[tokio::test]
async fn cancel_turn_when_idle_is_silent_noop() {
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::TextDelta {
            text: "hello".to_string(),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let runtime = AgentRuntime::new(client, ToolRegistry::default());

    let mut rx = runtime.cancel_turn().await;
    let events = drain(&mut rx).await;
    assert!(events.is_empty(), "idle cancel must not emit events");

    let mut rx = runtime.submit_user("hi").await;
    let events = drain(&mut rx).await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::TurnFinished { .. })
    ));
}

#[tokio::test]
async fn repeated_tool_errors_trigger_cascade_and_surface_in_telemetry() {
    // The model calls a failing tool twice in one turn; both error, crossing
    // the cascade threshold. The triggering turn still finishes on Flash, but
    // its telemetry must flag `cascade_triggered` so the escalation is visible.
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: indexed_tool_call_delta(0, "c1", FailingTool::NAME, "{}"),
            },
            AgentEvent::ToolCallDelta {
                delta: indexed_tool_call_delta(1, "c2", FailingTool::NAME, "{}"),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let config = AgentConfig {
        model: crate::model_registry::AUTO_MODEL.to_string(),
        approval_auto_allow: vec![FailingTool::NAME.to_string()],
        ..AgentConfig::builtin()
    };
    let mut registry = ToolRegistry::with_mock_tools();
    registry.register(FailingTool);
    let runtime = AgentRuntime::with_config(client, registry, config);

    let mut rx = runtime.submit_user("do the thing").await;
    let events = drain(&mut rx).await;
    let telemetry = events
        .iter()
        .rev()
        .find_map(|event| match event {
            RuntimeEvent::TurnFinished { telemetry, .. } => telemetry.clone(),
            _ => None,
        })
        .expect("turn should finish with telemetry");
    assert!(
        telemetry.cascade_triggered,
        "two tool-call failures in one turn must trigger cascade escalation"
    );
}

/// One scripted `write_file` delta targeting `path`, at batch `index`.
fn outside_write_delta(index: u32, path: &std::path::Path) -> AgentEvent {
    AgentEvent::ToolCallDelta {
        delta: indexed_tool_call_delta(
            index,
            &format!("c{index}"),
            "write_file",
            &serde_json::json!({"path": path.to_string_lossy(), "content": "x"}).to_string(),
        ),
    }
}

#[tokio::test]
async fn boundary_denials_trip_the_breaker_with_add_dir_guidance() {
    // Three write_file calls to an absolute path outside every granted root —
    // three boundary denials in one batch. The breaker must end the turn with
    // guidance naming /add-dir and the denied path, instead of feeding the
    // errors back for a fourth attempt; the scripted second response must
    // never be requested.
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    // Canonical spelling: the raw tempdir path goes through a symlink on
    // macOS (`/var` → `/private/var`), which trips the symlink ban before
    // containment — a different rejection class than the one under test.
    let target = outside.path().canonicalize().unwrap().join("f.txt");
    let client = ScriptedClient::new(vec![
        vec![
            outside_write_delta(0, &target),
            outside_write_delta(1, &target),
            outside_write_delta(2, &target),
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "unreachable".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let config = AgentConfig {
        approval_auto_allow: vec!["write_file".to_string()],
        ..AgentConfig::builtin()
    };
    let registry =
        crate::workspace_tools::workspace_tool_registry(workspace.path().to_path_buf()).unwrap();
    let runtime = AgentRuntime::with_config(client, registry, config);

    let mut rx = runtime.submit_user("edit the sibling repo").await;
    let events = drain(&mut rx).await;

    let message = events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("the breaker must surface a user-facing error event");
    assert!(
        message.contains("/add-dir"),
        "breaker guidance must name the remedy: {message}"
    );
    assert!(
        message.contains(target.to_string_lossy().as_ref()),
        "breaker guidance should name the denied path: {message}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnFinished { .. })),
        "the turn must abort at the breaker, not finish into another model round"
    );
}

#[tokio::test]
async fn boundary_denials_do_not_latch_cascade_escalation() {
    // Two boundary denials — exactly the count that latches the cascade when
    // the failures are ordinary — then a clean finish. Telemetry must NOT
    // flag an escalation: a stronger model cannot move the granted-roots
    // fence, so paying Pro prices for these retries is pure waste.
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    // Canonical for the same reason as the breaker test above.
    let target = outside.path().canonicalize().unwrap().join("f.txt");
    let client = ScriptedClient::new(vec![
        vec![
            outside_write_delta(0, &target),
            outside_write_delta(1, &target),
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "understood, asking the user".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let config = AgentConfig {
        model: crate::model_registry::AUTO_MODEL.to_string(),
        approval_auto_allow: vec!["write_file".to_string()],
        ..AgentConfig::builtin()
    };
    let registry =
        crate::workspace_tools::workspace_tool_registry(workspace.path().to_path_buf()).unwrap();
    let runtime = AgentRuntime::with_config(client, registry, config);

    let mut rx = runtime.submit_user("edit the sibling repo").await;
    let events = drain(&mut rx).await;
    let telemetry = events
        .iter()
        .rev()
        .find_map(|event| match event {
            RuntimeEvent::TurnFinished { telemetry, .. } => telemetry.clone(),
            _ => None,
        })
        .expect("two denials stay under the breaker; the turn finishes normally");
    assert!(
        !telemetry.cascade_triggered,
        "boundary denials must not latch the Pro escalation"
    );
}

/// A new `begin_turn` while a previous turn is still live (HTTP client
/// disconnected mid-turn, new prompt arrived) must cancel the old loop, and
/// the old loop's late finalization must not consume the new turn's state.
#[tokio::test]
async fn begin_turn_supersedes_live_turn_without_clobbering() {
    let client = ScriptedClient::new(vec![]);
    let runtime = AgentRuntime::new(client, ToolRegistry::default());

    runtime.begin_turn("first").await;
    let (first_id, first_token) = {
        let state = runtime.state.lock().await;
        (
            state.current_turn_id.clone().expect("first turn live"),
            state.cancel.clone(),
        )
    };

    runtime.begin_turn("second").await;
    assert!(
        first_token.is_cancelled(),
        "superseding begin_turn must cancel the previous turn's loop"
    );

    // The superseded loop finalizing late must be a no-op under the id guard.
    runtime.finish_turn(&first_id).await;
    let state = runtime.state.lock().await;
    assert!(
        state.current_turn.is_some(),
        "stale finalization must not consume the new turn's record"
    );
    assert_eq!(state.current_prompt.as_deref(), Some("second"));
    assert!(state.current_turn_id.is_some());
}

/// The SSE lease drop cancels via `cancel_turn_if(its_turn)`. A stale lease
/// from a finished/superseded turn must NOT cancel the successor turn that a
/// new request already began on the shared runtime — only a matching id fires.
#[tokio::test]
async fn cancel_turn_if_only_cancels_the_named_turn() {
    let client = ScriptedClient::new(vec![]);
    let runtime = AgentRuntime::new(client, ToolRegistry::default());

    runtime.begin_turn("first").await;
    let first_id = runtime.live_turn_id().await.expect("first turn live");

    // A successor turn supersedes it (fresh cancel token).
    runtime.begin_turn("second").await;
    let (second_id, second_token) = {
        let state = runtime.state.lock().await;
        (
            state.current_turn_id.clone().expect("second turn live"),
            state.cancel.clone(),
        )
    };

    // Stale lease for the first turn fires late — must be a no-op.
    drop(runtime.cancel_turn_if(first_id).await);
    assert!(
        !second_token.is_cancelled(),
        "a stale turn's lease must not cancel the successor"
    );
    assert_eq!(
        runtime.state.lock().await.current_prompt.as_deref(),
        Some("second"),
        "the successor turn stays live"
    );

    // The lease that actually owns the live turn still cancels it.
    drop(runtime.cancel_turn_if(second_id).await);
    assert!(
        second_token.is_cancelled(),
        "cancel_turn_if must cancel the turn it names"
    );
}

/// Shutdown must cancel a live turn and wait for its loop to finalize: the
/// tool-side cancel arms (the foreground shell's process-group kill, a
/// sub-agent's own turn cancel) only run while the loop is still polled.
/// Skipping this leaves them to `kill_on_drop` at process exit, which signals
/// the group leader alone — grandchildren survive and keep their ports.
#[tokio::test]
async fn shutdown_cancels_live_turn_and_waits_for_finalize() {
    let started = Arc::new(tokio::sync::Notify::new());
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::ToolCallDelta {
            delta: tool_call_delta("call_1", ParkUntilCancelledTool::NAME, "{}"),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let mut registry = ToolRegistry::default();
    registry.register(ParkUntilCancelledTool {
        started: Arc::clone(&started),
    });
    let runtime = AgentRuntime::new(client, registry);

    let _rx = runtime.submit_user("go").await;
    // Only proceed once the call is parked on the token; shutting down before
    // the tool starts would exercise nothing.
    started.notified().await;

    runtime.shutdown().await;

    assert!(
        runtime.live_turn_id().await.is_none(),
        "shutdown must cancel the live turn and see it finalized"
    );
}

/// Spend a tool reports out-of-band (the sub-agent shape) must fold into the
/// parent session's lifetime totals — cost AND cache counters, the same
/// accounting `record_classifier_cost` uses — even though the turn's own
/// telemetry never saw those requests.
#[tokio::test]
async fn tool_reported_spend_folds_into_session_totals() {
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_1", SpendReportingTool::NAME, "{}"),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let mut registry = ToolRegistry::default();
    registry.register(SpendReportingTool);
    let runtime = AgentRuntime::new(client, registry);

    let mut rx = runtime.submit_user("go").await;
    let events = drain(&mut rx).await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::TurnFinished { .. })
    ));

    let state = runtime.state.lock().await;
    assert!(
        (state.session_cost.usd - 0.5).abs() < 1e-9 && (state.session_cost.cny - 3.5).abs() < 1e-9,
        "reported cost must land in session_cost"
    );
    assert_eq!(state.session_cache_hit_tokens, 300);
    assert_eq!(state.session_cache_miss_tokens, 100);
    assert!(
        (state.session_cache_savings.usd - 0.2).abs() < 1e-9,
        "reported cache savings must land in session_cache_savings"
    );
}

/// A turn that spans multiple requests (tool call → follow-up) must price
/// every request, not just the final one. Context-shaped fields
/// (prompt_tokens) keep final-request semantics; cache totals cover the
/// whole turn.
#[tokio::test]
async fn multi_request_turn_accumulates_cost_across_requests() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};
    use crate::model::Usage;
    use crate::pricing::calculate_turn_cost;

    let usage_first = Usage {
        prompt_tokens: Some(1_000),
        completion_tokens: Some(200),
        total_tokens: Some(1_200),
        reasoning_tokens: None,
        prompt_cache_hit_tokens: Some(600),
        prompt_cache_miss_tokens: Some(400),
    };
    let usage_second = Usage {
        prompt_tokens: Some(1_500),
        completion_tokens: Some(50),
        total_tokens: Some(1_550),
        reasoning_tokens: None,
        prompt_cache_hit_tokens: Some(1_400),
        prompt_cache_miss_tokens: Some(100),
    };
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
            },
            AgentEvent::Done {
                usage: Some(usage_first.clone()),
            },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done {
                usage: Some(usage_second.clone()),
            },
        ],
    ]);
    let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools())
        .with_permission_mode(SharedPermissionMode::new(PermissionMode::Yolo));

    let mut rx = runtime.submit_user("please echo").await;
    let events = drain(&mut rx).await;
    let telemetry = events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::TurnFinished { telemetry, .. } => telemetry.as_ref(),
            _ => None,
        })
        .expect("turn finished with telemetry");

    let expected = {
        let first = calculate_turn_cost(&telemetry.effective_model, &usage_first);
        let second = calculate_turn_cost(&telemetry.effective_model, &usage_second);
        (first.usd + second.usd, first.cny + second.cny)
    };
    assert!(
        (telemetry.turn_cost.usd - expected.0).abs() < 1e-9
            && (telemetry.turn_cost.cny - expected.1).abs() < 1e-9,
        "turn cost must sum every request: got {:?}, want {expected:?}",
        telemetry.turn_cost
    );
    // Final-request semantics for the context-shaped fields.
    assert_eq!(telemetry.prompt_tokens, 1_500);
    assert_eq!(telemetry.completion_tokens, 50);
    // Whole-turn cache totals.
    assert_eq!(telemetry.cache_hit_tokens, Some(2_000));
    assert_eq!(telemetry.cache_miss_tokens, Some(500));
    // First turn of the session: session totals equal the turn's.
    assert!((telemetry.session_cost.usd - telemetry.turn_cost.usd).abs() < f64::EPSILON);
    assert_eq!(telemetry.session_cache_hit_tokens, 2_000);
    assert_eq!(telemetry.session_cache_miss_tokens, 500);
}

// ---------------------------------------------------------------------------
// request_write_root: the model-side doorbell for widening the write boundary
// ---------------------------------------------------------------------------

/// Registry + shared boundary for the root-grant tests: workspace tools and
/// the request tool built on ONE policy, exactly like a real launch.
fn root_grant_fixture(
    workspace: &std::path::Path,
) -> (ToolRegistry, crate::workspace_policy::WorkspacePolicy) {
    let policy = crate::workspace_policy::WorkspacePolicy::new(workspace.to_path_buf()).unwrap();
    let mut registry = crate::workspace_tools::workspace_tool_registry_from(policy.clone());
    registry.register(crate::root_grant::RequestWriteRootTool);
    (registry, policy)
}

fn request_root_script(target: &std::path::Path) -> Vec<AgentEvent> {
    vec![
        AgentEvent::ToolCallDelta {
            delta: tool_call_delta(
                "call_grant",
                "request_write_root",
                &serde_json::json!({
                    "path": target.to_string_lossy(),
                    "justification": "the build writes its artifacts there",
                })
                .to_string(),
            ),
        },
        AgentEvent::Done { usage: None },
    ]
}

/// The tool declares `deny_unknown_fields`, but the runtime intercepts it
/// before `Tool::run`, so nothing used to enforce that. It is not cosmetic: the
/// approval panel picks its action line by scanning arguments for the first
/// familiar key, and `command` outranks `path` — an extra key the schema
/// forbids could show the human text of the model's choosing while the grant
/// landed on `path`. The argument set is now validated before anyone is
/// prompted.
#[tokio::test]
async fn root_grant_with_unknown_arguments_bounces_without_prompting() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let (registry, policy) = root_grant_fixture(workspace.path());
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta(
                    "call_grant",
                    "request_write_root",
                    &serde_json::json!({
                        "path": outside.path().to_string_lossy(),
                        "justification": "the build writes its artifacts there",
                        // The decoy: forbidden by the schema, and the key the
                        // panel would rank ahead of `path`.
                        "command": "cat CHANGELOG.md",
                    })
                    .to_string(),
                ),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "ok".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::new(client, registry).with_boundary(Some(policy.clone()));

    let mut rx = runtime.submit_user("write there").await;
    let events = drain(&mut rx).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ApprovalRequired { .. })),
        "an argument set the schema forbids must never reach the human: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { result, .. }
                if result.tool_name == "request_write_root"
                    && result.status == ToolResultStatus::Error
                    && result.content.contains("invalid request_write_root arguments")
        )),
        "the model gets the precise reason: {events:?}"
    );
    assert_eq!(policy.granted_roots().len(), 1, "nothing granted");
}

/// Yolo auto-approves everything EXCEPT a root grant: widening the sandbox is
/// the one decision Yolo's containment story cannot delegate to itself.
#[tokio::test]
async fn yolo_mode_still_parks_a_root_grant() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let (registry, policy) = root_grant_fixture(workspace.path());
    let client = ScriptedClient::new(vec![request_root_script(outside.path())]);
    let runtime = AgentRuntime::new(client, registry)
        .with_boundary(Some(policy))
        .with_permission_mode(SharedPermissionMode::new(PermissionMode::Yolo));

    let mut rx = runtime.submit_user("build it").await;
    let events = drain(&mut rx).await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::ApprovalRequired { .. })),
        "a root grant must park even under Yolo: {events:?}"
    );
}

/// Neither standing consent channel may cover a root grant: not a config
/// `auto_allow` entry (exact tool name — this test spells it out in full),
/// not a recorded session approval.
#[tokio::test]
async fn standing_consents_never_cover_a_root_grant() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let (registry, policy) = root_grant_fixture(workspace.path());
    let client = ScriptedClient::new(vec![request_root_script(outside.path())]);
    let config = crate::config::AgentConfig {
        approval_auto_allow: vec!["request_write_root".to_string()],
        ..crate::config::AgentConfig::builtin()
    };
    let runtime = AgentRuntime::with_config(client, registry, config).with_boundary(Some(policy));
    runtime
        .state
        .lock()
        .await
        .session_approved
        .insert("request_write_root".to_string());

    let mut rx = runtime.submit_user("build it").await;
    let events = drain(&mut rx).await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::ApprovalRequired { .. })),
        "config auto_allow and session memory must not skip the human: {events:?}"
    );
}

/// The full approved path: the request parks (even under AcceptEdits), the
/// human approves, the boundary widens LIVE (a write_file into the new root
/// succeeds in the same session without any relaunch), the UI event fires,
/// and the request carried the model's justification to the prompt.
#[tokio::test]
async fn approved_root_grant_widens_the_boundary_live() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().canonicalize().unwrap();
    let (registry, policy) = root_grant_fixture(workspace.path());

    let file_in_new_root = target.join("artifact.txt");
    let client = ScriptedClient::new(vec![
        request_root_script(&target),
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta(
                    "call_write",
                    "write_file",
                    &serde_json::json!({
                        "path": file_in_new_root.to_string_lossy(),
                        "content": "built",
                    })
                    .to_string(),
                ),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    // AcceptEdits: write_file auto-approves, the root grant must NOT.
    let runtime = AgentRuntime::new(client, registry)
        .with_boundary(Some(policy.clone()))
        .with_permission_mode(SharedPermissionMode::new(PermissionMode::AcceptEdits));

    let mut rx = runtime.submit_user("build into the sibling dir").await;
    let first = drain(&mut rx).await;
    let request = first
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ApprovalRequired { request, .. } => Some(request.clone()),
            _ => None,
        })
        .expect("root grant parks under AcceptEdits");
    assert_eq!(
        request.justification.as_deref(),
        Some("the build writes its artifacts there"),
        "the model's reason must reach the prompt"
    );

    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    let events = drain(&mut rx).await;

    let granted_path = events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::RootGranted { path, .. } => Some(path.clone()),
            _ => None,
        })
        .expect("RootGranted event notifies the UI");
    assert_eq!(granted_path, target.display().to_string());
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallFinished { result, .. }
            if result.tool_name == "request_write_root"
                && result.status == ToolResultStatus::Success
                && result.content.contains("granted")
    )));
    // The load-bearing assertion: the SAME session's next tool call writes
    // into the just-granted root — no relaunch, no new registry.
    assert!(events.iter().any(|event| matches!(
        event,
        RuntimeEvent::ToolCallFinished { result, .. }
            if result.tool_name == "write_file" && result.status == ToolResultStatus::Success
    )));
    assert_eq!(std::fs::read_to_string(&file_in_new_root).unwrap(), "built");
    assert_eq!(
        policy.granted_roots().len(),
        2,
        "boundary records the grant"
    );
}

/// Denial is final and says so: the result tells the model not to re-request,
/// nothing is granted, and no UI event fires.
#[tokio::test]
async fn denied_root_grant_grants_nothing_and_says_so() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let (registry, policy) = root_grant_fixture(workspace.path());
    let client = ScriptedClient::new(vec![
        request_root_script(outside.path()),
        vec![
            AgentEvent::TextDelta {
                text: "ok".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::new(client, registry).with_boundary(Some(policy.clone()));

    let mut rx = runtime.submit_user("build it").await;
    drain(&mut rx).await;
    let mut rx = runtime.submit_approval(ApprovalDecision::Denied).await;
    let events = drain(&mut rx).await;

    let result = events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ToolCallFinished { result, .. }
                if result.tool_name == "request_write_root" =>
            {
                Some(result.clone())
            }
            _ => None,
        })
        .expect("denied call still records a result");
    assert_eq!(result.status, ToolResultStatus::Denied);
    assert!(
        result.content.contains("Do not request this path again"),
        "denial must be final and say so: {}",
        result.content
    );
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, RuntimeEvent::RootGranted { .. })),
        "no grant event on denial"
    );
    assert_eq!(policy.granted_roots().len(), 1, "boundary unchanged");
}

/// The grant persists with the session, like an `--add-dir`: after approval
/// the saved record carries the new root, so a resume restores it.
#[tokio::test]
async fn approved_root_grant_persists_into_the_session_record() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().canonicalize().unwrap();
    let (registry, policy) = root_grant_fixture(workspace.path());

    let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let mut record =
        crate::session_store::SessionRecord::new(workspace.path().to_path_buf(), "system");
    let session_id = record.id.clone();
    store.save(&mut record).unwrap();

    let client = ScriptedClient::new(vec![
        request_root_script(&target),
        vec![
            AgentEvent::TextDelta {
                text: "ok".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::from_session_record(
        client,
        registry,
        record,
        store,
        crate::config::AgentConfig::builtin(),
    )
    .with_boundary(Some(policy));

    let mut rx = runtime.submit_user("build it").await;
    drain(&mut rx).await;
    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    drain(&mut rx).await;
    runtime.shutdown().await; // flush the persistence actor

    let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let saved = store.load(&session_id).unwrap();
    assert!(
        saved.extra_roots.contains(&target),
        "the grant must survive in the record (resume restores it): {:?}",
        saved.extra_roots
    );
}

/// The TOCTOU pin: the prompt shows the directory the request resolved to at
/// prompt time, and the grant re-resolves on approval and must land on that
/// exact value. A symlink retargeted between the two (the requester CAN
/// write inside the workspace without approval, so it can shuffle links
/// there) is refused — the user never grants a directory they never saw.
///
/// Runs on Windows too: the policy-side symlink tests went cross-platform in
/// 93c4280, and this is the runtime half they explicitly delegate the
/// prompt-vs-grant equality to — green there means nothing if this half never
/// compiles on the platform.
#[tokio::test]
async fn root_grant_refuses_when_the_target_changes_under_the_approval() {
    let workspace = tempfile::tempdir().unwrap();
    let shown = tempfile::tempdir().unwrap();
    let swapped = tempfile::tempdir().unwrap();
    let shown = shown.path().canonicalize().unwrap();
    let swapped = swapped.path().canonicalize().unwrap();
    let link = workspace.path().join("build-cache");
    if !crate::test_symlinks::symlink_dir_for_test(&shown, &link) {
        return;
    }

    let (registry, policy) = root_grant_fixture(workspace.path());
    let client = ScriptedClient::new(vec![
        request_root_script(&link),
        vec![
            AgentEvent::TextDelta {
                text: "ok".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::new(client, registry).with_boundary(Some(policy.clone()));

    let mut rx = runtime.submit_user("cache the build").await;
    let first = drain(&mut rx).await;
    let request = first
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::ApprovalRequired { request, .. } => Some(request.clone()),
            _ => None,
        })
        .expect("resolvable request parks for approval");
    assert_eq!(
        request.resolved_target.as_deref(),
        Some(shown.display().to_string().as_str()),
        "the prompt names the RESOLVED directory, not the link spelling"
    );

    // Retarget the link while the human is looking at the prompt. The first
    // creation succeeded, so this process provably holds the privilege — a
    // failure here is a test bug and the helper panics rather than skips.
    crate::test_symlinks::remove_symlink_dir_for_test(&link);
    assert!(
        crate::test_symlinks::symlink_dir_for_test(&swapped, &link),
        "re-linking must work once the first symlink succeeded"
    );

    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    let events = drain(&mut rx).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { result, .. }
                if result.tool_name == "request_write_root"
                    && result.status == ToolResultStatus::Error
                    && result.content.contains("changed underneath the approval")
        )),
        "the grant must refuse the swapped target: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::RootGranted { .. })),
        "no grant event may fire"
    );
    assert_eq!(
        policy.granted_roots().len(),
        1,
        "the boundary must be untouched: {:?}",
        policy.granted_roots()
    );
}

/// A request that can never be granted (unresolvable path — same for the
/// home/root refusals) bounces straight back to the model with the reason;
/// the human is never prompted for a grant that cannot happen.
#[tokio::test]
async fn unresolvable_root_grant_bounces_without_prompting() {
    let workspace = tempfile::tempdir().unwrap();
    let (registry, policy) = root_grant_fixture(workspace.path());
    let missing = workspace.path().join("does-not-exist");
    let client = ScriptedClient::new(vec![
        request_root_script(&missing),
        vec![
            AgentEvent::TextDelta {
                text: "ok".to_string(),
            },
            AgentEvent::Done { usage: None },
        ],
    ]);
    let runtime = AgentRuntime::new(client, registry).with_boundary(Some(policy.clone()));

    let mut rx = runtime.submit_user("write there").await;
    let events = drain(&mut rx).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ApprovalRequired { .. })),
        "no human prompt for an impossible grant: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ToolCallFinished { result, .. }
                if result.tool_name == "request_write_root"
                    && result.status == ToolResultStatus::Error
                    && result.content.contains("cannot resolve")
        )),
        "the model gets the precise reason: {events:?}"
    );
    assert_eq!(policy.granted_roots().len(), 1, "nothing granted");
}

/// The `approval_preview` glue: the boundary-bearing runtime must hand the
/// panel a real preview built from the call's own arguments, and only for
/// call shapes that have one. All three collapses survived the sweep —
/// `None` (the human approves a write they never saw), `Some("")` and
/// `Some("xyzzy")` (a preview unrelated to the call) — because every
/// preview test exercised `build_approval_preview` directly, never the
/// method the runtime actually consults.
#[tokio::test]
async fn approval_preview_renders_through_the_live_boundary() {
    let workspace = tempfile::tempdir().unwrap();
    let policy =
        crate::workspace_policy::WorkspacePolicy::new(workspace.path().to_path_buf()).unwrap();
    let boundaryless = AgentRuntime::new(ScriptedClient::new(vec![]), ToolRegistry::default());
    let runtime = AgentRuntime::new(ScriptedClient::new(vec![]), ToolRegistry::default())
        .with_boundary(Some(policy));

    let write = crate::tool::ToolCall {
        id: "c1".to_string(),
        name: "write_file".to_string(),
        arguments: serde_json::json!({
            "path": "notes.txt",
            "content": "PREVIEW_TOKEN_a1b2\n",
        }),
    };
    let preview = runtime
        .approval_preview(&write)
        .expect("a write_file against the live boundary must carry a preview");
    assert!(
        preview.contains("PREVIEW_TOKEN_a1b2"),
        "the preview must show the call's own content, got: {preview}"
    );

    // No preview-able shape → no preview; the glue must not invent one.
    let shell = crate::tool::ToolCall {
        id: "c2".to_string(),
        name: "shell".to_string(),
        arguments: serde_json::json!({"command": "true"}),
    };
    assert_eq!(runtime.approval_preview(&shell), None);
    // And with no boundary there is nothing to resolve against.
    assert_eq!(boundaryless.approval_preview(&write), None);
}
