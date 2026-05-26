//! Embedded local runtime API for the desktop GUI.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use deep_code_runtime::{RUNTIME_TOKEN_ENV, RuntimeServerOptions, run_http_server};
use serde::{Deserialize, Serialize};

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 7878;

static SERVER_SPAWNED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeInfo {
    pub base_url: String,
    pub workspace: String,
    pub version: String,
    pub embedded: bool,
    pub auth_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
}

#[must_use]
pub fn resolve_workspace() -> PathBuf {
    std::env::var("DEEP_CODE_WORKSPACE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

#[must_use]
pub fn resolve_auth_token() -> Option<String> {
    std::env::var(RUNTIME_TOKEN_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

#[must_use]
pub fn base_url() -> String {
    format!("http://{DEFAULT_HOST}:{DEFAULT_PORT}")
}

pub async fn ensure_runtime_server() -> anyhow::Result<bool> {
    if is_runtime_healthy().await {
        return Ok(false);
    }

    if SERVER_SPAWNED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        wait_for_runtime(30).await?;
        return Ok(true);
    }

    let workspace = resolve_workspace();
    let options = RuntimeServerOptions {
        host: DEFAULT_HOST.to_string(),
        port: DEFAULT_PORT,
        workspace,
        ..RuntimeServerOptions::default()
    };

    tokio::spawn(async move {
        if let Err(error) = run_http_server(options).await {
            eprintln!("embedded runtime server failed: {error}");
            SERVER_SPAWNED.store(false, Ordering::SeqCst);
        }
    });

    wait_for_runtime(30).await?;
    Ok(true)
}

async fn is_runtime_healthy() -> bool {
    reqwest::Client::new()
        .get(format!("{}/health", base_url()))
        .send()
        .await
        .map(|response| response.status().is_success())
        .unwrap_or(false)
}

async fn wait_for_runtime(max_secs: u64) -> anyhow::Result<()> {
    let attempts = max_secs.saturating_mul(10);
    for _ in 0..attempts {
        if is_runtime_healthy().await {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::bail!("runtime server did not become ready on {}", base_url())
}

#[derive(Deserialize)]
struct HealthBody {
    version: String,
    #[serde(default)]
    auth_required: bool,
}

#[tauri::command]
pub async fn get_runtime_info() -> Result<RuntimeInfo, String> {
    let embedded = ensure_runtime_server()
        .await
        .map_err(|error| error.to_string())?;
    let workspace = resolve_workspace();
    let auth_token = resolve_auth_token();
    let health = reqwest::Client::new()
        .get(format!("{}/health", base_url()))
        .send()
        .await
        .map_err(|error| error.to_string())?
        .json::<HealthBody>()
        .await
        .map_err(|error| error.to_string())?;

    Ok(RuntimeInfo {
        base_url: base_url(),
        workspace: workspace.display().to_string(),
        version: health.version,
        embedded,
        auth_required: health.auth_required,
        auth_token,
    })
}
