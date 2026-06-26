//! TUI application state.
//!
//! This module is intentionally thin: the agent runtime owns the model loop,
//! tool registry, session, and approval gating. The UI only has to:
//!
//! 1. forward user prompts via [`AgentRuntimeHandle::submit_user`],
//! 2. render [`RuntimeEvent`]s as they arrive,
//! 3. forward approval decisions via [`AgentRuntimeHandle::submit_approval`].

use std::sync::Arc;

use std::path::PathBuf;

use crate::ui::{COMPOSER_MAX_VISIBLE_ROWS, layout_input};
use deep_code_agent::{
    AgentConfig, AgentRuntimeHandle, ApprovalDecision, ApprovalRequest, CostCurrency,
    JsonSessionStore, LaunchedRuntime, RuntimeEvent, SessionRecord, SessionStore,
    SharedSubAgentManager, TurnTelemetry, default_config_path, launch_runtime,
};
use tokio::sync::mpsc;

use crate::active_turn::ActiveTurn;
use crate::cli::workspace_root;
use crate::history::{HistoryCell, hydrate_history};

#[derive(Debug, Clone, Default)]
pub struct LaunchConfig {
    pub resume: Option<SessionRecord>,
}

/// Updates pushed from the bridge task into the UI thread.
#[derive(Debug, Clone, PartialEq)]
enum UiUpdate {
    Event(Box<RuntimeEvent>),
    StreamFinished,
}

enum StreamRequest {
    User(String),
    Approval(ApprovalDecision),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionKind {
    Slash,
    File,
}

/// Inline completion menu state: `/` commands or `@` workspace files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionMenu {
    pub(crate) kind: CompletionKind,
    /// (completion value, hint)
    pub(crate) items: Vec<(String, String)>,
    pub(crate) selected: usize,
}

const COMPLETION_MENU_ITEMS: usize = 8;

type UiUpdateReceiver = mpsc::UnboundedReceiver<UiUpdate>;

pub struct App {
    pub(crate) input_cursor: usize,
    pub input: String,
    pub history: Vec<HistoryCell>,
    pub active_turn: Option<ActiveTurn>,
    pub status: String,
    pub error: Option<String>,
    pub should_quit: bool,
    /// Armed by a first Ctrl+C on an idle, empty composer; a second
    /// consecutive Ctrl+C then quits. Reset by any other key.
    pub(crate) ctrl_c_pending: bool,
    pub is_streaming: bool,
    pub pending_approval: Option<ApprovalRequest>,
    pub last_checkpoint: Option<String>,
    pub session_id: Option<String>,
    pub(crate) resumed: bool,
    pub scroll_offset: usize,
    pub approval_scroll_offset: usize,
    /// Currently highlighted approval option: 0 = y (approve), 1 = a (session),
    /// 2 = n (deny). Navigated with ↑/↓, acted on with Enter.
    pub approval_focus: usize,
    pub(crate) runtime: Arc<dyn AgentRuntimeHandle>,
    pub(crate) backend_label: String,
    pub(crate) subagent_manager: SharedSubAgentManager,
    subagent_shutdown: Option<Box<dyn Fn() + Send + Sync>>,
    ui_rx: Option<UiUpdateReceiver>,
    pub(crate) cost_currency: CostCurrency,
    pub(crate) configured_model: String,
    pub(crate) configured_reasoning: String,
    pub(crate) last_telemetry: Option<TurnTelemetry>,
    pub(crate) prompt_history: Vec<String>,
    history_cursor: Option<usize>,
    history_draft: String,
    pub(crate) completion: Option<CompletionMenu>,
    pub(crate) workspace_files: Vec<String>,
    /// Target of `/apikey` `/model` `/logout` writes; overridable in tests.
    pub(crate) global_config_path: PathBuf,
    /// When the current stream segment began, for the live activity
    /// indicator. Only read while `is_streaming`.
    pub(crate) streaming_since: Option<std::time::Instant>,
    /// Collapsed pastes: `(placeholder token, real content)`. Large pastes
    /// show a compact `[粘贴 #N …]` chip in the composer; the real content is
    /// expanded back in on submit. Reset once a turn is sent or input cleared.
    pub(crate) pasted_blocks: Vec<(String, String)>,
    /// Geometry + plain text of the last transcript render, so mouse events
    /// can be mapped to a text position for drag-selection.
    pub(crate) transcript: Option<TranscriptSnapshot>,
    /// Active mouse selection over the transcript: `(anchor, head)` as
    /// `(line, display_col)` into [`TranscriptSnapshot::lines`].
    pub(crate) selection: Option<(TextPos, TextPos)>,
    /// Open `/resume` modal: rendered as an in-app overlay (no alt-screen
    /// churn, so switching sessions doesn't flicker) over the live TUI.
    pub(crate) resume_picker: Option<ResumePicker>,
}

/// In-session `/resume` modal state: the resumable sessions (newest-first) and
/// the highlighted row.
pub(crate) struct ResumePicker {
    pub(crate) sessions: Vec<SessionRecord>,
    pub(crate) selected: usize,
}

/// A position in the transcript line buffer: absolute line index + display
/// column (CJK counts as 2).
pub(crate) type TextPos = (usize, usize);

/// What the transcript looked like at the last render — used to translate a
/// mouse `(col, row)` into a `(line, display_col)` position.
#[derive(Debug, Clone, Default)]
pub(crate) struct TranscriptSnapshot {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub scroll_top: usize,
    pub lines: Vec<String>,
}

const PROMPT_HISTORY_CAP: usize = 100;

/// Number of characters (not bytes) in `s`.
fn char_count(s: &str) -> usize {
    s.chars().count()
}

/// Display width of a string (CJK counts as 2), for selection columns.
fn display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    UnicodeWidthStr::width(s)
}

/// Substring of `s` covering display columns `[from, to)`. A grapheme is
/// included when its cell range overlaps the requested span (so a CJK char
/// straddling the boundary is kept whole rather than split).
fn slice_by_display_cols(s: &str, from: usize, to: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    if from >= to {
        return String::new();
    }
    let mut out = String::new();
    let mut col = 0usize;
    for g in s.graphemes(true) {
        let w = UnicodeWidthStr::width(g).max(1);
        let g_end = col + w;
        if col < to && g_end > from {
            out.push_str(g);
        }
        col = g_end;
        if col >= to {
            break;
        }
    }
    out
}

/// Convert a 0-based character index to a byte index. Clamps to `s.len()`.
fn byte_idx(s: &str, char_index: usize) -> usize {
    if char_index == 0 {
        return 0;
    }
    s.char_indices().nth(char_index).map_or(s.len(), |(b, _)| b)
}

/// Build the startup welcome header from the resolved session/runtime state.
/// Shared by initial launch and `/clear` (which starts a fresh conversation).
fn welcome_cell(
    model: &str,
    reasoning: &str,
    offline: bool,
    workspace: String,
    session: String,
) -> HistoryCell {
    HistoryCell::Welcome {
        version: env!("CARGO_PKG_VERSION").to_string(),
        model: format!("DeepSeek {model} · 推理 {reasoning}"),
        offline,
        workspace,
        session,
    }
}

/// Render a path home-relative (`/Users/x/p` → `~/p`) for the welcome header.
fn home_relative(path: &std::path::Path) -> String {
    let shown = path.display().to_string();
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if !home.is_empty()
            && let Some(rest) = shown.strip_prefix(home.as_ref())
        {
            return format!("~{rest}");
        }
    }
    shown
}

/// Remove the character at `char_index` (0-based). Returns true when removed.
fn remove_char_at(s: &mut String, char_index: usize) -> bool {
    let start = byte_idx(s, char_index);
    if start >= s.len() {
        return false;
    }
    let end = byte_idx(s, char_index + 1);
    s.drain(start..end);
    true
}

impl App {
    #[must_use]
    pub fn launch(config: LaunchConfig) -> Self {
        let workspace = workspace_root();
        let workspace_files = deep_code_agent::list_workspace_files(&workspace, 2000);
        let loaded = AgentConfig::load(&workspace);
        let config_warnings = loaded.report.warnings.clone();
        let agent_config = loaded.config;
        let cost_currency = agent_config.cost_currency;
        let configured_model = agent_config.model.clone();
        let configured_reasoning = agent_config.reasoning_effort.as_setting().to_string();
        let workspace_display = home_relative(&workspace);
        let launched = launch_runtime(&agent_config, workspace, config.resume.clone());
        let runtime = launched.handle;
        let backend_label = launched.backend_label;
        let session_id = launched.session_id;
        let subagent_manager = launched.subagent_manager;
        let subagent_shutdown = Some(launched.stop_hook);
        let resumed = config.resume.is_some();
        let persistent = session_id.is_some();
        let session_summary = if resumed {
            let turns = config
                .resume
                .as_ref()
                .map(|record| {
                    record
                        .messages
                        .iter()
                        .filter(|message| message.role == deep_code_agent::Role::User)
                        .count()
                })
                .unwrap_or(0);
            format!("已恢复 · {turns} 轮对话")
        } else if persistent {
            "新会话 · 已持久化".to_string()
        } else {
            "新会话 · 未持久化".to_string()
        };
        let mut history = vec![welcome_cell(
            &configured_model,
            &configured_reasoning,
            backend_label.contains("offline echo"),
            workspace_display,
            session_summary,
        )];

        if !config_warnings.is_empty() {
            history.push(HistoryCell::system(format!(
                "配置警告:\n{}",
                config_warnings.join("\n")
            )));
        }

        if let Some(record) = config.resume.as_ref() {
            history.extend(hydrate_history(record));
        }

        let status = if let Some(id) = &session_id {
            if resumed {
                format!("Ready (resumed) - {backend_label} | session {id}")
            } else {
                format!("Ready - {backend_label} | session {id}")
            }
        } else {
            format!("Ready - {backend_label}")
        };

        Self {
            input_cursor: 0,
            input: String::new(),
            history,
            active_turn: None,
            status,
            error: None,
            should_quit: false,
            ctrl_c_pending: false,
            is_streaming: false,
            pending_approval: None,
            last_checkpoint: None,
            session_id,
            resumed,
            scroll_offset: 0,
            approval_scroll_offset: 0,
            approval_focus: 0,
            runtime,
            backend_label,
            subagent_manager,
            subagent_shutdown,
            ui_rx: None,
            cost_currency,
            configured_model,
            configured_reasoning,
            last_telemetry: None,
            prompt_history: Vec::new(),
            history_cursor: None,
            history_draft: String::new(),
            completion: None,
            workspace_files,
            global_config_path: default_config_path(),
            streaming_since: None,
            pasted_blocks: Vec::new(),
            transcript: None,
            selection: None,
            resume_picker: None,
        }
    }

    /// Live activity label shown while streaming: an animated spinner plus
    /// elapsed seconds, so a long time-to-first-token wait reads as
    /// "生成中" rather than a frozen screen.
    #[must_use]
    pub(crate) fn streaming_activity(&self) -> Option<String> {
        if !self.is_streaming {
            return None;
        }
        let elapsed = self.streaming_since?.elapsed();
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frame = FRAMES[(elapsed.as_millis() / 120 % FRAMES.len() as u128) as usize];
        Some(format!("{frame} 生成中 {}s", elapsed.as_secs()))
    }

    #[must_use]
    pub fn new() -> Self {
        Self::launch(LaunchConfig::default())
    }

    pub fn push_char(&mut self, value: char) {
        if self.is_streaming {
            return;
        }
        let cursor = self.input_cursor.min(char_count(&self.input));
        let byte = byte_idx(&self.input, cursor);
        self.input.insert(byte, value);
        self.input_cursor = cursor + 1;
        self.history_cursor = None;
        self.refresh_completion();
    }

    pub fn backspace(&mut self) {
        if self.is_streaming || self.input_cursor == 0 {
            return;
        }
        let target = self.input_cursor.saturating_sub(1);
        if remove_char_at(&mut self.input, target) {
            self.input_cursor = target;
        }
        self.history_cursor = None;
        self.refresh_completion();
    }

    /// Insert a newline into the composer (Alt+Enter / Ctrl+J).
    pub fn push_newline(&mut self) {
        if self.is_streaming {
            return;
        }
        let cursor = self.input_cursor.min(char_count(&self.input));
        let byte = byte_idx(&self.input, cursor);
        self.input.insert(byte, '\n');
        self.input_cursor = cursor + 1;
        self.history_cursor = None;
        self.refresh_completion();
    }

    /// Delete the character after the cursor (Delete key).
    pub fn delete_forward(&mut self) {
        if self.is_streaming {
            return;
        }
        if self.input_cursor >= char_count(&self.input) {
            return;
        }
        remove_char_at(&mut self.input, self.input_cursor);
        // cursor stays — next char slides left into its place.
        self.history_cursor = None;
        self.refresh_completion();
    }

    /// Move cursor one character left.
    pub fn cursor_left(&mut self) {
        if self.is_streaming || self.input_cursor == 0 {
            return;
        }
        self.input_cursor -= 1;
    }

    /// Move cursor one character right.
    pub fn cursor_right(&mut self) {
        if self.is_streaming {
            return;
        }
        if self.input_cursor < char_count(&self.input) {
            self.input_cursor += 1;
        }
    }

    /// Move cursor to start of the current logical line.
    pub fn cursor_home(&mut self) {
        if self.is_streaming {
            return;
        }
        let byte = byte_idx(&self.input, self.input_cursor.min(char_count(&self.input)));
        let prefix = &self.input[..byte];
        let line_start_byte = prefix.rfind('\n').map_or(0, |pos| pos + 1);
        self.input_cursor = self.input[..line_start_byte].chars().count();
    }

    /// Move cursor to end of the current logical line.
    pub fn cursor_end(&mut self) {
        if self.is_streaming {
            return;
        }
        let byte = byte_idx(&self.input, self.input_cursor.min(char_count(&self.input)));
        let tail = &self.input[byte..];
        let eol = tail.find('\n').unwrap_or(tail.len());
        let line_end_byte = byte + eol;
        self.input_cursor = self.input[..line_end_byte].chars().count();
    }

    /// Move cursor to the very end of input.
    pub fn cursor_to_end(&mut self) {
        self.input_cursor = char_count(&self.input);
    }

    /// Record what the transcript render produced, for mouse → text mapping.
    pub(crate) fn set_transcript_snapshot(&mut self, snap: TranscriptSnapshot) {
        self.transcript = Some(snap);
    }

    /// Map an absolute mouse `(col, row)` to a `(line, display_col)` position
    /// in the transcript buffer, or `None` if outside the transcript area.
    fn mouse_to_text(&self, col: u16, row: u16) -> Option<TextPos> {
        let snap = self.transcript.as_ref()?;
        if row < snap.y
            || row >= snap.y.saturating_add(snap.height)
            || col < snap.x
            || col >= snap.x.saturating_add(snap.width)
        {
            return None;
        }
        // Text starts one column in (the left padding gutter).
        let text_x = snap.x.saturating_add(1);
        let line = snap.scroll_top + usize::from(row - snap.y);
        if line >= snap.lines.len() {
            // Below the last line → clamp to end of the last line.
            let last = snap.lines.len().saturating_sub(1);
            let width = snap.lines.get(last).map_or(0, |l| display_width(l));
            return Some((last, width));
        }
        let display_col = usize::from(col.saturating_sub(text_x));
        let max = display_width(&snap.lines[line]);
        Some((line, display_col.min(max)))
    }

    /// Begin a selection at a mouse position (left button down).
    pub(crate) fn selection_begin(&mut self, col: u16, row: u16) {
        match self.mouse_to_text(col, row) {
            Some(pos) => self.selection = Some((pos, pos)),
            None => self.selection = None,
        }
    }

    /// Extend the in-progress selection (left button drag).
    pub(crate) fn selection_update(&mut self, col: u16, row: u16) {
        if let (Some((anchor, _)), Some(pos)) = (self.selection, self.mouse_to_text(col, row)) {
            self.selection = Some((anchor, pos));
        }
    }

    /// Finish a selection (left button up): returns the selected text to copy,
    /// or `None` for an empty selection (a plain click), which clears it.
    pub(crate) fn selection_finish(&mut self) -> Option<String> {
        let (anchor, head) = self.selection?;
        if anchor == head {
            self.selection = None;
            return None;
        }
        self.selected_text()
    }

    pub(crate) fn clear_selection(&mut self) {
        self.selection = None;
    }

    /// Extract the currently selected transcript text.
    pub(crate) fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection?;
        let snap = self.transcript.as_ref()?;
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        let mut out = String::new();
        for line in start.0..=end.0 {
            let text = snap.lines.get(line)?;
            let from = if line == start.0 { start.1 } else { 0 };
            let to = if line == end.0 {
                end.1
            } else {
                display_width(text)
            };
            out.push_str(&slice_by_display_cols(text, from, to));
            if line != end.0 {
                out.push('\n');
            }
        }
        Some(out)
    }

    fn insert_str_at_cursor(&mut self, text: &str) {
        let cursor = self.input_cursor.min(char_count(&self.input));
        let byte = byte_idx(&self.input, cursor);
        self.input.insert_str(byte, text);
        self.input_cursor = cursor + char_count(text);
        self.history_cursor = None;
        self.refresh_completion();
    }

    /// Handle a bracketed-paste payload. Large pastes (multi-line or long)
    /// collapse to a compact `[粘贴 #N …]` chip whose real content is kept in
    /// `pasted_blocks` and expanded back in on submit; short single-line
    /// pastes insert inline.
    pub fn paste_str(&mut self, text: String) {
        if self.is_streaming {
            return;
        }
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.is_empty() {
            return;
        }
        let multiline = normalized.contains('\n');
        let chars = char_count(&normalized);
        if multiline || chars > 120 {
            let id = self.pasted_blocks.len() + 1;
            let placeholder = if multiline {
                format!("[粘贴 #{id} +{} 行]", normalized.lines().count().max(1))
            } else {
                format!("[粘贴 #{id} · {chars} 字]")
            };
            self.insert_str_at_cursor(&placeholder);
            self.pasted_blocks.push((placeholder, normalized));
        } else {
            self.insert_str_at_cursor(&normalized);
        }
    }

    /// Replace any collapsed-paste placeholders with their real content.
    fn expand_pasted(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (placeholder, content) in &self.pasted_blocks {
            out = out.replace(placeholder.as_str(), content);
        }
        out
    }

    /// Delete the word (and any whitespace) before the cursor (Ctrl+W).
    pub fn delete_word_back(&mut self) {
        if self.is_streaming || self.input_cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.input.chars().collect();
        let mut start = self.input_cursor.min(chars.len());
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        self.drain_chars(start, self.input_cursor);
        self.input_cursor = start;
        self.history_cursor = None;
        self.refresh_completion();
    }

    /// Delete from the current logical line's start up to the cursor (Ctrl+U).
    pub fn kill_to_line_start(&mut self) {
        if self.is_streaming || self.input_cursor == 0 {
            return;
        }
        let start = self.current_line_start_char();
        self.drain_chars(start, self.input_cursor);
        self.input_cursor = start;
        self.history_cursor = None;
        self.refresh_completion();
    }

    /// Delete from the cursor to the end of the current logical line (Ctrl+K).
    pub fn kill_to_line_end(&mut self) {
        if self.is_streaming {
            return;
        }
        let end = self.current_line_end_char();
        self.drain_chars(self.input_cursor, end);
        self.history_cursor = None;
        self.refresh_completion();
    }

    /// Move cursor to the previous word start (Ctrl/Alt + Left).
    pub fn word_left(&mut self) {
        if self.is_streaming || self.input_cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.input_cursor.min(chars.len());
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.input_cursor = i;
    }

    /// Move cursor to the next word end (Ctrl/Alt + Right).
    pub fn word_right(&mut self) {
        if self.is_streaming {
            return;
        }
        let chars: Vec<char> = self.input.chars().collect();
        let len = chars.len();
        let mut i = self.input_cursor.min(len);
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        while i < len && !chars[i].is_whitespace() {
            i += 1;
        }
        self.input_cursor = i;
    }

    fn drain_chars(&mut self, start: usize, end: usize) {
        if start >= end {
            return;
        }
        let mut chars: Vec<char> = self.input.chars().collect();
        let end = end.min(chars.len());
        chars.drain(start..end);
        self.input = chars.into_iter().collect();
    }

    fn current_line_start_char(&self) -> usize {
        let byte = byte_idx(&self.input, self.input_cursor.min(char_count(&self.input)));
        let start_byte = self.input[..byte].rfind('\n').map_or(0, |pos| pos + 1);
        self.input[..start_byte].chars().count()
    }

    fn current_line_end_char(&self) -> usize {
        let byte = byte_idx(&self.input, self.input_cursor.min(char_count(&self.input)));
        let eol = self.input[byte..]
            .find('\n')
            .unwrap_or(self.input.len() - byte);
        self.input[..byte + eol].chars().count()
    }

    /// Up arrow drives the composer only — never the transcript (that's
    /// PageUp/PageDown). In a multi-line draft it moves the cursor up a line
    /// (no-op at the top, so the draft is never clobbered); on a single-line
    /// or empty composer it recalls the previous prompt.
    pub fn on_up(&mut self) {
        if self.is_streaming {
            return;
        }
        if self.input.contains('\n') {
            self.cursor_up_logical();
        } else {
            self.history_prev();
        }
    }

    /// Down arrow: mirror of [`on_up`].
    pub fn on_down(&mut self) {
        if self.is_streaming {
            return;
        }
        if self.input.contains('\n') {
            self.cursor_down_logical();
        } else {
            self.history_next();
        }
    }

    /// Move one logical line up, preserving column. Returns false when already
    /// on the first line (so the caller can fall back to history).
    fn cursor_up_logical(&mut self) -> bool {
        let (line, col) = self.cursor_line_col();
        if line == 0 {
            return false;
        }
        let target = line - 1;
        let len = self.logical_line_len(target);
        self.input_cursor = self.line_start_char(target) + col.min(len);
        true
    }

    fn cursor_down_logical(&mut self) -> bool {
        let (line, col) = self.cursor_line_col();
        let total = self.input.split('\n').count();
        if line + 1 >= total {
            return false;
        }
        let target = line + 1;
        let len = self.logical_line_len(target);
        self.input_cursor = self.line_start_char(target) + col.min(len);
        true
    }

    /// (logical line index, column in chars) of the cursor.
    fn cursor_line_col(&self) -> (usize, usize) {
        let mut line = 0usize;
        let mut col = 0usize;
        for (i, ch) in self.input.chars().enumerate() {
            if i >= self.input_cursor {
                break;
            }
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    /// Char index where logical line `line` starts.
    fn line_start_char(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        let mut seen = 0usize;
        for (i, ch) in self.input.chars().enumerate() {
            if ch == '\n' {
                seen += 1;
                if seen == line {
                    return i + 1;
                }
            }
        }
        char_count(&self.input)
    }

    fn logical_line_len(&self, line: usize) -> usize {
        self.input
            .split('\n')
            .nth(line)
            .map_or(0, |l| l.chars().count())
    }

    #[must_use]
    pub(crate) fn completion_open(&self) -> bool {
        self.completion.is_some()
    }

    pub(crate) fn close_completion(&mut self) {
        self.completion = None;
    }

    /// Recompute the menu from the current input: `/command` prefix while no
    /// whitespace was typed, or a trailing `@file` token.
    fn refresh_completion(&mut self) {
        self.completion = self.compute_completion();
    }

    fn compute_completion(&self) -> Option<CompletionMenu> {
        if let Some(rest) = self.input.strip_prefix('/') {
            if self.input.contains(char::is_whitespace) {
                return None;
            }
            let filter = rest.to_lowercase();
            let items: Vec<(String, String)> = crate::commands::SLASH_COMMANDS
                .iter()
                .filter(|(name, _, _)| name[1..].starts_with(&filter))
                .map(|(name, hint, takes_arg)| {
                    let value = if *takes_arg {
                        format!("{name} ")
                    } else {
                        (*name).to_string()
                    };
                    (value, (*hint).to_string())
                })
                .collect();
            return (!items.is_empty()).then_some(CompletionMenu {
                kind: CompletionKind::Slash,
                items,
                selected: 0,
            });
        }

        let token_start = self.trailing_token_start();
        let token = &self.input[token_start..];
        let filter = token.strip_prefix('@')?;
        let filter_lower = filter.to_lowercase();
        let mut matched: Vec<&String> = self
            .workspace_files
            .iter()
            .filter(|file| file.to_lowercase().contains(&filter_lower))
            .collect();
        matched.sort_by_key(|file| {
            (
                !file.to_lowercase().starts_with(&filter_lower),
                file.len(),
                (*file).clone(),
            )
        });
        let items: Vec<(String, String)> = matched
            .into_iter()
            .take(COMPLETION_MENU_ITEMS)
            .map(|file| (file.clone(), String::new()))
            .collect();
        (!items.is_empty()).then_some(CompletionMenu {
            kind: CompletionKind::File,
            items,
            selected: 0,
        })
    }

    /// Byte index where the trailing whitespace-delimited token begins.
    fn trailing_token_start(&self) -> usize {
        self.input
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map_or(0, |(index, ch)| index + ch.len_utf8())
    }

    pub(crate) fn completion_up(&mut self) {
        if let Some(menu) = self.completion.as_mut() {
            let len = menu.items.len();
            menu.selected = (menu.selected + len - 1) % len;
        }
    }

    pub(crate) fn completion_down(&mut self) {
        if let Some(menu) = self.completion.as_mut() {
            menu.selected = (menu.selected + 1) % menu.items.len();
        }
    }

    /// Apply the selected completion to the input. Returns true when the
    /// completed value is a ready-to-run slash command (no argument), so the
    /// caller can submit immediately on Enter.
    pub(crate) fn accept_completion(&mut self) -> bool {
        let Some(menu) = self.completion.take() else {
            return false;
        };
        let Some((value, _)) = menu.items.get(menu.selected) else {
            return false;
        };
        match menu.kind {
            CompletionKind::Slash => {
                self.input = value.clone();
                self.cursor_to_end();
                !value.ends_with(' ')
            }
            CompletionKind::File => {
                let token_start = self.trailing_token_start();
                self.input.truncate(token_start);
                self.input.push_str(value);
                self.input.push(' ');
                self.cursor_to_end();
                false
            }
        }
    }

    /// Composer area height including borders; content grows 1..=MAX visual rows.
    #[must_use]
    #[allow(dead_code)]
    pub fn input_height(&self, inner_width: u16) -> u16 {
        let layout = layout_input(
            &self.input,
            self.input_cursor,
            usize::from(inner_width.max(1)),
            COMPOSER_MAX_VISIBLE_ROWS,
        );
        (layout.total_rows.clamp(1, COMPOSER_MAX_VISIBLE_ROWS) as u16) + 2
    }

    /// Recall the previous sent prompt (Ctrl+P). The live input is stashed as
    /// a draft and restored when navigating past the newest entry.
    pub fn history_prev(&mut self) {
        if self.is_streaming || self.prompt_history.is_empty() {
            return;
        }
        let cursor = match self.history_cursor {
            None => {
                self.history_draft = std::mem::take(&mut self.input);
                self.prompt_history.len() - 1
            }
            Some(0) => 0,
            Some(index) => index - 1,
        };
        self.history_cursor = Some(cursor);
        self.input = self.prompt_history[cursor].clone();
        self.cursor_to_end();
    }

    /// Walk back toward the draft (Ctrl+N).
    pub fn history_next(&mut self) {
        if self.is_streaming {
            return;
        }
        match self.history_cursor {
            None => {}
            Some(index) if index + 1 < self.prompt_history.len() => {
                self.history_cursor = Some(index + 1);
                self.input = self.prompt_history[index + 1].clone();
                self.cursor_to_end();
            }
            Some(_) => {
                self.history_cursor = None;
                self.input = std::mem::take(&mut self.history_draft);
                self.cursor_to_end();
            }
        }
    }

    fn remember_prompt(&mut self, prompt: &str) {
        if self.prompt_history.last().map(String::as_str) != Some(prompt) {
            self.prompt_history.push(prompt.to_string());
            if self.prompt_history.len() > PROMPT_HISTORY_CAP {
                self.prompt_history.remove(0);
            }
        }
        self.history_cursor = None;
        self.history_draft.clear();
    }

    pub fn submit(&mut self) {
        if self.is_streaming || self.pending_approval.is_some() {
            return;
        }
        self.close_completion();
        // The transcript is about to grow; drop any stale selection.
        self.clear_selection();

        // `display` keeps the compact `[粘贴 #N …]` chips (shown in the
        // transcript and recalled by Ctrl+P); `sent` expands them to the real
        // pasted content for the model.
        let display = self.input.trim().to_string();
        if display.is_empty() {
            self.status = "Enter a prompt before sending.".to_string();
            return;
        }
        let sent = self.expand_pasted(&display);
        // Never let the API key into the recallable prompt history.
        if !display.starts_with("/apikey") {
            self.remember_prompt(&display);
        }

        if display.starts_with('/') && self.handle_slash_command(&display) {
            self.clear_input();
            return;
        }

        self.clear_input();
        self.error = None;
        self.active_turn = None;
        self.scroll_offset = 0;
        self.approval_scroll_offset = 0;
        self.is_streaming = true;
        self.status = format!("Streaming from {}...", self.backend_label);

        self.history.push(HistoryCell::user(display));

        self.start_stream(StreamRequest::User(sent));
    }

    /// Clear the composer and any pending collapsed-paste blocks.
    fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.pasted_blocks.clear();
    }

    /// Esc cancel stack: close completion menu > deny approval (handled by
    /// the approval branch in `ui::handle_key`) > cancel the streaming turn
    /// > clear input > quit.
    pub fn handle_escape(&mut self) {
        if self.completion_open() {
            self.close_completion();
        } else if self.is_streaming {
            self.cancel_streaming_turn();
        } else if !self.input.is_empty() {
            self.clear_input();
            self.history_cursor = None;
            self.status = "已清空输入 (再按 Esc 退出)".to_string();
        } else {
            self.should_quit = true;
        }
    }

    /// Graceful Ctrl+C: interrupt a stream, else clear input, else require a
    /// second consecutive press to actually quit.
    pub fn handle_ctrl_c(&mut self) {
        if self.is_streaming {
            self.ctrl_c_pending = false;
            self.cancel_streaming_turn();
        } else if !self.input.is_empty() {
            self.clear_input();
            self.history_cursor = None;
            self.ctrl_c_pending = false;
            self.status = "已清空输入 (再按 Ctrl+C 退出)".to_string();
        } else if self.ctrl_c_pending {
            self.should_quit = true;
        } else {
            self.ctrl_c_pending = true;
            self.status = "再按一次 Ctrl+C 退出".to_string();
        }
    }

    /// Any non-Ctrl+C key disarms the quit guard.
    pub fn clear_ctrl_c_guard(&mut self) {
        self.ctrl_c_pending = false;
    }

    fn cancel_streaming_turn(&mut self) {
        self.status = "正在取消本轮... (取消在工具边界生效)".to_string();
        let runtime = Arc::clone(&self.runtime);
        // The streaming loop emits TurnCancelled on the live channel that the
        // bridge task is already pumping; the receiver returned here stays
        // empty, so it can be dropped.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = runtime.cancel_turn().await;
            });
        }
    }

    pub fn approve_pending_tool(&mut self) {
        self.resolve_pending_tool(ApprovalDecision::Approved);
    }

    /// "a": approve and remember the tool for this session. The runtime
    /// downgrades shell-class tools to a one-time approve.
    pub fn approve_pending_tool_for_session(&mut self) {
        self.resolve_pending_tool(ApprovalDecision::ApprovedForSession);
    }

    pub fn deny_pending_tool(&mut self) {
        self.resolve_pending_tool(ApprovalDecision::Denied);
    }

    /// Move the approval highlight to the previous option (wrap around).
    pub fn approval_focus_up(&mut self) {
        self.approval_focus = if self.approval_focus == 0 {
            2
        } else {
            self.approval_focus - 1
        };
    }

    /// Move the approval highlight to the next option (wrap around).
    pub fn approval_focus_down(&mut self) {
        self.approval_focus = if self.approval_focus == 2 {
            0
        } else {
            self.approval_focus + 1
        };
    }

    /// Execute the currently highlighted approval action.
    pub fn execute_focused_approval(&mut self) {
        match self.approval_focus {
            0 => self.approve_pending_tool(),
            1 => self.approve_pending_tool_for_session(),
            _ => self.deny_pending_tool(),
        }
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(3);
    }

    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(3);
    }

    #[allow(dead_code)]
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn scroll_approval_up(&mut self) {
        self.approval_scroll_offset = self.approval_scroll_offset.saturating_sub(3);
    }

    pub fn scroll_approval_down(&mut self) {
        self.approval_scroll_offset = self
            .approval_scroll_offset
            .saturating_add(3)
            .min(self.approval_scroll_max());
    }

    pub fn scroll_approval_to_top(&mut self) {
        self.approval_scroll_offset = 0;
    }

    #[must_use]
    pub fn clamped_approval_scroll_offset(&self) -> usize {
        self.approval_scroll_offset.min(self.approval_scroll_max())
    }

    pub(crate) fn approval_cell(&self) -> Option<HistoryCell> {
        self.pending_approval
            .as_ref()
            .map(|request| HistoryCell::Approval {
                tool_name: request.tool_name.clone(),
                description: request.description.clone(),
                risk_level: format!("{:?}", request.risk_level),
                requires_sandbox: request.requires_sandbox,
                matched_rule: request.matched_rule.clone(),
                arguments: request.arguments.to_string(),
            })
    }

    fn approval_scroll_max(&self) -> usize {
        self.approval_cell()
            .map(|cell| cell.lines().len().saturating_sub(1))
            .unwrap_or(0)
    }

    /// Apply queued runtime updates; returns whether anything changed (the
    /// render loop uses this to skip redundant redraws).
    pub fn drain_stream_updates(&mut self) -> bool {
        let Some(mut rx) = self.ui_rx.take() else {
            return false;
        };

        let mut applied = false;
        while let Ok(update) = rx.try_recv() {
            self.apply_ui_update(update);
            applied = true;
        }

        if self.is_streaming {
            self.ui_rx = Some(rx);
        }
        applied
    }

    fn resolve_pending_tool(&mut self, decision: ApprovalDecision) {
        if self.pending_approval.take().is_none() {
            return;
        }

        let label = match decision {
            ApprovalDecision::Approved => "approved",
            ApprovalDecision::ApprovedForSession => "approved (session)",
            ApprovalDecision::Denied => "denied",
        };
        self.approval_scroll_offset = 0;
        self.status = format!("Tool {label}, resuming...");
        self.is_streaming = true;
        self.start_stream(StreamRequest::Approval(decision));
    }

    fn adopt_runtime(&mut self, launched: LaunchedRuntime) {
        self.runtime = launched.handle;
        self.backend_label = launched.backend_label;
        self.session_id = launched.session_id;
        self.subagent_manager = launched.subagent_manager;
        self.subagent_shutdown = Some(launched.stop_hook);
    }

    /// Rebuild the runtime with the (re-loaded) layered config, resuming the
    /// current persisted session so the conversation continues seamlessly.
    /// The old runtime is shut down first so its persistence flush lands
    /// before the session record is re-read.
    pub(crate) fn relaunch_runtime(&mut self) -> Result<(), String> {
        if self.is_streaming || self.pending_approval.is_some() {
            return Err("正在流式输出或等待审批，请稍后再切换配置".to_string());
        }

        if let Some(stop) = self.subagent_shutdown.take() {
            stop();
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let runtime = Arc::clone(&self.runtime);
            tokio::task::block_in_place(|| handle.block_on(runtime.shutdown()));
        }

        let workspace = workspace_root();
        let project = workspace.join(".deep-code").join("config.toml");
        let loaded = AgentConfig::load_with(
            Some(self.global_config_path.clone()),
            Some(project),
            &|name| std::env::var(name).ok(),
        );
        let agent_config = loaded.config;

        let resume = self.session_id.as_ref().and_then(|id| {
            let store = JsonSessionStore::for_workspace(&workspace).ok()?;
            let session_id = deep_code_agent::SessionId::parse(id).ok()?;
            store.load(&session_id).ok()
        });
        if resume.is_none() && self.session_id.is_some() {
            return Err("无法读取当前会话记录，已取消配置切换".to_string());
        }
        let resumed = resume.is_some();

        let launched = launch_runtime(&agent_config, workspace, resume);
        self.cost_currency = agent_config.cost_currency;
        self.configured_model = agent_config.model.clone();
        self.configured_reasoning = agent_config.reasoning_effort.as_setting().to_string();
        self.resumed = resumed;
        self.adopt_runtime(launched);
        Ok(())
    }

    pub(crate) fn resume_picker_open(&self) -> bool {
        self.resume_picker.is_some()
    }

    /// Open the in-app `/resume` modal, listing the workspace's non-empty
    /// sessions (newest-first). No-ops with a status note when none qualify.
    pub(crate) fn open_resume_picker(&mut self) {
        if self.is_streaming || self.pending_approval.is_some() {
            self.status = "正在流式输出或等待审批，无法切换会话".to_string();
            return;
        }
        let sessions = match JsonSessionStore::for_workspace(workspace_root())
            .and_then(|store| store.list())
        {
            Ok(list) => list
                .into_iter()
                .filter(crate::startup::has_user_message)
                .collect::<Vec<_>>(),
            Err(error) => {
                self.status = format!("无法读取历史会话: {error}");
                return;
            }
        };
        if sessions.is_empty() {
            self.status = "没有可恢复的历史会话".to_string();
            return;
        }
        self.close_completion();
        self.clear_selection();
        self.resume_picker = Some(ResumePicker {
            sessions,
            selected: 0,
        });
        self.status = "选择要恢复的历史会话 (↑/↓ · Enter · Esc 取消)".to_string();
    }

    pub(crate) fn resume_picker_up(&mut self) {
        if let Some(picker) = self.resume_picker.as_mut() {
            picker.selected = picker.selected.saturating_sub(1);
        }
    }

    pub(crate) fn resume_picker_down(&mut self) {
        if let Some(picker) = self.resume_picker.as_mut()
            && picker.selected + 1 < picker.sessions.len()
        {
            picker.selected += 1;
        }
    }

    pub(crate) fn resume_picker_cancel(&mut self) {
        if self.resume_picker.take().is_some() {
            self.status = "已取消，留在当前会话".to_string();
        }
    }

    /// Switch to the highlighted session and close the modal.
    pub(crate) fn resume_picker_accept(&mut self) {
        let Some(mut picker) = self.resume_picker.take() else {
            return;
        };
        if picker.selected >= picker.sessions.len() {
            return;
        }
        let record = picker.sessions.swap_remove(picker.selected);
        if let Err(message) = self.switch_session(record) {
            self.status = message;
        }
    }

    /// Load session `id` and switch to it in place. Surfaces a readable status
    /// on a bad id / missing record rather than failing the command.
    pub(crate) fn switch_session_by_id(&mut self, id: &str) -> Result<(), String> {
        let workspace = workspace_root();
        let store = JsonSessionStore::for_workspace(&workspace)
            .map_err(|error| format!("无法打开会话存储: {error}"))?;
        let session_id = deep_code_agent::SessionId::parse(id)
            .map_err(|error| format!("无效的会话 id '{id}': {error}"))?;
        let record = store
            .load(&session_id)
            .map_err(|error| format!("找不到会话 '{id}': {error}"))?;
        self.switch_session(record)
    }

    /// Switch the live session to `record` in place: shut the current runtime
    /// down (flushing its persistence), relaunch resuming `record`, and rebuild
    /// the visible transcript. Mirrors Claude Code's `/resume`.
    pub(crate) fn switch_session(&mut self, record: SessionRecord) -> Result<(), String> {
        if self.is_streaming || self.pending_approval.is_some() {
            return Err("正在流式输出或等待审批，请稍后再切换会话".to_string());
        }
        if self.session_id.as_deref() == Some(record.id.as_str()) {
            self.status = "已是当前会话".to_string();
            return Ok(());
        }

        if let Some(stop) = self.subagent_shutdown.take() {
            stop();
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let runtime = Arc::clone(&self.runtime);
            tokio::task::block_in_place(|| handle.block_on(runtime.shutdown()));
        }

        let workspace = workspace_root();
        let loaded = AgentConfig::load(&workspace);
        let agent_config = loaded.config;
        let launched = launch_runtime(&agent_config, workspace, Some(record.clone()));
        self.cost_currency = agent_config.cost_currency;
        self.configured_model = agent_config.model.clone();
        self.configured_reasoning = agent_config.reasoning_effort.as_setting().to_string();
        self.resumed = true;
        self.adopt_runtime(launched);

        self.history.clear();
        self.active_turn = None;
        self.clear_selection();
        self.scroll_offset = 0;
        self.last_telemetry = None;
        self.error = None;
        self.history.extend(hydrate_history(&record));
        self.status = format!(
            "已切换到会话 {} - {}",
            record.id.as_str(),
            self.backend_label
        );
        Ok(())
    }

    /// Start a fresh conversation in place — Claude Code's `/clear`. The current
    /// session is flushed to disk (recoverable via `/resume`), a brand-new
    /// session is launched, and the view resets to the welcome header.
    pub(crate) fn start_new_conversation(&mut self) {
        if self.is_streaming || self.pending_approval.is_some() {
            self.status = "正在流式输出或等待审批，无法开启新对话".to_string();
            return;
        }

        if let Some(stop) = self.subagent_shutdown.take() {
            stop();
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let runtime = Arc::clone(&self.runtime);
            tokio::task::block_in_place(|| handle.block_on(runtime.shutdown()));
        }

        let workspace = workspace_root();
        let workspace_display = home_relative(&workspace);
        // Match `relaunch_runtime`: load through `self.global_config_path` so a
        // key saved via `/apikey` (and test overrides of the path) survives the
        // reload instead of falling back to the offline echo backend.
        let project = workspace.join(".deep-code").join("config.toml");
        let loaded = AgentConfig::load_with(
            Some(self.global_config_path.clone()),
            Some(project),
            &|name| std::env::var(name).ok(),
        );
        let agent_config = loaded.config;
        let launched = launch_runtime(&agent_config, workspace, None);
        self.cost_currency = agent_config.cost_currency;
        self.configured_model = agent_config.model.clone();
        self.configured_reasoning = agent_config.reasoning_effort.as_setting().to_string();
        self.resumed = false;
        self.adopt_runtime(launched);

        let persistent = self.session_id.is_some();
        self.history.clear();
        self.active_turn = None;
        self.clear_selection();
        self.close_completion();
        self.scroll_offset = 0;
        self.last_telemetry = None;
        self.last_checkpoint = None;
        self.error = None;
        let cell = welcome_cell(
            &self.configured_model,
            &self.configured_reasoning,
            self.backend_label.contains("offline echo"),
            workspace_display,
            if persistent {
                "新会话 · 已持久化".to_string()
            } else {
                "新会话 · 未持久化".to_string()
            },
        );
        self.history.push(cell);
        self.status = "已开启新对话（旧对话可用 /resume 找回）".to_string();
    }

    fn start_stream(&mut self, request: StreamRequest) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.ui_rx = Some(rx);
        self.streaming_since = Some(std::time::Instant::now());

        let runtime = Arc::clone(&self.runtime);
        tokio::spawn(async move {
            let mut events = match request {
                StreamRequest::User(prompt) => runtime.submit_user(prompt).await,
                StreamRequest::Approval(decision) => runtime.submit_approval(decision).await,
            };

            while let Some(event) = events.recv().await {
                if tx.send(UiUpdate::Event(Box::new(event.clone()))).is_err() {
                    return;
                }
                if matches!(
                    event,
                    RuntimeEvent::TurnFinished { .. }
                        | RuntimeEvent::TurnCancelled { .. }
                        | RuntimeEvent::ApprovalRequired { .. }
                        | RuntimeEvent::Error { .. }
                ) {
                    break;
                }
            }

            let _ = tx.send(UiUpdate::StreamFinished);
        });
    }

    fn apply_ui_update(&mut self, update: UiUpdate) {
        match update {
            UiUpdate::Event(event) => self.apply_runtime_event(*event),
            UiUpdate::StreamFinished => {
                self.is_streaming = false;
                self.ui_rx = None;
            }
        }
    }

    pub(crate) fn record_error(&mut self, message: String) {
        // Keep any streamed partial content visible: flush the active turn
        // into history before appending the error cell, otherwise the next
        // TurnStarted would silently discard it.
        self.flush_active_turn();
        self.error = Some(message.clone());
        self.status = "Agent error.".to_string();
        self.history
            .push(HistoryCell::system(format!("Error: {message}")));
        self.is_streaming = false;
        self.clear_stream_receiver();
    }

    pub(crate) fn clear_stream_receiver(&mut self) {
        self.ui_rx = None;
    }

    #[must_use]
    pub fn status_line(&self) -> String {
        let mode = if self.error.is_some() {
            "error"
        } else if self.pending_approval.is_some() {
            "approval"
        } else if self.is_streaming {
            "streaming"
        } else if self.resumed {
            "ready (resumed)"
        } else {
            "ready"
        };
        let session = self
            .session_id
            .as_deref()
            .map(|id| format!(" | session {id}"))
            .unwrap_or_else(|| " | session none".to_string());
        let checkpoint = self
            .last_checkpoint
            .as_deref()
            .map(|id| format!(" | checkpoint {id}"))
            .unwrap_or_default();
        let telemetry = self
            .last_telemetry
            .as_ref()
            .map(|value| {
                format!(
                    " | {} | turn {} | total {} | ctx {}%",
                    value.route_label,
                    value.turn_cost.format(self.cost_currency),
                    value.session_cost.format(self.cost_currency),
                    value.context_usage_percent
                )
            })
            .unwrap_or_default();
        format!(
            "{mode} - {}{session}{checkpoint} | {}{telemetry}",
            self.backend_label, self.status
        )
    }

    pub async fn shutdown_runtime(&self) {
        if let Some(shutdown) = &self.subagent_shutdown {
            shutdown();
        }
        self.runtime.shutdown().await;
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_help_clear_and_status_update_history() {
        let mut app = App::new();

        assert!(app.handle_slash_command("/help"));
        assert!(matches!(
            app.history.last(),
            Some(HistoryCell::System { text }) if text.contains("/status")
        ));

        assert!(app.handle_slash_command("/status"));
        assert!(matches!(
            app.history.last(),
            Some(HistoryCell::System { text }) if text.contains("backend=")
        ));

        // `/clear` starts a fresh conversation: the transcript resets to just
        // the welcome header rather than going empty.
        assert!(app.handle_slash_command("/clear"));
        assert!(
            matches!(app.history.as_slice(), [HistoryCell::Welcome { .. }]),
            "clear leaves only the welcome header, got {} cells",
            app.history.len()
        );
        assert!(app.status.contains("新对话"));
    }

    #[test]
    fn resume_picker_navigates_and_cancels() {
        use deep_code_agent::{AgentConfig, Message};
        let make = |prompt: &str| {
            let mut record = SessionRecord::new(
                std::path::PathBuf::from("/tmp/ws"),
                &AgentConfig::builtin(),
                "system",
            );
            record.messages = vec![Message::system("system"), Message::user(prompt)];
            record
        };
        let mut app = App::new();
        app.resume_picker = Some(ResumePicker {
            sessions: vec![make("a"), make("b"), make("c")],
            selected: 0,
        });
        assert!(app.resume_picker_open());

        app.resume_picker_down();
        app.resume_picker_down();
        assert_eq!(app.resume_picker.as_ref().unwrap().selected, 2);
        app.resume_picker_down(); // clamp at last row
        assert_eq!(app.resume_picker.as_ref().unwrap().selected, 2);
        app.resume_picker_up();
        assert_eq!(app.resume_picker.as_ref().unwrap().selected, 1);

        app.resume_picker_cancel();
        assert!(app.resume_picker.is_none());
        assert!(app.status.contains("已取消"));
    }

    #[test]
    fn slash_resume_blocked_while_streaming() {
        let mut app = App::new();
        app.is_streaming = true;
        assert!(app.handle_slash_command("/resume"));
        assert!(app.resume_picker.is_none());
        assert!(
            app.status.contains("无法切换会话"),
            "status: {}",
            app.status
        );
    }

    #[test]
    fn resume_command_is_registered() {
        assert!(
            crate::commands::SLASH_COMMANDS
                .iter()
                .any(|(name, _, _)| *name == "/resume")
        );
    }

    #[test]
    fn scroll_helpers_adjust_offset() {
        let mut app = App::new();
        app.scroll_up();
        assert_eq!(app.scroll_offset, 3);
        app.scroll_down();
        assert_eq!(app.scroll_offset, 0);
        app.scroll_up();
        app.scroll_to_bottom();
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn approval_scroll_helpers_adjust_panel_offset() {
        let mut app = App::new();
        app.pending_approval = Some(deep_code_agent::ApprovalRequest {
            call_id: "call_1".to_string(),
            tool_name: "write_file".to_string(),
            description: "Write a file".to_string(),
            arguments: serde_json::json!({ "path": "note.txt", "content": "hello" }),
            risk_level: deep_code_agent::RiskLevel::High,
            requires_sandbox: true,
            read_only: false,
            matched_rule: Some("write".to_string()),
        });
        for _ in 0..10 {
            app.scroll_approval_down();
        }
        assert_eq!(
            app.approval_scroll_offset,
            app.clamped_approval_scroll_offset()
        );
        assert!(app.approval_scroll_offset > 0);
        app.scroll_approval_up();
        assert!(app.approval_scroll_offset < app.approval_scroll_max());
        app.scroll_approval_down();
        app.scroll_approval_to_top();
        assert_eq!(app.approval_scroll_offset, 0);
    }

    #[test]
    fn status_includes_deepseek_native_telemetry() {
        let mut app = App::new();
        app.last_telemetry = Some(TurnTelemetry {
            route_label: "auto→deepseek-v4-flash (high)".to_string(),
            effective_model: "deepseek-v4-flash".to_string(),
            reasoning_effort: "high".to_string(),
            prompt_tokens: 100,
            completion_tokens: 20,
            cache_hit_tokens: Some(80),
            cache_miss_tokens: Some(20),
            session_cache_hit_tokens: 80,
            session_cache_miss_tokens: 20,
            session_cache_savings: deep_code_agent::CostEstimate::default(),
            prefix_status: deep_code_agent::PrefixStatus::Stable,
            route_reason: "短提示优先使用 Flash".to_string(),
            route_source: "heuristic".to_string(),
            fallback_reason: None,
            context_window: 1_000_000,
            estimated_context_tokens: 120,
            context_usage_percent: 1,
            near_compaction_threshold: false,
            used_model_fallback: false,
            stream_retries: 2,
            turn_cost: deep_code_agent::CostEstimate {
                cny: 0.001,
                usd: 0.0001,
            },
            session_cost: deep_code_agent::CostEstimate {
                cny: 0.002,
                usd: 0.0002,
            },
        });

        assert!(app.handle_slash_command("/status"));
        assert!(matches!(
            app.history.last(),
            Some(HistoryCell::System { text })
                if text.contains("effective_model=deepseek-v4-flash")
                    && text.contains("auto_reason=短提示优先使用 Flash")
                    && text.contains("session_cost=¥0.0020")
                    && text.contains("stream_retries=2")
        ));
    }

    #[test]
    fn tool_finished_flushes_tool_call_before_result_without_duplicate() {
        let mut app = App::new();
        let turn_id = deep_code_agent::TurnId("turn_1".to_string());
        let tool_call_id = deep_code_agent::ToolCallId("call_1".to_string());
        app.apply_runtime_event(RuntimeEvent::TurnStarted {
            turn_id: turn_id.clone(),
            prompt: "please echo".to_string(),
        });
        app.apply_runtime_event(RuntimeEvent::ToolCallStarted {
            turn_id: turn_id.clone(),
            tool_call_id: tool_call_id.clone(),
            tool_name: "mock_echo".to_string(),
            arguments: serde_json::json!({ "message": "hi" }),
        });

        let result = deep_code_agent::ToolResult::success("call_1", "mock_echo", "mock_echo: hi");
        app.apply_runtime_event(RuntimeEvent::ToolCallFinished {
            turn_id: Some(turn_id),
            tool_call_id,
            result: result.clone(),
        });
        app.apply_runtime_event(RuntimeEvent::ToolResult { result });

        let tool_call_index = app
            .history
            .iter()
            .position(|cell| matches!(cell, HistoryCell::ToolCall { .. }))
            .expect("tool call cell");
        let tool_result_indices = app
            .history
            .iter()
            .enumerate()
            .filter_map(|(index, cell)| {
                matches!(cell, HistoryCell::ToolResult { .. }).then_some(index)
            })
            .collect::<Vec<_>>();

        assert_eq!(tool_result_indices.len(), 1);
        assert!(tool_call_index < tool_result_indices[0]);
    }

    #[test]
    fn multi_tool_cells_flush_independently_per_finished_call() {
        let mut app = App::new();
        let turn_id = deep_code_agent::TurnId("turn_1".to_string());
        let call_1 = deep_code_agent::ToolCallId("call_1".to_string());
        let call_2 = deep_code_agent::ToolCallId("call_2".to_string());
        app.apply_runtime_event(RuntimeEvent::TurnStarted {
            turn_id: turn_id.clone(),
            prompt: "run both".to_string(),
        });
        app.apply_runtime_event(RuntimeEvent::ToolCallStarted {
            turn_id: turn_id.clone(),
            tool_call_id: call_1.clone(),
            tool_name: "git_echo".to_string(),
            arguments: serde_json::json!({ "message": "one" }),
        });
        app.apply_runtime_event(RuntimeEvent::ToolCallStarted {
            turn_id: turn_id.clone(),
            tool_call_id: call_2.clone(),
            tool_name: "mock_echo".to_string(),
            arguments: serde_json::json!({ "message": "two" }),
        });

        app.apply_runtime_event(RuntimeEvent::ToolCallFinished {
            turn_id: Some(turn_id.clone()),
            tool_call_id: call_1,
            result: deep_code_agent::ToolResult::success("call_1", "git_echo", "git_echo: one"),
        });

        // call_1 cell flushed to history; call_2 still streaming in active turn.
        assert!(app.history.iter().any(|cell| matches!(
            cell,
            HistoryCell::ToolCall { tool_name, .. } if tool_name == "git_echo"
        )));
        assert!(app.history.iter().all(|cell| !matches!(
            cell,
            HistoryCell::ToolCall { tool_name, .. } if tool_name == "mock_echo"
        )));
        let active = app.active_turn.as_ref().expect("active turn kept");
        assert_eq!(active.tools.len(), 1);
        assert_eq!(active.tools[0].tool_name, "mock_echo");

        app.apply_runtime_event(RuntimeEvent::ToolCallFinished {
            turn_id: Some(turn_id),
            tool_call_id: call_2,
            result: deep_code_agent::ToolResult::success("call_2", "mock_echo", "mock_echo: two"),
        });

        let tool_cells = app
            .history
            .iter()
            .filter_map(|cell| match cell {
                HistoryCell::ToolCall { tool_name, .. } => Some(tool_name.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_cells, vec!["git_echo", "mock_echo"]);
        let result_cells = app
            .history
            .iter()
            .filter(|cell| matches!(cell, HistoryCell::ToolResult { .. }))
            .count();
        assert_eq!(result_cells, 2);
        assert!(
            app.active_turn
                .as_ref()
                .is_some_and(|active| active.tools.is_empty())
        );
    }

    #[test]
    fn composer_newline_and_height_clamp() {
        let mut app = App::new();
        assert_eq!(
            app.input_height(80),
            3,
            "empty input is one row plus borders"
        );

        app.push_char('a');
        app.push_newline();
        app.push_char('b');
        assert_eq!(app.input, "a\nb");
        assert_eq!(app.input_height(80), 4);

        for _ in 0..10 {
            app.push_newline();
        }
        assert_eq!(
            app.input_height(80),
            8,
            "content rows clamp at 6 (+2 borders)"
        );

        app.is_streaming = true;
        let before = app.input.clone();
        app.push_newline();
        assert_eq!(app.input, before, "no edits while streaming");
    }

    #[test]
    fn prompt_history_navigates_and_preserves_draft() {
        let mut app = App::new();
        app.remember_prompt("first");
        app.remember_prompt("second");
        app.remember_prompt("second");
        assert_eq!(
            app.prompt_history,
            vec!["first".to_string(), "second".to_string()],
            "consecutive duplicates collapse"
        );

        app.input = "draft".to_string();
        app.history_prev();
        assert_eq!(app.input, "second");
        app.history_prev();
        assert_eq!(app.input, "first");
        app.history_prev();
        assert_eq!(app.input, "first", "clamped at oldest");

        app.history_next();
        assert_eq!(app.input, "second");
        app.history_next();
        assert_eq!(app.input, "draft", "draft restored past newest");

        // Editing a recalled entry detaches from history without mutating it.
        app.history_prev();
        assert_eq!(app.input, "second");
        app.push_char('!');
        assert_eq!(app.input, "second!");
        assert_eq!(app.prompt_history[1], "second");
        app.history_prev();
        assert_eq!(
            app.input, "second",
            "after edits, Ctrl+P starts a fresh walk"
        );
    }

    #[test]
    fn prompt_history_caps_at_limit() {
        let mut app = App::new();
        for index in 0..150 {
            app.remember_prompt(&format!("prompt-{index}"));
        }
        assert_eq!(app.prompt_history.len(), 100);
        assert_eq!(app.prompt_history[0], "prompt-50");
        assert_eq!(app.prompt_history[99], "prompt-149");
    }

    #[test]
    fn apikey_command_validates_writes_and_stays_out_of_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new();
        app.global_config_path = dir.path().join("config.toml");

        // Invalid key: rejected, nothing written, nothing remembered.
        app.input = "/apikey short".to_string();
        app.submit();
        assert!(!app.global_config_path.exists());
        assert!(app.prompt_history.is_empty());
        assert!(app.status.contains("API key") || app.status.contains("长度"));

        // Valid key: persisted, runtime relaunched onto DeepSeek, and the
        // key is recallable nowhere (history, prompts, status).
        app.input = "/apikey sk-0123456789abcdef".to_string();
        app.submit();
        let contents = std::fs::read_to_string(&app.global_config_path).unwrap();
        assert!(contents.contains("sk-0123456789abcdef"));
        assert!(
            app.prompt_history.is_empty(),
            "key must not enter Ctrl+P history"
        );
        assert!(app.backend_label.contains("DeepSeek"));
        assert!(!app.status.contains("sk-0123456789abcdef"));
        assert!(
            app.history
                .iter()
                .all(|cell| !format!("{cell:?}").contains("sk-0123456789abcdef")),
            "key must not appear in any transcript cell"
        );

        // Ordinary slash commands ARE remembered (contrast).
        app.input = "/help".to_string();
        app.submit();
        assert_eq!(app.prompt_history, vec!["/help".to_string()]);
    }

    #[test]
    fn model_command_resolves_aliases_persists_and_keeps_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new();
        app.global_config_path = dir.path().join("config.toml");
        let original_session = app.session_id.clone();
        assert!(
            original_session.is_some(),
            "test relies on persisted session"
        );

        assert!(app.handle_slash_command("/model flash"));
        assert_eq!(app.configured_model, deep_code_agent::DEEPSEEK_V4_FLASH);
        let contents = std::fs::read_to_string(&app.global_config_path).unwrap();
        assert!(contents.contains("deepseek-v4-flash"));
        assert_eq!(
            app.session_id, original_session,
            "config switch must resume the same session"
        );

        assert!(app.handle_slash_command("/model nope"));
        assert!(app.status.contains("未知模型"));
        assert!(app.status.contains("auto"));

        assert!(app.handle_slash_command("/model"));
        assert!(matches!(
            app.history.last(),
            Some(HistoryCell::System { text }) if text.contains("用法")
        ));
    }

    #[test]
    fn logout_removes_key_from_global_config() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new();
        app.global_config_path = dir.path().join("config.toml");

        assert!(app.handle_slash_command("/apikey sk-0123456789abcdef"));
        assert!(
            std::fs::read_to_string(&app.global_config_path)
                .unwrap()
                .contains("api_key")
        );

        assert!(app.handle_slash_command("/logout"));
        assert!(
            !std::fs::read_to_string(&app.global_config_path)
                .unwrap()
                .contains("api_key")
        );
    }

    #[test]
    fn slash_menu_filters_navigates_and_completes() {
        let mut app = App::new();
        app.push_char('/');
        assert!(app.completion_open(), "typing '/' opens the command menu");
        app.push_char('h');
        app.push_char('e');
        let menu = app.completion.as_ref().expect("menu open");
        assert_eq!(menu.kind, CompletionKind::Slash);
        assert_eq!(menu.items.len(), 1);
        assert_eq!(menu.items[0].0, "/help");

        let ready = app.accept_completion();
        assert!(ready, "argless command is ready to run");
        assert_eq!(app.input, "/help");
        assert!(!app.completion_open());

        // Argument-taking command completes with a trailing space, not ready.
        app.input.clear();
        app.push_char('/');
        app.push_char('r');
        app.push_char('e');
        let ready = app.accept_completion();
        assert!(!ready);
        assert_eq!(app.input, "/restore ");

        // After a space the slash menu stays closed.
        app.push_char('x');
        assert!(!app.completion_open());
    }

    #[test]
    fn file_menu_completes_trailing_at_token() {
        let mut app = App::new();
        app.workspace_files = vec![
            "Cargo.toml".to_string(),
            "src/main.rs".to_string(),
            "src/markdown.rs".to_string(),
        ];
        for ch in "see @ma".chars() {
            app.push_char(ch);
        }
        let menu = app.completion.as_ref().expect("file menu open");
        assert_eq!(menu.kind, CompletionKind::File);
        assert!(menu.items.iter().any(|(value, _)| value == "src/main.rs"));

        app.completion_down();
        app.completion_up();
        let ready = app.accept_completion();
        assert!(!ready);
        assert!(app.input.starts_with("see src/ma"), "input: {}", app.input);
        assert!(app.input.ends_with(' '));
        assert!(!app.input.contains('@'), "@ marker is stripped");

        // An email-like token does not start with '@' → no menu.
        app.input.clear();
        for ch in "mail a@b".chars() {
            app.push_char(ch);
        }
        assert!(!app.completion_open());
    }

    #[test]
    fn escape_closes_completion_before_clearing_input() {
        let mut app = App::new();
        app.push_char('/');
        app.push_char('h');
        assert!(app.completion_open());

        app.handle_escape();
        assert!(!app.completion_open(), "first Esc closes the menu");
        assert_eq!(app.input, "/h", "input preserved");

        app.handle_escape();
        assert!(app.input.is_empty(), "second Esc clears input");
        assert!(!app.should_quit);
    }

    #[tokio::test]
    async fn approval_a_key_resolves_for_session() {
        let mut app = App::new();
        app.pending_approval = Some(deep_code_agent::ApprovalRequest {
            call_id: "call_1".to_string(),
            tool_name: "mock_echo".to_string(),
            description: "echo".to_string(),
            arguments: serde_json::json!({}),
            risk_level: deep_code_agent::RiskLevel::Low,
            requires_sandbox: false,
            read_only: true,
            matched_rule: None,
        });

        app.approve_pending_tool_for_session();

        assert!(app.pending_approval.is_none());
        assert!(app.is_streaming);
        assert!(app.status.contains("approved (session)"));
    }

    #[test]
    fn multiline_paste_collapses_to_placeholder_and_expands_on_send() {
        let mut app = App::new();
        for ch in "看这个 ".chars() {
            app.push_char(ch);
        }
        app.paste_str("line1\nline2\nline3".to_string());

        // Composer shows a compact chip, not the raw content.
        assert!(app.input.contains("[粘贴 #1 +3 行]"));
        assert!(!app.input.contains("line2"));
        assert_eq!(app.pasted_blocks.len(), 1);

        // Sent form expands the chip back to the real content.
        let sent = app.expand_pasted(&app.input);
        assert!(sent.contains("line1\nline2\nline3"));
        assert!(!sent.contains("[粘贴"));
    }

    #[test]
    fn short_single_line_paste_stays_inline() {
        let mut app = App::new();
        app.paste_str("quick".to_string());
        assert_eq!(app.input, "quick");
        assert!(app.pasted_blocks.is_empty());
    }

    #[test]
    fn long_single_line_paste_collapses_with_char_count() {
        let mut app = App::new();
        app.paste_str("x".repeat(200));
        assert!(app.input.contains("[粘贴 #1 · 200 字]"));
        assert_eq!(app.pasted_blocks.len(), 1);
    }

    #[test]
    fn ctrl_w_deletes_previous_word() {
        let mut app = App::new();
        for ch in "hello world".chars() {
            app.push_char(ch);
        }
        app.delete_word_back();
        assert_eq!(app.input, "hello ");
        assert_eq!(app.input_cursor, 6);
        app.delete_word_back();
        assert_eq!(app.input, "");
    }

    #[test]
    fn ctrl_u_and_ctrl_k_kill_line_segments() {
        let mut app = App::new();
        for ch in "abcdef".chars() {
            app.push_char(ch);
        }
        app.cursor_left();
        app.cursor_left();
        app.cursor_left(); // cursor at index 3 (between c and d)
        app.kill_to_line_end();
        assert_eq!(app.input, "abc");
        app.kill_to_line_start();
        assert_eq!(app.input, "");
    }

    #[test]
    fn word_movement_jumps_between_words() {
        let mut app = App::new();
        for ch in "foo bar baz".chars() {
            app.push_char(ch);
        }
        assert_eq!(app.input_cursor, 11);
        app.word_left();
        assert_eq!(app.input_cursor, 8); // start of "baz"
        app.word_left();
        assert_eq!(app.input_cursor, 4); // start of "bar"
        app.word_right();
        assert_eq!(app.input_cursor, 7); // end of "bar"
    }

    fn snapshot(lines: &[&str]) -> TranscriptSnapshot {
        TranscriptSnapshot {
            x: 0,
            y: 0,
            width: 80,
            height: 10,
            scroll_top: 0,
            lines: lines.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn slice_by_display_cols_handles_cjk_boundaries() {
        assert_eq!(slice_by_display_cols("hello", 1, 4), "ell");
        // "你好世界" = 8 display cols; cols [2,6) spans 好世.
        assert_eq!(slice_by_display_cols("你好世界", 2, 6), "好世");
        // A CJK char straddling the boundary is kept whole.
        assert_eq!(slice_by_display_cols("你好", 1, 2), "你");
    }

    #[test]
    fn drag_selection_extracts_multiline_text() {
        let mut app = App::new();
        // text origin x = snapshot.x + 1 = 1; row 0 = line 0.
        app.set_transcript_snapshot(snapshot(&["first line", "second line", "third"]));

        // Drag from line 0 col 6 ("line") to line 1 col 7 ("second ").
        app.selection_begin(1 + 6, 0);
        app.selection_update(1 + 7, 1);
        let text = app.selection_finish().expect("non-empty selection");
        assert_eq!(text, "line\nsecond ");
    }

    #[test]
    fn plain_click_makes_no_selection() {
        let mut app = App::new();
        app.set_transcript_snapshot(snapshot(&["abc"]));
        app.selection_begin(2, 0);
        app.selection_update(2, 0); // no movement
        assert!(app.selection_finish().is_none());
        assert!(app.selection.is_none());
    }

    #[test]
    fn mouse_outside_transcript_clears_selection() {
        let mut app = App::new();
        app.set_transcript_snapshot(snapshot(&["abc"]));
        app.selection_begin(200, 200); // outside
        assert!(app.selection.is_none());
    }

    #[test]
    fn arrows_drive_draft_and_history_never_scroll() {
        let mut app = App::new();
        app.remember_prompt("earlier");

        // Single-line draft: ↑ recalls history (stashing the draft), ↓ restores
        // it; the transcript scroll offset never changes.
        for ch in "typing".chars() {
            app.push_char(ch);
        }
        app.on_up();
        assert_eq!(app.scroll_offset, 0, "↑ never scrolls");
        assert_eq!(app.input, "earlier", "single-line ↑ recalls history");
        app.on_down();
        assert_eq!(app.input, "typing", "↓ restores the stashed draft");
        assert_eq!(app.scroll_offset, 0);

        // Multi-line draft: ↑ moves the cursor; at the top line it does nothing
        // (must not clobber the draft with history).
        app.input.clear();
        app.input_cursor = 0;
        for ch in "l1\nl2".chars() {
            app.push_char(ch);
        }
        app.on_up();
        assert_eq!(app.cursor_line_col().0, 0, "↑ moved cursor to first line");
        let at_top = app.input.clone();
        app.on_up();
        assert_eq!(app.input, at_top, "top-line ↑ must not recall history");
        assert_eq!(app.scroll_offset, 0);
    }

    #[test]
    fn ctrl_c_requires_double_press_when_idle() {
        let mut app = App::new();
        app.handle_ctrl_c();
        assert!(!app.should_quit, "first Ctrl+C must not quit");
        assert!(app.ctrl_c_pending);
        assert!(app.status.contains("再按一次"));
        app.handle_ctrl_c();
        assert!(app.should_quit, "second consecutive Ctrl+C quits");
    }

    #[test]
    fn ctrl_c_clears_input_before_quitting() {
        let mut app = App::new();
        for ch in "draft".chars() {
            app.push_char(ch);
        }
        app.handle_ctrl_c();
        assert!(app.input.is_empty());
        assert!(!app.should_quit);
        assert!(!app.ctrl_c_pending, "clearing input does not arm the guard");
    }

    #[test]
    fn ctrl_c_guard_disarmed_by_other_key() {
        let mut app = App::new();
        app.handle_ctrl_c(); // arm
        assert!(app.ctrl_c_pending);
        app.clear_ctrl_c_guard(); // any other key disarms
        app.handle_ctrl_c(); // counts as a fresh first press
        assert!(!app.should_quit);
        assert!(app.ctrl_c_pending);
    }

    #[test]
    fn ctrl_c_while_streaming_does_not_quit() {
        let mut app = App::new();
        app.is_streaming = true;
        app.handle_ctrl_c();
        assert!(!app.should_quit, "Ctrl+C interrupts the turn, not the app");
    }

    #[test]
    fn streaming_activity_shows_only_while_streaming() {
        let mut app = App::new();
        assert!(
            app.streaming_activity().is_none(),
            "idle shows no indicator"
        );

        app.is_streaming = true;
        app.streaming_since = Some(std::time::Instant::now());
        let activity = app.streaming_activity().expect("indicator while streaming");
        assert!(activity.contains("生成中"));

        app.is_streaming = false;
        assert!(app.streaming_activity().is_none());
    }

    #[test]
    fn escape_ladder_clears_input_then_quits() {
        let mut app = App::new();
        app.input = "draft".to_string();

        app.handle_escape();
        assert!(app.input.is_empty());
        assert!(!app.should_quit);
        assert!(app.status.contains("已清空输入"));

        app.handle_escape();
        assert!(app.should_quit);
    }

    #[test]
    fn escape_during_streaming_requests_cancel_without_quitting() {
        let mut app = App::new();
        app.is_streaming = true;
        app.input = "keep me".to_string();

        app.handle_escape();

        assert!(!app.should_quit);
        assert_eq!(app.input, "keep me");
        assert!(app.status.contains("取消"));
    }

    #[test]
    fn error_event_flushes_partial_assistant_before_error_cell() {
        let mut app = App::new();
        let turn_id = deep_code_agent::TurnId("turn_1".to_string());
        app.apply_runtime_event(RuntimeEvent::TurnStarted {
            turn_id: turn_id.clone(),
            prompt: "hi".to_string(),
        });
        app.apply_runtime_event(RuntimeEvent::AssistantDelta {
            turn_id: turn_id.clone(),
            text: "partial".to_string(),
        });

        app.apply_runtime_event(RuntimeEvent::Error {
            turn_id: Some(turn_id),
            message: "boom".to_string(),
        });

        assert!(app.active_turn.is_none(), "active turn flushed on error");
        let assistant_index = app
            .history
            .iter()
            .position(|cell| matches!(cell, HistoryCell::Assistant { text } if text == "partial"))
            .expect("partial assistant kept in history");
        let error_index = app
            .history
            .iter()
            .position(|cell| matches!(cell, HistoryCell::System { text } if text.contains("boom")))
            .expect("error cell");
        assert!(assistant_index < error_index);
    }

    #[test]
    fn turn_cancelled_event_flushes_active_turn_and_resets_state() {
        let mut app = App::new();
        let turn_id = deep_code_agent::TurnId("turn_1".to_string());
        app.apply_runtime_event(RuntimeEvent::TurnStarted {
            turn_id: turn_id.clone(),
            prompt: "hang".to_string(),
        });
        app.apply_runtime_event(RuntimeEvent::AssistantDelta {
            turn_id: turn_id.clone(),
            text: "partial".to_string(),
        });
        app.is_streaming = true;

        app.apply_runtime_event(RuntimeEvent::TurnCancelled { turn_id });

        assert!(!app.is_streaming);
        assert!(app.status.contains("已取消"));
        assert!(app.active_turn.is_none());
        assert!(app.history.iter().any(|cell| matches!(
            cell,
            HistoryCell::Assistant { text } if text == "partial"
        )));
        assert!(app.history.iter().any(|cell| matches!(
            cell,
            HistoryCell::System { text } if text.contains("已取消")
        )));
    }

    #[test]
    fn approval_events_render_pending_and_resolved_tool_metadata() {
        let mut app = App::new();
        app.scroll_up();
        app.scroll_approval_down();
        let turn_id = deep_code_agent::TurnId("turn_1".to_string());
        let tool_call_id = deep_code_agent::ToolCallId("call_1".to_string());
        app.apply_runtime_event(RuntimeEvent::TurnStarted {
            turn_id: turn_id.clone(),
            prompt: "write something".to_string(),
        });
        app.apply_runtime_event(RuntimeEvent::ToolCallStarted {
            turn_id: turn_id.clone(),
            tool_call_id: tool_call_id.clone(),
            tool_name: "write_file".to_string(),
            arguments: serde_json::json!({ "path": "note.txt", "content": "hello" }),
        });
        app.apply_runtime_event(RuntimeEvent::ApprovalRequired {
            turn_id: Some(turn_id.clone()),
            tool_call_id: Some(tool_call_id.clone()),
            request: deep_code_agent::ApprovalRequest {
                call_id: "call_1".to_string(),
                tool_name: "write_file".to_string(),
                description: "Write note.txt".to_string(),
                arguments: serde_json::json!({ "path": "note.txt", "content": "hello" }),
                risk_level: deep_code_agent::RiskLevel::High,
                requires_sandbox: true,
                read_only: false,
                matched_rule: Some("write".to_string()),
            },
        });

        let preview = app.active_turn.as_ref().unwrap().preview_cells();
        assert!(preview.iter().any(|cell| matches!(
            cell,
            HistoryCell::ToolCall {
                risk_level: Some(risk),
                requires_sandbox: Some(true),
                approval,
                ..
            } if risk == "High" && *approval == crate::history::ToolApprovalState::Required
        )));
        // The approval is surfaced by the dedicated panel (app.pending_approval),
        // not duplicated inline in the transcript preview.
        assert!(
            !preview
                .iter()
                .any(|cell| matches!(cell, HistoryCell::Approval { .. })),
            "approval must not be duplicated in the transcript preview"
        );
        let pending = app.pending_approval.as_ref().expect("pending approval set");
        assert_eq!(pending.matched_rule.as_deref(), Some("write"));
        assert_eq!(format!("{:?}", pending.risk_level), "High");
        assert!(pending.requires_sandbox);
        assert_eq!(app.scroll_offset, 3);
        assert_eq!(app.approval_scroll_offset, 0);

        app.pending_approval = None;
        app.apply_runtime_event(RuntimeEvent::ApprovalResolved {
            turn_id: Some(turn_id.clone()),
            tool_call_id: tool_call_id.clone(),
            decision: deep_code_agent::ApprovalDecision::Approved,
        });
        let result =
            deep_code_agent::ToolResult::success("call_1", "write_file", "{\"bytes_written\":5}");
        app.apply_runtime_event(RuntimeEvent::ToolCallFinished {
            turn_id: Some(turn_id),
            tool_call_id,
            result,
        });
        assert!(app.history.iter().any(|cell| matches!(
            cell,
            HistoryCell::ToolCall { approval, .. }
                if *approval == crate::history::ToolApprovalState::Approved
        )));
    }

    #[test]
    fn diagnostics_are_flushed_after_tool_call_before_result() {
        let mut app = App::new();
        let turn_id = deep_code_agent::TurnId("turn_1".to_string());
        let tool_call_id = deep_code_agent::ToolCallId("call_1".to_string());
        app.apply_runtime_event(RuntimeEvent::TurnStarted {
            turn_id: turn_id.clone(),
            prompt: "edit file".to_string(),
        });
        app.apply_runtime_event(RuntimeEvent::ToolCallStarted {
            turn_id: turn_id.clone(),
            tool_call_id: tool_call_id.clone(),
            tool_name: "write_file".to_string(),
            arguments: serde_json::json!({ "path": "src/main.rs" }),
        });
        app.apply_runtime_event(RuntimeEvent::DiagnosticsUpdated {
            summary: "1 warning".to_string(),
            rendered: "warning: unused variable".to_string(),
        });
        assert!(
            app.history
                .iter()
                .all(|cell| !matches!(cell, HistoryCell::Diagnostics { .. }))
        );

        let result = deep_code_agent::ToolResult::success(
            "call_1",
            "write_file",
            "{\"path\":\"src/main.rs\",\"bytes_written\":10}",
        );
        app.apply_runtime_event(RuntimeEvent::ToolCallFinished {
            turn_id: Some(turn_id),
            tool_call_id,
            result,
        });

        let tool_call_index = app
            .history
            .iter()
            .position(|cell| matches!(cell, HistoryCell::ToolCall { .. }))
            .expect("tool call");
        let diagnostics_index = app
            .history
            .iter()
            .position(|cell| matches!(cell, HistoryCell::Diagnostics { .. }))
            .expect("diagnostics");
        let tool_result_index = app
            .history
            .iter()
            .position(|cell| matches!(cell, HistoryCell::ToolResult { .. }))
            .expect("tool result");

        assert!(tool_call_index < diagnostics_index);
        assert!(diagnostics_index < tool_result_index);
    }

    #[test]
    fn status_line_includes_mode_backend_session_checkpoint_and_cost() {
        let mut app = App::new();
        app.session_id = Some("session_1".to_string());
        app.last_checkpoint = Some("checkpoint_1".to_string());
        app.last_telemetry = Some(TurnTelemetry {
            route_label: "auto->deepseek-v4-flash (high)".to_string(),
            effective_model: "deepseek-v4-flash".to_string(),
            reasoning_effort: "high".to_string(),
            prompt_tokens: 100,
            completion_tokens: 20,
            cache_hit_tokens: Some(80),
            cache_miss_tokens: Some(20),
            session_cache_hit_tokens: 80,
            session_cache_miss_tokens: 20,
            session_cache_savings: deep_code_agent::CostEstimate::default(),
            prefix_status: deep_code_agent::PrefixStatus::Stable,
            route_reason: "short prompt".to_string(),
            route_source: "heuristic".to_string(),
            fallback_reason: None,
            context_window: 1_000_000,
            estimated_context_tokens: 120,
            context_usage_percent: 1,
            near_compaction_threshold: false,
            used_model_fallback: false,
            stream_retries: 0,
            turn_cost: deep_code_agent::CostEstimate {
                cny: 0.001,
                usd: 0.0001,
            },
            session_cost: deep_code_agent::CostEstimate {
                cny: 0.002,
                usd: 0.0002,
            },
        });

        let status = app.status_line();
        assert!(status.contains("ready"));
        assert!(status.contains("session session_1"));
        assert!(status.contains("checkpoint checkpoint_1"));
        assert!(status.contains("auto->deepseek-v4-flash"));
        assert!(status.contains("total ¥0.0020"));
        assert!(status.contains("ctx 1%"));
    }

    #[test]
    fn provider_text_is_fallback_without_duplicating_structured_delta() {
        let mut app = App::new();
        app.apply_runtime_event(RuntimeEvent::Provider(
            deep_code_agent::AgentEvent::TextDelta {
                text: "legacy".to_string(),
            },
        ));
        app.apply_runtime_event(RuntimeEvent::TurnFinished {
            turn_id: deep_code_agent::TurnId("turn_legacy".to_string()),
            usage: None,
            telemetry: None,
        });
        assert!(matches!(
            app.history.last(),
            Some(HistoryCell::Assistant { text }) if text == "legacy"
        ));

        let mut app = App::new();
        let turn_id = deep_code_agent::TurnId("turn_1".to_string());
        app.apply_runtime_event(RuntimeEvent::TurnStarted {
            turn_id: turn_id.clone(),
            prompt: "hi".to_string(),
        });
        app.apply_runtime_event(RuntimeEvent::AssistantDelta {
            turn_id: turn_id.clone(),
            text: "hello".to_string(),
        });
        app.apply_runtime_event(RuntimeEvent::Provider(
            deep_code_agent::AgentEvent::TextDelta {
                text: "hello".to_string(),
            },
        ));
        app.apply_runtime_event(RuntimeEvent::TurnFinished {
            turn_id,
            usage: None,
            telemetry: None,
        });
        assert!(matches!(
            app.history.last(),
            Some(HistoryCell::Assistant { text }) if text == "hello"
        ));
    }
}
