mod active_turn;
mod app;
mod cli;
mod clipboard;
mod commands;
mod doctor_cli;
mod eval_cli;
mod event_routing;
mod history;
mod markdown;
mod startup;
mod ui;

use cli::{CliArgs, RunMode, parse_args, run_session_command, workspace_root};
use deep_code_agent::JsonSessionStore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let CliArgs { mode } = parse_args();
    match mode {
        RunMode::Tui { intent } => {
            let store = JsonSessionStore::for_workspace(workspace_root())?;
            let record = startup::choose_startup(&store, intent)?;
            ui::run(app::LaunchConfig { resume: record }).await
        }
        RunMode::Doctor { json } => doctor_cli::run_doctor(json),
        RunMode::Serve {
            host,
            port,
            auth_token,
            resume,
            autonomous_approvals,
        } => {
            deep_code_runtime::run_http_server(deep_code_runtime::RuntimeServerOptions {
                host,
                port,
                auth_token,
                workspace: workspace_root(),
                resume_session_id: resume,
                autonomous_approvals,
            })
            .await
        }
        RunMode::Eval {
            subset,
            split,
            sample,
            parallel,
            json,
            markdown,
            timeout_secs,
            out_dir,
        } => {
            eval_cli::run_eval(
                subset,
                split,
                sample,
                parallel,
                json,
                markdown,
                timeout_secs,
                out_dir,
            )
            .await
        }
        other => run_session_command(other),
    }
}
