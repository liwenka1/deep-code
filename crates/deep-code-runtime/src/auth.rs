//! Optional bearer token guard for `/v1/*` routes.

use axum::http::HeaderMap;

pub const RUNTIME_TOKEN_ENV: &str = "DEEP_CODE_RUNTIME_TOKEN";

const TOKEN_HEADER: &str = "x-deep-code-runtime-token";

pub fn token_matches(expected: &str, headers: &HeaderMap) -> bool {
    token_from_request(headers)
        .is_some_and(|presented| constant_time_eq(presented.as_bytes(), expected.as_bytes()))
}

/// Length-checked constant-time byte comparison. The length check may leak the
/// expected length (acceptable for a bearer token); the byte loop never
/// short-circuits on the first differing byte, so a matching prefix reveals
/// nothing through timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
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

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn constant_time_eq_matches_only_identical_bytes() {
        assert!(constant_time_eq(b"secret-token", b"secret-token"));
        assert!(!constant_time_eq(b"secret-token", b"secret-toke"));
        assert!(!constant_time_eq(b"secret-token", b"Secret-token"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }
}
