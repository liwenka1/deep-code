use super::*;
use deep_code_agent::DoctorReport;
use serde_json::json;
use std::time::Duration;

static RUNTIME_TOKEN_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_runtime_token_env() -> std::sync::MutexGuard<'static, ()> {
    RUNTIME_TOKEN_ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn test_state(workspace: PathBuf, auth_token: Option<String>) -> AppState {
    AppState {
        version: "0.1.0".to_string(),
        auth_token,
        runtime: Arc::new(Mutex::new(launch_runtime(
            &AgentConfig::default(),
            workspace,
            None,
        ))),
        approval: Arc::new(Mutex::new(None)),
        active_turn: Arc::new(StdMutex::new(None)),
        autonomous_approvals: false,
    }
}

fn test_router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/v1/prompt", post(prompt_sse))
        .route("/v1/approvals", post(submit_approval))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .with_state(state.clone());

    Router::new()
        .route("/health", get(health))
        .merge(protected)
        .with_state(state)
}

async fn spawn_test_server(state: AppState) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = test_router(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

fn envelopes_from_sse(body: &str) -> Vec<RuntimeEnvelope> {
    body.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<RuntimeEnvelope>(line).ok())
        .collect()
}

#[test]
fn default_options_use_localhost() {
    let options = RuntimeServerOptions::default();
    assert_eq!(options.host, DEFAULT_HOST);
    assert_eq!(options.port, DEFAULT_PORT);
}

#[test]
fn resolve_auth_token_falls_back_to_env() {
    let _guard = lock_runtime_token_env();
    unsafe {
        std::env::set_var(RUNTIME_TOKEN_ENV, "from-env");
    }
    let options = RuntimeServerOptions {
        auth_token: None,
        ..RuntimeServerOptions {
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
            auth_token: None,
            workspace: PathBuf::from("."),
            extra_roots: Vec::new(),
            resume_session_id: None,
            autonomous_approvals: false,
        }
    }
    .resolve_auth_token();
    assert_eq!(options.auth_token.as_deref(), Some("from-env"));
    unsafe {
        std::env::remove_var(RUNTIME_TOKEN_ENV);
    }
}

#[test]
fn cli_auth_token_overrides_env() {
    let _guard = lock_runtime_token_env();
    unsafe {
        std::env::set_var(RUNTIME_TOKEN_ENV, "from-env");
    }
    let options = RuntimeServerOptions {
        auth_token: Some("from-cli".to_string()),
        ..RuntimeServerOptions {
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
            auth_token: None,
            workspace: PathBuf::from("."),
            extra_roots: Vec::new(),
            resume_session_id: None,
            autonomous_approvals: false,
        }
    }
    .resolve_auth_token();
    assert_eq!(options.auth_token.as_deref(), Some("from-cli"));
    unsafe {
        std::env::remove_var(RUNTIME_TOKEN_ENV);
    }
}

#[tokio::test]
async fn stale_approval_after_disconnect_returns_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path().to_path_buf(), None);
    let (tx, rx) = oneshot::channel();
    // The prompt stream that parked this approval is gone.
    drop(rx);
    {
        let mut slot = state.approval.lock().await;
        *slot = Some(PendingApproval {
            request: serde_json::from_value(json!({
                "call_id": "call-1",
                "tool_name": "shell",
                "description": "run",
                "arguments": {}
            }))
            .unwrap(),
            respond: tx,
        });
    }
    let result = resolve_pending_approval(
        state,
        ApprovalRequestBody {
            call_id: "call-1".to_string(),
            decision: ApprovalDecision::Approved,
        },
    )
    .await;
    let Err(error) = result else {
        panic!("a dead approval receiver must not be reported as accepted");
    };
    assert_eq!(error.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn health_route_via_router_smoke() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path().to_path_buf(), None);
    let addr = spawn_test_server(state).await;

    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let body: HealthResponse = response.json().await.unwrap();
    assert_eq!(body.status, "ok");
    let _ = DoctorReport::collect(dir.path(), &AgentConfig::default());
}

#[tokio::test]
async fn protected_routes_require_auth_token() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path().to_path_buf(), Some("secret123".to_string()));
    let addr = spawn_test_server(state).await;
    let client = reqwest::Client::new();

    let unauth = client
        .post(format!("http://{addr}/v1/prompt"))
        .json(&json!({ "prompt": "hi" }))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    let authed = client
        .post(format!("http://{addr}/v1/prompt"))
        .header("Authorization", "Bearer secret123")
        .json(&json!({ "prompt": "hi" }))
        .send()
        .await
        .unwrap();
    assert_eq!(authed.status(), StatusCode::OK);
}

#[tokio::test]
async fn prompt_sse_returns_assistant_delta_and_turn_completed() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path().to_path_buf(), None);
    let addr = spawn_test_server(state).await;
    let client = reqwest::Client::new();

    let response = client
        .post(format!("http://{addr}/v1/prompt"))
        .json(&json!({ "prompt": "hello runtime test" }))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let body = response.text().await.unwrap();
    assert!(
        body.contains("event: assistant.delta"),
        "expected assistant.delta SSE events, got: {body}"
    );
    assert!(
        body.contains("event: turn.completed"),
        "expected turn.completed SSE event, got: {body}"
    );
    // Wire contract (shared by SSE clients and the headless `stream-json`
    // output, which render the same envelopes): first
    // envelope is the user message, seq starts at 1 and increases
    // monotonically, and assistant.delta payloads carry `.text`.
    let envelopes = envelopes_from_sse(&body);
    let first = envelopes.first().expect("at least one envelope");
    assert_eq!(first.item.kind, "user.message");
    assert_eq!(first.seq, 1);
    assert!(
        envelopes.windows(2).all(|pair| pair[1].seq > pair[0].seq),
        "seq must increase monotonically"
    );
    assert!(
        envelopes.iter().all(|envelope| envelope.item.item_id
            == format!("{}_item_{}", envelope.thread_id, envelope.seq)),
        "item_id shape must stay stable"
    );
    let delta_text = envelopes
        .iter()
        .filter(|envelope| envelope.item.kind == "assistant.delta")
        .filter_map(|envelope| envelope.item.payload.get("text")?.as_str())
        .collect::<String>();
    assert!(
        !delta_text.is_empty(),
        "assistant.delta payload must expose `.text` for the jq consumer"
    );
}

#[tokio::test]
async fn second_concurrent_prompt_is_rejected_with_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let state = test_state(dir.path().to_path_buf(), None);
    let _lease = acquire_active_turn(&state, "prompt_busy").expect("first lease");
    let error = acquire_active_turn(&state, "prompt_second")
        .err()
        .expect("second concurrent turn must be rejected");
    assert_eq!(error.status, StatusCode::CONFLICT);
}

/// Minimal scripted client: first turn requests one `mock_echo` tool
/// call, the resumed turn finishes with text — enough to drive the HTTP
/// approval flow end to end without a real model.
struct ToolCallingClient {
    turns: StdMutex<u32>,
}

#[async_trait::async_trait]
impl deep_code_agent::LlmClient for ToolCallingClient {
    fn provider_name(&self) -> &'static str {
        "scripted"
    }

    fn model(&self) -> &str {
        "scripted"
    }

    async fn stream_chat(
        &self,
        _request: deep_code_agent::ChatRequest,
    ) -> deep_code_agent::AgentResult<deep_code_agent::AgentEventStream> {
        let turn = {
            let mut turns = self.turns.lock().unwrap();
            let current = *turns;
            *turns += 1;
            current
        };
        let stream = async_stream::try_stream! {
            if turn == 0 {
                yield deep_code_agent::AgentEvent::ToolCallDelta {
                    delta: deep_code_agent::ToolCallDelta {
                        index: Some(0),
                        id: Some("call_1".to_string()),
                        call_type: Some("function".to_string()),
                        function: Some(deep_code_agent::FunctionCallDelta {
                            name: Some(deep_code_agent::MockEchoTool::NAME.to_string()),
                            arguments: Some(r#"{"message":"hello"}"#.to_string()),
                        }),
                    },
                };
            } else {
                yield deep_code_agent::AgentEvent::TextDelta {
                    text: "done".to_string(),
                };
            }
            yield deep_code_agent::AgentEvent::Done { usage: None };
        };
        Ok(Box::pin(stream))
    }
}

fn tool_calling_state(autonomous_approvals: bool) -> AppState {
    let runtime = deep_code_agent::AgentRuntime::new(
        ToolCallingClient {
            turns: StdMutex::new(0),
        },
        deep_code_agent::ToolRegistry::with_mock_tools(),
    );
    let launched = LaunchedRuntime {
        handle: Arc::new(runtime),
        backend_label: "scripted".to_string(),
        session_id: None,
        subagent_manager: Arc::new(std::sync::RwLock::new(
            deep_code_agent::SubAgentManager::new(2),
        )),
        job_store: deep_code_agent::JobStore::default(),
        stop_hook: Box::new(|| {}),
        offline: false,
        warnings: Vec::new(),
        permission_mode: deep_code_agent::SharedPermissionMode::default(),
        extra_roots: Vec::new(),
    };
    AppState {
        version: "0.1.0".to_string(),
        auth_token: None,
        runtime: Arc::new(Mutex::new(launched)),
        approval: Arc::new(Mutex::new(None)),
        active_turn: Arc::new(StdMutex::new(None)),
        autonomous_approvals,
    }
}

#[tokio::test]
async fn approval_rejects_mismatched_call_id() {
    let state = tool_calling_state(false);
    let addr = spawn_test_server(state).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let prompt_url = format!("http://{addr}/v1/prompt");
    let approvals_url = format!("http://{addr}/v1/approvals");

    let prompt_client = client.clone();
    let prompt_handle = tokio::spawn(async move {
        prompt_client
            .post(prompt_url)
            .json(&json!({ "prompt": "hello" }))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    let bad = client
        .post(&approvals_url)
        .json(&json!({ "call_id": "wrong-id", "decision": "approved" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);

    let good = client
        .post(&approvals_url)
        .json(&json!({ "call_id": "call_1", "decision": "approved" }))
        .send()
        .await
        .unwrap();
    assert_eq!(good.status(), StatusCode::OK);

    let body = prompt_handle.await.unwrap();
    assert!(body.contains("event: approval.required"));
    assert!(body.contains("event: turn.completed"));
}

#[tokio::test]
async fn autonomous_mode_auto_denies_instead_of_hanging() {
    let state = tool_calling_state(true);
    let addr = spawn_test_server(state).await;
    // A short client timeout is the point: in interactive mode the turn
    // would park forever (no one POSTs /v1/approvals) and trip this timeout.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    let body = client
        .post(format!("http://{addr}/v1/prompt"))
        .json(&json!({ "prompt": "hello" }))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // Approval was surfaced, then auto-denied, and the turn still finished —
    // no /v1/approvals call was ever made.
    assert!(
        body.contains("event: approval.required"),
        "expected approval.required, got: {body}"
    );
    assert!(
        body.contains("event: turn.completed"),
        "expected auto-denied turn to complete, got: {body}"
    );
}
