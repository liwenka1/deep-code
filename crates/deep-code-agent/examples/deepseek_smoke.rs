//! Optional real DeepSeek smoke test.
//!
//! Run with:
//! `DEEPSEEK_API_KEY=... cargo run -p deep-code-agent --example deepseek_smoke`
//!
//! The example exits early without network access when `DEEPSEEK_API_KEY` is not set.

use deep_code_agent::{AgentConfig, ChatRequest, DeepSeekClient, LlmClient, Message};
use futures_util::StreamExt;

#[tokio::main]
async fn main() -> deep_code_agent::AgentResult<()> {
    let config = AgentConfig::from_env();
    if config.api_key.is_none() {
        println!("Set DEEPSEEK_API_KEY to run the real DeepSeek smoke test.");
        return Ok(());
    }

    let client = DeepSeekClient::new(config.clone())?;
    let request = ChatRequest::streaming(
        config.model.clone(),
        vec![Message::user("Reply with exactly: pong")],
    );

    let mut events = client.stream_chat(request).await?;
    while let Some(event) = events.next().await {
        println!("{:?}", event?);
    }

    Ok(())
}
