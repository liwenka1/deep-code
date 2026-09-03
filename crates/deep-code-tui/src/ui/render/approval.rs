//! The approval panel: risk display, action extraction, the pinned
//! decision-critical head (root grants above all), body layout and the
//! arming rule. Read `approval_lines`' doc before touching field order.

use super::*;
use deep_code_agent::RiskLevel;

/// Risk tier → (localized tag, accent colour). Risk is shown as colour, not a
/// `Risk: …` field. Matched on the real `RiskLevel` enum (label via
/// `text_id`), so a new variant fails to compile here instead of silently
/// falling through to an amber default — the old `format!("{:?}")` → string
/// match hid exactly that.
pub(super) fn risk_display(risk: RiskLevel, lang: Lang) -> (&'static str, Color) {
    let color = match risk {
        RiskLevel::High => Color::Red,
        RiskLevel::Medium => Color::Yellow,
        RiskLevel::Low => Color::DarkGray,
    };
    (tr(lang, risk.text_id()), color)
}

/// The human-meaningful action behind a tool call — the shell command, the file
/// path, etc. — instead of the raw JSON blob. Falls back to compact arguments.
///
/// The key that may occupy the line — `command`, `path`, … in that order, and
/// `path` alone for `request_write_root` — is decided by the agent's
/// [`deep_code_agent::action_summary`], the same table the auto-mode judge
/// reads a gated call through, so the panel and the judge can never describe
/// one call two ways. Arguments that are not JSON at all are shown as they
/// are, collapsed onto one line.
pub(super) fn extract_action(tool_name: &str, arguments_json: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(arguments_json) {
        Ok(value) => deep_code_agent::action_summary(tool_name, &value),
        Err(_) => crate::history::collapse_whitespace(arguments_json),
    }
}

/// Model-influenced text about to become an approval panel line: control
/// characters become spaces and the invisible reordering/padding family is
/// deleted (see [`neutralize_display_text`]), and the width is capped. Every
/// free-text argument of [`approval_lines`] passes through one of the two,
/// except `risk`: [`risk_display`] maps it to a `&'static str`, so that one
/// cannot echo its input at all. Pinned by
/// `approval_lines_sanitize_every_text_field`, which asserts one marker per
/// field — with both an escape sequence and a bidi override in every marker,
/// so a field that loses either half names itself — and renders the
/// root-grant branch as well, or the fields gated behind it would be passed
/// in and never drawn, pinning nothing.
///
/// The cap is in terminal **columns**, not characters: the panel reserves rows
/// by measuring its own wrapped body, so a cap the layout cannot convert to
/// rows is not a bound at all. Capping 240 *characters* let 240 CJK characters
/// claim 480 columns — seven rows at an 80-column terminal — which is how
/// model-supplied text could still push the resolved grant target past the
/// bottom edge after the height itself had been made content-sized.
pub(super) fn sanitize_panel_text(text: &str, max_cols: usize) -> String {
    crate::history::truncate_display_width(neutralize_display_text(text).trim(), max_cols)
}

/// The decision-critical head of a `request_write_root` panel: the boundary
/// caution, the directory the grant would ACTUALLY land on, a symlink warning
/// when the spelling resolves elsewhere, and — last, and labelled as such —
/// the model's own spelling.
///
/// Split out so the panel can render it as a PINNED block, outside the
/// scrollable region. Scrolling used to carry it away: `End` (bound for
/// reading a long justification) clamps to the bottom of the body, which put
/// the resolved target above the viewport with no "more above" marker and the
/// panel still armed — the same "approve a directory you were never shown"
/// the content-sized panel was meant to end, reached by a keystroke instead of
/// a small terminal.
///
/// One source of truth: `approval_lines` extends with exactly this, so the
/// count the panel pins cannot drift from what it draws.
pub(super) fn root_grant_lines(
    resolved_target: Option<&str>,
    action: &str,
    arguments_json: &str,
    width: usize,
    lang: Lang,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = Vec::new();
    let caution = Style::default().fg(Color::Yellow);
    lines.extend(wrap_prefixed(
        "  ",
        tr(lang, TextId::ApprovalRootGrant),
        width,
        caution,
        caution,
    ));
    match resolved_target {
        Some(target) => {
            let target_style = Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD);
            // Sanitized like every panel line. The runtime already
            // refuses targets with control characters in the name, so
            // this is the defense-in-depth layer, not the only one.
            let shown = sanitize_panel_text(target, 240);
            lines.extend(wrap_prefixed(
                "  ",
                &tr_with(lang, TextId::ApprovalRootGrantTarget, &[("path", &shown)]),
                width,
                target_style,
                target_style,
            ));
            // The request resolves somewhere its spelling doesn't say
            // (symlink in it): call that out, or an innocuous-looking
            // spelling could pass for the real target. Compared by path
            // components, so a benign respelling — trailing slash, `.`
            // segments — is not accused of resolving elsewhere.
            let requested = serde_json::from_str::<serde_json::Value>(arguments_json)
                .ok()
                .and_then(|arguments| {
                    arguments
                        .get("path")
                        .and_then(|value| value.as_str().map(|path| path.trim().to_string()))
                });
            let resolves_elsewhere = requested.as_deref().is_none_or(|raw| {
                std::path::Path::new(raw)
                    .components()
                    .ne(std::path::Path::new(target).components())
            });
            if resolves_elsewhere {
                lines.extend(wrap_prefixed(
                    "  ",
                    tr(lang, TextId::ApprovalRootGrantSymlink),
                    width,
                    caution,
                    caution,
                ));
            }
        }
        // Defensive: with prompt-time triage a root grant is only parked
        // WITH a resolved target; still, never render a boundary prompt
        // that silently lacks the one line that matters.
        None => lines.extend(wrap_prefixed(
            "  ",
            tr(lang, TextId::ApprovalRootGrantUnresolved),
            width,
            caution,
            caution,
        )),
    }
    // The model's own spelling comes last and says so: it is what was
    // asked for, not what approving would grant.
    lines.extend(wrap_prefixed(
        "  ",
        &tr_with(
            lang,
            TextId::ApprovalRootGrantRequested,
            &[("path", action)],
        ),
        width,
        dim,
        dim,
    ));
    lines
}

/// The decision-critical head of ANY approval panel: the header, and the
/// subject the decision is about — for a root grant the resolved-directory
/// block, for every other tool the action line (the command, the file being
/// written).
///
/// Rendered by the panel as a PINNED block, outside the scrollable region, so
/// that "armed" can mean "the human can see what they are deciding about" for
/// every tool rather than only for root grants.
///
/// Pinning the head used to cover the root grant alone, and the generic arming
/// condition — "at least one body row was painted" — was left as a proxy for
/// the real invariant. It only ever held for the pinned case: body row 0 is the
/// header, which names the tool but never the action, so on a 5- or 6-row
/// terminal a `shell` or `write_file` prompt armed with its subject one row
/// below the edge (and at 5 rows the overflow indicator, splitting `[Min(1),
/// Length(1)]` over a single row, got zero rows and vanished too). Focus starts
/// on Approve for those, so one `y` ran a command that was never displayed.
///
/// One source of truth: [`approval_lines`] *is* this function plus the
/// scrollable remainder, so the count the panel pins cannot drift from what it
/// draws. The previous split recomputed the count from an unsanitized action
/// while the body used the capped one, which over-counted and let the pinned
/// block swallow the whole body.
pub(super) fn approval_head_lines(
    tool_name: &str,
    risk: RiskLevel,
    action: &str,
    resolved_target: Option<&str>,
    arguments_json: &str,
    width: usize,
    lang: Lang,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let (risk_tag, risk_color) = risk_display(risk, lang);
    let risk_style = Style::default().fg(risk_color);

    let mut header = vec![
        Span::styled("● ", risk_style),
        Span::styled(
            tr(lang, TextId::ApprovalNeeded),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · ", dim),
        Span::styled(
            sanitize_panel_text(tool_name, 120),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !risk_tag.is_empty() {
        header.push(Span::styled(" · ", dim));
        header.push(Span::styled(risk_tag, risk_style));
    }
    let mut lines = vec![Line::from(header)];

    // A root grant changes the boundary itself, not just this one run —
    // called out in warning color, together with the directory the grant would
    // ACTUALLY land on: the runtime resolves the request once for this prompt
    // and later refuses the grant unless it still resolves identically, so
    // this line — not the model's raw spelling in the arguments — is what the
    // human is judging.
    //
    // These lines come BEFORE the requested spelling, and the spelling is
    // rendered labelled rather than as a bare action line. Both facts are
    // load-bearing. The spelling is model-controlled and of model-chosen
    // width, so as the first body element it could push the resolved target
    // off the bottom of a content-sized panel; and a bare, unlabelled action
    // line wraps into continuation rows that are indistinguishable from a
    // field row, which let a spelling ending in `.../Grant target (resolved):
    // /tmp/safe` paint a counterfeit target above the real one. Putting the
    // resolved directory first and labelling the spelling removes both.
    if tool_name == deep_code_agent::REQUEST_WRITE_ROOT_TOOL {
        lines.extend(root_grant_lines(
            resolved_target,
            action,
            arguments_json,
            width,
            lang,
        ));
    } else {
        lines.extend(wrap_prefixed(
            "  ",
            action,
            width,
            Style::default(),
            Style::default(),
        ));
    }
    lines
}

/// Minimal, borderless approval block matching the welcome/picker style: a
/// risk-coloured `●` + tool, the action it will take (prominent), an optional
/// dim description, and only meaningful metadata (sandbox / matched rule).
///
/// Starts with [`approval_head_lines`], which the panel pins: this function is
/// that head plus the scrollable remainder, which is what makes the pinned row
/// count impossible to drift from what is drawn.
#[allow(clippy::too_many_arguments)]
pub(super) fn approval_lines(
    tool_name: &str,
    risk: RiskLevel,
    requires_sandbox: bool,
    network: bool,
    justification: Option<&str>,
    resolved_target: Option<&str>,
    matched_rule: Option<&str>,
    description: &str,
    arguments_json: &str,
    preview: Option<&str>,
    safety_notes: &[SafetyNote],
    width: usize,
    lang: Lang,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let action = sanitize_panel_text(&extract_action(tool_name, arguments_json), 240);
    let mut lines = approval_head_lines(
        tool_name,
        risk,
        &action,
        resolved_target,
        arguments_json,
        width,
        lang,
    );

    // Neutralised but not capped: tool descriptions run past any line cap, and
    // they are written by this crate, not the model — the filter is uniformity
    // with the rest of the panel, not a boundary of its own.
    let description = neutralize_display_text(description);
    let description = description.trim();
    if !description.is_empty() && description != action {
        lines.extend(wrap_prefixed("  ", description, width, dim, dim));
    }

    // The model's own words, clearly labelled as its claim (it wrote this
    // text; approving is still entirely the human's judgement).
    if let Some(text) = justification {
        let clean = sanitize_panel_text(text, 240);
        if !clean.is_empty() {
            let claim = Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC);
            lines.extend(wrap_prefixed(
                "  ",
                &tr_with(lang, TextId::ApprovalJustification, &[("text", &clean)]),
                width,
                claim,
                claim,
            ));
        }
    }

    let mut meta = Vec::new();
    // The network ask leads: it is what makes this approval different from an
    // ordinary run of the same command.
    if network {
        meta.push(tr(lang, TextId::ApprovalNetwork).to_string());
    }
    if requires_sandbox {
        // `requires_sandbox` is what the *policy* asked for. Whether the host can
        // deliver it is a separate question — the Windows Job Object confines
        // neither writes nor network, so claiming "sandboxed execution" there
        // would tell the user they are protected while they approve a command
        // that is not. Say which it is.
        //
        // Three answers, not two: a host can also confine everything except a
        // right its kernel is too old to express (Landlock before 6.2 does not
        // govern `truncate(2)`). Rounding that up to "sandboxed" is the same
        // overclaim in a quieter form, and rounding it down to "no sandbox"
        // would push users off a boundary that is holding.
        let text = match deep_code_agent::sandbox_enforcement() {
            deep_code_agent::Enforcement::Full => tr(lang, TextId::ApprovalSandbox).to_string(),
            // Names the binary the user actually invoked. This string used to
            // hardcode `deepcode`, which is only the npm spelling — a source
            // build installs `deep-code`, so the one actionable step in a
            // security-path message was a command those users do not have.
            deep_code_agent::Enforcement::Partial { .. } => tr_with(
                lang,
                TextId::ApprovalPartialSandbox,
                &[("program", &crate::cli::program_name())],
            ),
            deep_code_agent::Enforcement::None => tr(lang, TextId::ApprovalNoSandbox).to_string(),
        };
        meta.push(text);
    }
    if let Some(rule) = matched_rule {
        let rule = sanitize_panel_text(rule, 120);
        meta.push(tr_with(lang, TextId::ApprovalRule, &[("rule", &rule)]));
    }
    if !meta.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  {}", meta.join(" · ")),
            dim,
        )));
    }

    // Advisory static notes for shell commands: why this warrants review and a
    // paired suggestion. Not a dry-run — just a heads-up before the user acts.
    if !safety_notes.is_empty() {
        let caution = Style::default().fg(Color::Yellow);
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!("  {}", tr(lang, TextId::ApprovalCautionHeader)),
            caution,
        )));
        for note in safety_notes {
            lines.extend(wrap_prefixed(
                "  • ",
                tr(lang, note.reason),
                width,
                caution,
                caution,
            ));
            lines.extend(wrap_prefixed(
                "    ↳ ",
                tr(lang, note.suggestion),
                width,
                dim,
                dim,
            ));
        }
    }

    if let Some(preview) = preview.filter(|preview| !preview.trim().is_empty()) {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!("  {}", tr(lang, TextId::ApprovalPreviewHeader)),
            dim,
        )));
        let added = Style::default().fg(Color::Green);
        let removed = Style::default().fg(Color::Red);
        for raw in preview.lines() {
            // The widest model-controlled text on the panel: a write_file diff
            // is that call's own `content` argument (apply_patch's `old`/`new`)
            // rendered verbatim, so it gets the same neutralisation as the
            // action and the justification. Not trimmed — a diff line's leading
            // spaces are its alignment. Colour is decided from the neutralised
            // text so a leading escape byte cannot borrow the `+` styling.
            let line = neutralize_display_text(raw);
            let style = match line.as_bytes().first() {
                Some(b'+') => added,
                Some(b'-') => removed,
                _ => dim,
            };
            lines.extend(wrap_prefixed("  ", &line, width, style, style));
        }
    }
    lines
}

/// Rows the y/a/n choice block occupies at the bottom of the panel.
pub(super) const APPROVAL_OPTION_ROWS: u16 = 3;
/// Floor for the whole panel — the historical fixed size.
pub(super) const APPROVAL_PANEL_MIN_ROWS: u16 = 6;
/// Ceiling for the whole panel. Past this the body scrolls: a long diff
/// preview must not push the transcript off the screen.
pub(super) const APPROVAL_PANEL_MAX_ROWS: u16 = 16;

/// Everything an [`ApprovalRequest`] contributes to the panel.
///
/// The production path to [`approval_lines`] goes through this struct so that
/// `approval_lines_sanitize_every_text_field` can destructure it exhaustively:
/// adding a field here fails to compile until that test accounts for it. The
/// positional signature alone was not a guard — a new parameter is silenced at
/// every call site by passing `None`, and the sanitisation test stayed green
/// while the field rendered raw. Two real gaps (`preview`, `description`)
/// reached users that way.
///
/// [`ApprovalRequest`]: deep_code_agent::ApprovalRequest
pub(super) struct ApprovalPanelText<'a> {
    pub(super) tool_name: &'a str,
    pub(super) risk: RiskLevel,
    pub(super) requires_sandbox: bool,
    pub(super) network: bool,
    pub(super) justification: Option<&'a str>,
    pub(super) resolved_target: Option<&'a str>,
    pub(super) matched_rule: Option<&'a str>,
    pub(super) description: &'a str,
    pub(super) arguments_json: String,
    pub(super) preview: Option<&'a str>,
    pub(super) safety_notes: &'a [SafetyNote],
}

impl<'a> ApprovalPanelText<'a> {
    pub(super) fn from_request(request: &'a deep_code_agent::ApprovalRequest) -> Self {
        Self {
            tool_name: &request.tool_name,
            risk: request.risk_level,
            requires_sandbox: request.requires_sandbox,
            network: request.network,
            justification: request.justification.as_deref(),
            resolved_target: request.resolved_target.as_deref(),
            matched_rule: request.matched_rule.as_deref(),
            description: &request.description,
            arguments_json: request.arguments.to_string(),
            preview: request.preview.as_deref(),
            safety_notes: &request.safety_notes,
        }
    }

    /// Rows of [`Self::render`] that form the pinned head. Goes through the
    /// same struct and the same function the body is built from, so the two
    /// cannot disagree about where the head ends.
    pub(super) fn head_rows(&self, width: usize, lang: Lang) -> usize {
        let action =
            sanitize_panel_text(&extract_action(self.tool_name, &self.arguments_json), 240);
        approval_head_lines(
            self.tool_name,
            self.risk,
            &action,
            self.resolved_target,
            &self.arguments_json,
            width,
            lang,
        )
        .len()
    }

    pub(super) fn render(&self, width: usize, lang: Lang) -> Vec<Line<'static>> {
        approval_lines(
            self.tool_name,
            self.risk,
            self.requires_sandbox,
            self.network,
            self.justification,
            self.resolved_target,
            self.matched_rule,
            self.description,
            &self.arguments_json,
            self.preview,
            self.safety_notes,
            width,
            lang,
        )
    }
}

/// The panel body for the pending approval, wrapped to `width`.
///
/// Shared by the renderer and [`approval_panel_rows`] so the height the layout
/// reserves is measured from the very lines that will be drawn — a panel sized
/// from a second, drifting estimate is how the resolved-target line ends up
/// just off the bottom edge.
pub(super) fn approval_body(app: &App, width: usize) -> Vec<Line<'static>> {
    let Some(request) = app.pending_approval.as_ref() else {
        return Vec::new();
    };
    ApprovalPanelText::from_request(request).render(width, app.lang)
}

/// Rows to reserve for the approval panel, sized to its content.
///
/// A fixed height silently truncated the prompt: three body rows meant a
/// `request_write_root` showed its header, action and boundary warning while
/// the resolved directory — the line the human is told to judge by, and the
/// whole point of resolving before prompting — sat one row below the edge with
/// no overflow indicator. Growing to fit puts every decision-critical line on
/// screen; content past [`APPROVAL_PANEL_MAX_ROWS`] (a long diff preview)
/// still scrolls.
///
/// The old ceiling also capped the panel at half the frame, which read as
/// politeness but was the bug: on a 15-row terminal half a frame cannot hold
/// the prompt, and the rows that fell off the bottom were the resolved target
/// and the overflow indicator both. A share of the screen is not something to
/// negotiate when the alternative is asking the human to approve a directory
/// the panel never named — so the only ceiling left is the frame itself
/// (minus the status row), and [`APPROVAL_PANEL_MAX_ROWS`] on top of it.
pub(super) fn approval_panel_rows(app: &App, area: ratatui::layout::Rect) -> u16 {
    let width = usize::from(area.width.saturating_sub(2)).max(8);
    let wanted = u16::try_from(approval_body(app, width).len())
        .unwrap_or(u16::MAX)
        .saturating_add(APPROVAL_OPTION_ROWS);
    // Everything below the status row may be taken. `floor` is itself capped
    // by `ceiling`, because `clamp` panics when min > max and a 6-row terminal
    // would otherwise reach that.
    let available = area.height.saturating_sub(1);
    let ceiling = APPROVAL_PANEL_MAX_ROWS.min(available);
    let floor = APPROVAL_PANEL_MIN_ROWS.min(ceiling);
    wanted.clamp(floor, ceiling)
}

pub(super) fn render_approval_panel(
    frame: &mut Frame<'_>,
    app: &mut App,
    area: ratatui::layout::Rect,
) {
    if app.pending_approval.is_none() {
        return;
    }
    // Body (scrollable) on top; the y/a/n choices pinned to the bottom rows so
    // they stay visible even when a long command wraps.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(APPROVAL_OPTION_ROWS)])
        .split(area);

    let width = usize::from(chunks[0].width.saturating_sub(2)).max(8);
    let mut body = approval_body(app, width);

    // The head does not scroll, for ANY tool. It is the same lines
    // `approval_body` already produced (drained off the front, so the two
    // cannot drift), lifted out of the scrollable region and drawn above it:
    // the subject of the decision must be on screen at the moment the decision
    // keys are live. `End` — the natural keystroke for reading a long
    // justification — otherwise clamped the body to its bottom and carried that
    // line above the viewport, armed and with no "more above" marker; and a
    // short terminal cut it off below. Pinning covers both, and covering every
    // tool is what lets `approval_armed` below mean the invariant instead of
    // approximating it.
    let pinned_rows = app
        .pending_approval
        .as_ref()
        .map(|request| ApprovalPanelText::from_request(request).head_rows(width, app.lang))
        .unwrap_or(0)
        .min(body.len());
    let pinned: Vec<Line<'static>> = body.drain(..pinned_rows).collect();
    let (pinned_area, chunk_body) = if pinned.is_empty() {
        (None, chunks[0])
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(u16::try_from(pinned.len()).unwrap_or(u16::MAX)),
                Constraint::Min(0),
            ])
            .split(chunks[0]);
        (Some(rows[0]), rows[1])
    };
    // A `Length` longer than the space available is clamped, so this is how a
    // head that did not fit reports itself. Feeds `approval_armed` below.
    let head_drawn = pinned_area.map_or(pinned.is_empty(), |area| {
        usize::from(area.height) >= pinned.len()
    });
    let chunks = [chunk_body, chunks[1]];
    let body_len = body.len();
    // A body taller than its area gives up its last row to an overflow
    // indicator. Without one, a panel that ends mid-content looks like the
    // whole prompt — and "there is more you have not read" is precisely what a
    // boundary prompt must not hide. Reserving the row cannot create the
    // overflow it reports: this branch is only taken when the body already
    // exceeds the untrimmed height.
    let overflows = body_len > usize::from(chunks[0].height);
    let (content_area, hint_area) = if overflows {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(chunks[0]);
        (rows[0], Some(rows[1]))
    } else {
        (chunks[0], None)
    };
    // Clamp against the real rendered body (wrapped lines, safety notes, diff
    // preview) so the user can scroll to the very end before deciding. Only the
    // render layer knows the true wrapped height, so it also writes the clamped
    // value back — otherwise PageDown past the end accumulates unbounded and a
    // later PageUp has to burn off the overshoot before the view moves.
    let viewport = usize::from(content_area.height).max(1);
    let max_scroll = body_len.saturating_sub(viewport);
    let scroll = app.approval_scroll_offset.min(max_scroll);
    app.approval_scroll_offset = scroll;
    if let Some(pinned_area) = pinned_area {
        frame.render_widget(
            Paragraph::new(pinned).block(Block::default().padding(Padding::new(1, 0, 0, 0))),
            pinned_area,
        );
    }
    let body_paragraph = Paragraph::new(body)
        .block(Block::default().padding(Padding::new(1, 0, 0, 0)))
        .scroll((scroll as u16, 0));
    frame.render_widget(body_paragraph, content_area);
    if let Some(hint_area) = hint_area
        && max_scroll > scroll
    {
        let hint = tr_with(
            app.lang,
            TextId::ApprovalMoreBelow,
            &[("count", &(max_scroll - scroll).to_string())],
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  {hint}"),
                Style::default().fg(Color::Yellow),
            )))
            .block(Block::default().padding(Padding::new(1, 0, 0, 0))),
            hint_area,
        );
    }

    let key_y = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let key_a = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let key_n = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);

    let focus = app.approval_focus;
    // A root grant offers no "approve for session": consent is per-directory
    // by design, so the option (and its key) disappear rather than silently
    // downgrade.
    let mut options: Vec<(&str, &str, Style)> = vec![
        ("  y", tr(app.lang, TextId::ApprovalOptApprove), key_y),
        ("  a", tr(app.lang, TextId::ApprovalOptSession), key_a),
        ("  n", tr(app.lang, TextId::ApprovalOptDeny), key_n),
    ];
    if app.pending_is_root_grant() {
        options.remove(1);
    }
    let options_body: Vec<Line> = options
        .iter()
        .enumerate()
        .map(|(i, &(key_label, desc, style))| {
            if i == focus {
                let arrow = Span::styled(" ▶", style);
                let key = Span::styled(key_label, style);
                let desc = Span::styled(
                    format!("  {desc}"),
                    Style::default().add_modifier(Modifier::BOLD),
                );
                Line::from(vec![arrow, key, desc])
            } else {
                let arrow = Span::styled("  ", dim);
                let key = Span::styled(key_label, dim);
                let desc = Span::styled(format!("  {desc}"), dim);
                Line::from(vec![arrow, key, desc])
            }
        })
        .collect();

    let option_count = options_body.len();
    let options =
        Paragraph::new(options_body).block(Block::default().padding(Padding::new(1, 0, 0, 0)));
    frame.render_widget(options, chunks[1]);

    // Armed only now, and only if the rows a decision rests on were actually
    // painted: the whole pinned head — which carries the subject of the
    // decision for every tool — and room for EVERY choice. A viewport showing
    // `y Approve` while `n Deny` fell off the bottom is the worst of the two,
    // since deny is where the focus starts on a root grant.
    //
    // This used to be the first statement in the function, set
    // unconditionally, so a panel squeezed to zero rows — not one cell drawn
    // — still accepted `y`: the queued-keystroke guard was off exactly when
    // the user could see nothing. A frame with no room leaves the prompt
    // disarmed; the next one (a resize, a redraw) arms it.
    //
    // The sole condition used to be `content_area.height > 0` — "at least one
    // body row". That row is the header, which names the tool and never the
    // action, so every non-root-grant prompt armed on a 5- or 6-row terminal
    // with its subject off-screen and (at 5 rows) no overflow marker either.
    // Only root grants were safe, and only because their head was pinned.
    //
    // Pinning the head for EVERY tool is what fixes that, and it does so
    // through this same term: a head too tall for the space takes the whole
    // region, leaving the scrollable remainder zero rows. So `head_drawn` is
    // belt-and-braces today — deliberately, as the invariant stated outright
    // instead of inferred from a `Length` being clamped and a `Min(0)`
    // collapsing, two layouts away. Inference of exactly that kind is what let
    // this panel arm blind twice; a later layout change must not be able to
    // quietly reinstate it.
    app.approval_armed =
        head_drawn && content_area.height > 0 && usize::from(chunks[1].height) >= option_count;
}
