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

    #[error("请求超时：{seconds}s 内未收到 DeepSeek 响应头")]
    RequestTimeout { seconds: u64 },

    #[error("流式响应卡顿：连续 {seconds}s 未收到新数据")]
    StreamStalled { seconds: u64 },

    #[error("流式响应总时长超过 {seconds}s 上限")]
    StreamDeadlineExceeded { seconds: u64 },

    #[error("流式响应过大：累计内容超过 {limit_bytes} 字节上限")]
    StreamOverflow { limit_bytes: u64 },
}

impl AgentError {
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::MissingApiKey => format!("{self}\n{}", api_key_setup_hint()),
            Self::Http(error) => format!(
                "{self}\n网络连接 DeepSeek 失败，请检查本机网络、代理设置或 DeepSeek endpoint；国内网络不稳定时建议配置可用代理。底层错误：{error}"
            ),
            Self::Api { status, message } if *status == reqwest::StatusCode::UNAUTHORIZED => {
                format!(
                    "{self}\n鉴权失败：请确认 DEEPSEEK_API_KEY 是否正确、未过期，并确认当前 endpoint 为 DeepSeek 兼容地址。原始信息：{message}"
                )
            }
            Self::Api { status, message } if *status == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                format!(
                    "{self}\n请求被限流：请稍后重试，或切换到 Flash/开启 auto 成本优先以降低压力。原始信息：{message}"
                )
            }
            Self::Api { status, message } if status.is_server_error() => {
                format!(
                    "{self}\nDeepSeek 服务暂时不可用或网络链路异常：可稍后重试，auto mode 会在可重试场景下尝试从 Pro 降级到 Flash。原始信息：{message}"
                )
            }
            Self::RequestTimeout { .. } => format!(
                "{self}\n网络或 DeepSeek 服务响应缓慢：请检查网络后重试，或调大 provider.timeout_secs。"
            ),
            Self::StreamStalled { .. } => format!(
                "{self}\n网络可能卡顿或代理中断：请检查网络后重试；如确属长任务，可调大 DEEP_CODE_STREAM_CHUNK_TIMEOUT_SECS。"
            ),
            Self::StreamDeadlineExceeded { .. } => format!(
                "{self}\n本轮响应耗时过长已被终止：可拆分任务后重试，或调大 DEEP_CODE_STREAM_TOTAL_TIMEOUT_SECS。"
            ),
            Self::StreamOverflow { .. } => format!(
                "{self}\n响应内容异常巨大已被截断保护：请缩小任务范围，或调大 DEEP_CODE_STREAM_MAX_BYTES。"
            ),
            _ => self.to_string(),
        }
    }
}

#[must_use]
pub fn api_key_setup_hint() -> &'static str {
    "1. 在 https://platform.deepseek.com 注册并创建 API Key\n\
     2. export DEEPSEEK_API_KEY=sk-...\n\
     3. 国内网络若连接不稳定，可尝试代理或镜像端点（DEEP_CODE 默认使用 https://api.deepseek.com/beta）"
}
