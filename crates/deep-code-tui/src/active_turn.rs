use deep_code_agent::{ApprovalRequest, ToolCallId};

use crate::history::{HistoryCell, ToolApprovalState, quiet_while_running};

/// Bound on the buffered live-output tail per running tool (display only —
/// the agent-side ring buffer keeps the full 128 KiB).
const LIVE_OUTPUT_MAX_CHARS: usize = 4_096;
/// How many trailing output lines the transcript preview shows per tool.
const LIVE_OUTPUT_PREVIEW_LINES: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LiveOutput(String);

impl LiveOutput {
    pub fn push(&mut self, text: &str) {
        self.0.push_str(text);
        let count = self.0.chars().count();
        if count > LIVE_OUTPUT_MAX_CHARS {
            self.0 = self.0.chars().skip(count - LIVE_OUTPUT_MAX_CHARS).collect();
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Last few complete lines for the transcript preview.
    #[must_use]
    pub fn preview_tail(&self) -> String {
        let lines: Vec<&str> = self.0.lines().collect();
        let start = lines.len().saturating_sub(LIVE_OUTPUT_PREVIEW_LINES);
        lines[start..].join("\n")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveToolCell {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub arguments: String,
    pub approval: ToolApprovalState,
    pub live_output: LiveOutput,
    /// When this call started running, for the "still alive, N seconds in"
    /// readouts (status bar + transcript preview). A minutes-long tool (agent,
    /// a build) with no clock reads as a hang.
    pub started_at: std::time::Instant,
}

/// What one finished tool call flushes into the transcript, in the order the
/// cells must be pushed: the prose that streamed before it, the call itself,
/// then any diagnostics that arrived for it.
///
/// Split out rather than returned as one `Vec` because the call is the only
/// part whose fate depends on its result — see [`ActiveTurn::take_finished_tool`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FinishedTool {
    pub prose: Vec<HistoryCell>,
    pub call: Option<HistoryCell>,
    pub diagnostics: Vec<HistoryCell>,
}

/// The turn currently streaming. It carries no turn id: the TUI shows one
/// turn at a time and attributes every event to it, so nothing ever reads the
/// id back — a turn that arrives without a `TurnStarted` (a late delta, an
/// approval) is started the same way as one that does.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ActiveTurn {
    pub assistant_buffer: String,
    pub reasoning_buffer: String,
    /// Whether THIS turn's reasoning block is expanded. It starts folded — the
    /// whole point — and is carried into the cell when the buffered text is
    /// flushed, because a block that snapped shut mid-read the moment a tool
    /// call landed would be worse than one that never opened. The flush resets
    /// it, so the NEXT block of the same turn starts folded again: expanding
    /// is a per-block act, never a sticky mode.
    pub reasoning_expanded: bool,
    pub tools: Vec<ActiveToolCell>,
    pub diagnostics: Vec<HistoryCell>,
    pub pending_approval: Option<ApprovalRequest>,
}

impl ActiveTurn {
    pub fn push_assistant_delta(&mut self, text: &str) {
        self.assistant_buffer.push_str(text);
    }

    pub fn push_reasoning_delta(&mut self, text: &str) {
        self.reasoning_buffer.push_str(text);
    }

    pub fn upsert_tool(&mut self, cell: ActiveToolCell) {
        if let Some(existing) = self
            .tools
            .iter_mut()
            .find(|tool| tool.tool_call_id == cell.tool_call_id)
        {
            // A re-upsert (duplicate ToolCallStarted) must not wipe output
            // that already streamed in, nor restart the clock.
            let live_output = std::mem::take(&mut existing.live_output);
            let started_at = existing.started_at;
            *existing = cell;
            existing.live_output = live_output;
            existing.started_at = started_at;
        } else {
            self.tools.push(cell);
        }
    }

    pub fn append_tool_output(&mut self, tool_call_id: &ToolCallId, text: &str) {
        if let Some(existing) = self
            .tools
            .iter_mut()
            .find(|tool| &tool.tool_call_id == tool_call_id)
        {
            existing.live_output.push(text);
        }
    }

    pub fn append_tool_arguments(&mut self, tool_call_id: &ToolCallId, delta: &str) {
        // Nothing to append to, and that is the point.
        //
        // A provider streams a call's arguments BEFORE the runtime knows which
        // tool they belong to: `ToolCallUpdated` fires per delta while the model
        // is still talking, and `ToolCallStarted` — which carries the tool's
        // real name and the whole argument string — only follows once that
        // stream ends. Creating a provisional cell here would paint a row naming
        // the call by its id, and then, when the real name arrived and turned
        // out to be a quiet tool, that row would vanish. Nothing to show yet is
        // the right thing to show: the deltas are lost nothing, because
        // `upsert_tool` replaces the arguments wholesale when the identity
        // lands.
        if let Some(existing) = self
            .tools
            .iter_mut()
            .find(|tool| &tool.tool_call_id == tool_call_id)
        {
            existing.arguments.push_str(delta);
        }
    }

    pub fn mark_approval_required(&mut self, request: &ApprovalRequest) {
        let tool_call_id = ToolCallId::from(request.call_id.clone());
        if let Some(existing) = self
            .tools
            .iter_mut()
            .find(|tool| tool.tool_call_id == tool_call_id)
        {
            existing.approval = ToolApprovalState::Required;
        } else {
            self.tools.push(ActiveToolCell {
                tool_call_id,
                tool_name: request.tool_name.clone(),
                arguments: request.arguments.to_string(),
                approval: ToolApprovalState::Required,
                live_output: LiveOutput::default(),
                started_at: std::time::Instant::now(),
            });
        }
    }

    pub fn resolve_approval(&mut self, decision: deep_code_agent::ApprovalDecision) {
        let Some(request) = self.pending_approval.take() else {
            return;
        };
        let tool_call_id = ToolCallId::from(request.call_id);
        let approval = match decision {
            deep_code_agent::ApprovalDecision::Approved
            | deep_code_agent::ApprovalDecision::ApprovedForSession => ToolApprovalState::Approved,
            deep_code_agent::ApprovalDecision::Denied => ToolApprovalState::Denied,
        };
        if let Some(existing) = self
            .tools
            .iter_mut()
            .find(|tool| tool.tool_call_id == tool_call_id)
        {
            existing.approval = approval;
        }
    }

    /// Give up on any request nobody answered, when the turn ends without one.
    ///
    /// A badge means a human answered, so a call left waiting when its turn is
    /// cancelled must not keep the `pending` badge: nobody is going to answer it
    /// now, and the transcript would assert a decision that never happened. It
    /// becomes `Unknown` — "no answer was recorded" — which is also what a
    /// resumed transcript reports for the same call, since the runtime records
    /// the cancel as an unasked call (`tool_result.rs::finish_cancelled_calls`).
    pub fn abandon_unanswered_approvals(&mut self) {
        for tool in &mut self.tools {
            if tool.approval == ToolApprovalState::Required {
                tool.approval = ToolApprovalState::Unknown;
            }
        }
    }

    pub fn push_diagnostics(&mut self, summary: String, rendered: String) {
        self.diagnostics
            .push(HistoryCell::Diagnostics { summary, rendered });
    }

    /// Flush only what belongs to one finished tool call: the streamed
    /// text/reasoning so far (once), that tool's cell, and accumulated
    /// diagnostics. Other still-running tool cells stay in the active turn.
    ///
    /// The call's own cell comes back **separately** from the rest because only
    /// the caller knows its result — and therefore whether it can join a folded
    /// batch (see `App::push_finished_tool`). Returning it inside the vector
    /// would force the caller to pick it back out of a list whose order it
    /// would then have to know.
    pub fn take_finished_tool(&mut self, tool_call_id: &ToolCallId) -> FinishedTool {
        let mut finished = FinishedTool::default();
        if !self.reasoning_buffer.is_empty() {
            finished.prose.push(HistoryCell::Reasoning {
                text: std::mem::take(&mut self.reasoning_buffer),
                expanded: self.reasoning_expanded,
            });
            // The block that just left for history keeps what the user chose;
            // the next block of this turn starts folded again.
            self.reasoning_expanded = false;
        }
        if !self.assistant_buffer.is_empty() {
            finished.prose.push(HistoryCell::Assistant {
                text: std::mem::take(&mut self.assistant_buffer),
            });
        }
        if let Some(position) = self
            .tools
            .iter()
            .position(|tool| &tool.tool_call_id == tool_call_id)
        {
            let tool = self.tools.remove(position);
            finished.call = Some(HistoryCell::ToolCall {
                tool_name: tool.tool_name,
                arguments: tool.arguments,
                approval: tool.approval,
                // Finished: the ToolResult line right under it says how it
                // ended; a stale clock would just be noise.
                running_for_secs: None,
            });
        }
        finished.diagnostics = std::mem::take(&mut self.diagnostics);
        finished
    }

    /// What the live preview draws for the turn in flight.
    ///
    /// A quiet call (`quiet_while_running`) contributes **nothing** — neither
    /// its command nor its streamed output. Anything drawn here and then
    /// swallowed when the call lands is a flicker, and because the transcript is
    /// bottom-anchored every one of them also drags the rows below it up and
    /// down, so a burst of commands reads as a stutter.
    ///
    /// What the call produced is not lost: it is in the run's result, one click
    /// away once the call lands, and the status line keeps its clock ticking
    /// meanwhile. A call a human has to answer, or one that writes, is never
    /// quiet and keeps both its row and its output.
    #[must_use]
    pub fn preview_cells(&self) -> Vec<HistoryCell> {
        self.cells(true)
    }

    /// What an abandoned turn leaves behind.
    ///
    /// Every tool still in flight gets its row, quiet ones included: a call
    /// whose outcome never arrived is exactly what a reader needs to see, and
    /// the fold policy — which is a verdict on *finished* calls — has nothing
    /// to say about it.
    #[must_use]
    pub fn flushed_cells(&self) -> Vec<HistoryCell> {
        self.cells(false)
    }

    fn cells(&self, hide_quiet_calls: bool) -> Vec<HistoryCell> {
        let mut cells = Vec::new();
        if !self.reasoning_buffer.is_empty() {
            cells.push(HistoryCell::Reasoning {
                text: self.reasoning_buffer.clone(),
                // Read here AND by `flush_active_turn`, which drains the turn
                // through this very type — so the turn-end flush inherits the
                // user's choice with no second code path to keep in step.
                expanded: self.reasoning_expanded,
            });
        }
        if !self.assistant_buffer.is_empty() {
            cells.push(HistoryCell::Assistant {
                text: self.assistant_buffer.clone(),
            });
        }
        for tool in &self.tools {
            // A quiet call has no footprint at all — neither its command nor
            // its output. A line that appears and is then swallowed is the same
            // flicker as a command that appears and is then swallowed, and
            // because the transcript is bottom-anchored every one of them also
            // shifts everything below it. What the call produced is in the
            // run's result, one click away, and the status line keeps its clock
            // ticking in the meantime.
            if hide_quiet_calls && quiet_while_running(&tool.tool_name, tool.approval) {
                continue;
            }
            cells.push(HistoryCell::ToolCall {
                tool_name: tool.tool_name.clone(),
                arguments: tool.arguments.clone(),
                approval: tool.approval,
                // Recomputed on every render tick, so the line reads
                // "agent … · 47s" and visibly counts while the call runs.
                running_for_secs: Some(tool.started_at.elapsed().as_secs()),
            });
            if !tool.live_output.is_empty() {
                cells.push(HistoryCell::ToolStream {
                    text: tool.live_output.preview_tail(),
                });
            }
        }
        cells.extend(self.diagnostics.iter().cloned());
        // The pending approval is shown by the dedicated panel (with the y/a/n
        // choices); don't also duplicate it inline in the transcript preview.
        cells
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_cells_exposes_streaming_reasoning_assistant_and_tool() {
        let mut turn = ActiveTurn::default();
        turn.push_reasoning_delta("thinking");
        turn.push_assistant_delta("answer");
        turn.upsert_tool(ActiveToolCell {
            tool_call_id: ToolCallId("call_1".to_string()),
            tool_name: "mock_echo".to_string(),
            arguments: "{\"message\":\"hi\"}".to_string(),
            approval: ToolApprovalState::NotRequired,
            live_output: LiveOutput::default(),
            started_at: std::time::Instant::now(),
        });

        let cells = turn.preview_cells();
        assert!(matches!(cells[0], HistoryCell::Reasoning { .. }));
        assert!(matches!(cells[1], HistoryCell::Assistant { .. }));
        assert!(matches!(
            &cells[2],
            HistoryCell::ToolCall { tool_name, .. } if tool_name == "mock_echo"
        ));
    }

    /// The live-output preview cap, exercised through a tool the preview
    /// actually draws.
    ///
    /// `job` rather than `shell`: a shell call nobody had to approve is a quiet
    /// call, and the preview draws no part of one — not its row, not its
    /// output (see `a_quiet_running_call_draws_nothing_but_a_watched_one_draws_both`).
    #[test]
    fn streamed_tool_output_previews_tail_and_never_reaches_history() {
        let mut turn = ActiveTurn::default();
        let id = ToolCallId("call_1".to_string());
        let cell = || ActiveToolCell {
            tool_call_id: id.clone(),
            tool_name: "job".to_string(),
            arguments: "{\"action\":\"start\"}".to_string(),
            approval: ToolApprovalState::NotRequired,
            live_output: LiveOutput::default(),
            started_at: std::time::Instant::now(),
        };
        turn.upsert_tool(cell());

        for line in 0..10 {
            turn.append_tool_output(&id, &format!("line-{line}\n"));
        }

        let cells = turn.preview_cells();
        let Some(HistoryCell::ToolStream { text }) = cells
            .iter()
            .find(|cell| matches!(cell, HistoryCell::ToolStream { .. }))
        else {
            panic!("expected a live-output preview cell");
        };
        // Only the trailing lines survive the preview cap.
        assert!(text.contains("line-9"));
        assert!(!text.contains("line-0"));

        // A duplicate upsert must not wipe streamed output.
        turn.upsert_tool(cell());
        assert!(!turn.tools[0].live_output.is_empty());

        // The finished-tool flush drops live output: the final ToolResult
        // summary replaces it in history.
        let finished = turn.take_finished_tool(&id);
        assert!(
            finished
                .call
                .is_some_and(|cell| matches!(cell, HistoryCell::ToolCall { .. })),
            "the flush hands back the call, not its live-output tail"
        );
        assert!(
            finished
                .prose
                .iter()
                .all(|cell| !matches!(cell, HistoryCell::ToolStream { .. })),
            "no live output leaks into the flushed prose"
        );
    }

    #[test]
    fn preview_cells_carry_the_live_expansion_choice() {
        let mut turn = ActiveTurn::default();
        turn.push_reasoning_delta("thinking");

        assert!(matches!(
            turn.preview_cells()[0],
            HistoryCell::Reasoning {
                expanded: false,
                ..
            }
        ));
        turn.reasoning_expanded = true;
        assert!(matches!(
            turn.preview_cells()[0],
            HistoryCell::Reasoning { expanded: true, .. }
        ));
    }

    /// Opening a block and then watching a tool call land must not fold it shut
    /// mid-read — but the choice is per-block, so the block that starts after
    /// the flush opens folded like every other.
    #[test]
    fn flushing_an_open_block_keeps_it_open_and_the_next_one_starts_folded() {
        let mut turn = ActiveTurn::default();
        let id = ToolCallId("call_1".to_string());
        turn.upsert_tool(ActiveToolCell {
            tool_call_id: id.clone(),
            tool_name: "shell".to_string(),
            arguments: "{}".to_string(),
            approval: ToolApprovalState::NotRequired,
            live_output: LiveOutput::default(),
            started_at: std::time::Instant::now(),
        });
        turn.push_reasoning_delta("first");
        turn.reasoning_expanded = true;

        let finished = turn.take_finished_tool(&id);
        assert!(
            matches!(
                finished.prose[0],
                HistoryCell::Reasoning { expanded: true, .. }
            ),
            "the block the user opened must stay open once flushed"
        );
        assert!(
            !turn.reasoning_expanded,
            "the next reasoning block of this turn starts folded"
        );

        turn.push_reasoning_delta("second");
        let finished = turn.take_finished_tool(&id);
        assert!(matches!(
            finished.prose[0],
            HistoryCell::Reasoning {
                expanded: false,
                ..
            }
        ));
    }

    /// An `ApprovalResolved` with no parked request must change nothing.
    ///
    /// This is the hinge the whole `asked` record rests on. A standing consent
    /// (`auto_allow`, a remembered command, AcceptEdits, Auto, Yolo) resolves the
    /// gate without ever emitting `ApprovalRequired`, so this event arrives with
    /// `pending_approval` already `None` — and it must leave the cell alone. If
    /// it marked the call `Approved` instead, the live view would show a badge
    /// and keep the row for a call nobody was asked about, while the record (and
    /// therefore `/resume`) says `asked: false` and folds it.
    #[test]
    fn an_approval_that_was_never_asked_leaves_the_call_quiet() {
        let mut turn = ActiveTurn::default();
        let id = ToolCallId("call_1".to_string());
        turn.upsert_tool(ActiveToolCell {
            tool_call_id: id.clone(),
            tool_name: "shell".to_string(),
            arguments: "{\"command\":\"cargo test\"}".to_string(),
            approval: ToolApprovalState::NotRequired,
            live_output: LiveOutput::default(),
            started_at: std::time::Instant::now(),
        });

        turn.resolve_approval(deep_code_agent::ApprovalDecision::Approved);

        assert_eq!(
            turn.tools[0].approval,
            ToolApprovalState::NotRequired,
            "nobody was asked, so nothing was answered"
        );
        assert!(
            quiet_while_running("shell", turn.tools[0].approval),
            "and the call stays quiet, exactly as the record says"
        );
    }

    /// The minimum a `mark_approval_required` / `resolve_approval` pair needs.
    fn gated_shell_request() -> deep_code_agent::ApprovalRequest {
        deep_code_agent::ApprovalRequest {
            call_id: "call_1".to_string(),
            tool_name: "shell".to_string(),
            description: "runs a command".to_string(),
            arguments: serde_json::json!({ "command": "cargo test" }),
            risk_level: deep_code_agent::RiskLevel::Medium,
            requires_sandbox: true,
            network: false,
            justification: None,
            resolved_target: None,
            read_only: false,
            matched_rule: None,
            preview: None,
            safety_notes: Vec::new(),
        }
    }

    /// The parked path still resolves, or the fix above would have broken it.
    #[test]
    fn an_approval_with_a_parked_request_still_resolves() {
        let mut turn = ActiveTurn::default();
        let request = gated_shell_request();
        turn.mark_approval_required(&request);
        turn.pending_approval = Some(request);

        turn.resolve_approval(deep_code_agent::ApprovalDecision::Approved);

        assert_eq!(turn.tools[0].approval, ToolApprovalState::Approved);
        assert!(!quiet_while_running("shell", turn.tools[0].approval));
    }

    /// A run that was still waiting when its turn was cancelled must not keep
    /// the `pending` badge: nobody will answer it now.
    #[test]
    fn a_cancelled_turn_gives_up_on_an_unanswered_request() {
        let mut turn = ActiveTurn::default();
        let request = gated_shell_request();
        turn.mark_approval_required(&request);
        assert_eq!(turn.tools[0].approval, ToolApprovalState::Required);

        turn.abandon_unanswered_approvals();

        assert_eq!(turn.tools[0].approval, ToolApprovalState::Unknown);
        assert_eq!(
            turn.tools[0]
                .approval
                .label(deep_code_agent::i18n::Lang::Zh),
            ""
        );
    }

    #[test]
    fn live_output_buffer_keeps_bounded_tail() {
        let mut output = LiveOutput::default();
        output.push(&"a".repeat(5_000));
        output.push("tail-marker");
        let preview = output.preview_tail();
        assert!(preview.contains("tail-marker"));
        assert!(preview.chars().count() <= 4_096);
    }
}
