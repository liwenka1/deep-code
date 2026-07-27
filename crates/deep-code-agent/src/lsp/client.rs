//! Stdio transport speaking the LSP base protocol to a language server.
//!
//! The transport is deliberately narrow: it exists to push document contents
//! at the server (`didOpen`/`didChange`) and harvest the resulting
//! `textDocument/publishDiagnostics` notifications. It never issues requests
//! that expect replies beyond the `initialize` handshake, so there is no
//! request/response correlation machinery — one background task decodes the
//! server's stdout and forwards diagnostics batches over a channel, while
//! each write to the child's stdin is one whole frame, performed on a
//! spawned task behind a mutex so cancellation can never tear a frame.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout_at};

use super::diagnostics::{Diagnostic, DiagnosticRange, Severity};
use super::path_util::{normalize_path, paths_equal};
use super::registry::Language;

/// How the manager talks to a language server. Production code uses
/// [`StdioLspTransport`]; tests substitute in-process stubs.
#[async_trait]
pub trait LspTransport: Send + Sync {
    /// Push the current `text` of `path` to the server (`didOpen` on first
    /// contact, `didChange` afterwards), then wait at most `wait` for the
    /// server to publish diagnostics for that file. An elapsed budget yields
    /// an empty list, not an error.
    async fn diagnostics_for(
        &self,
        path: &Path,
        text: &str,
        wait: Duration,
    ) -> Result<Vec<Diagnostic>>;

    /// Tear down the underlying server process (best effort).
    async fn shutdown(&self);
}

/// One `publishDiagnostics` notification, decoded.
struct Publication {
    file: PathBuf,
    /// Document version the batch was computed against, when the server
    /// reports one (the field is optional per the LSP spec). Used to reject
    /// stale batches left over from an earlier `didChange`.
    version: Option<i64>,
    items: Vec<Diagnostic>,
}

/// Shared handle to the server's stdin. Boxed so tests can substitute an
/// in-memory pipe for the real child process.
type SharedSink = Arc<AsyncMutex<Box<dyn AsyncWrite + Send + Unpin>>>;

/// Capacity of the publication channel. Servers publish one batch per
/// analyzed file and the manager drains between edits, so backlog stays tiny;
/// this bound only guards against a runaway server flooding notifications.
const PUBLICATION_QUEUE: usize = 32;

/// Transport over a child process's stdin/stdout using `Content-Length`
/// framed JSON-RPC, per the LSP base protocol.
pub struct StdioLspTransport {
    /// The server process. Killed on [`shutdown`](LspTransport::shutdown);
    /// `kill_on_drop` covers the paths that never call it.
    server: AsyncMutex<Option<Child>>,
    /// Writes are whole frames, serialized by this lock. Shared with the
    /// spawned writer tasks in [`write_frame`], which own each write so a
    /// cancelled caller can never leave half a frame on the wire.
    stdin: SharedSink,
    /// Diagnostics batches decoded by the reader task, oldest first.
    publications: AsyncMutex<mpsc::Receiver<Publication>>,
    /// `languageId` reported when opening documents.
    language_id: &'static str,
    /// Version per opened document; presence means `didOpen` was already sent.
    doc_versions: AsyncMutex<HashMap<PathBuf, i64>>,
}

impl StdioLspTransport {
    /// Launch `command args…`, wire up the reader task, and run the
    /// `initialize`/`initialized` handshake rooted at `workspace`.
    pub async fn spawn(
        command: &str,
        args: &[String],
        language: Language,
        workspace: PathBuf,
    ) -> Result<Self> {
        let mut launch = Command::new(command);
        launch
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // A language server has no business reading the agent's credentials:
        // scrub provider/runtime secrets from the environment it inherits.
        for name in crate::config::SUBPROCESS_SECRET_ENV {
            launch.env_remove(name);
        }

        let mut server = launch
            .spawn()
            .with_context(|| format!("could not launch language server `{command}`"))?;
        let stdin = server
            .stdin
            .take()
            .context("language server exposes no stdin pipe")?;
        let stdin: SharedSink = Arc::new(AsyncMutex::new(Box::new(stdin)));
        let stdout = server
            .stdout
            .take()
            .context("language server exposes no stdout pipe")?;

        let (publish_tx, publish_rx) = mpsc::channel::<Publication>(PUBLICATION_QUEUE);
        tokio::spawn(pump_server_output(stdout, publish_tx));

        let root = file_uri(&workspace);
        write_frame(
            &stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "processId": std::process::id(),
                    "rootUri": root,
                    "capabilities": {
                        "textDocument": {
                            "publishDiagnostics": { "relatedInformation": false }
                        }
                    },
                    "workspaceFolders": [{ "uri": root, "name": "workspace" }]
                }
            }),
        )
        .await?;
        // Servers queue client notifications until they are ready, so fire
        // `initialized` without blocking on the initialize reply — the first
        // publishDiagnostics arrives once analysis has actually warmed up,
        // and blocking here would only add latency to the first edit.
        write_frame(
            &stdin,
            &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
        )
        .await?;

        Ok(Self {
            server: AsyncMutex::new(Some(server)),
            stdin,
            publications: AsyncMutex::new(publish_rx),
            language_id: language.language_id(),
            doc_versions: AsyncMutex::new(HashMap::new()),
        })
    }

    /// Transport wired to a caller-supplied sink and publication queue, with
    /// no server process behind it. Lets tests drive `diagnostics_for`
    /// without spawning anything.
    #[cfg(test)]
    fn for_tests(
        sink: Box<dyn AsyncWrite + Send + Unpin>,
        publications: mpsc::Receiver<Publication>,
    ) -> Self {
        Self {
            server: AsyncMutex::new(None),
            stdin: Arc::new(AsyncMutex::new(sink)),
            publications: AsyncMutex::new(publications),
            language_id: "rust",
            doc_versions: AsyncMutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl LspTransport for StdioLspTransport {
    async fn diagnostics_for(
        &self,
        path: &Path,
        text: &str,
        wait: Duration,
    ) -> Result<Vec<Diagnostic>> {
        // Canonicalize once so the URI we send and the paths the server
        // publishes back compare under one spelling (symlinks, /var vs
        // /private/var on macOS).
        let file = normalize_path(path);
        let uri = file_uri(&file);

        let (version, first_contact) = {
            let mut versions = self.doc_versions.lock().await;
            let slot = versions.entry(file.clone()).or_insert(0);
            *slot += 1;
            (*slot, *slot == 1)
        };

        let notification = if first_contact {
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": uri,
                        "languageId": self.language_id,
                        "version": version,
                        "text": text
                    }
                }
            })
        } else {
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{ "text": text }]
                }
            })
        };
        // Hold the queue for the whole purge → send → wait sequence so a
        // concurrent query cannot interleave and consume batches meant for
        // this one. Known trade-off: this serializes same-transport queries
        // even for different files, so their waits queue up instead of
        // overlapping. Correctness needs the exclusivity (the queue carries
        // publications for every file, and a purge must not eat another
        // query's answer); the latency cost is bounded by the per-query
        // timeout and the mostly-serial nature of the servers themselves.
        let mut publications = self.publications.lock().await;

        // First staleness layer: drop everything already queued. A late
        // batch from an earlier `didChange` must not be mistaken for the
        // answer to this query; batches for other files are leftovers the
        // wait loop below would discard anyway.
        while publications.try_recv().is_ok() {}

        write_frame(&self.stdin, &notification).await?;

        // Wait for the batch that names *this* file; batches for other files
        // (from earlier opens) are discarded along the way. Budget exhaustion
        // and a closed channel both end the wait with whatever we have.
        let deadline = Instant::now() + wait;
        while let Ok(Some(batch)) = timeout_at(deadline, publications.recv()).await {
            if !paths_equal(&batch.file, &file) {
                continue;
            }
            // Second staleness layer: when the server echoes the document
            // version, a batch computed against an older version than the
            // one just sent is skipped in favor of a newer one.
            if batch.version.is_some_and(|reported| reported < version) {
                continue;
            }
            return Ok(batch.items);
        }
        Ok(Vec::new())
    }

    async fn shutdown(&self) {
        if let Some(mut server) = self.server.lock().await.take() {
            let _ = server.start_kill();
            let _ = server.wait().await;
        }
    }
}

/// Encode `message` with base-protocol framing into one contiguous buffer.
/// Header and body must travel as a single write: two writes would let a
/// cancellation land between them and desynchronize the stream.
fn encode_frame(message: &Value) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(message).context("encode JSON-RPC body")?;
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Write one complete frame to `sink`. The write runs on a spawned task, so
/// it is atomic under cancellation: if the awaiting future is dropped (the
/// manager wraps queries in a timeout), the task still finishes the frame
/// instead of leaving a torn header behind and wedging the stream for the
/// rest of the session.
async fn write_frame(sink: &SharedSink, message: &Value) -> Result<()> {
    let frame = encode_frame(message)?;
    let sink = Arc::clone(sink);
    let write = tokio::spawn(async move {
        let mut sink = sink.lock().await;
        sink.write_all(&frame)
            .await
            .context("write frame to language server")?;
        sink.flush().await.context("flush language server stdin")
    });
    write.await.context("frame writer task terminated")?
}

/// Background task owning the server's stdout: decodes frames and forwards
/// every `publishDiagnostics` batch. All other traffic — the initialize
/// reply, log notifications, server-to-client requests — is dropped, since
/// this transport never awaits a reply. Ends when the pipe or channel closes.
async fn pump_server_output(
    mut stdout: tokio::process::ChildStdout,
    publish_tx: mpsc::Sender<Publication>,
) {
    let mut decoder = FrameDecoder::default();
    let mut chunk = [0u8; 4096];
    loop {
        let read = match stdout.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        decoder.feed(&chunk[..read]);
        while let Some(body) = decoder.next_frame() {
            let Ok(message) = serde_json::from_slice::<Value>(&body) else {
                continue; // undecodable body: skip the frame, keep the stream
            };
            let is_publish = message.get("method").and_then(Value::as_str)
                == Some("textDocument/publishDiagnostics");
            if is_publish
                && let Some(publication) = decode_publish(&message)
                && publish_tx.send(publication).await.is_err()
            {
                return; // receiver gone: transport was dropped
            }
        }
    }
}

/// Incremental decoder for `Content-Length` framed byte streams.
///
/// Feed bytes as they arrive; complete frame bodies come out. Header names
/// match case-insensitively (the base protocol allows any case) and a header
/// block without a usable `Content-Length` is skipped rather than wedging
/// the stream.
#[derive(Default)]
struct FrameDecoder {
    pending: Vec<u8>,
}

impl FrameDecoder {
    fn feed(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    fn next_frame(&mut self) -> Option<Vec<u8>> {
        loop {
            let header_end = find_header_terminator(&self.pending)?;
            let body_start = header_end + 4;
            match declared_length(&self.pending[..header_end]) {
                Some(length) => {
                    if self.pending.len() < body_start + length {
                        return None; // body not fully buffered yet
                    }
                    let body = self.pending[body_start..body_start + length].to_vec();
                    self.pending.drain(..body_start + length);
                    return Some(body);
                }
                None => {
                    // Malformed header block: discard it and keep scanning.
                    self.pending.drain(..body_start);
                }
            }
        }
    }
}

/// Byte offset of the `\r\n\r\n` that ends the header block, if buffered.
fn find_header_terminator(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|quad| quad == b"\r\n\r\n")
}

/// Pull the `Content-Length` value out of a raw header block.
fn declared_length(header: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(header).ok()?;
    for line in text.lines() {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            return value.trim().parse().ok();
        }
    }
    None
}

/// Decode a `publishDiagnostics` notification into our normalized shape.
fn decode_publish(message: &Value) -> Option<Publication> {
    let params = message.get("params")?;
    let file = normalize_path(&uri_to_path(params.get("uri")?.as_str()?)?);
    let version = params.get("version").and_then(Value::as_i64);
    let raw_items = params.get("diagnostics")?.as_array()?;
    let mut items = Vec::with_capacity(raw_items.len());
    for raw in raw_items {
        items.push(decode_diagnostic(&file, raw)?);
    }
    Some(Publication {
        file,
        version,
        items,
    })
}

fn decode_diagnostic(file: &Path, raw: &Value) -> Option<Diagnostic> {
    let range = raw.get("range")?;
    Some(Diagnostic {
        file: file.to_path_buf(),
        range: DiagnosticRange {
            start_line: position(range, "start", "line")?,
            start_column: position(range, "start", "character")?,
            end_line: position(range, "end", "line")?,
            end_column: position(range, "end", "character")?,
        },
        // An absent or unknown severity is treated as an error: better to
        // over-report than to silently drop a finding.
        severity: Severity::from_lsp(raw.get("severity").and_then(Value::as_i64))
            .unwrap_or(Severity::Error),
        message: raw
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        source: raw.get("source").and_then(Value::as_str).map(String::from),
        code: raw.get("code").and_then(code_text),
    })
}

/// Fetch `range.{edge}.{axis}` and shift from the wire's 0-based coordinates
/// to the 1-based ones used for display.
fn position(range: &Value, edge: &str, axis: &str) -> Option<u32> {
    let zero_based = range.get(edge)?.get(axis)?.as_u64()?;
    u32::try_from(zero_based).ok().map(|n| n.saturating_add(1))
}

/// A diagnostic `code` may arrive as a string, a number, or (LSP 3.16+) an
/// object wrapping a `value`; flatten all three to display text.
fn code_text(raw: &Value) -> Option<String> {
    match raw {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Object(fields) => fields.get("value").and_then(code_text),
        _ => None,
    }
}

/// Render a path as a `file://` URI. The path is canonicalized first so the
/// server and our bookkeeping agree on one spelling, and percent-encoded
/// (RFC 3986: unreserved plus `/` and `:` stay literal) — without encoding,
/// spaces or non-ASCII path segments make servers echo an escaped URI that
/// never matches ours, and their diagnostics are silently dropped.
/// Unix-oriented: Windows drive letters are out of scope for now.
fn file_uri(path: &Path) -> String {
    let resolved = normalize_path(path);
    let text = resolved.to_string_lossy();
    let trimmed = text.strip_prefix('/').unwrap_or(&text);
    let mut encoded = String::with_capacity(trimmed.len());
    for byte in trimmed.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    format!("file:///{encoded}")
}

/// Extract the filesystem path from a `file://` URI, percent-decoding it so a
/// server-escaped URI (spaces, non-ASCII) maps back to the real path. Any
/// other scheme — or a malformed escape — is a document we cannot attribute,
/// so it maps to `None`.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(rest.len());
    let mut iter = rest.bytes();
    while let Some(byte) = iter.next() {
        if byte == b'%' {
            let hex = [iter.next()?, iter.next()?];
            let hex = std::str::from_utf8(&hex).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
        } else {
            bytes.push(byte);
        }
    }
    Some(PathBuf::from(String::from_utf8_lossy(&bytes).into_owned()))
}

#[cfg(test)]
mod tests {
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
}
