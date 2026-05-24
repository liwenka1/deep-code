mod app;
mod cli;
mod echo_client;
mod startup;
mod ui;

use cli::{CliArgs, RunMode, parse_args, run_session_command, workspace_root};
use deep_code_agent::JsonSessionStore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let CliArgs { mode } = parse_args();
    match mode {
        RunMode::Tui {
            resume,
            force_new,
        } => {
            let store = JsonSessionStore::for_workspace(workspace_root())?;
            let record =
                startup::choose_startup(&store, force_new, resume.as_deref())?;
            ui::run(app::LaunchConfig { resume: record }).await
        }
        other => run_session_command(other),
    }
}
