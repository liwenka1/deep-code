use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One local HTTP response; no credentials or external service involved.
async fn client_for_response(
    response: Vec<u8>,
    stall: bool,
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
    let client = DeepSeekClient {
        http: reqwest::Client::builder().no_proxy().build().unwrap(),
        config: AgentConfig {
            api_key: Some("local-test-placeholder".into()),
            base_url: format!("http://{addr}"),
            timeout: Some(Duration::from_millis(100)),
            ..AgentConfig::builtin()
        },
    };
    (client, server)
}

#[tokio::test]
async fn stalled_error_body_returns_status_in_time_for_retry() {
    let (client, server) = client_for_response(
        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 100\r\n\r\nx".to_vec(),
        true,
    )
    .await;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        client.stream_chat(ChatRequest::streaming("test", vec![])),
    )
    .await;
    server.abort();
    let Err(AgentError::Api { status, message }) = result.expect("error-body read must terminate")
    else {
        panic!("the HTTP status must survive the body timeout");
    };
    assert_eq!(status, reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert!(message.contains("timed out"), "{message}");
}

#[tokio::test]
async fn oversized_error_body_is_capped_even_without_content_length() {
    let payload = "x".repeat(MAX_ERROR_BODY_BYTES * 2);
    let (client, server) = client_for_response(
        format!("HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n{payload}").into_bytes(),
        true,
    )
    .await;
    let result = client
        .stream_chat(ChatRequest::streaming("test", vec![]))
        .await;
    server.abort();
    let Err(AgentError::Api { status, message }) = result else {
        panic!("expected API error");
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
    let (client, server) = client_for_response(
        format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes(),
        false,
    )
    .await;
    let Err(error) = client
        .stream_chat(ChatRequest::streaming("test", vec![]))
        .await
    else {
        panic!("expected API error");
    };
    server.await.unwrap();
    assert!(error.blames_context_window(), "{error}");
    assert!(matches!(error, AgentError::Api { message, .. } if message == body));
}
