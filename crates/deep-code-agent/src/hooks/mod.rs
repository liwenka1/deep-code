//! Tool-lifecycle observability and pre-execution gating.
//!
//! Two independent extension points live in this module:
//!
//! * **Observers** ([`HookSink`]) receive a copy of every lifecycle event and
//!   can never influence execution. They exist for logging, auditing, and for
//!   driving external tooling off the agent's activity. Delivery is
//!   best-effort: a broken sink is skipped, never surfaced to the model.
//! * **Gates** ([`ToolInterceptor`]) run synchronously right before a tool
//!   executes and may veto it. In-process features such as a read-only plan
//!   mode hang off this seam.
//!
//! User approval is deliberately *not* modelled here. Whether a call needs a
//! human sign-off is decided by the execution policy before the dispatcher is
//! ever consulted; a gate is an extra latch layered on top of an
//! already-approved call, never a substitute for approval. Folding approval
//! into the gate chain would let an observer-side feature silently widen or
//! narrow what the user agreed to, so the two stay separate by construction.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::paths::home_dir;
use crate::tool::{ToolCall, ToolResult};

/// A lifecycle event surrounding one tool invocation.
///
/// Events are consumed as JSON (see [`HookEvent::to_json`]); the enum itself
/// is the in-process representation.
#[derive(Debug, Clone, PartialEq)]
pub enum HookEvent {
    /// Fired after approval but before the tool body runs (and before gates
    /// get a chance to veto, so blocked calls are still visible in logs).
    ToolPre {
        tool_name: String,
        call_id: String,
        arguments: Value,
    },
    /// Fired once the tool finished, failed, or was vetoed by a gate.
    ToolPost {
        tool_name: String,
        call_id: String,
        result: ToolOutcomeSnapshot,
    },
}

impl HookEvent {
    /// Render the event as a JSON object.
    ///
    /// The wire shape is stable: a `"type"` field carrying `"tool_pre"` /
    /// `"tool_post"`, with the remaining fields flattened alongside it. The
    /// JSON is assembled by hand per variant so the log format cannot drift
    /// accidentally when the in-memory types are refactored.
    #[must_use]
    pub fn to_json(&self) -> Value {
        match self {
            Self::ToolPre {
                tool_name,
                call_id,
                arguments,
            } => json!({
                "type": "tool_pre",
                "tool_name": tool_name,
                "call_id": call_id,
                "arguments": arguments,
            }),
            Self::ToolPost {
                tool_name,
                call_id,
                result,
            } => json!({
                "type": "tool_post",
                "tool_name": tool_name,
                "call_id": call_id,
                "result": {
                    "status": result.status,
                    "output": result.output,
                },
            }),
        }
    }
}

/// The observable slice of a finished [`ToolResult`]: a lowercase status word
/// plus the textual output. Details and internal bookkeeping are dropped on
/// purpose — hook consumers get what the model gets.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcomeSnapshot {
    pub status: String,
    pub output: String,
}

impl ToolOutcomeSnapshot {
    #[must_use]
    pub fn capture(result: &ToolResult) -> Self {
        Self {
            status: format!("{:?}", result.status).to_ascii_lowercase(),
            output: result.content.clone(),
        }
    }
}

/// An observer of [`HookEvent`]s.
///
/// Implementations own their transport (stdout, files, ...) and must swallow
/// their own failures: event delivery is fire-and-forget and never feeds back
/// into the agent loop.
pub trait HookSink: Send + Sync {
    fn emit(&self, event: &HookEvent);
}

/// A gate's verdict on a tool call that is about to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolGate {
    /// Let the tool execute.
    Allow,
    /// Stop the tool; `reason` is surfaced to the model as a failed tool result.
    Block { reason: String },
}

/// A programmatic veto consulted right before a tool executes (after any user
/// approval has already been resolved). Unlike [`HookSink`] observers, a gate
/// can stop the call — this is where features like a plan/read-only mode
/// plug in. A gate can only ever remove capability, never grant it: approval
/// remains the sole mechanism that authorizes a call.
pub trait ToolInterceptor: Send + Sync {
    fn before_tool(&self, call: &ToolCall) -> ToolGate;
}

/// Observer that prints each event as one JSON line on stdout.
#[derive(Default)]
pub struct StdoutHookSink;

impl HookSink for StdoutHookSink {
    fn emit(&self, event: &HookEvent) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{}", event.to_json());
    }
}

/// Observer that appends events to a JSONL file, one object per line:
/// `{"at_ms": <unix millis>, "event": {...}}`.
///
/// The file is opened per event with `O_APPEND` (creating missing parent
/// directories on demand). A cached handle would be cheaper but silently
/// unsafe under log rotation: on Unix, writing to a renamed or unlinked file
/// *succeeds*, so every audit event after a rotation would vanish into an
/// orphaned inode without a single error. Hook events are per-tool-call rare,
/// so the extra open is noise; correctness of the audit trail is not.
pub struct JsonlHookSink {
    target: PathBuf,
    /// Serializes appends so concurrent events keep whole-line ordering.
    order: Mutex<()>,
}

impl JsonlHookSink {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            target: path,
            order: Mutex::new(()),
        }
    }

    fn append_line(&self, line: &str) {
        let _order = self.order.lock().expect("jsonl hook sink poisoned");
        if let Some(mut file) = self.open_target() {
            let _ = writeln!(file, "{line}");
        }
    }

    fn open_target(&self) -> Option<File> {
        let open = || {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.target)
                .ok()
        };
        open().or_else(|| {
            // First failure is usually a missing parent; create it and retry.
            if let Some(dir) = self.target.parent() {
                let _ = fs::create_dir_all(dir);
            }
            open()
        })
    }
}

impl HookSink for JsonlHookSink {
    fn emit(&self, event: &HookEvent) {
        let record = json!({
            "at_ms": crate::session_store::now_ms(),
            "event": event.to_json(),
        });
        self.append_line(&record.to_string());
    }
}

/// User-facing hooks configuration (`~/.deep-code/hooks.toml`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HooksConfig {
    /// Mirror events to stdout.
    #[serde(default)]
    pub stdout: bool,
    /// Append events to this JSONL file.
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

/// Central hub wiring observers and gates into the tool loop.
///
/// The dispatcher is cheap to clone and share: all registrations live behind
/// one shared [`Arc`], and registering after a clone copies the wiring on
/// write, so clones handed to spawned tasks keep seeing a consistent set.
#[derive(Default, Clone)]
pub struct HookDispatcher {
    wiring: Arc<Wiring>,
}

#[derive(Default)]
struct Wiring {
    observers: Vec<Arc<dyn HookSink>>,
    gates: Vec<Arc<dyn ToolInterceptor>>,
}

impl Clone for Wiring {
    fn clone(&self) -> Self {
        Self {
            observers: self.observers.clone(),
            gates: self.gates.clone(),
        }
    }
}

impl HookDispatcher {
    /// Build a dispatcher with the sinks the user asked for in config.
    #[must_use]
    pub fn from_config(config: &HooksConfig) -> Self {
        let mut dispatcher = Self::default();
        if config.stdout {
            dispatcher.add_sink(Arc::new(StdoutHookSink));
        }
        if let Some(path) = &config.jsonl {
            dispatcher.add_sink(Arc::new(JsonlHookSink::new(path.clone())));
        }
        dispatcher
    }

    /// Register an observer; it will see every event emitted from now on.
    pub fn add_sink(&mut self, sink: Arc<dyn HookSink>) {
        Arc::make_mut(&mut self.wiring).observers.push(sink);
    }

    /// Register a gate consulted before every tool execution. Gates run in
    /// registration order and the first [`ToolGate::Block`] ends the chain.
    pub fn add_interceptor(&mut self, gate: Arc<dyn ToolInterceptor>) {
        Arc::make_mut(&mut self.wiring).gates.push(gate);
    }

    /// True when at least one observer or gate is wired up.
    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.wiring.observers.is_empty() || !self.wiring.gates.is_empty()
    }

    /// The pre-execution seam: publish `tool_pre` for observability, then walk
    /// the gate chain. The first blocking verdict short-circuits the remaining
    /// gates; the caller turns it into a failed tool result the model reads.
    /// The `tool_pre` event fires unconditionally so blocked calls still show
    /// up in logs.
    pub fn before_tool(&self, call: &ToolCall) -> ToolGate {
        self.emit_tool_pre(call);
        self.wiring
            .gates
            .iter()
            .map(|gate| gate.before_tool(call))
            .find(|verdict| matches!(verdict, ToolGate::Block { .. }))
            .unwrap_or(ToolGate::Allow)
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
            result: ToolOutcomeSnapshot::capture(result),
        });
    }

    /// Fan an event out to every observer.
    pub fn emit(&self, event: HookEvent) {
        for observer in &self.wiring.observers {
            observer.emit(&event);
        }
    }
}

/// Where the global hooks config lives: `~/.deep-code/hooks.toml` (relative
/// fallback when `HOME` is unset).
#[must_use]
pub fn default_hooks_config_path() -> PathBuf {
    home_dir()
        .map(|home| home.join(".deep-code").join("hooks.toml"))
        .unwrap_or_else(|| PathBuf::from(".deep-code/hooks.toml"))
}

/// Load the global hooks config, treating a missing or broken file as "no
/// hooks configured" — observability must never keep the agent from starting.
#[must_use]
pub fn load_hooks_config() -> HooksConfig {
    HooksConfig::load(&default_hooks_config_path()).unwrap_or_default()
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::tool::{ToolCall, ToolResult, ToolResultStatus};

    /// Test observer keeping the raw events (not their JSON) for inspection.
    #[derive(Default)]
    struct CaptureSink {
        seen: Mutex<Vec<HookEvent>>,
    }

    impl CaptureSink {
        fn snapshot(&self) -> Vec<HookEvent> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl HookSink for CaptureSink {
        fn emit(&self, event: &HookEvent) {
            self.seen.lock().unwrap().push(event.clone());
        }
    }

    /// Gate that counts how often it was consulted, always allowing.
    #[derive(Default)]
    struct CountingGate(AtomicUsize);

    impl ToolInterceptor for CountingGate {
        fn before_tool(&self, _call: &ToolCall) -> ToolGate {
            self.0.fetch_add(1, Ordering::SeqCst);
            ToolGate::Allow
        }
    }

    struct RefusingGate(&'static str);

    impl ToolInterceptor for RefusingGate {
        fn before_tool(&self, _call: &ToolCall) -> ToolGate {
            ToolGate::Block {
                reason: self.0.to_string(),
            }
        }
    }

    fn sample_call() -> ToolCall {
        ToolCall::new("t-9", "write_file", json!({"path": "src/lib.rs"}))
    }

    fn sample_failure(call: &ToolCall) -> ToolResult {
        ToolResult {
            call_id: call.id.clone(),
            tool_name: call.name.clone(),
            status: ToolResultStatus::Error,
            content: "denied by test".to_string(),
            details: None,
        }
    }

    #[test]
    fn every_observer_sees_pre_and_post() {
        let first = Arc::new(CaptureSink::default());
        let second = Arc::new(CaptureSink::default());
        let mut hub = HookDispatcher::default();
        hub.add_sink(first.clone());
        hub.add_sink(second.clone());

        let call = sample_call();
        hub.emit_tool_pre(&call);
        hub.emit_tool_post(&call, &sample_failure(&call));

        for sink in [&first, &second] {
            let events = sink.snapshot();
            assert_eq!(events.len(), 2);
            assert_eq!(events[0].to_json()["type"], "tool_pre");
            assert_eq!(events[1].to_json()["type"], "tool_post");
        }
    }

    #[test]
    fn tool_post_json_carries_status_and_output() {
        let call = sample_call();
        let event = HookEvent::ToolPost {
            tool_name: call.name.clone(),
            call_id: call.id.clone(),
            result: ToolOutcomeSnapshot::capture(&sample_failure(&call)),
        };
        let payload = event.to_json();
        assert_eq!(payload["type"], "tool_post");
        assert_eq!(payload["tool_name"], "write_file");
        assert_eq!(payload["call_id"], "t-9");
        assert_eq!(payload["result"]["status"], "error");
        assert_eq!(payload["result"]["output"], "denied by test");
    }

    #[test]
    fn empty_gate_chain_allows() {
        let hub = HookDispatcher::default();
        assert_eq!(hub.before_tool(&sample_call()), ToolGate::Allow);
        assert!(!hub.enabled());
    }

    #[test]
    fn first_refusal_stops_the_chain() {
        let tail = Arc::new(CountingGate::default());
        let mut hub = HookDispatcher::default();
        hub.add_interceptor(Arc::new(CountingGate::default()));
        hub.add_interceptor(Arc::new(RefusingGate("read-only session")));
        hub.add_interceptor(tail.clone());

        let verdict = hub.before_tool(&sample_call());
        assert_eq!(
            verdict,
            ToolGate::Block {
                reason: "read-only session".to_string()
            }
        );
        // Gates behind the refusal were never consulted.
        assert_eq!(tail.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn blocked_calls_still_reach_observers() {
        let sink = Arc::new(CaptureSink::default());
        let mut hub = HookDispatcher::default();
        hub.add_sink(sink.clone());
        hub.add_interceptor(Arc::new(RefusingGate("nope")));

        let _ = hub.before_tool(&sample_call());
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].to_json()["type"], "tool_pre");
    }

    #[test]
    fn clones_share_registrations_made_before_the_clone() {
        let sink = Arc::new(CaptureSink::default());
        let mut hub = HookDispatcher::default();
        hub.add_sink(sink.clone());
        let clone = hub.clone();

        clone.emit_tool_pre(&sample_call());
        assert_eq!(sink.snapshot().len(), 1);
        assert!(clone.enabled());
    }

    #[test]
    fn jsonl_sink_appends_one_line_per_event() {
        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("nested").join("events.jsonl");
        let sink = JsonlHookSink::new(log_path.clone());

        let call = sample_call();
        sink.emit(&HookEvent::ToolPre {
            tool_name: call.name.clone(),
            call_id: call.id.clone(),
            arguments: call.arguments.clone(),
        });
        sink.emit(&HookEvent::ToolPost {
            tool_name: call.name.clone(),
            call_id: call.id.clone(),
            result: ToolOutcomeSnapshot::capture(&sample_failure(&call)),
        });

        let raw = fs::read_to_string(&log_path).unwrap();
        let records: Vec<Value> = raw
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        assert!(records[0]["at_ms"].is_u64());
        assert_eq!(records[0]["event"]["type"], "tool_pre");
        assert_eq!(records[0]["event"]["tool_name"], "write_file");
        assert_eq!(records[1]["event"]["type"], "tool_post");
    }

    #[test]
    fn config_wires_requested_sinks() {
        let dir = tempfile::TempDir::new().unwrap();
        let hub = HookDispatcher::from_config(&HooksConfig {
            stdout: false,
            jsonl: Some(dir.path().join("hooks.jsonl")),
        });
        assert!(hub.enabled());
        assert!(!HookDispatcher::from_config(&HooksConfig::default()).enabled());
    }
}
