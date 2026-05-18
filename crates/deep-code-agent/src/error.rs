use thiserror::Error;

pub type AgentResult<T> = Result<T, AgentError>;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("missing DeepSeek API key; set DEEPSEEK_API_KEY")]
    MissingApiKey,

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("api error ({status}): {message}")]
    Api {
        status: reqwest::StatusCode,
        message: String,
    },

    #[error("failed to parse provider response: {0}")]
    Parse(String),

    #[error("serde error: {0}")]
    Serde(#[from] serde_json::Error),
}
