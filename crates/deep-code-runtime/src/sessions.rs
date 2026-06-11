//! Switch the in-process agent runtime to a new or resumed session.

use axum::Json;
use axum::extract::{Path, State};
use deep_code_agent::{
    AgentConfig, JsonSessionStore, SessionId, SessionRecord, SessionStore, launch_runtime,
};
use serde::Serialize;

use crate::server::{ApiError, AppState};

#[derive(Debug, Clone, Serialize)]
pub struct ActiveSessionResponse {
    pub session_id: Option<String>,
    pub backend_label: String,
}

pub async fn new_session(
    State(state): State<AppState>,
) -> Result<Json<ActiveSessionResponse>, ApiError> {
    Ok(Json(switch_runtime(&state, None).await?))
}

pub async fn resume_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ActiveSessionResponse>, ApiError> {
    let store = JsonSessionStore::for_workspace(&state.workspace)?;
    let record = store.load(&SessionId::parse(&id)?)?;
    Ok(Json(switch_runtime(&state, Some(record)).await?))
}

pub(crate) async fn switch_runtime(
    state: &AppState,
    resume: Option<SessionRecord>,
) -> Result<ActiveSessionResponse, ApiError> {
    state.clear_pending_approval().await;

    let config = AgentConfig::load(&state.workspace).config;
    let mut guard = state.runtime.lock().await;
    (guard.stop_hook)();
    guard.handle.shutdown().await;
    let launched = launch_runtime(&config, state.workspace.clone(), resume);
    let response = ActiveSessionResponse {
        session_id: launched.session_id.clone(),
        backend_label: launched.backend_label.clone(),
    };
    *guard = launched;
    Ok(response)
}
