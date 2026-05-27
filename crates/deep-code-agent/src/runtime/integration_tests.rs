use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_stream::try_stream;
use futures_core::Stream;

use super::*;
use crate::client::AgentEventStream;
use crate::error::AgentResult;
use crate::event::AgentEvent;
use crate::model::{FunctionCallDelta, ToolCallDelta};
use crate::tool::{MockEchoTool, ToolRegistry, ToolResultStatus};

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
        let stream: Pin<Box<dyn Stream<Item = AgentResult<AgentEvent>> + Send>> =
            Box::pin(stream);
        Ok(stream)
    }
}

fn tool_call_delta(id: &str, name: &str, arguments: &str) -> ToolCallDelta {
    ToolCallDelta {
        index: Some(0),
        id: Some(id.to_string()),
        call_type: Some("function".to_string()),
        function: Some(FunctionCallDelta {
            name: Some(name.to_string()),
            arguments: Some(arguments.to_string()),
        }),
    }
}

async fn drain(rx: &mut RuntimeEventReceiver) -> Vec<RuntimeEvent> {
    let mut out = Vec::new();
    while let Some(event) = rx.recv().await {
        out.push(event);
    }
    out
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
            RuntimeEvent::ToolResult { result } => {
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
        RuntimeEvent::ToolResult { result } => Some(result),
        _ => None,
    });
    let denied = denied.expect("expected ToolResult on deny path");
    assert_eq!(denied.status, ToolResultStatus::Denied);

    let messages = runtime.session_messages().await;
    assert!(
        messages
            .iter()
            .any(|m| matches!(m.role, crate::message::Role::Tool)
                && m.content.contains("denied"))
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
async fn turn_snapshots_emit_checkpoint_events() {
    let workspace = tempfile::tempdir().unwrap();
    let client = ScriptedClient::new(vec![vec![
        AgentEvent::TextDelta {
            text: "done".to_string(),
        },
        AgentEvent::Done { usage: None },
    ]]);
    let runtime =
        AgentRuntime::new(client, ToolRegistry::default()).with_checkpoints(workspace.path());

    let mut rx = runtime.submit_user("hi").await;
    let events = drain(&mut rx).await;

    let before = events.iter().find_map(|event| match event {
        RuntimeEvent::CheckpointCreated { id, label } if label == "before_turn" => {
            Some(id.0.clone())
        }
        _ => None,
    });
    let after = events.iter().find_map(|event| match event {
        RuntimeEvent::CheckpointCreated { id, label } if label == "after_turn" => {
            Some(id.0.clone())
        }
        _ => None,
    });
    assert!(before.is_some(), "expected before_turn checkpoint");
    assert!(after.is_some(), "expected after_turn checkpoint");
}

#[tokio::test]
async fn submit_approval_without_pending_emits_error() {
    let client = ScriptedClient::new(vec![]);
    let runtime = AgentRuntime::new(client, ToolRegistry::default());

    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    let events = drain(&mut rx).await;
    assert!(matches!(events.first(), Some(RuntimeEvent::Error { .. })));
}

#[tokio::test]
async fn write_file_appends_lsp_diagnostics_to_session() {
    use std::sync::Arc;

    use async_trait::async_trait;

    use crate::lsp::{
        Diagnostic, DiagnosticRange, Language, LspConfig, LspManager, LspTransport, Severity,
    };
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
    let manager = LspManager::new(LspConfig::default(), dir.path().to_path_buf());
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
        RuntimeEvent::ToolResult { result } => Some(result),
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
        &crate::config::AgentConfig::default(),
    )
    .unwrap();

    let session_id = runtime.session_id().await.expect("session id");
    let mut rx = runtime.submit_user("hi").await;
    drain(&mut rx).await;
    runtime.shutdown().await;

    let store =
        crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let record = store.load(&session_id).unwrap();
    assert_eq!(record.messages.len(), 3);
    assert_eq!(record.turns.len(), 1);
    assert_eq!(record.turns[0].user_prompt, "hi");
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
        &crate::config::AgentConfig::default(),
    )
    .unwrap();

    let session_id = runtime.session_id().await.expect("session id");
    let mut rx = runtime.submit_user("hi").await;
    drain(&mut rx).await;
    runtime.shutdown().await;

    let store =
        crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let record = store.load(&session_id).unwrap();
    assert_eq!(record.turns.len(), 1);
    assert_eq!(record.turns[0].user_prompt, "hi");
    assert!(record.turns[0].finished_at_ms.is_some());
}

#[tokio::test]
async fn persistence_saves_tool_results_in_turn() {
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
        &crate::config::AgentConfig::default(),
    )
    .unwrap();

    let session_id = runtime.session_id().await.expect("session id");
    let mut rx = runtime.submit_user("please echo").await;
    drain(&mut rx).await;
    let mut rx = runtime.submit_approval(ApprovalDecision::Approved).await;
    drain(&mut rx).await;
    runtime.shutdown().await;

    let store =
        crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let record = store.load(&session_id).unwrap();
    assert_eq!(record.turns.len(), 1);
    assert_eq!(record.turns[0].user_prompt, "please echo");
    assert_eq!(record.turns[0].tool_results.len(), 1);
    assert_eq!(
        record.turns[0].tool_results[0].tool_name,
        MockEchoTool::NAME
    );
    assert_eq!(record.turns[0].tool_results[0].content, "mock_echo: hi");
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
        &crate::config::AgentConfig::default(),
    )
    .unwrap();

    let session_id = runtime.session_id().await.expect("session id");
    let mut rx = runtime.submit_user("first").await;
    drain(&mut rx).await;
    runtime.shutdown().await;

    let store =
        crate::session_store::JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let record = store.load(&session_id).unwrap();
    assert_eq!(record.messages.len(), 3);

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
        AgentConfig::default(),
    );
    let mut rx = resumed.submit_user("second").await;
    drain(&mut rx).await;
    let messages = resumed.session_messages().await;
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[3].content, "second");
    assert_eq!(messages[4].content, "world");
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
    let config = AgentConfig {
        model: AUTO_MODEL.to_string(),
        ..AgentConfig::default()
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
    let config = AgentConfig {
        model: AUTO_MODEL.to_string(),
        ..AgentConfig::default()
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
    assert!(matches!(events.last(), Some(RuntimeEvent::Error { .. })));
}
