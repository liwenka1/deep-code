use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const RESPONSE_WATCHDOG: Duration = Duration::from_secs(15);
const STALLED_503: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 100\r\n\r\nx";

/// One local HTTP response; no credentials or external service involved.
async fn client_for_response(
    response: Vec<u8>,
    stall: bool,
    timeout_override: Option<Duration>,
) -> (DeepSeekClient, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut peer, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let n = peer.read(&mut buffer).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&buffer[..n]);
        }
        // The reader may stop early on an oversized response.
        let _ = peer.write_all(&response).await;
        if stall {
            std::future::pending::<()>().await;
        }
    });
    let mut client = DeepSeekClient {
        http: reqwest::Client::builder().no_proxy().build().unwrap(),
        config: AgentConfig {
            api_key: Some("local-test-placeholder".into()),
            base_url: format!("http://{addr}"),
            ..AgentConfig::builtin()
        },
    };
    client.config.timeout = timeout_override.or(client.config.timeout);
    (client, server)
}

async fn stop_server(server: tokio::task::JoinHandle<()>) {
    server.abort();
    if let Err(error) = server.await {
        assert!(error.is_cancelled(), "test HTTP server failed: {error}");
    }
}

async fn error_for_response(
    response: Vec<u8>,
    stall: bool,
    timeout_override: Option<Duration>,
) -> AgentError {
    let (client, server) = client_for_response(response, stall, timeout_override).await;
    let result = tokio::time::timeout(
        RESPONSE_WATCHDOG,
        client.stream_chat(ChatRequest::streaming("test", vec![])),
    )
    .await;
    stop_server(server).await;
    match result.expect("local HTTP request must finish") {
        Err(error) => error,
        Ok(_) => panic!("expected an HTTP error, got a stream"),
    }
}

async fn assert_stalled_error_body_timeout(
    timeout_override: Option<Duration>,
    expected_timeout: Duration,
) {
    let (client, server) = client_for_response(STALLED_503.to_vec(), true, timeout_override).await;
    let response = tokio::time::timeout(
        RESPONSE_WATCHDOG,
        client.http.get(client.config.chat_completions_url()).send(),
    )
    .await
    .expect("local HTTP headers must arrive")
    .expect("local HTTP request must succeed");

    // The socket now holds a stalled body. Measure only the diagnostic read
    // with virtual time, independent of how long the loopback request took.
    tokio::time::pause();
    let start = tokio::time::Instant::now();
    let result = tokio::time::timeout(RESPONSE_WATCHDOG, client.read_api_error(response)).await;
    let elapsed = start.elapsed();
    stop_server(server).await;
    let error = result.expect("error-body read must terminate");
    let AgentError::Api { status, message } = error else {
        panic!("the HTTP status must survive the body timeout, got {error:?}");
    };
    assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert!(message.contains("timed out"), "{message}");

    // Tokio timers can round a deadline up to the next millisecond.
    assert!(
        (expected_timeout..=expected_timeout + Duration::from_millis(1)).contains(&elapsed),
        "expected a {expected_timeout:?} error-body timeout, got {elapsed:?}"
    );
}

#[tokio::test]
async fn stalled_error_body_returns_status_in_time_for_retry() {
    // This budget also covers the loopback request and response headers.
    let error = error_for_response(STALLED_503.to_vec(), true, Some(Duration::from_secs(1))).await;
    let AgentError::Api { status, message } = error else {
        panic!("the HTTP status must survive the body timeout, got {error:?}");
    };
    assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert!(message.contains("timed out"), "{message}");
}

#[tokio::test]
async fn stalled_error_body_honors_default_timeout_cap() {
    assert_stalled_error_body_timeout(None, Duration::from_secs(5)).await;
}

#[tokio::test]
async fn stalled_error_body_honors_shorter_configured_timeout() {
    let configured_timeout = Duration::from_secs(2);
    assert_stalled_error_body_timeout(Some(configured_timeout), configured_timeout).await;
}

#[tokio::test]
async fn oversized_error_body_is_capped_even_without_content_length() {
    let payload = "x".repeat(MAX_ERROR_BODY_BYTES * 2);
    let error = error_for_response(
        format!("HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n{payload}").into_bytes(),
        true,
        None,
    )
    .await;
    let AgentError::Api { status, message } = error else {
        panic!("expected API error, got {error:?}");
    };
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        message,
        format!(
            "{}\n[error response truncated]",
            "x".repeat(MAX_ERROR_BODY_BYTES)
        )
    );
}

#[tokio::test]
async fn ordinary_error_body_keeps_context_overflow_diagnostics() {
    let body = r#"{"error":{"code":"context_length_exceeded"}}"#;
    let error = error_for_response(
        format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes(),
        false,
        None,
    )
    .await;
    assert!(error.blames_context_window(), "{error}");
    assert!(
        matches!(&error, AgentError::Api { message, .. } if message == body),
        "{error:?}"
    );
}
