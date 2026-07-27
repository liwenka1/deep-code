//! Axum HTTP/SSE server for the local runtime API.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use async_stream::stream;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, middleware};
use deep_code_agent::{
    AgentConfig, ApprovalDecision, ApprovalRequest, JsonSessionStore, LaunchedRuntime,
    RuntimeEvent, SessionId, SessionRecord, SessionStore, launch_runtime, now_ms,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};

use crate::auth::{RUNTIME_TOKEN_ENV, token_matches};
use crate::events::{EnvelopeStream, RuntimeEnvelope};

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 7878;

#[derive(Debug, Clone)]
pub struct RuntimeServerOptions {
    pub host: String,
    pub port: u16,
    pub auth_token: Option<String>,
    pub workspace: PathBuf,
    pub resume_session_id: Option<String>,
    /// Headless/unattended: auto-deny (never park) any approval that reaches the
    /// server, so a gated tool that slipped past auto-allow can't hang the turn
    /// on a `/v1/approvals` callback that never arrives.
    pub autonomous_approvals: bool,
}

impl RuntimeServerOptions {
    /// CLI `--auth-token` wins; otherwise fall back to `DEEP_CODE_RUNTIME_TOKEN`.
    #[must_use]
    pub fn resolve_auth_token(mut self) -> Self {
        if self.auth_token.is_none() {
            self.auth_token = std::env::var(RUNTIME_TOKEN_ENV)
                .ok()
                .filter(|token| !token.trim().is_empty());
        }
        self
    }
}

impl Default for RuntimeServerOptions {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
            auth_token: None,
            workspace: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            resume_session_id: None,
            autonomous_approvals: false,
        }
        .resolve_auth_token()
    }
}

#[derive(Clone)]
pub(crate) struct AppState {
    version: String,
    auth_token: Option<String>,
    pub(crate) runtime: Arc<Mutex<LaunchedRuntime>>,
    approval: Arc<Mutex<Option<PendingApproval>>>,
    active_turn: Arc<StdMutex<Option<String>>>,
    /// See [`RuntimeServerOptions::autonomous_approvals`].
    autonomous_approvals: bool,
}

struct PendingApproval {
    request: ApprovalRequest,
    respond: oneshot::Sender<ApprovalDecision>,
}

struct ActiveTurnLease {
    active_turn: Arc<StdMutex<Option<String>>>,
}

impl Drop for ActiveTurnLease {
    fn drop(&mut self) {
        if let Ok(mut active_turn) = self.active_turn.lock() {
            *active_turn = None;
        }
    }
}

/// Cleanup for an approval parked on a client that may disconnect: if the SSE
/// stream is dropped while waiting on `/v1/approvals`, remove the stale slot
/// entry (so later callers get an honest 409 instead of "accepted") and deny
/// the runtime's pending approval so the turn completes instead of wedging
/// mid-approval. Disarmed on the normal resume path.
struct ApprovalSlotGuard {
    slot: Arc<Mutex<Option<PendingApproval>>>,
    runtime: Arc<Mutex<LaunchedRuntime>>,
    call_id: String,
    armed: bool,
}

impl ApprovalSlotGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ApprovalSlotGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let slot = Arc::clone(&self.slot);
        let runtime = Arc::clone(&self.runtime);
        let call_id = std::mem::take(&mut self.call_id);
        tokio::spawn(async move {
            let removed = {
                let mut slot = slot.lock().await;
                if slot
                    .as_ref()
                    .is_some_and(|pending| pending.request.call_id == call_id)
                {
                    slot.take()
                } else {
                    None
                }
            };
            // Only deny if the stale entry was still ours; a no-longer-pending
            // approval makes this a benign no-op on the runtime side.
            if removed.is_some() {
                let runtime = runtime.lock().await;
                drop(
                    runtime
                        .handle
                        .submit_approval(ApprovalDecision::Denied)
                        .await,
                );
            }
        });
    }
}

/// A host is loopback-only if it names localhost or parses to a loopback IP.
/// Unclassifiable hostnames are treated as non-loopback, so a missing auth
/// token there fails closed (refuses to bind) rather than exposing the agent.
fn host_is_loopback(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false)
}

#[cfg(test)]
mod host_tests {
    use super::host_is_loopback;

    #[test]
    fn classifies_loopback_hosts() {
        assert!(host_is_loopback("127.0.0.1"));
        assert!(host_is_loopback("::1"));
        assert!(host_is_loopback("localhost"));
        assert!(host_is_loopback("LOCALHOST"));
        assert!(!host_is_loopback("0.0.0.0"));
        assert!(!host_is_loopback("192.168.1.9"));
        assert!(!host_is_loopback("example.com"));
    }
}

pub async fn run_http_server(options: RuntimeServerOptions) -> Result<()> {
    let options = options.resolve_auth_token();
    if options.auth_token.is_none() && !host_is_loopback(&options.host) {
        anyhow::bail!(
            "refusing to bind non-loopback host '{}' without an auth token: \
             any machine that can reach it could drive the agent. Pass \
             --auth-token <TOKEN>, set {}, or bind 127.0.0.1.",
            options.host,
            RUNTIME_TOKEN_ENV
        );
    }
    let loaded = AgentConfig::load(&options.workspace);
    for warning in &loaded.report.warnings {
        eprintln!("config warning: {warning}");
    }
    let config = loaded.config;
    let resume = load_resume_record(&options)?;
    let launched = launch_runtime(&config, options.workspace.clone(), resume);
    for warning in &launched.warnings {
        eprintln!("warning: {warning}");
    }
    eprintln!(
        "deep-code runtime API listening on http://{}:{} ({})",
        options.host, options.port, launched.backend_label
    );
    if options.auth_token.is_some() {
        eprintln!("auth: bearer token required for /v1/* routes");
    } else {
        eprintln!(
            "警告：未设置 auth token，本机任意进程都可调用 /v1/* 驱动 agent 执行工具。\
             建议 --auth-token <TOKEN> 或设置 DEEP_CODE_RUNTIME_TOKEN。"
        );
    }

    let state = AppState {
        version: env!("CARGO_PKG_VERSION").to_string(),
        auth_token: options.auth_token,
        runtime: Arc::new(Mutex::new(launched)),
        approval: Arc::new(Mutex::new(None)),
        active_turn: Arc::new(StdMutex::new(None)),
        autonomous_approvals: options.autonomous_approvals,
    };

    let protected = Router::new()
        .route("/v1/prompt", post(prompt_sse))
        .route("/v1/approvals", post(submit_approval))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .with_state(state.clone());

    // Deliberately no CORS layer: the consumers are same-host CLI/CI clients
    // (curl, the GitHub bot). A permissive CORS policy would let any web page
    // script drive the local agent when no auth token is configured.
    let app = Router::new()
        .route("/health", get(health))
        .merge(protected)
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", options.host, options.port)
        .parse()
        .context("invalid listen address")?;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    axum::serve(listener, app)
        .await
        .context("runtime HTTP server exited with error")
}

fn load_resume_record(options: &RuntimeServerOptions) -> Result<Option<SessionRecord>> {
    let Some(id) = options.resume_session_id.as_deref() else {
        return Ok(None);
    };
    let store = JsonSessionStore::for_workspace(&options.workspace)?;
    Ok(Some(store.load(&SessionId::parse(id)?)?))
}

#[derive(Serialize, Deserialize)]
struct HealthResponse {
    status: String,
    version: String,
    auth_required: bool,
}

async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        version: state.version,
        auth_required: state.auth_token.is_some(),
    })
}

async fn require_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(expected) = &state.auth_token
        && !token_matches(expected, request.headers())
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "missing or invalid runtime token"
            })),
        )
            .into_response();
    }
    next.run(request).await
}

#[derive(Deserialize)]
struct PromptRequest {
    prompt: String,
}

fn thread_sse_event(envelope: RuntimeEnvelope) -> Event {
    Event::default()
        .event(envelope.item.kind.clone())
        .json_data(envelope)
        .unwrap_or_else(|_| Event::default().data("serialization error"))
}

fn acquire_active_turn(state: &AppState, thread_id: &str) -> Result<ActiveTurnLease, ApiError> {
    let mut active_turn = state
        .active_turn
        .lock()
        .map_err(|_| ApiError::internal("active turn gate poisoned"))?;
    if let Some(active_thread_id) = active_turn.as_ref() {
        return Err(ApiError::conflict(format!(
            "runtime already has an active turn on thread '{active_thread_id}'"
        )));
    }
    *active_turn = Some(thread_id.to_string());
    Ok(ActiveTurnLease {
        active_turn: Arc::clone(&state.active_turn),
    })
}

async fn prompt_sse(
    State(state): State<AppState>,
    Json(body): Json<PromptRequest>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    if body.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt must not be empty"));
    }
    let prompt = body.prompt;
    let thread_id = format!("prompt_{}", now_ms());

    let active_turn_lease = acquire_active_turn(&state, &thread_id)?;
    let runtime = state.runtime.clone();
    let approval_gate = state.approval.clone();
    let autonomous_approvals = state.autonomous_approvals;
    let stream = stream! {
        let _active_turn_lease = active_turn_lease;
        let mut envelopes = EnvelopeStream::new(thread_id);
        yield Ok(thread_sse_event(envelopes.manual(
            "user.message",
            serde_json::json!({ "content": prompt }),
        )));

        let mut event_stream = {
            let runtime = runtime.lock().await;
            runtime.handle.submit_user(prompt).await
        };

        loop {
            let mut resume_after_approval = false;
            while let Some(event) = event_stream.recv().await {
                yield Ok(thread_sse_event(envelopes.event(&event)));

                match event {
                    RuntimeEvent::ApprovalRequired { request, .. } => {
                        let decision = if autonomous_approvals {
                            // Headless/unattended: no HTTP client will POST to
                            // /v1/approvals, so parking here would hang the turn
                            // until the connection dies. Deny deterministically
                            // instead — the agent records a denied result and
                            // continues (it never blocks on the callback).
                            ApprovalDecision::Denied
                        } else {
                            let (tx, rx) = oneshot::channel();
                            let call_id = request.call_id.clone();
                            {
                                let mut slot = approval_gate.lock().await;
                                *slot = Some(PendingApproval {
                                    request,
                                    respond: tx,
                                });
                            }
                            let mut slot_guard = ApprovalSlotGuard {
                                slot: approval_gate.clone(),
                                runtime: runtime.clone(),
                                call_id,
                                armed: true,
                            };
                            let decision = rx.await.unwrap_or(ApprovalDecision::Denied);
                            slot_guard.disarm();
                            {
                                let mut slot = approval_gate.lock().await;
                                *slot = None;
                            }
                            decision
                        };
                        let runtime = runtime.lock().await;
                        event_stream = runtime.handle.submit_approval(decision).await;
                        resume_after_approval = true;
                        break;
                    }
                    RuntimeEvent::TurnFinished { .. }
                    | RuntimeEvent::TurnCancelled { .. }
                    | RuntimeEvent::Error { .. } => {
                        return;
                    }
                    _ => {}
                }
            }
            if !resume_after_approval {
                break;
            }
        }
    };

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("keepalive"),
    ))
}

#[derive(Deserialize)]
struct ApprovalRequestBody {
    call_id: String,
    decision: ApprovalDecisionWire,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ApprovalDecisionWire {
    Approved,
    ApprovedForSession,
    Denied,
}

impl From<ApprovalDecisionWire> for ApprovalDecision {
    fn from(value: ApprovalDecisionWire) -> Self {
        match value {
            ApprovalDecisionWire::Approved => Self::Approved,
            ApprovalDecisionWire::ApprovedForSession => Self::ApprovedForSession,
            ApprovalDecisionWire::Denied => Self::Denied,
        }
    }
}

#[derive(Serialize)]
struct ApprovalResponse {
    accepted: bool,
    call_id: String,
}

async fn submit_approval(
    State(state): State<AppState>,
    Json(body): Json<ApprovalRequestBody>,
) -> Result<Json<ApprovalResponse>, ApiError> {
    resolve_pending_approval(state, body).await
}

async fn resolve_pending_approval(
    state: AppState,
    body: ApprovalRequestBody,
) -> Result<Json<ApprovalResponse>, ApiError> {
    let pending = {
        let mut slot = state.approval.lock().await;
        slot.take()
    };
    let Some(pending) = pending else {
        return Err(ApiError::conflict("no pending approval"));
    };
    if pending.request.call_id != body.call_id {
        let expected = pending.request.call_id.clone();
        let mut slot = state.approval.lock().await;
        *slot = Some(pending);
        return Err(ApiError::bad_request(format!(
            "call_id mismatch: expected '{expected}', got '{}'",
            body.call_id
        )));
    }
    if pending.respond.send(body.decision.into()).is_err() {
        // The prompt stream that parked this approval is gone (client
        // disconnected); claiming "accepted" would be a lie.
        return Err(ApiError::conflict(
            "approval stream disconnected; pending approval is stale",
        ));
    }
    Ok(Json(ApprovalResponse {
        accepted: true,
        call_id: body.call_id,
    }))
}

#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: message.into(),
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
}

impl From<deep_code_agent::SessionStoreError> for ApiError {
    fn from(error: deep_code_agent::SessionStoreError) -> Self {
        match error {
            deep_code_agent::SessionStoreError::NotFound { id } => Self {
                status: StatusCode::NOT_FOUND,
                message: format!("session '{id}' not found"),
            },
            other => Self {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: other.to_string(),
            },
        }
    }
}

impl From<deep_code_agent::ToolError> for ApiError {
    fn from(error: deep_code_agent::ToolError) -> Self {
        match &error {
            deep_code_agent::ToolError::ExecutionFailed { message, .. }
                if message.contains("does not exist") =>
            {
                Self::not_found(error.to_string())
            }
            _ => Self::internal(error.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
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
                decision: ApprovalDecisionWire::Approved,
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
        // Wire contract (the bot workflow's jq depends on all of this): first
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

    fn tool_calling_state(workspace: PathBuf, autonomous_approvals: bool) -> AppState {
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
                deep_code_agent::SubAgentManager::new(workspace, 2),
            )),
            job_store: deep_code_agent::JobStore::default(),
            stop_hook: Box::new(|| {}),
            offline: false,
            warnings: Vec::new(),
            permission_mode: deep_code_agent::SharedPermissionMode::default(),
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
        let dir = tempfile::tempdir().unwrap();
        let state = tool_calling_state(dir.path().to_path_buf(), false);
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
        let dir = tempfile::tempdir().unwrap();
        let state = tool_calling_state(dir.path().to_path_buf(), true);
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
}
