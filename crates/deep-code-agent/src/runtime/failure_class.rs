//! Failure classification for recorded tool results: which failures feed the
//! cascade escalation latch (Flash → Pro) and which feed the boundary-denial
//! circuit breaker. The two classes need opposite responses — an ordinary
//! failure is the model fumbling something a stronger model might fix, a
//! boundary denial is the granted-roots fence holding: deterministic, and
//! only the user can change it (`/add-dir`). Escalating on a denial would pay
//! Pro prices for retries the kernel refuses either way.

use crate::runtime::state::RuntimeState;
use crate::runtime::tool_result::CANCELLED_TOOL_RESULT;
use crate::tool::{ToolCall, ToolResult, ToolResultStatus};

/// Tool-call execution failures within a single turn that latch cascade
/// escalation (Flash → Pro for the rest of the session). Two mirrors the
/// "2–3 failed self-corrections, then escalate" rule of thumb without waiting
/// so long that a whole turn is wasted flailing on the weak model.
const CASCADE_ESCALATE_TOOL_ERRORS: u32 = 2;

/// Fold one recorded tool result into the turn's failure counters: boundary
/// denials feed their own counter (read by the turn loop's circuit breaker),
/// genuine execution failures feed the cascade latch (read by the next turn's
/// router). Called with the state lock already held by `record_tool_result`.
pub(super) fn record_failure_signals(
    state: &mut RuntimeState,
    call: &ToolCall,
    result: &ToolResult,
) {
    let boundary_denial = is_boundary_denial(call, result);
    if boundary_denial {
        state.turn_boundary_denials += 1;
        // The denied path (when the call carries one — file tools do,
        // shell doesn't) makes the breaker's guidance concrete: the
        // user sees "/add-dir <dir>" with the real directory. Last
        // write wins; any one of them names the tree the model wants.
        if let Some(path) = call.arguments.get("path").and_then(|value| value.as_str()) {
            state.last_boundary_denial_path = Some(path.to_string());
        }
    }
    // Cascade signal: a genuine execution failure means the model
    // fumbled this tool call. Denials carry their own status and user
    // cancellations carry a known marker, so neither counts. Sub-agent
    // failures are excluded too — a child that timed out, was cancelled,
    // or hit the concurrency cap is not the parent model fumbling a
    // primitive, and must not latch a session-wide Pro escalation.
    // Enough real fumbles in one turn latch escalation onto Pro (sticky
    // for the session); the latch is read by the next turn's router.
    if result.status == ToolResultStatus::Error
        && !boundary_denial
        && result.content != CANCELLED_TOOL_RESULT
        && !crate::subagent::is_subagent_tool(&call.name)
    {
        state.turn_tool_errors += 1;
        if state.turn_tool_errors >= CASCADE_ESCALATE_TOOL_ERRORS && !state.cascade_escalated {
            state.cascade_escalated = true;
            // Mark the triggering turn so telemetry can surface the
            // escalation now, not just on the next (Pro) turn.
            state.cascade_triggered_this_turn = true;
        }
    }
}

/// Whether a tool result is a *boundary denial*: a write the granted-roots
/// fence refused, at either enforcement layer. Detected by the in-band
/// markers the producers embed — the tool layer's [`OUTSIDE_ROOTS`] rejection
/// (an Error result) and the sandbox's [`WRITE_DENIAL_NOTE`] appended to a
/// failed shell/job run (a Success result carrying a failed exit code) —
/// via the shared constants, so classification cannot drift from production.
///
/// The note check is gated to shell/job calls: those are the only producers,
/// and the gate keeps a `read_file`/`grep_files` result whose *content*
/// happens to quote the marker from ever counting. Sub-agent results are
/// excluded outright — a child quoting a denial in its final answer is
/// reporting one, not hitting one, and the child's own runtime already
/// classified the original.
///
/// [`OUTSIDE_ROOTS`]: crate::workspace_policy::OUTSIDE_ROOTS
/// [`WRITE_DENIAL_NOTE`]: crate::sandbox::WRITE_DENIAL_NOTE
fn is_boundary_denial(call: &ToolCall, result: &ToolResult) -> bool {
    if crate::subagent::is_subagent_tool(&call.name) {
        return false;
    }
    if result.status == ToolResultStatus::Error
        && result
            .content
            .contains(crate::workspace_policy::OUTSIDE_ROOTS)
    {
        return true;
    }
    matches!(call.name.as_str(), "shell" | "job")
        && result.content.contains(crate::sandbox::WRITE_DENIAL_NOTE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The network note must never feed the write-boundary breaker: its
    /// remedy is `network=true` on the next call, not `/add-dir`, and the
    /// breaker's advice names the latter. Guarded by test because the two
    /// notes share a producer and a prefix — a refactor folding them into one
    /// constant would silently route offline network failures into "grant a
    /// directory" advice again.
    #[test]
    fn network_denial_note_is_not_a_write_boundary_denial() {
        let call = ToolCall::new("c1", "shell", serde_json::json!({"command": "git push"}));
        let mut result = ToolResult::success(
            "c1",
            "shell",
            format!("fatal: ...\n{}", crate::sandbox::NETWORK_DENIAL_NOTE),
        );
        assert!(
            !is_boundary_denial(&call, &result),
            "a network denial is not the granted-roots fence"
        );
        result.content = format!("sh: ...\n{}", crate::sandbox::WRITE_DENIAL_NOTE);
        assert!(
            is_boundary_denial(&call, &result),
            "the write note keeps feeding the breaker"
        );
    }
}
