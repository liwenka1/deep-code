//! Minimal stdio JSON-RPC client for post-edit LSP diagnostics.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use super::diagnostics::{Diagnostic, DiagnosticRange, Severity};
use super::path_util::{normalize_path, paths_equal};
use super::registry::Language;

#[async_trait]
pub trait LspTransport: Send + Sync {
    async fn diagnostics_for(
        &self,
        path: &Path,
        text: &str,
        wait: Duration,
    ) -> Result<Vec<Diagnostic>>;

    async fn shutdown(&self);
}

pub struct StdioLspTransport {
    #[allow(dead_code)]
    child: AsyncMutex<Option<Child>>,
    tx_outbound: mpsc::Sender<Vec<u8>>,
    diagnostics_rx: AsyncMutex<mpsc::Receiver<(PathBuf, Vec<Diagnostic>)>>,
    /// Reserved for future LSP request/reply methods.
    #[allow(dead_code)]
    pending: Arc<AsyncMutex<HashMap<i64, oneshot::Sender<Value>>>>,
    language_id: &'static str,
    opened: AsyncMutex<HashMap<PathBuf, i64>>,
}

impl StdioLspTransport {
    pub async fn spawn(
        command: &str,
        args: &[String],
        language: Language,
        workspace: PathBuf,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        // No LSP server needs the provider/runtime secrets; strip them so a
        // third-party language server can't read them from its environment.
        for var in crate::config::SUBPROCESS_SECRET_ENV {
            cmd.env_remove(var);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn LSP server `{command}`"))?;

        let stdin = child
            .stdin
            .take()
            .context("LSP child has no stdin handle")?;
        let stdout = child
            .stdout
            .take()
            .context("LSP child has no stdout handle")?;

        let (tx_outbound, rx_outbound) = mpsc::channel::<Vec<u8>>(64);
        let (tx_inbound, rx_inbound) = mpsc::channel::<Value>(64);
        let (tx_diag, rx_diag) = mpsc::channel::<(PathBuf, Vec<Diagnostic>)>(64);

        tokio::spawn(writer_task(stdin, rx_outbound));
        tokio::spawn(reader_task(stdout, tx_inbound));

        let pending: Arc<AsyncMutex<HashMap<i64, oneshot::Sender<Value>>>> =
            Arc::new(AsyncMutex::new(HashMap::new()));
        tokio::spawn(dispatcher_task(rx_inbound, tx_diag, pending.clone()));

        let init_payload = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": uri_from_path(&workspace),
                "capabilities": {
                    "textDocument": {
                        "publishDiagnostics": { "relatedInformation": false }
                    }
                },
                "workspaceFolders": [{
                    "uri": uri_from_path(&workspace),
                    "name": "workspace"
                }]
            }
        });
        send_message(&tx_outbound, &init_payload).await?;

        let initialized = json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        });
        send_message(&tx_outbound, &initialized).await?;

        Ok(Self {
            child: AsyncMutex::new(Some(child)),
            tx_outbound,
            diagnostics_rx: AsyncMutex::new(rx_diag),
            pending,
            language_id: language.language_id(),
            opened: AsyncMutex::new(HashMap::new()),
        })
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
        let path_key = normalize_path(path);
        let uri = uri_from_path(&path_key);

        let mut opened = self.opened.lock().await;
        let is_new = !opened.contains_key(&path_key);
        let new_version = opened.get(&path_key).copied().unwrap_or(0) + 1;
        opened.insert(path_key.clone(), new_version);
        drop(opened);

        let payload = if is_new {
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": uri.clone(),
                        "languageId": self.language_id,
                        "version": new_version,
                        "text": text
                    }
                }
            })
        } else {
            json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": {
                        "uri": uri.clone(),
                        "version": new_version
                    },
                    "contentChanges": [{ "text": text }]
                }
            })
        };
        send_message(&self.tx_outbound, &payload).await?;

        let deadline = tokio::time::Instant::now() + wait;
        let mut latest: Option<Vec<Diagnostic>> = None;

        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            let mut rx = self.diagnostics_rx.lock().await;
            let next = match timeout(remaining, rx.recv()).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => break,
            };
            drop(rx);
            let (file, items) = next;
            if paths_equal(&file, &path_key) {
                latest = Some(items);
                break;
            }
        }
        Ok(latest.unwrap_or_default())
    }

    async fn shutdown(&self) {
        let mut child = self.child.lock().await;
        if let Some(mut process) = child.take() {
            let _ = process.start_kill();
            let _ = process.wait().await;
        }
    }
}

async fn send_message(tx: &mpsc::Sender<Vec<u8>>, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value).context("serialize LSP message")?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut frame = Vec::with_capacity(header.len() + body.len());
    frame.extend_from_slice(header.as_bytes());
    frame.extend_from_slice(&body);
    tx.send(frame)
        .await
        .map_err(|_| anyhow!("LSP outbound channel closed"))?;
    Ok(())
}

async fn writer_task(mut stdin: tokio::process::ChildStdin, mut rx: mpsc::Receiver<Vec<u8>>) {
    while let Some(frame) = rx.recv().await {
        if stdin.write_all(&frame).await.is_err() {
            break;
        }
        if stdin.flush().await.is_err() {
            break;
        }
    }
}

async fn reader_task(mut stdout: tokio::process::ChildStdout, tx: mpsc::Sender<Value>) {
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut tmp = [0u8; 4096];
    loop {
        let n = match stdout.read(&mut tmp).await {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };
        buf.extend_from_slice(&tmp[..n]);
        while let Some((header_end, content_length)) = parse_header(&buf) {
            if buf.len() < header_end + content_length {
                break;
            }
            let body = &buf[header_end..header_end + content_length];
            let parsed = serde_json::from_slice::<Value>(body).ok();
            buf.drain(..header_end + content_length);
            if let Some(value) = parsed
                && tx.send(value).await.is_err()
            {
                return;
            }
        }
    }
}

fn parse_header(buf: &[u8]) -> Option<(usize, usize)> {
    let term = b"\r\n\r\n";
    let pos = buf.windows(term.len()).position(|window| window == term)?;
    let header = std::str::from_utf8(&buf[..pos]).ok()?;
    let mut content_length: Option<usize> = None;
    for line in header.split("\r\n") {
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse::<usize>().ok();
        }
    }
    content_length.map(|length| (pos + term.len(), length))
}

async fn dispatcher_task(
    mut rx: mpsc::Receiver<Value>,
    tx_diag: mpsc::Sender<(PathBuf, Vec<Diagnostic>)>,
    pending: Arc<AsyncMutex<HashMap<i64, oneshot::Sender<Value>>>>,
) {
    while let Some(value) = rx.recv().await {
        let method = value.get("method").and_then(Value::as_str);
        if method == Some("textDocument/publishDiagnostics") {
            if let Some((path, diags)) = parse_publish_diagnostics(&value) {
                let _ = tx_diag.send((path, diags)).await;
            }
            continue;
        }
        if let Some(id) = value.get("id").and_then(Value::as_i64) {
            let mut map = pending.lock().await;
            if let Some(slot) = map.remove(&id) {
                let _ = slot.send(value);
            }
        }
    }
}

fn parse_publish_diagnostics(value: &Value) -> Option<(PathBuf, Vec<Diagnostic>)> {
    let params = value.get("params")?;
    let uri = params.get("uri")?.as_str()?;
    let path = normalize_path(&path_from_uri(uri)?);
    let raw = params.get("diagnostics")?.as_array()?;
    let mut out = Vec::with_capacity(raw.len());
    for entry in raw {
        let range = entry.get("range")?;
        let start = range.get("start")?;
        let end = range.get("end")?;
        let severity = Severity::from_lsp(entry.get("severity").and_then(Value::as_i64))
            .unwrap_or(Severity::Error);
        let message = entry
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let source = entry
            .get("source")
            .and_then(Value::as_str)
            .map(str::to_string);
        let code = decode_diagnostic_code(entry.get("code"));
        out.push(Diagnostic {
            file: path.clone(),
            range: DiagnosticRange {
                start_line: start.get("line")?.as_u64()? as u32 + 1,
                start_column: start.get("character")?.as_u64()? as u32 + 1,
                end_line: end.get("line")?.as_u64()? as u32 + 1,
                end_column: end.get("character")?.as_u64()? as u32 + 1,
            },
            severity,
            message,
            source,
            code,
        });
    }
    Some((path, out))
}

fn decode_diagnostic_code(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Number(number)) => Some(number.to_string()),
        Some(Value::Object(object)) => object.get("value").and_then(|inner| match inner {
            Value::String(text) => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        }),
        _ => None,
    }
}

fn uri_from_path(path: &Path) -> String {
    let canonical = normalize_path(path);
    let text = canonical.to_string_lossy();
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{}", text.trim_start_matches('/'))
    }
}

fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let stripped = uri.strip_prefix("file://")?;
    Some(PathBuf::from(stripped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsp_header() {
        let frame = b"Content-Length: 5\r\n\r\nhello";
        let (end, len) = parse_header(frame).expect("header parses");
        assert_eq!(end, 21);
        assert_eq!(len, 5);
    }

    #[test]
    fn parses_publish_diagnostics_payload() {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": "file:///tmp/foo.rs",
                "diagnostics": [{
                    "range": {
                        "start": { "line": 11, "character": 7 },
                        "end": { "line": 11, "character": 8 }
                    },
                    "severity": 1,
                    "message": "missing semicolon",
                    "source": "rust-analyzer",
                    "code": "E0101"
                }]
            }
        });
        let (path, diags) = parse_publish_diagnostics(&payload).expect("parses");
        assert_eq!(path, PathBuf::from("/tmp/foo.rs"));
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].range.start_line, 12);
        assert_eq!(diags[0].code.as_deref(), Some("E0101"));
    }

    // Unix-only for now: Windows file URIs (file:///C:/…, backslashes,
    // drive-letter case) aren't normalized for path comparison yet — tracked
    // separately from the sandbox work.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn canonical_publish_uri_matches_non_canonical_query_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("foo.rs");
        std::fs::write(&file, b"fn main() {}").unwrap();
        let canonical = normalize_path(&file);
        let uri = uri_from_path(&file);
        assert!(uri.contains(&canonical.to_string_lossy().to_string()));

        let payload = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "diagnostics": [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 1 }
                    },
                    "severity": 1,
                    "message": "test"
                }]
            }
        });
        let (published, _) = parse_publish_diagnostics(&payload).expect("parses");
        assert!(paths_equal(&published, &file));
    }
}
