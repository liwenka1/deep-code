mod active_turn;
mod app;
mod cli;
mod clipboard;
mod commands;
mod doctor_cli;
mod eval_cli;
mod event_routing;
mod github;
mod headless;
mod highlight;
mod history;
mod markdown;
mod session_cli;
mod startup;
mod ui;

use cli::{CliArgs, RunMode, parse_args, workspace_root};
use deep_code_agent::JsonSessionStore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let CliArgs { mode } = parse_args();
    match mode {
        RunMode::Tui { intent, add_dirs } => {
            let workspace = workspace_root();
            let store = JsonSessionStore::for_workspace(workspace.clone())?;
            // The picker (only the `-r` path) resolves the UI language itself,
            // lazily, from the workspace config — App::launch loads it again
            // for the main UI.
            let record = startup::choose_startup(&store, intent, &workspace)?;
            ui::run(app::LaunchConfig {
                resume: record,
                workspace: None,
                extra_roots: add_dirs,
            })
            .await
        }
        RunMode::Print(print_args) => {
            // Exit code is part of the headless contract (0 ok / 1 error /
            // 2 usage / 124 timeout / 130 interrupt), so bypass `?`-style
            // error mapping and exit explicitly.
            let code = headless::run_print(print_args).await;
            std::process::exit(code);
        }
        RunMode::Github(command) => std::process::exit(github::run(command)),
        RunMode::Doctor { json } => doctor_cli::run_doctor(json),
        RunMode::Serve {
            host,
            port,
            auth_token,
            resume,
            autonomous_approvals,
            add_dirs,
        } => {
            deep_code_runtime::run_http_server(deep_code_runtime::RuntimeServerOptions {
                host,
                port,
                auth_token,
                workspace: workspace_root(),
                extra_roots: add_dirs,
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
        RunMode::SessionList => session_cli::list(),
        RunMode::SessionDelete { id } => session_cli::delete(id),
        RunMode::SessionExport { id } => session_cli::export(id),
    }
}
