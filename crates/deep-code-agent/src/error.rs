use std::sync::LazyLock;

use regex::Regex;
use thiserror::Error;

use crate::i18n::{Lang, TextId, tr, tr_with};

pub type AgentResult<T> = Result<T, AgentError>;

/// Keep provider diagnostics useful while removing a configured credential.
pub(crate) fn redact_api_key(key: Option<&str>, mut message: String, truncated: bool) -> String {
    let Some(key) = crate::config::normalized_api_key(key) else {
        return message;
    };
    // Match complete strings and a final string cut short by the HTTP body cap.
    static JSON_STRING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#""(?:[^"\\\r\n]|\\.)*(?:(")|\\?$)"#).expect("JSON string pattern")
    });
    if message.contains('\\') {
        let mut redacted = String::new();
        let mut copied = 0;
        for captures in JSON_STRING.captures_iter(&message) {
            let token = captures.get(0).expect("JSON string match");
            if !token.as_str().contains('\\') {
                continue;
            }
            let replacement = if captures.get(1).is_some() {
                serde_json::from_str::<String>(token.as_str())
                    .ok()
                    .filter(|decoded| decoded.contains(key))
                    .map(|decoded| decoded.replace(key, "[redacted]"))
            } else if truncated {
                match serde_json::from_str::<String>(&format!("{}\"", token.as_str())) {
                    Ok(decoded) if decoded.contains(key) => {
                        Some(decoded.replace(key, "[redacted]"))
                    }
                    Ok(_) => None,
                    // A cut through an escape leaves this fragment undecodable.
                    Err(_) => Some("[truncated JSON string]".into()),
                }
            } else {
                None
            };
            let Some(replacement) = replacement else {
                continue;
            };
            redacted.push_str(&message[copied..token.start()]);
            redacted.push_str(&serde_json::to_string(&replacement).expect("a string serializes"));
            copied = token.end();
        }
        if copied > 0 {
            redacted.push_str(&message[copied..]);
            message = redacted;
        }
    }
    if message.contains(key) {
        message.replace(key, "[redacted]")
    } else {
        message
    }
}

/// `Display` stays English and terse — it is the log/`{:?}` form for
/// developers. User-facing text (localized, with guidance) comes from
/// [`AgentError::user_message`], which the runtime formats in the configured
/// language before emitting a `RuntimeEvent::Error`.
#[derive(Debug, Error)]
pub enum AgentError {
    #[error(
        "unknown model. Set provider.model or DEEP_CODE_MODEL to a model listed by `deepcode doctor`."
    )]
    UnknownModel,

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

    #[error("{0}")]
    Attachment(String),

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
            Self::UnknownModel => tr(lang, TextId::ErrUnknownModel).to_string(),
            Self::MissingApiKey => tr(lang, TextId::ErrMissingApiKey).to_string(),
            Self::Http(error) => tr_with(lang, TextId::ErrHttp, &[("error", &error.to_string())]),
            Self::Api { status, message } => tr_with(
                lang,
                TextId::ErrApiGeneric,
                &[("status", status.as_str()), ("message", message)],
            ),
            Self::Parse(detail) => tr_with(lang, TextId::ErrParse, &[("detail", detail)]),
            Self::Serde(error) => {
                tr_with(lang, TextId::ErrSerde, &[("detail", &error.to_string())])
            }
            Self::RequestTimeout { seconds } => tr_with(
                lang,
                TextId::ErrRequestTimeout,
                &[("seconds", &seconds.to_string())],
            ),
            Self::Attachment(message) => message.clone(),
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
    fn api_error_shows_provider_status_and_message() {
        let unauthorized = AgentError::Api {
            status: reqwest::StatusCode::UNAUTHORIZED,
            message: "bad key".to_string(),
        };
        for lang in [Lang::Zh, Lang::En] {
            let message = unauthorized.user_message(lang);
            assert!(message.contains("401") && message.contains("bad key"));
        }
    }
}
