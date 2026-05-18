mod app;
mod ui;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    ui::run()
}
