use super::*;
use base64::Engine as _;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

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
        let (peer, _) = listener.accept().await.unwrap();
        let (mut peer, _, _) = read_request(peer).await;
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
    assert!(
        matches!(&error, AgentError::Api { status, message } if status.as_u16() == 400 && message == body),
        "{error:?}"
    );
}

#[tokio::test]
async fn provider_echoed_credentials_are_removed_from_http_errors() {
    let body = r#"{"error":{"message":"rejected \u006cocal-test-\u0070laceholder"}}"#;
    let error = error_for_response(
        format!(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes(),
        false,
        None,
    )
    .await;
    assert!(matches!(error, AgentError::Api { status, message }
        if status.as_u16() == 401 && message.contains("rejected [redacted]")
            && !message.contains("local-test-placeholder")));
}

#[tokio::test]
async fn a_truncated_json_error_cannot_expose_an_escaped_credential() {
    let prefix = r#"{"error":{"message":"rejected \u006cocal-test-\u0070laceholder "#;
    for cut_escape in [false, true] {
        let cut = if cut_escape { r"\u0" } else { "" };
        let padding = "x".repeat(MAX_ERROR_BODY_BYTES - prefix.len() - cut.len());
        let tail = if cut_escape { "041" } else { "longer" };
        let body = format!("{prefix}{padding}{cut}{tail}\"}}}}");
        let error = error_for_response(
            format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_bytes(),
            false,
            None,
        )
        .await;
        let AgentError::Api { status, message } = error else {
            panic!("expected provider error")
        };
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(message.ends_with("[error response truncated]"));
        assert!(!message.contains("laceholder"));
        if cut_escape {
            assert!(message.contains("[truncated JSON string]"));
        } else {
            assert!(message.contains("rejected [redacted]"));
        }
    }
}

async fn read_request(peer: tokio::net::TcpStream) -> (tokio::net::TcpStream, String, Vec<u8>) {
    let mut peer = BufReader::new(peer);
    let mut request_line = String::new();
    peer.read_line(&mut request_line).await.unwrap();
    let mut length = None;
    let mut authorized = false;
    loop {
        let mut line = String::new();
        assert!(peer.read_line(&mut line).await.unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
        authorized |= line
            .trim_end_matches(['\r', '\n'])
            .eq_ignore_ascii_case("authorization: Bearer local-test-placeholder");
    }
    let length = if request_line.starts_with("POST ") {
        assert!(authorized);
        length.expect("POST includes a Content-Length")
    } else {
        assert!(request_line.starts_with("GET "), "{request_line:?}");
        length.unwrap_or(0)
    };
    // File-backed uploads can arrive after the headers. Closing a socket with
    // unread request bytes can reset it before the client receives the response.
    let mut body = vec![0; length];
    peer.read_exact(&mut body).await.unwrap();
    (peer.into_inner(), request_line, body)
}

#[tokio::test]
async fn file_backed_post_preserves_exact_json_through_same_origin_redirects() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    let mut raw = b"GIF89a".to_vec();
    // Several encoder blocks followed by a padded final block.
    raw.resize(72 * 1024 + 2, 0x5a);
    let original = workspace.join("original.gif");
    std::fs::write(&original, &raw).unwrap();
    let asset = crate::image::snapshot_path(&workspace, &original).unwrap();
    let request = ChatRequest::streaming(
        "test",
        vec![crate::Message::user_with_images(
            "read this",
            vec![asset.into()],
        )],
    );
    let request =
        crate::image::prepare_request_images(request, Some(crate::ImageDetail::Low), false, &[0])
            .unwrap()
            .request;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut first_body = None;
        for (path, redirect) in [
            ("/chat/completions", Some((307, "/upload/continue"))),
            ("/upload/continue", Some((308, "final"))),
            ("/upload/final", None),
        ] {
            let (peer, _) = listener.accept().await.unwrap();
            let (mut peer, request_line, body) = read_request(peer).await;
            assert_eq!(request_line, format!("POST {path} HTTP/1.1\r\n"));
            if let Some(expected) = &first_body {
                assert_eq!(&body, expected);
            }
            if let Some((status, location)) = redirect {
                first_body.get_or_insert(body);
                let response = format!(
                    "HTTP/1.1 {status} Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                peer.write_all(response.as_bytes()).await.unwrap();
                continue;
            }
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let image = &json["messages"][0]["content"][1]["image_url"];
            assert_eq!(image["detail"], "low");
            let url = image["url"].as_str().unwrap();
            let encoded = url.strip_prefix("data:image/gif;base64,").unwrap();
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .unwrap(),
                raw
            );
            peer.write_all(
            b"HTTP/1.1 413 Payload Too Large\r\nContent-Length: 7\r\nConnection: close\r\n\r\nrefused"
        ).await.unwrap();
        }
    });

    let client = DeepSeekClient {
        http: reqwest::Client::builder().no_proxy().build().unwrap(),
        config: AgentConfig {
            api_key: Some(" \tlocal-test-placeholder\r\n".into()),
            base_url: format!("http://{address}"),
            ..AgentConfig::builtin()
        },
    };
    let result = tokio::time::timeout(RESPONSE_WATCHDOG, client.stream_chat(request))
        .await
        .unwrap();
    server.await.unwrap();
    assert!(matches!(result, Err(AgentError::Api { status, message })
        if status.as_u16() == 413 && message == "refused"));
}

#[tokio::test]
async fn replay_keeps_verified_pixels_through_http_retry_and_rechecks_the_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().canonicalize().unwrap();
    let asset = crate::snapshot_bytes(&workspace, b"GIF89a-old-observation").unwrap();
    let path = asset.path();
    let data_url = asset.data_url().unwrap();
    let mut record = crate::SessionRecord::new(workspace, "system");
    record
        .entries
        .push(std::sync::Arc::new(crate::SessionEntry::user_with_images(
            "previous",
            vec![asset],
        )));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = AgentConfig {
        api_key: Some("local-test-placeholder".into()),
        base_url: format!("http://{}", listener.local_addr().unwrap()),
        stream_max_retries: 1,
        ..AgentConfig::builtin()
    };
    let client = DeepSeekClient {
        http: reqwest::Client::builder().no_proxy().build().unwrap(),
        config: config.clone(),
    };
    let runtime = crate::AgentRuntime::from_record(
        client,
        crate::ToolRegistry::default(),
        record,
        None,
        config,
    );
    let server = tokio::spawn(async move {
        for attempt in 0..3 {
            let (peer, _) = listener.accept().await.unwrap();
            let (mut peer, _, body) = read_request(peer).await;
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let history = &json["messages"][1]["content"];
            if attempt < 2 {
                assert_eq!(history[1]["image_url"]["url"], data_url);
            } else {
                assert!(history.as_str().unwrap().contains("image unavailable"));
            }
            if attempt == 0 {
                std::fs::remove_file(&path).unwrap();
                peer.write_all(
                    b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            } else {
                let events = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",
                    events.len()
                );
                peer.write_all(response.as_bytes()).await.unwrap();
            }
        }
    });
    tokio::time::timeout(RESPONSE_WATCHDOG, async {
        for (text, included) in [("follow up", true), ("continue", false)] {
            let mut turn = runtime.start_turn(text).await;
            let mut observed = None;
            while let Some(event) = turn.recv().await {
                if let crate::RuntimeEvent::ImageRequestPrepared { images, .. } = event {
                    observed = images.first().map(|image| image.included);
                }
            }
            assert_eq!(
                turn.outcome().unwrap().reason,
                crate::FinishReason::Completed
            );
            assert_eq!(observed, Some(included));
        }
        server.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn file_backed_post_does_not_replay_to_a_different_origin() {
    let other = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let response = format!(
        "HTTP/1.1 307 Redirect\r\nLocation: http://{}/upload\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        other.local_addr().unwrap()
    );
    let (client, server) = client_for_response(response.into_bytes(), false, None).await;
    let request = ChatRequest::streaming(
        "test",
        vec![crate::Message::user_with_images(
            "read this",
            vec![crate::ImageRef::Url {
                url: "data:image/gif;base64,R0lGODlh".into(),
                detail: None,
            }],
        )],
    );
    let result = tokio::time::timeout(RESPONSE_WATCHDOG, async {
        tokio::select! {
            biased;
            accepted = other.accept() => {
                accepted.expect("accept on the second origin");
                panic!("image request connected to a different origin");
            }
            result = client.stream_chat(request) => result,
        }
    })
    .await
    .expect("image request must finish");
    let error = result.err().expect("expected the original 307 response");
    assert!(
        matches!(
            error,
            AgentError::Api {
                status: StatusCode::TEMPORARY_REDIRECT,
                ..
            }
        ),
        "expected an HTTP 307, got {error:?}"
    );
    server.await.unwrap();
}
