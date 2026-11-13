mod active_turn;
mod app;
mod cli;
mod clipboard;
mod commands;
mod doctor_cli;
mod eval_cli;
mod event_routing;
mod git_diff;
mod github;
mod headless;
mod highlight;
mod history;
mod interrupt;
mod interruptible_output;
mod markdown;
mod prompt_history;
mod session_cli;
mod session_scan;
mod startup;
mod ui;

use cli::{CliArgs, RunMode, parse_args, workspace_root};
use deep_code_agent::JsonSessionStore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    run().await
}

async fn run() -> anyhow::Result<()> {
    let CliArgs { mode } = parse_args();
    match mode {
        RunMode::Tui { intent, add_dirs } => {
            std::process::exit(run_tui(intent, add_dirs).await);
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
        RunMode::Eval(args) => eval_cli::run_eval(args).await,
        RunMode::SessionList => session_cli::list(),
        RunMode::SessionDelete { id } => session_cli::delete(id),
        RunMode::SessionExport { id } => session_cli::export(id),
    }
}

/// Own the listener through error reporting as well as startup and rendering.
async fn run_tui(intent: cli::StartupIntent, add_dirs: Vec<std::path::PathBuf>) -> i32 {
    let workspace = workspace_root();
    let interrupted = match ui::watch_interrupt() {
        Ok(interrupted) => interrupted,
        Err(_) => return 1,
    };
    let result: anyhow::Result<()> = async {
        let store_workspace = workspace.clone();
        let store = startup::blocking_io(&interrupted, move || {
            JsonSessionStore::for_workspace(store_workspace)
        })
        .await??;
        // Resume scans run off-thread and service Ctrl+C before raw mode.
        let record = startup::choose_startup(&store, intent, &workspace, &interrupted).await?;
        ui::run(
            app::LaunchConfig {
                resume: record,
                workspace: None,
                extra_roots: add_dirs,
            },
            &interrupted,
        )
        .await
    }
    .await;
    // Handle errors while the listener is still alive. Returning an
    // anyhow error from main would print synchronously after dropping it.
    if interrupted.is_cancelled() {
        130
    } else if let Err(error) = result {
        if let Ok(output) = interruptible_output::Output::stderr(&interrupted) {
            let _ = output.send(format!("Error: {error:#}\n").as_bytes()).await;
        }
        if interrupted.is_cancelled() { 130 } else { 1 }
    } else {
        0
    }
}
