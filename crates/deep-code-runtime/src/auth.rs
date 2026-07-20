//! Optional bearer token guard for `/v1/*` routes.

use axum::http::HeaderMap;

pub const RUNTIME_TOKEN_ENV: &str = "DEEP_CODE_RUNTIME_TOKEN";

const TOKEN_HEADER: &str = "x-deep-code-runtime-token";

pub fn token_matches(expected: &str, headers: &HeaderMap) -> bool {
    token_from_request(headers).as_deref() == Some(expected)
}

fn token_from_request(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(value.to_string());
    }
    headers
        .get(TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}
