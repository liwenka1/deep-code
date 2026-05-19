mod app;
mod echo_client;
mod ui;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    ui::run()
}
