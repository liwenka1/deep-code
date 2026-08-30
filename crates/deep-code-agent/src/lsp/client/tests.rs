use super::*;

fn frame(body: &str) -> Vec<u8> {
    format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

#[test]
fn decoder_yields_a_complete_frame() {
    let mut decoder = FrameDecoder::default();
    decoder.feed(&frame(r#"{"ok":true}"#));
    assert_eq!(
        decoder.next_frame().as_deref(),
        Some(br#"{"ok":true}"#.as_slice())
    );
    assert!(decoder.next_frame().is_none());
}

#[test]
fn decoder_survives_byte_by_byte_delivery() {
    let mut decoder = FrameDecoder::default();
    let wire = frame("abc");
    for (i, byte) in wire.iter().enumerate() {
        decoder.feed(std::slice::from_ref(byte));
        if i + 1 < wire.len() {
            assert!(
                decoder.next_frame().is_none(),
                "premature frame at byte {i}"
            );
        }
    }
    assert_eq!(decoder.next_frame().as_deref(), Some(b"abc".as_slice()));
}

#[test]
fn decoder_splits_coalesced_frames() {
    let mut decoder = FrameDecoder::default();
    let mut wire = frame("first");
    wire.extend_from_slice(&frame("second!"));
    decoder.feed(&wire);
    assert_eq!(decoder.next_frame().as_deref(), Some(b"first".as_slice()));
    assert_eq!(decoder.next_frame().as_deref(), Some(b"second!".as_slice()));
    assert!(decoder.next_frame().is_none());
}

#[test]
fn header_matching_is_case_insensitive_and_skips_extras() {
    let mut decoder = FrameDecoder::default();
    decoder.feed(b"Content-Type: application/vscode-jsonrpc\r\ncontent-length: 2\r\n\r\nhi");
    assert_eq!(decoder.next_frame().as_deref(), Some(b"hi".as_slice()));
}

#[test]
fn header_without_length_does_not_wedge_the_stream() {
    let mut decoder = FrameDecoder::default();
    decoder.feed(b"X-Nonsense: yes\r\n\r\n");
    decoder.feed(&frame("ok"));
    assert_eq!(decoder.next_frame().as_deref(), Some(b"ok".as_slice()));
}

#[test]
fn publish_notification_decodes_with_one_based_positions() {
    let message = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": {
            "uri": "file:///work/lib/parse.rs",
            "version": 7,
            "diagnostics": [{
                "range": {
                    "start": { "line": 0, "character": 4 },
                    "end": { "line": 2, "character": 0 }
                },
                "severity": 2,
                "message": "unreachable expression",
                "source": "rustc",
                "code": 42
            }]
        }
    });
    let publication = decode_publish(&message).expect("valid publish payload");
    assert_eq!(publication.file, PathBuf::from("/work/lib/parse.rs"));
    assert_eq!(publication.version, Some(7));
    let item = &publication.items[0];
    assert_eq!(item.range.start_line, 1);
    assert_eq!(item.range.start_column, 5);
    assert_eq!(item.range.end_line, 3);
    assert_eq!(item.severity, Severity::Warning);
    assert_eq!(
        item.code.as_deref(),
        Some("42"),
        "numeric code becomes text"
    );
    assert_eq!(item.source.as_deref(), Some("rustc"));
}

#[test]
fn missing_severity_defaults_to_error_and_object_codes_flatten() {
    let raw = json!({
        "range": {
            "start": { "line": 9, "character": 0 },
            "end": { "line": 9, "character": 1 }
        },
        "message": "boom",
        "code": { "value": "TS2304", "target": "https://example.invalid" }
    });
    let item = decode_diagnostic(Path::new("/w/a.ts"), &raw).expect("decodes");
    assert_eq!(item.severity, Severity::Error);
    assert_eq!(item.code.as_deref(), Some("TS2304"));
}

#[test]
fn file_uri_round_trips_spaces_and_non_ascii() {
    let path = Path::new("/tmp/我的 项目/src/main.rs");
    let uri = file_uri(path);
    assert!(uri.starts_with("file:///"));
    assert!(!uri.contains(' '), "spaces must be percent-encoded: {uri}");
    assert_eq!(uri_to_path(&uri).as_deref(), Some(path));
}

#[test]
fn uri_to_path_decodes_server_escaped_uris() {
    assert_eq!(
        uri_to_path("file:///a/with%20space/%E4%B8%AD.rs").as_deref(),
        Some(Path::new("/a/with space/中.rs"))
    );
    assert!(uri_to_path("file:///bad%2").is_none());
    assert!(uri_to_path("file:///bad%zz").is_none());
}

#[test]
fn non_file_uris_are_rejected() {
    assert!(uri_to_path("untitled:Untitled-1").is_none());
    assert_eq!(
        uri_to_path("file:///a/b.rs"),
        Some(PathBuf::from("/a/b.rs"))
    );
}

#[test]
fn version_is_optional_and_decodes_to_none_when_absent() {
    let message = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": { "uri": "file:///work/a.rs", "diagnostics": [] }
    });
    let publication = decode_publish(&message).expect("decodes");
    assert_eq!(publication.version, None);
}

#[test]
fn absurd_content_length_is_rejected_instead_of_panicking() {
    // `usize::MAX` used to overflow `body_start + length`; with
    // overflow-checks off (release) it wrapped, passed the buffered-body
    // test, and panicked slicing start > end — inside a detached task under
    // `panic = "abort"`, killing the process. It must now read as a
    // malformed header: dropped, and scanning resyncs on the next frame.
    let mut decoder = FrameDecoder::default();
    decoder.feed(format!("Content-Length: {}\r\n\r\n", usize::MAX).as_bytes());
    assert!(decoder.next_frame().is_none());

    let body = br#"{"jsonrpc":"2.0"}"#;
    decoder.feed(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    decoder.feed(body);
    assert_eq!(decoder.next_frame().as_deref(), Some(body.as_slice()));
}

#[test]
fn oversized_but_parsable_content_length_is_rejected() {
    // Not an overflow — just unbounded buffering. 64 MiB is the ceiling.
    let mut decoder = FrameDecoder::default();
    decoder.feed(b"Content-Length: 4000000000\r\n\r\n");
    assert!(decoder.next_frame().is_none());
    assert!(declared_length(b"Content-Length: 4000000000").is_none());
    assert_eq!(declared_length(b"Content-Length: 17"), Some(17));
}

#[test]
fn frames_encode_as_one_contiguous_buffer() {
    let message = json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} });
    let body = serde_json::to_vec(&message).unwrap();
    let mut expected = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    expected.extend_from_slice(&body);
    assert_eq!(encode_frame(&message).unwrap(), expected);
}

#[tokio::test]
async fn cancelled_write_still_delivers_a_complete_frame() {
    // A 16-byte pipe with no reader forces the write to block mid-frame.
    let (near, mut far) = tokio::io::duplex(16);
    let sink: SharedSink = Arc::new(AsyncMutex::new(Box::new(near)));
    let message = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/didChange",
        "params": { "pad": "x".repeat(512) }
    });
    let expected = encode_frame(&message).unwrap();

    // Cancel the awaiting future while the writer is still blocked; the
    // spawned task must finish the frame anyway.
    let cancelled =
        tokio::time::timeout(Duration::from_millis(5), write_frame(&sink, &message)).await;
    assert!(
        cancelled.is_err(),
        "the blocked write should outlive the timeout"
    );

    let mut received = vec![0u8; expected.len()];
    far.read_exact(&mut received).await.unwrap();
    assert_eq!(received, expected, "no torn frame despite the cancellation");
}

fn item(file: &Path, message: &str) -> Diagnostic {
    Diagnostic {
        file: file.to_path_buf(),
        range: DiagnosticRange {
            start_line: 1,
            start_column: 1,
            end_line: 1,
            end_column: 2,
        },
        severity: Severity::Error,
        message: message.to_owned(),
        source: None,
        code: None,
    }
}

#[tokio::test]
async fn stale_queued_publication_is_not_returned() {
    let (feed, queue) = mpsc::channel(PUBLICATION_QUEUE);
    let transport = StdioLspTransport::for_tests(Box::new(tokio::io::sink()), queue);
    let file = PathBuf::from("/work/src/lib.rs");

    // A leftover batch from a query that timed out earlier.
    feed.send(Publication {
        file: file.clone(),
        version: None,
        items: vec![item(&file, "already fixed")],
    })
    .await
    .unwrap();

    let items = transport
        .diagnostics_for(&file, "fn main() {}", Duration::from_millis(30))
        .await
        .unwrap();
    assert!(items.is_empty(), "a batch queued before the query is stale");
}

#[tokio::test]
async fn version_tagged_stale_batch_yields_to_the_newer_one() {
    let (feed, queue) = mpsc::channel(PUBLICATION_QUEUE);
    let transport = StdioLspTransport::for_tests(Box::new(tokio::io::sink()), queue);
    let file = PathBuf::from("/work/src/lib.rs");

    // First contact bumps the document to version 1; nothing arrives.
    let opened = transport
        .diagnostics_for(&file, "fn main() {", Duration::from_millis(10))
        .await
        .unwrap();
    assert!(opened.is_empty());

    // The second query runs against version 2. A late version-1 batch
    // lands first and must be skipped in favor of the version-2 one.
    let feeder = tokio::spawn({
        let file = file.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            feed.send(Publication {
                file: file.clone(),
                version: Some(1),
                items: vec![item(&file, "expected `}`")],
            })
            .await
            .unwrap();
            feed.send(Publication {
                file: file.clone(),
                version: Some(2),
                items: vec![item(&file, "fresh finding")],
            })
            .await
            .unwrap();
        }
    });

    let items = transport
        .diagnostics_for(&file, "fn main() {}", Duration::from_secs(5))
        .await
        .unwrap();
    feeder.await.unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].message, "fresh finding");
}

// Unix-focused: Windows drive-letter URIs are not normalized yet.
#[cfg(unix)]
#[test]
fn published_paths_match_queries_through_symlinked_temp_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("mod.rs");
    std::fs::write(&source, b"pub fn f() {}").unwrap();

    // Round-trip: our own URI for the file, fed back through a publish
    // payload, must compare equal to the original (possibly symlinked)
    // query path.
    let message = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": { "uri": file_uri(&source), "diagnostics": [] }
    });
    let publication = decode_publish(&message).expect("decodes");
    assert!(paths_equal(&publication.file, &source));
    assert!(publication.items.is_empty());
}
