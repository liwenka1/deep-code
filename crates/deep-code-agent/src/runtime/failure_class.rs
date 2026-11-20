//! Tool-result classification for the write-boundary circuit breaker.

use crate::runtime::state::RuntimeState;
use crate::tool::{ToolCall, ToolResult, ToolResultStatus};

/// Count a tool result refused by the write boundary.
pub(super) fn record_boundary_denial(
    state: &mut RuntimeState,
    call: &ToolCall,
    result: &ToolResult,
) {
    if is_boundary_denial(call, result) {
        state.turn_boundary_denials += 1;
        // The denied path (when the call carries one — file tools do,
        // shell doesn't) makes the breaker's guidance concrete: the
        // user sees "/add-dir <dir>" with the real directory. Last
        // write wins; any one of them names the tree the model wants.
        if let Some(path) = call.arguments.get("path").and_then(|value| value.as_str()) {
            state.last_boundary_denial_path = Some(path.to_string());
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
            .text()
            .contains(crate::workspace_policy::OUTSIDE_ROOTS)
    {
        return true;
    }
    matches!(call.name.as_str(), "shell" | "job")
        && result
            .content
            .text()
            .contains(crate::sandbox::WRITE_DENIAL_NOTE)
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
        result.content = format!("sh: ...\n{}", crate::sandbox::WRITE_DENIAL_NOTE).into();
        assert!(
            is_boundary_denial(&call, &result),
            "the write note keeps feeding the breaker"
        );
    }
}
