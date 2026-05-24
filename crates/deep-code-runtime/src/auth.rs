//! Optional bearer token guard for `/v1/*` routes.

use axum::http::HeaderMap;

pub const RUNTIME_TOKEN_ENV: &str = "DEEP_CODE_RUNTIME_TOKEN";

const TOKEN_HEADER: &str = "x-deep-code-runtime-token";

pub fn token_matches(expected: &str, headers: &HeaderMap, query: Option<&str>) -> bool {
    token_from_request(headers, query).as_deref() == Some(expected)
}

fn token_from_request(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    if let Some(value) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(value.to_string());
    }
    if let Some(value) = headers
        .get(TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Some(value.to_string());
    }
    query_token(query)
}

fn query_token(query: Option<&str>) -> Option<String> {
    let query = query?;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=')?;
        if key == "token" && !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_token_parses_token_param() {
        assert_eq!(
            query_token(Some("token=abc123&other=1")).as_deref(),
            Some("abc123")
        );
    }
}
