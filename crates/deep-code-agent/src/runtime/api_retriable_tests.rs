use super::*;
use reqwest::StatusCode;

fn api_error(status: StatusCode) -> AgentError {
    AgentError::Api {
        status,
        message: "test".to_string(),
    }
}

#[test]
fn retriable_statuses_include_rate_limit_and_gateway_errors() {
    for status in [
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::BAD_GATEWAY,
        StatusCode::SERVICE_UNAVAILABLE,
        StatusCode::GATEWAY_TIMEOUT,
    ] {
        assert!(api_error_retriable(&api_error(status)), "{status}");
    }
}

#[test]
fn auth_and_bad_request_errors_are_not_retriable() {
    for status in [
        StatusCode::BAD_REQUEST,
        StatusCode::UNAUTHORIZED,
        StatusCode::NOT_FOUND,
    ] {
        assert!(!api_error_retriable(&api_error(status)), "{status}");
    }
}
