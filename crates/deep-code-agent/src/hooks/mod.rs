use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::tool::{ToolCall, ToolResult};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookEvent {
    ToolPre {
        tool_name: String,
        call_id: String,
        arguments: Value,
    },
    ToolPost {
        tool_name: String,
        call_id: String,
        result: ToolHookResult,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ToolHookResult {
    pub status: String,
    pub output: String,
}

impl ToolHookResult {
    #[must_use]
    pub fn from_tool_result(result: &ToolResult) -> Self {
        Self {
            status: format!("{:?}", result.status).to_ascii_lowercase(),
            output: result.content.clone(),
        }
    }
}

impl HookEvent {
    #[must_use]
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or_else(|_| json!({"type": "serialization_error"}))
    }
}

pub trait HookSink: Send + Sync {
    fn emit(&self, event: &HookEvent);
}

/// An interceptor's verdict on a tool call about to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolGate {
    /// Let the tool execute.
    Allow,
    /// Stop the tool; `reason` is surfaced to the model as a failed tool result.
    Block { reason: String },
}

/// A programmatic veto evaluated right before a tool executes (after any user
/// approval). Unlike [`HookSink`] observers, an interceptor can BLOCK a call —
/// this is the seam in-process features like a plan/read-only mode plug into.
///
/// Approval stays a SEPARATE mechanism (the execution policy), not part of this
/// chain: both pi and CodeWhale keep permission out of the observer/interceptor
/// path, and folding it in here would tangle two concerns. An interceptor is an
/// additional gate, never a replacement for approval.
pub trait ToolInterceptor: Send + Sync {
    fn before_tool(&self, call: &ToolCall) -> ToolGate;
}

#[derive(Default)]
pub struct StdoutHookSink;

impl HookSink for StdoutHookSink {
    fn emit(&self, event: &HookEvent) {
        println!("{}", event.to_json());
    }
}

pub struct JsonlHookSink {
    path: PathBuf,
    lock: Mutex<()>,
}

impl JsonlHookSink {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }
}

impl HookSink for JsonlHookSink {
    fn emit(&self, event: &HookEvent) {
        let _guard = self.lock.lock().expect("jsonl hook lock");
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            return;
        };
        let payload = json!({
            "at_ms": crate::session_store::now_ms(),
            "event": event,
        });
        if let Ok(encoded) = serde_json::to_string(&payload) {
            let _ = writeln!(file, "{encoded}");
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HooksConfig {
    #[serde(default)]
    pub stdout: bool,
    #[serde(default)]
    pub jsonl: Option<PathBuf>,
}

impl HooksConfig {
    pub fn load(path: &Path) -> Result<Self, HookError> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path).map_err(|error| HookError::Io {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        toml::from_str(&raw).map_err(|error| HookError::Parse {
            path: path.to_path_buf(),
            message: error.to_string(),
        })
    }
}

#[derive(Default, Clone)]
pub struct HookDispatcher {
    sinks: Arc<Vec<Arc<dyn HookSink>>>,
    interceptors: Arc<Vec<Arc<dyn ToolInterceptor>>>,
}

impl HookDispatcher {
    pub fn from_config(config: &HooksConfig) -> Self {
        let mut sinks: Vec<Arc<dyn HookSink>> = Vec::new();
        if config.stdout {
            sinks.push(Arc::new(StdoutHookSink));
        }
        if let Some(path) = &config.jsonl {
            sinks.push(Arc::new(JsonlHookSink::new(path.clone())));
        }
        Self {
            sinks: Arc::new(sinks),
            interceptors: Arc::new(Vec::new()),
        }
    }

    pub fn add_sink(&mut self, sink: Arc<dyn HookSink>) {
        let mut sinks = (*self.sinks).clone();
        sinks.push(sink);
        self.sinks = Arc::new(sinks);
    }

    /// Register a programmatic gate run before every tool executes. Interceptors
    /// run in registration order; the first [`ToolGate::Block`] wins.
    pub fn add_interceptor(&mut self, interceptor: Arc<dyn ToolInterceptor>) {
        let mut interceptors = (*self.interceptors).clone();
        interceptors.push(interceptor);
        self.interceptors = Arc::new(interceptors);
    }

    pub fn enabled(&self) -> bool {
        !self.sinks.is_empty() || !self.interceptors.is_empty()
    }

    /// The pre-execution seam: emit the `tool_pre` observability event, then run
    /// the interceptor chain. Returns the gate — the first interceptor to block
    /// short-circuits the rest, and the caller turns a block into a failed tool
    /// result the model reads (approval is handled separately, upstream).
    pub fn before_tool(&self, call: &ToolCall) -> ToolGate {
        self.emit_tool_pre(call);
        for interceptor in self.interceptors.iter() {
            let gate = interceptor.before_tool(call);
            if let ToolGate::Block { .. } = gate {
                return gate;
            }
        }
        ToolGate::Allow
    }

    pub fn emit_tool_pre(&self, call: &ToolCall) {
        self.emit(HookEvent::ToolPre {
            tool_name: call.name.clone(),
            call_id: call.id.clone(),
            arguments: call.arguments.clone(),
        });
    }

    pub fn emit_tool_post(&self, call: &ToolCall, result: &ToolResult) {
        self.emit(HookEvent::ToolPost {
            tool_name: call.name.clone(),
            call_id: call.id.clone(),
            result: ToolHookResult::from_tool_result(result),
        });
    }

    pub fn emit(&self, event: HookEvent) {
        for sink in self.sinks.iter() {
            sink.emit(&event);
        }
    }
}

#[must_use]
pub fn default_hooks_config_path() -> PathBuf {
    home_dir()
        .map(|home| home.join(".deep-code").join("hooks.toml"))
        .unwrap_or_else(|| PathBuf::from(".deep-code/hooks.toml"))
}

pub fn load_hooks_config() -> HooksConfig {
    HooksConfig::load(&default_hooks_config_path()).unwrap_or_default()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("failed to read hooks config at {path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("failed to parse hooks config at {path}: {message}")]
    Parse { path: PathBuf, message: String },
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::tool::{ToolCall, ToolResult, ToolResultStatus};

    #[derive(Default)]
    struct RecordingSink {
        events: Mutex<Vec<Value>>,
    }

    impl HookSink for RecordingSink {
        fn emit(&self, event: &HookEvent) {
            self.events.lock().unwrap().push(event.to_json());
        }
    }

    #[test]
    fn dispatcher_records_tool_lifecycle() {
        let sink = Arc::new(RecordingSink::default());
        let mut dispatcher = HookDispatcher::default();
        dispatcher.add_sink(sink.clone());
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            arguments: json!({"path": "a.rs"}),
        };
        dispatcher.emit_tool_pre(&call);
        dispatcher.emit_tool_post(
            &call,
            &ToolResult {
                call_id: call.id.clone(),
                tool_name: call.name.clone(),
                status: ToolResultStatus::Success,
                content: "ok".to_string(),
                details: None,
            },
        );
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "tool_pre");
        assert_eq!(events[1]["type"], "tool_post");
    }

    struct BlockingInterceptor {
        reason: &'static str,
    }
    impl ToolInterceptor for BlockingInterceptor {
        fn before_tool(&self, _call: &ToolCall) -> ToolGate {
            ToolGate::Block {
                reason: self.reason.to_string(),
            }
        }
    }

    struct AllowInterceptor;
    impl ToolInterceptor for AllowInterceptor {
        fn before_tool(&self, _call: &ToolCall) -> ToolGate {
            ToolGate::Allow
        }
    }

    fn a_call() -> ToolCall {
        ToolCall {
            id: "c1".to_string(),
            name: "shell".to_string(),
            arguments: json!({"command": "ls"}),
        }
    }

    #[test]
    fn before_tool_allows_when_no_interceptors() {
        assert_eq!(
            HookDispatcher::default().before_tool(&a_call()),
            ToolGate::Allow
        );
    }

    #[test]
    fn before_tool_first_block_wins_and_short_circuits() {
        let mut dispatcher = HookDispatcher::default();
        dispatcher.add_interceptor(Arc::new(AllowInterceptor));
        dispatcher.add_interceptor(Arc::new(BlockingInterceptor {
            reason: "plan mode",
        }));
        assert_eq!(
            dispatcher.before_tool(&a_call()),
            ToolGate::Block {
                reason: "plan mode".to_string()
            }
        );
    }

    #[test]
    fn before_tool_still_emits_tool_pre_observability() {
        let sink = Arc::new(RecordingSink::default());
        let mut dispatcher = HookDispatcher::default();
        dispatcher.add_sink(sink.clone());
        dispatcher.add_interceptor(Arc::new(BlockingInterceptor { reason: "no" }));
        let _ = dispatcher.before_tool(&a_call());
        // The tool_pre event fires even though the call is blocked.
        assert_eq!(sink.events.lock().unwrap()[0]["type"], "tool_pre");
    }

    #[test]
    fn jsonl_sink_appends_events() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("hooks.jsonl");
        let sink = JsonlHookSink::new(path.clone());
        sink.emit(&HookEvent::ToolPre {
            tool_name: "grep_files".to_string(),
            call_id: "call_2".to_string(),
            arguments: json!({}),
        });
        let raw = fs::read_to_string(path).unwrap();
        assert!(raw.contains("tool_pre"));
        assert!(raw.contains("grep_files"));
    }
}
