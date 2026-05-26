use thiserror::Error;

pub type AgentResult<T> = Result<T, AgentError>;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(
        "缺少 DeepSeek API Key；请设置环境变量 DEEPSEEK_API_KEY，或在 https://platform.deepseek.com 获取密钥"
    )]
    MissingApiKey,

    #[error("配置无效：{0}")]
    InvalidConfig(String),

    #[error("HTTP 请求失败：{0}")]
    Http(#[from] reqwest::Error),

    #[error("API 错误 ({status})：{message}")]
    Api {
        status: reqwest::StatusCode,
        message: String,
    },

    #[error("无法解析 provider 响应：{0}")]
    Parse(String),

    #[error("序列化错误：{0}")]
    Serde(#[from] serde_json::Error),
}

#[must_use]
pub fn api_key_setup_hint() -> &'static str {
    "1. 在 https://platform.deepseek.com 注册并创建 API Key\n\
     2. export DEEPSEEK_API_KEY=sk-...\n\
     3. 国内网络若连接不稳定，可尝试代理或镜像端点（DEEP_CODE 默认使用 https://api.deepseek.com/beta）"
}
