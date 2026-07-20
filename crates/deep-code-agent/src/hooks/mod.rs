//! Pre-execution tool gating.
//!
//! A [`ToolInterceptor`] runs synchronously right before a tool executes and
//! may veto it. In-process features such as a read-only plan mode hang off
//! this seam.
//!
//! User approval is deliberately *not* modelled here. Whether a call needs a
//! human sign-off is decided by the execution policy before the dispatcher is
//! ever consulted; a gate is an extra latch layered on top of an
//! already-approved call, never a substitute for approval. A gate can only
//! ever remove capability, never grant it.

use std::sync::Arc;

use crate::tool::ToolCall;

/// A gate's verdict on a tool call that is about to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolGate {
    /// Let the tool execute.
    Allow,
    /// Stop the tool; `reason` is surfaced to the model as a failed tool result.
    Block { reason: String },
}

/// A programmatic veto consulted right before a tool executes (after any user
/// approval has already been resolved). This is where features like a
/// plan/read-only mode plug in. A gate can only ever remove capability, never
/// grant it: approval remains the sole mechanism that authorizes a call.
pub trait ToolInterceptor: Send + Sync {
    fn before_tool(&self, call: &ToolCall) -> ToolGate;
}

/// Central hub wiring gates into the tool loop.
///
/// The dispatcher is cheap to clone and share: registrations live behind one
/// shared [`Arc`], and registering after a clone copies the wiring on write,
/// so clones handed to spawned tasks keep seeing a consistent set.
#[derive(Default, Clone)]
pub struct HookDispatcher {
    gates: Arc<Vec<Arc<dyn ToolInterceptor>>>,
}

impl HookDispatcher {
    /// Register a gate consulted before every tool execution. Gates run in
    /// registration order and the first [`ToolGate::Block`] ends the chain.
    pub fn add_interceptor(&mut self, gate: Arc<dyn ToolInterceptor>) {
        Arc::make_mut(&mut self.gates).push(gate);
    }

    /// True when at least one gate is wired up.
    #[must_use]
    pub fn enabled(&self) -> bool {
        !self.gates.is_empty()
    }

    /// The pre-execution seam: walk the gate chain. The first blocking verdict
    /// short-circuits the remaining gates; the caller turns it into a failed
    /// tool result the model reads.
    pub fn before_tool(&self, call: &ToolCall) -> ToolGate {
        self.gates
            .iter()
            .map(|gate| gate.before_tool(call))
            .find(|verdict| matches!(verdict, ToolGate::Block { .. }))
            .unwrap_or(ToolGate::Allow)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;
    use crate::tool::ToolCall;

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
    fn clones_share_registrations_made_before_the_clone() {
        let mut hub = HookDispatcher::default();
        hub.add_interceptor(Arc::new(RefusingGate("nope")));
        let clone = hub.clone();
        assert!(clone.enabled());
        assert!(matches!(
            clone.before_tool(&sample_call()),
            ToolGate::Block { .. }
        ));
    }
}
