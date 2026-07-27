use thiserror::Error;

use crate::i18n::{Lang, TextId, tr, tr_with};

pub type AgentResult<T> = Result<T, AgentError>;

/// `Display` stays English and terse — it is the log/`{:?}` form for
/// developers. User-facing text (localized, with guidance) comes from
/// [`AgentError::user_message`], which the runtime formats in the configured
/// language before emitting a `RuntimeEvent::Error`.
#[derive(Debug, Error)]
pub enum AgentError {
    #[error("missing DeepSeek API key")]
    MissingApiKey,

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("API error ({status}): {message}")]
    Api {
        status: reqwest::StatusCode,
        message: String,
    },

    #[error("failed to parse provider response: {0}")]
    Parse(String),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("request timed out: no response headers within {seconds}s")]
    RequestTimeout { seconds: u64 },

    #[error("stream stalled: no data for {seconds}s")]
    StreamStalled { seconds: u64 },

    #[error("stream exceeded {seconds}s total deadline")]
    StreamDeadlineExceeded { seconds: u64 },

    #[error("stream overflow: content exceeded {limit_bytes} bytes")]
    StreamOverflow { limit_bytes: u64 },
}

impl AgentError {
    /// The localized, guidance-carrying message shown to the user (status line
    /// + error cell). `Display` remains the English log form.
    #[must_use]
    pub fn user_message(&self, lang: Lang) -> String {
        match self {
            Self::MissingApiKey => tr(lang, TextId::ErrMissingApiKey).to_string(),
            Self::Http(error) => tr_with(lang, TextId::ErrHttp, &[("error", &error.to_string())]),
            Self::Api { status, message } => {
                let id = if *status == reqwest::StatusCode::UNAUTHORIZED {
                    TextId::ErrApiUnauthorized
                } else if *status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    TextId::ErrApiRateLimited
                } else if status.is_server_error() {
                    TextId::ErrApiServer
                } else {
                    TextId::ErrApiGeneric
                };
                tr_with(
                    lang,
                    id,
                    &[("status", status.as_str()), ("message", message)],
                )
            }
            Self::Parse(detail) => tr_with(lang, TextId::ErrParse, &[("detail", detail)]),
            Self::Serde(error) => {
                tr_with(lang, TextId::ErrSerde, &[("detail", &error.to_string())])
            }
            Self::RequestTimeout { seconds } => tr_with(
                lang,
                TextId::ErrRequestTimeout,
                &[("seconds", &seconds.to_string())],
            ),
            Self::StreamStalled { seconds } => tr_with(
                lang,
                TextId::ErrStreamStalled,
                &[("seconds", &seconds.to_string())],
            ),
            Self::StreamDeadlineExceeded { seconds } => tr_with(
                lang,
                TextId::ErrStreamDeadline,
                &[("seconds", &seconds.to_string())],
            ),
            Self::StreamOverflow { limit_bytes } => tr_with(
                lang,
                TextId::ErrStreamOverflow,
                &[("limit", &limit_bytes.to_string())],
            ),
        }
    }
}

/// The API-key setup steps, localized. Doctor and the offline welcome reuse it.
#[must_use]
pub fn api_key_setup_hint(lang: Lang) -> String {
    // Reuse the MissingApiKey guidance body (headline + 3 numbered steps).
    tr(lang, TextId::ErrMissingApiKey).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_localizes_while_display_stays_english() {
        let err = AgentError::RequestTimeout { seconds: 30 };
        let zh = err.user_message(Lang::Zh);
        let en = err.user_message(Lang::En);
        assert!(zh.contains("请求超时") && zh.contains("30"), "{zh}");
        assert!(en.contains("timed out") && en.contains("30"), "{en}");
        assert_ne!(zh, en);
        // Display is the English log form regardless of UI language.
        assert!(err.to_string().contains("timed out"));
        assert!(!err.to_string().contains("请求超时"));
    }

    #[test]
    fn api_error_selects_variant_by_status() {
        let unauthorized = AgentError::Api {
            status: reqwest::StatusCode::UNAUTHORIZED,
            message: "bad key".to_string(),
        };
        assert!(unauthorized.user_message(Lang::Zh).contains("鉴权失败"));
        assert!(
            unauthorized
                .user_message(Lang::En)
                .contains("authentication failed")
        );
    }
}
