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

/// Provider phrases that name the context window in a rejected request's body.
///
/// Both spellings are OpenAI-compatible: the code half
/// (`context_length_exceeded`) and the human half ("This model's maximum
/// context length is N tokens"). The list is a list precisely because the body
/// belongs to the provider, not to us — a third spelling costs one more entry
/// and nothing else.
///
/// Matched against the RAW body rather than a parsed field: the code appears
/// verbatim in the JSON, so parsing would add a failure mode (a proxy returning
/// a non-JSON page) without adding any reach.
const CONTEXT_WINDOW_HINTS: &[&str] = &["context_length_exceeded", "context length"];

impl AgentError {
    /// Does this look like the provider refusing the request because it
    /// exceeded the model's context window?
    ///
    /// Only answers for a `400`; a `429`/`5xx` is the retry path's business, and
    /// the two must not be confused — one retries the same request, the other
    /// has to shrink it first.
    ///
    /// Asymmetric on purpose. `false` is always safe: the caller falls back to
    /// reporting the error exactly as it did before. A wrong `true` costs one
    /// wasted request. That is why the caller pairs this with its own token
    /// estimate rather than trusting the provider's wording alone, and why the
    /// match is on a couple of distinctive phrases rather than anything looser
    /// ("too long" would catch unrelated 400s).
    #[must_use]
    pub fn blames_context_window(&self) -> bool {
        let Self::Api { status, message } = self else {
            return false;
        };
        if status.as_u16() != 400 {
            return false;
        }
        let lowered = message.to_ascii_lowercase();
        CONTEXT_WINDOW_HINTS
            .iter()
            .any(|hint| lowered.contains(hint))
    }

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

    #[test]
    fn only_a_400_mentioning_the_context_window_blames_it() {
        let api = |status: u16, message: &str| AgentError::Api {
            status: reqwest::StatusCode::from_u16(status).unwrap(),
            message: message.to_string(),
        };

        // Both spellings the provider may use for the same refusal.
        assert!(
            api(
                400,
                r#"{"error":{"message":"This model's maximum context length is 131072 tokens.","code":"context_length_exceeded"}}"#
            )
            .blames_context_window()
        );
        assert!(
            api(
                400,
                r#"{"error":{"message":"This model's maximum context length is 131072 tokens."}}"#
            )
            .blames_context_window()
        );

        // A 400 naming something else must not reach the compaction path: it
        // would burn a compaction and a request before reporting the real
        // problem.
        assert!(
            !api(400, r#"{"error":{"message":"invalid tool schema"}}"#).blames_context_window()
        );
        // ...and neither must a context-flavoured message that is NOT a 400 —
        // a 429 is the retry path's, and must keep retrying the same request.
        assert!(
            !api(429, "context length exceeded, slow down").blames_context_window(),
            "a retriable status must never be re-read as an overflow"
        );
        // Non-API variants have no body to read.
        assert!(!AgentError::RequestTimeout { seconds: 30 }.blames_context_window());
    }
}
