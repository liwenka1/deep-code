//! The approval-panel state machine on [`App`]: parking an arriving request,
//! moving and acting on the focused option (approve / session / deny), the
//! root-grant carve-outs, and the scroll offsets. Split from `app/mod.rs` as one
//! cohesive concept; the `impl App` methods here are the whole approval concern.

use super::*;

impl App {
    pub fn approve_pending_tool(&mut self) {
        self.resolve_pending_tool(ApprovalDecision::Approved);
    }

    /// "a": approve and remember the tool (or, for shell, the command
    /// identity) for this session. Ignored wherever the panel does not offer
    /// the option — a root grant, a sub-agent dispatch, a job control action,
    /// a compound shell command — because there the runtime would record
    /// nothing and silently downgrade to a one-time approve; an option that
    /// is not shown must not act from its key either.
    pub fn approve_pending_tool_for_session(&mut self) {
        if !self.pending_offers_session_consent() {
            return;
        }
        self.resolve_pending_tool(ApprovalDecision::ApprovedForSession);
    }

    pub fn deny_pending_tool(&mut self) {
        self.resolve_pending_tool(ApprovalDecision::Denied);
    }

    /// Whether the pending approval offers "approve for session" at all. The
    /// runtime decides ([`deep_code_agent::session_consent_recordable`], the
    /// same two rules its recording path applies): a by-name consent for
    /// ordinary tools, a command-identity consent for one simple shell
    /// command. A root grant (per-directory by design), a sub-agent dispatch
    /// (what it authorizes lives in the arguments), a job status/tail/cancel
    /// and a compound shell command record nothing, so the option — and its
    /// key — disappear rather than silently downgrade to a one-time approve.
    pub fn pending_offers_session_consent(&self) -> bool {
        self.pending_approval.as_ref().is_some_and(|request| {
            deep_code_agent::session_consent_recordable(&request.tool_name, &request.arguments)
        })
    }

    /// Park an arriving approval: reset the view, choose the starting option,
    /// and disarm the decision keys until the panel has been drawn.
    ///
    /// The reflex `Enter` belongs to the reversible answer, so two classes
    /// start focused on **deny**: a root grant, which widens the session's
    /// write boundary for good rather than authorizing one action; and
    /// anything reaching the network, where a mis-keyed yes costs whatever the
    /// far end sends back or receives — `curl … | sh` is one keystroke either
    /// way. The user still approves with `y` or by moving the highlight;
    /// nothing became harder to reach, the default just changed side.
    ///
    /// A *local* High-risk action deliberately keeps approve as its default.
    /// The genuinely unrecoverable ones are hard-refused by the deny floor
    /// before a prompt exists at all, and flipping the whole tier would train
    /// the user to reach for `y` reflexively instead — which is the habit this
    /// is here to protect.
    pub(crate) fn park_approval(&mut self, request: ApprovalRequest) {
        let is_root_grant = request.tool_name == deep_code_agent::REQUEST_WRITE_ROOT_TOOL;
        let deny_by_default = is_root_grant || request.network;
        self.pending_approval = Some(request);
        self.approval_scroll_offset = 0;
        // Deny is last either way, but a prompt with no recordable consent
        // renders y/n (see `pending_offers_session_consent`) against y/a/n.
        let options = self.approval_option_count();
        self.approval_focus = if deny_by_default { options - 1 } else { 0 };
        self.approval_armed = false;
        self.is_streaming = false;
    }

    /// How many options the approval panel offers (y/a/n, or y/n when the
    /// prompt has no recordable session consent).
    fn approval_option_count(&self) -> usize {
        if self.pending_offers_session_consent() {
            3
        } else {
            2
        }
    }

    /// Move the approval highlight to the previous option (wrap around).
    pub fn approval_focus_up(&mut self) {
        let last = self.approval_option_count() - 1;
        self.approval_focus = if self.approval_focus == 0 {
            last
        } else {
            self.approval_focus - 1
        };
    }

    /// Move the approval highlight to the next option (wrap around).
    pub fn approval_focus_down(&mut self) {
        let last = self.approval_option_count() - 1;
        self.approval_focus = if self.approval_focus >= last {
            0
        } else {
            self.approval_focus + 1
        };
    }

    /// Execute the currently highlighted approval action.
    pub fn execute_focused_approval(&mut self) {
        if !self.pending_offers_session_consent() {
            // Two options: 0 = approve, 1 = deny.
            match self.approval_focus {
                0 => self.approve_pending_tool(),
                _ => self.deny_pending_tool(),
            }
            return;
        }
        match self.approval_focus {
            0 => self.approve_pending_tool(),
            1 => self.approve_pending_tool_for_session(),
            _ => self.deny_pending_tool(),
        }
    }

    pub fn scroll_approval_up(&mut self) {
        self.approval_scroll_offset = self.approval_scroll_offset.saturating_sub(3);
    }

    /// Unclamped, like the transcript's `scroll_up`: only the render layer
    /// knows the real (width-wrapped, preview-carrying) panel height, so it
    /// clamps against actual lines there.
    pub fn scroll_approval_down(&mut self) {
        self.approval_scroll_offset = self.approval_scroll_offset.saturating_add(3);
    }

    pub fn scroll_approval_to_top(&mut self) {
        self.approval_scroll_offset = 0;
    }

    /// Jump to the end of the panel body. Deliberately unclamped for the same
    /// reason as [`Self::scroll_approval_down`]: the render layer owns the real
    /// height and clamps this back to the last line.
    pub fn scroll_approval_to_bottom(&mut self) {
        self.approval_scroll_offset = usize::MAX;
    }

    fn resolve_pending_tool(&mut self, decision: ApprovalDecision) {
        if self.pending_approval.take().is_none() {
            return;
        }

        let label = self.tr(decision.text_id());
        self.approval_scroll_offset = 0;
        self.status = self.tr_with(TextId::StatusToolResolved, &[("decision", label)]);
        self.is_streaming = true;
        self.start_stream(StreamRequest::Approval(decision));
    }
}
