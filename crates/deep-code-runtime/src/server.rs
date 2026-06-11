//! Axum HTTP/SSE server for the local runtime API.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

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
use deep_code_agent::{
    AgentConfig, ApprovalDecision, ApprovalRequest, JsonSessionStore, LaunchedRuntime,
    RuntimeEvent, SessionId, SessionRecord, SessionStore, TurnId, launch_runtime,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, oneshot};
use tower_http::cors::{Any, CorsLayer};

use crate::auth::{RUNTIME_TOKEN_ENV, token_matches};
use crate::threads::{RuntimeEnvelope, RuntimeThread, RuntimeThreadDetail, RuntimeThreadStore};

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
    active_turn: Arc<StdMutex<Option<String>>>,
    threads: RuntimeThreadStore,
}

impl AppState {
    pub(crate) async fn clear_pending_approval(&self) {
        let mut slot = self.approval.lock().await;
        *slot = None;
    }

    async fn active_runtime_session_id(&self) -> Option<String> {
        self.runtime.lock().await.session_id.clone()
    }

    fn thread_detail(
        &self,
        detail: RuntimeThreadDetail,
    ) -> impl std::future::Future<Output = RuntimeThreadDetail> + Send {
        let active = self.active_runtime_session_id();
        async move {
            RuntimeThreadStore::with_active_runtime_session(detail, active.await)
        }
    }

    async fn ensure_runtime_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), ApiError> {
        let current = self.active_runtime_session_id().await;
        if current.as_deref() == Some(session_id.as_str()) {
            return Ok(());
        }
        let store = JsonSessionStore::for_workspace(&self.workspace).map_err(ApiError::from)?;
        let record = store.load(session_id)?;
        crate::sessions::switch_runtime(self, Some(record)).await?;
        Ok(())
    }
}

struct PendingApproval {
    request: ApprovalRequest,
    thread_id: String,
    turn_id: Option<TurnId>,
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

pub async fn run_http_server(options: RuntimeServerOptions) -> Result<()> {
    let options = options.resolve_auth_token();
    let loaded = AgentConfig::load(&options.workspace);
    for warning in &loaded.report.warnings {
        eprintln!("config warning: {warning}");
    }
    let config = loaded.config;
    let resume = load_resume_record(&options)?;
    let launched = launch_runtime(&config, options.workspace.clone(), resume);
    eprintln!(
        "deep-code runtime API listening on http://{}:{} ({})",
        options.host, options.port, launched.backend_label
    );
    if options.auth_token.is_some() {
        eprintln!("auth: bearer token required for /v1/* routes");
    }

    let threads = RuntimeThreadStore::new();
    if let Ok(store) = JsonSessionStore::for_workspace(&options.workspace)
        && let Ok(records) = store.list()
    {
        threads.hydrate_sessions(records).await;
    }

    let state = AppState {
        version: env!("CARGO_PKG_VERSION").to_string(),
        workspace: options.workspace,
        auth_token: options.auth_token,
        runtime: Arc::new(Mutex::new(launched)),
        approval: Arc::new(Mutex::new(None)),
        active_turn: Arc::new(StdMutex::new(None)),
        threads,
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
        .route("/v1/threads", get(list_threads).post(create_thread))
        .route("/v1/threads/{id}", get(get_thread).patch(update_thread))
        .route("/v1/threads/{id}/turns", post(post_thread_turn))
        .route(
            "/v1/threads/{id}/turns/{turn_id}/approvals",
            post(submit_thread_turn_approval),
        )
        .route("/v1/threads/{id}/events", get(thread_events))
        .route("/v1/doctor", get(crate::meta::doctor))
        .route("/v1/checkpoints", get(crate::meta::list_checkpoints))
        .route(
            "/v1/checkpoints/{id}/restore",
            post(crate::meta::restore_checkpoint),
        )
        .route("/v1/subagents", get(crate::meta::list_subagents))
        .route("/v1/jobs", get(crate::meta::list_jobs))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
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

async fn require_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(expected) = &state.auth_token
        && !token_matches(expected, request.headers(), request.uri().query())
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

#[derive(Deserialize)]
struct CreateThreadRequest {
    #[serde(default)]
    title: Option<String>,
    /// Bind this thread to an on-disk session (`session_<id>` thread id).
    #[serde(default)]
    session_id: Option<String>,
}

#[derive(Deserialize)]
struct PatchThreadRequest {
    #[serde(default)]
    title: Option<String>,
}

#[derive(Deserialize)]
struct ThreadEventsQuery {
    #[serde(default)]
    since_seq: u64,
    #[serde(default)]
    replay_only: bool,
}

async fn list_threads(State(state): State<AppState>) -> Result<Json<Vec<RuntimeThread>>, ApiError> {
    Ok(Json(state.threads.list_threads().await))
}

async fn create_thread(
    State(state): State<AppState>,
    body: Option<Json<CreateThreadRequest>>,
) -> Result<Json<RuntimeThread>, ApiError> {
    let body = body.map(|Json(body)| body);
    let title = body.as_ref().and_then(|body| body.title.clone());
    if let Some(session_token) = body.as_ref().and_then(|body| body.session_id.as_deref()) {
        let session_id = SessionId::parse(session_token)?;
        let thread_id = format!("session_{session_token}");
        let thread = state
            .threads
            .ensure_thread_with_session(thread_id, title, session_id)
            .await;
        return Ok(Json(thread));
    }
    Ok(Json(state.threads.create_thread(title).await))
}

async fn get_thread(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<RuntimeThreadDetail>, ApiError> {
    let detail = state
        .threads
        .get_thread(&id)
        .await
        .ok_or_else(|| ApiError::not_found(format!("thread '{id}' not found")))?;
    Ok(Json(state.thread_detail(detail).await))
}

async fn update_thread(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchThreadRequest>,
) -> Result<Json<RuntimeThreadDetail>, ApiError> {
    if state.threads.get_thread(&id).await.is_none() {
        return Err(ApiError::not_found(format!("thread '{id}' not found")));
    }
    if let Some(title) = body.title {
        state
            .threads
            .update_thread_title(&id, Some(title.clone()))
            .await;
        let _ = state
            .threads
            .append_manual_item(&id, "thread.updated", serde_json::json!({ "title": title }))
            .await;
    }
    let detail = state
        .threads
        .get_thread(&id)
        .await
        .ok_or_else(|| ApiError::not_found(format!("thread '{id}' not found")))?;
    Ok(Json(state.thread_detail(detail).await))
}

async fn post_thread_turn(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PromptRequest>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    if body.prompt.trim().is_empty() {
        return Err(ApiError::bad_request("prompt must not be empty"));
    }
    if state.threads.get_thread(&id).await.is_none() {
        let _ = state
            .threads
            .ensure_thread(id.clone(), Some(id.clone()))
            .await;
    }
    if let Some(session_id) = state
        .threads
        .get_thread(&id)
        .await
        .and_then(|detail| detail.thread.session_id)
    {
        state.ensure_runtime_session(&session_id).await?;
    }
    prompt_sse_for_thread(state, id, body.prompt).await
}

async fn thread_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ThreadEventsQuery>,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    if state.threads.get_thread(&id).await.is_none() {
        return Err(ApiError::not_found(format!("thread '{id}' not found")));
    }
    let store = state.threads.clone();
    let thread_id = id.clone();
    let replay_only = query.replay_only;
    let since_seq = query.since_seq;
    let stream = stream! {
        // Subscribe before replay so events emitted during replay are not lost.
        let mut live = store.subscribe();
        let replay = store.replay_since(&thread_id, since_seq).await;
        let mut high_water = replay.last().map(|envelope| envelope.seq).unwrap_or(since_seq);
        for envelope in replay {
            high_water = envelope.seq;
            yield Ok(thread_sse_event(envelope));
        }
        if replay_only {
            return;
        }
        loop {
            match live.recv().await {
                Ok(envelope) if envelope.thread_id == thread_id && envelope.seq > high_water => {
                    high_water = envelope.seq;
                    yield Ok(thread_sse_event(envelope));
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    yield Ok(Event::default().event("stream.lagged").json_data(serde_json::json!({
                        "thread_id": thread_id,
                        "since_seq": high_water,
                        "action": "reconnect_with_since_seq",
                    })).unwrap_or_else(|_| Event::default().data("stream lagged")));
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("keepalive"),
    ))
}

async fn submit_thread_turn_approval(
    State(state): State<AppState>,
    Path((thread_id, turn_id)): Path<(String, String)>,
    Json(body): Json<ApprovalRequestBody>,
) -> Result<Json<ApprovalResponse>, ApiError> {
    resolve_pending_approval(state, body, Some((&thread_id, &turn_id))).await
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

    let thread = state
        .threads
        .create_thread(Some("prompt".to_string()))
        .await;
    prompt_sse_for_thread(state, thread.thread_id, body.prompt).await
}

async fn prompt_sse_for_thread(
    state: AppState,
    thread_id: String,
    prompt: String,
) -> Result<Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let active_turn_lease = acquire_active_turn(&state, &thread_id)?;
    let runtime = state.runtime.clone();
    let approval_gate = state.approval.clone();
    let threads = state.threads.clone();
    let stream = stream! {
        let _active_turn_lease = active_turn_lease;
        let user_envelope = threads
            .append_manual_item(
                &thread_id,
                "user.message",
                serde_json::json!({ "content": prompt }),
            )
            .await;
        yield Ok(thread_sse_event(user_envelope));

        let mut event_stream = {
            let runtime = runtime.lock().await;
            runtime.handle.submit_user(prompt).await
        };

        loop {
            let mut resume_after_approval = false;
            while let Some(event) = event_stream.recv().await {
                let envelope = threads.append_event(&thread_id, &event).await;
                yield Ok(thread_sse_event(envelope));

                match event {
                    RuntimeEvent::ApprovalRequired {
                        turn_id, request, ..
                    } => {
                        let (tx, rx) = oneshot::channel();
                        {
                            let mut slot = approval_gate.lock().await;
                            *slot = Some(PendingApproval {
                                request,
                                thread_id: thread_id.clone(),
                                turn_id,
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
    resolve_pending_approval(state, body, None).await
}

async fn resolve_pending_approval(
    state: AppState,
    body: ApprovalRequestBody,
    expected_scope: Option<(&str, &str)>,
) -> Result<Json<ApprovalResponse>, ApiError> {
    let pending = {
        let mut slot = state.approval.lock().await;
        slot.take()
    };
    let Some(pending) = pending else {
        return Err(ApiError::conflict("no pending approval"));
    };
    if let Some((expected_thread_id, expected_turn_id)) = expected_scope {
        let actual_thread_id = pending.thread_id.clone();
        let actual_turn_id = pending
            .turn_id
            .as_ref()
            .map(TurnId::as_str)
            .unwrap_or("unknown")
            .to_string();
        if actual_thread_id != expected_thread_id || actual_turn_id != expected_turn_id {
            let mut slot = state.approval.lock().await;
            *slot = Some(pending);
            return Err(ApiError::bad_request(format!(
                "approval scope mismatch: expected thread='{expected_thread_id}' turn='{expected_turn_id}', active thread='{actual_thread_id}' turn='{actual_turn_id}'"
            )));
        }
    }
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
            workspace,
            auth_token,
            runtime: Arc::new(Mutex::new(launch_runtime(
                &AgentConfig::default(),
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                None,
            ))),
            approval: Arc::new(Mutex::new(None)),
            active_turn: Arc::new(StdMutex::new(None)),
            threads: RuntimeThreadStore::new(),
        }
    }

    fn test_router(state: AppState) -> Router {
        let protected = Router::new()
            .route("/v1/sessions", get(list_sessions))
            .route("/v1/prompt", post(prompt_sse))
            .route("/v1/approvals", post(submit_approval))
            .route("/v1/threads", get(list_threads).post(create_thread))
            .route("/v1/threads/{id}", get(get_thread).patch(update_thread))
            .route("/v1/threads/{id}/turns", post(post_thread_turn))
            .route(
                "/v1/threads/{id}/turns/{turn_id}/approvals",
                post(submit_thread_turn_approval),
            )
            .route("/v1/threads/{id}/events", get(thread_events))
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
    async fn thread_turn_records_detail_and_replays_events() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), None);
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::new();

        let thread: RuntimeThread = client
            .post(format!("http://{addr}/v1/threads"))
            .json(&json!({ "title": "runtime test" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let turn_body = client
            .post(format!(
                "http://{addr}/v1/threads/{}/turns",
                thread.thread_id
            ))
            .json(&json!({ "prompt": "hello thread api" }))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            turn_body.contains("event: turn.completed"),
            "expected completed turn, got: {turn_body}"
        );
        let turn_envelopes = envelopes_from_sse(&turn_body);
        assert!(
            turn_envelopes
                .iter()
                .enumerate()
                .all(|(index, envelope)| envelope.seq == index as u64 + 1),
            "expected durable thread seqs starting at 1, got: {turn_body}"
        );

        let detail: RuntimeThreadDetail = client
            .get(format!("http://{addr}/v1/threads/{}", thread.thread_id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(detail.thread.thread_id, thread.thread_id);
        assert_eq!(detail.turns.len(), 1);
        assert!(
            detail
                .items
                .iter()
                .any(|item| item.kind == "turn.completed")
        );

        let replay = client
            .get(format!(
                "http://{addr}/v1/threads/{}/events?since_seq=0",
                thread.thread_id
            ))
            .query(&[("replay_only", "true")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            replay.contains("event: turn.started"),
            "expected replayed turn.started event, got: {replay}"
        );
        assert!(
            replay.contains("event: turn.completed"),
            "expected replayed turn.completed event, got: {replay}"
        );
        let replay_envelopes = envelopes_from_sse(&replay);
        assert_eq!(
            replay_envelopes
                .iter()
                .map(|envelope| envelope.seq)
                .collect::<Vec<_>>(),
            turn_envelopes
                .iter()
                .map(|envelope| envelope.seq)
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn thread_turn_rejects_second_active_turn_until_approval_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), None);
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();

        let thread: RuntimeThread = client
            .post(format!("http://{addr}/v1/threads"))
            .json(&json!({ "title": "active gate test" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let turns_url = format!("http://{addr}/v1/threads/{}/turns", thread.thread_id);
        let approvals_url = format!("http://{addr}/v1/approvals");

        let prompt_client = client.clone();
        let first_turn_url = turns_url.clone();
        let prompt_handle = tokio::spawn(async move {
            prompt_client
                .post(first_turn_url)
                .json(&json!({ "prompt": "/mock-tool hello" }))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        });

        tokio::time::sleep(Duration::from_millis(200)).await;

        let conflict = client
            .post(&turns_url)
            .json(&json!({ "prompt": "second turn" }))
            .send()
            .await
            .unwrap();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);

        let approval = client
            .post(&approvals_url)
            .json(&json!({ "call_id": "echo_call_1", "decision": "approved" }))
            .send()
            .await
            .unwrap();
        assert_eq!(approval.status(), StatusCode::OK);

        let body = prompt_handle.await.unwrap();
        assert!(body.contains("event: approval.required"));
        assert!(body.contains("event: turn.completed"));
    }

    #[tokio::test]
    async fn thread_turn_approval_rejects_wrong_scope() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), None);
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();

        let thread: RuntimeThread = client
            .post(format!("http://{addr}/v1/threads"))
            .json(&json!({ "title": "approval scope test" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let turns_url = format!("http://{addr}/v1/threads/{}/turns", thread.thread_id);
        let approvals_url = format!("http://{addr}/v1/approvals");
        let scoped_approvals_url = format!(
            "http://{addr}/v1/threads/{}/turns/wrong_turn/approvals",
            thread.thread_id
        );

        let prompt_client = client.clone();
        let prompt_handle = tokio::spawn(async move {
            prompt_client
                .post(turns_url)
                .json(&json!({ "prompt": "/mock-tool hello" }))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        });

        tokio::time::sleep(Duration::from_millis(200)).await;

        let wrong_scope = client
            .post(&scoped_approvals_url)
            .json(&json!({ "call_id": "echo_call_1", "decision": "approved" }))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong_scope.status(), StatusCode::BAD_REQUEST);

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

    #[tokio::test]
    async fn patch_thread_returns_fresh_detail_with_updated_item() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), None);
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::new();

        let thread: RuntimeThread = client
            .post(format!("http://{addr}/v1/threads"))
            .json(&json!({ "title": "before" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let detail: RuntimeThreadDetail = client
            .patch(format!("http://{addr}/v1/threads/{}", thread.thread_id))
            .json(&json!({ "title": "after" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(detail.thread.title.as_deref(), Some("after"));
        assert!(
            detail
                .items
                .iter()
                .any(|item| item.kind == "thread.updated"),
            "expected thread.updated item in fresh detail"
        );
    }

    #[tokio::test]
    async fn thread_turn_includes_user_message_item() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(dir.path().to_path_buf(), None);
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::new();

        let thread: RuntimeThread = client
            .post(format!("http://{addr}/v1/threads"))
            .json(&json!({ "title": "user item test" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        let turn_body = client
            .post(format!(
                "http://{addr}/v1/threads/{}/turns",
                thread.thread_id
            ))
            .json(&json!({ "prompt": "hello thread api" }))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(
            turn_body.contains("event: user.message"),
            "expected user.message SSE item, got: {turn_body}"
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
