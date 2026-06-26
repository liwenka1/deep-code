//! Read-only runtime metadata routes (doctor, checkpoints, sub-agents, jobs).

use axum::Json;
use axum::extract::{Path, State};
use deep_code_agent::{
    AgentConfig, BackgroundJobSummary, CheckpointId, CheckpointStore, DoctorReport, SubAgentRecord,
};
use serde::Serialize;

use crate::server::{ApiError, AppState};

#[derive(Serialize)]
pub(crate) struct CheckpointSummary {
    id: String,
}

#[derive(Serialize)]
pub(crate) struct RestoreResponse {
    restored: bool,
    id: String,
}

pub async fn doctor(State(state): State<AppState>) -> Json<DoctorReport> {
    let loaded = AgentConfig::load(&state.workspace);
    Json(DoctorReport::collect(&state.workspace, &loaded.config).with_config_layers(&loaded.report))
}

pub async fn list_checkpoints(
    State(state): State<AppState>,
) -> Result<Json<Vec<CheckpointSummary>>, ApiError> {
    let store = CheckpointStore::new(&state.workspace)?;
    let checkpoints = store
        .list()?
        .into_iter()
        .map(|id| CheckpointSummary { id: id.0 })
        .collect();
    Ok(Json(checkpoints))
}

pub async fn restore_checkpoint(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<RestoreResponse>, ApiError> {
    let checkpoint_id = CheckpointId(id.clone());
    {
        let runtime = state.runtime.lock().await;
        runtime
            .handle
            .restore_checkpoint(checkpoint_id)
            .await
            .map_err(ApiError::from)?;
    }
    Ok(Json(RestoreResponse { restored: true, id }))
}

pub async fn list_subagents(
    State(state): State<AppState>,
) -> Result<Json<Vec<SubAgentRecord>>, ApiError> {
    let runtime = state.runtime.lock().await;
    let manager = runtime
        .subagent_manager
        .read()
        .map_err(|error| ApiError::internal(format!("sub-agent manager poisoned: {error}")))?;
    Ok(Json(manager.list_current_session()))
}

pub async fn list_jobs(State(state): State<AppState>) -> Json<Vec<BackgroundJobSummary>> {
    let runtime = state.runtime.lock().await;
    Json(runtime.job_store.list_summaries())
}
