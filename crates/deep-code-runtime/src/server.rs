//! Axum HTTP/SSE server for the local runtime API.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_stream::stream;
use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, middleware};
use chrono::Utc;
use deep_code_agent::{
    AgentConfig, ApprovalDecision, ApprovalRequest, JsonSessionStore, LaunchedRuntime,
    RuntimeEvent, SessionId, SessionRecord, SessionStore, launch_runtime,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tower_http::cors::{Any, CorsLayer};

use crate::auth::{RUNTIME_TOKEN_ENV, token_matches};

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 7878;

#[derive(Debug, Clone)]
pub struct RuntimeServerOptions {
    pub host: String,
    pub port: u16,
    pub auth_token: Option<String>,
    pub workspace: PathBuf,
    pub resume_session_id: Option<String>,
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
        }
        .resolve_auth_token()
    }
}

#[derive(Clone)]
pub(crate) struct AppState {
    version: String,
    pub(crate) workspace: PathBuf,
    auth_token: Option<String>,
    pub(crate) runtime: Arc<Mutex<LaunchedRuntime>>,
    approval: Arc<Mutex<Option<PendingApproval>>>,
}

impl AppState {
    pub(crate) async fn clear_pending_approval(&self) {
        let mut slot = self.approval.lock().await;
        *slot = None;
    }
}

struct PendingApproval {
    request: ApprovalRequest,
    respond: oneshot::Sender<ApprovalDecision>,
}

pub async fn run_http_server(options: RuntimeServerOptions) -> Result<()> {
    let options = options.resolve_auth_token();
    let config = AgentConfig::from_env();
    let resume = load_resume_record(&options)?;
    let launched = launch_runtime(&config, options.workspace.clone(), resume);
    eprintln!(
        "deep-code runtime API listening on http://{}:{} ({})",
        options.host,
        options.port,
        launched.backend_label
    );
    if options.auth_token.is_some() {
        eprintln!("auth: bearer token required for /v1/* routes");
    }

    let state = AppState {
        version: env!("CARGO_PKG_VERSION").to_string(),
        workspace: options.workspace,
        auth_token: options.auth_token,
        runtime: Arc::new(Mutex::new(launched)),
        approval: Arc::new(Mutex::new(None)),
    };

    let protected = Router::new()
        .route("/v1/sessions", get(list_sessions))
        .route("/v1/sessions/new", post(crate::sessions::new_session))
        .route(
            "/v1/sessions/{id}/resume",
            post(crate::sessions::resume_session),
        )
        .route("/v1/sessions/{id}", get(get_session).delete(delete_session))
        .route("/v1/prompt", post(prompt_sse))
        .route("/v1/approvals", post(submit_approval))
        .route("/v1/doctor", get(crate::meta::doctor))
        .route("/v1/checkpoints", get(crate::meta::list_checkpoints))
        .route(
            "/v1/checkpoints/{id}/restore",
            post(crate::meta::restore_checkpoint),
        )
        .route("/v1/subagents", get(crate::meta::list_subagents))
        .route("/v1/jobs", get(crate::meta::list_jobs))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_auth,
        ))
        .with_state(state.clone());

    let app = Router::new()
        .route("/health", get(health))
        .merge(protected)
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
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

async fn require_auth(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(expected) = &state.auth_token {
        if !token_matches(
            expected,
            request.headers(),
            request.uri().query(),
        ) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({
                    "error": "missing or invalid runtime token"
                })),
            )
                .into_response();
        }
    }
    next.run(request).await
}

#[derive(Serialize)]
struct SessionSummary {
    id: String,
    updated_at_ms: u64,
    message_count: usize,
    preview: String,
}

#[derive(Deserialize)]
struct ListSessionsQuery {
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    50
}

async fn list_sessions(
    State(state): State<AppState>,
    Query(query): Query<ListSessionsQuery>,
) -> Result<Json<Vec<SessionSummary>>, ApiError> {
    let store = JsonSessionStore::for_workspace(&state.workspace).map_err(ApiError::from)?;
    let limit = query.limit.clamp(1, 200);
    let sessions = store
        .list()?
        .into_iter()
        .take(limit)
        .map(|record| SessionSummary {
            id: record.id.as_str().to_string(),
            updated_at_ms: record.updated_at_ms,
            message_count: record.messages.len(),
            preview: record.preview(),
        })
        .collect();
    Ok(Json(sessions))
}

async fn get_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<SessionRecord>, ApiError> {
    let store = JsonSessionStore::for_workspace(&state.workspace).map_err(ApiError::from)?;
    Ok(Json(store.load(&SessionId::parse(&id)?)?))
}

async fn delete_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let store = JsonSessionStore::for_workspace(&state.workspace).map_err(ApiError::from)?;
    store.delete(&SessionId::parse(&id)?)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PromptRequest {
    prompt: String,
}

#[derive(Serialize)]
struct SseEnvelope {
    seq: u64,
    timestamp: String,
    event: String,
    payload: serde_json::Value,
}

fn sse_payload(event: &RuntimeEvent) -> serde_json::Value {
    match event {
        RuntimeEvent::Provider(agent) => {
            serde_json::json!({ "category": "provider", "provider": agent })
        }
        other => serde_json::to_value(other).unwrap_or_else(|_| serde_json::json!({})),
    }
}

async fn prompt_sse(
    State(state): State<AppState>,
    Json(body): Json<PromptRequest>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    if body.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt must not be empty"));
    }

    let runtime = state.runtime.clone();
    let approval_gate = state.approval.clone();
    let prompt = body.prompt;

    let stream = stream! {
        let mut seq = 0u64;
        let mut event_stream = {
            let runtime = runtime.lock().await;
            runtime.handle.submit_user(prompt).await
        };

        loop {
            let mut resume_after_approval = false;
            while let Some(event) = event_stream.recv().await {
                seq += 1;
                let envelope = SseEnvelope {
                    seq,
                    timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    event: runtime_event_name(&event).to_string(),
                    payload: sse_payload(&event),
                };
                yield Ok(Event::default()
                    .event(envelope.event.clone())
                    .json_data(envelope)
                    .unwrap_or_else(|_| Event::default().data("serialization error")));

                match event {
                    RuntimeEvent::ApprovalRequired { request } => {
                        let (tx, rx) = oneshot::channel();
                        {
                            let mut slot = approval_gate.lock().await;
                            *slot = Some(PendingApproval {
                                request,
                                respond: tx,
                            });
                        }
                        let decision = rx.await.unwrap_or(ApprovalDecision::Denied);
                        {
                            let mut slot = approval_gate.lock().await;
                            *slot = None;
                        }
                        let runtime = runtime.lock().await;
                        event_stream = runtime.handle.submit_approval(decision).await;
                        resume_after_approval = true;
                        break;
                    }
                    RuntimeEvent::TurnFinished { .. } | RuntimeEvent::Error { .. } => {
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
    Denied,
}

impl From<ApprovalDecisionWire> for ApprovalDecision {
    fn from(value: ApprovalDecisionWire) -> Self {
        match value {
            ApprovalDecisionWire::Approved => Self::Approved,
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
    let _ = pending.respond.send(body.decision.into());
    Ok(Json(ApprovalResponse {
        accepted: true,
        call_id: body.call_id,
    }))
}

fn runtime_event_name(event: &RuntimeEvent) -> &'static str {
    match event {
        RuntimeEvent::Provider(_) => "provider",
        RuntimeEvent::ApprovalRequired { .. } => "approval.required",
        RuntimeEvent::ToolResult { .. } => "tool.result",
        RuntimeEvent::TurnFinished { .. } => "turn.completed",
        RuntimeEvent::CheckpointCreated { .. } => "checkpoint.created",
        RuntimeEvent::WorkspaceRestored { .. } => "workspace.restored",
        RuntimeEvent::DiagnosticsUpdated { .. } => "diagnostics.updated",
        RuntimeEvent::CompactionApplied { .. } => "compaction.applied",
        RuntimeEvent::Error { .. } => "error",
    }
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

    fn test_state(workspace: PathBuf, auth_token: Option<String>) -> AppState {
        AppState {
            version: "0.1.0".to_string(),
            workspace,
            auth_token,
            runtime: Arc::new(Mutex::new(launch_runtime(
                &AgentConfig::default(),
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                None,
            ))),
            approval: Arc::new(Mutex::new(None)),
        }
    }

    fn test_router(state: AppState) -> Router {
        let protected = Router::new()
            .route("/v1/sessions", get(list_sessions))
            .route("/v1/prompt", post(prompt_sse))
            .route("/v1/approvals", post(submit_approval))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                require_auth,
            ))
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

    #[test]
    fn default_options_use_localhost() {
        let options = RuntimeServerOptions::default();
        assert_eq!(options.host, DEFAULT_HOST);
        assert_eq!(options.port, DEFAULT_PORT);
    }

    #[test]
    fn resolve_auth_token_falls_back_to_env() {
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
            }
        }
        .resolve_auth_token();
        assert_eq!(options.auth_token.as_deref(), Some("from-cli"));
        unsafe {
            std::env::remove_var(RUNTIME_TOKEN_ENV);
        }
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
            .get(format!("http://{addr}/v1/sessions"))
            .send()
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

        let authed = client
            .get(format!("http://{addr}/v1/sessions"))
            .header("Authorization", "Bearer secret123")
            .send()
            .await
            .unwrap();
        assert_eq!(authed.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn prompt_sse_returns_provider_and_turn_completed() {
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
            body.contains("event: provider"),
            "expected provider SSE events, got: {body}"
        );
        assert!(
            body.contains("event: turn.completed"),
            "expected turn.completed SSE event, got: {body}"
        );
    }

    #[tokio::test]
    async fn approval_rejects_mismatched_call_id() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), None);
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
                .json(&json!({ "prompt": "/mock-tool hello" }))
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
            .json(&json!({ "call_id": "echo_call_1", "decision": "approved" }))
            .send()
            .await
            .unwrap();
        assert_eq!(good.status(), StatusCode::OK);

        let body = prompt_handle.await.unwrap();
        assert!(body.contains("event: approval.required"));
        assert!(body.contains("event: turn.completed"));
    }
}
