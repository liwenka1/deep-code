//! TUI application state.
//!
//! This module is intentionally thin: the agent runtime owns the model loop,
//! tool registry, session, and approval gating. The UI only has to:
//!
//! 1. forward user prompts via [`AgentRuntime::submit_user`],
//! 2. render [`RuntimeEvent`]s as they arrive,
//! 3. forward approval decisions via [`AgentRuntime::submit_approval`].

use std::sync::Arc;

use std::path::PathBuf;

use deep_code_agent::{
    AgentConfig, AgentRuntime, ApprovalDecision, ApprovalRequest, CostCurrency, JobStore,
    JsonSessionStore, LaunchedRuntime, RuntimeEvent, SessionRecord, SessionStore,
    SharedSubAgentManager, TurnTelemetry, default_config_path, launch_runtime,
};
use tokio::sync::mpsc;

use crate::active_turn::ActiveTurn;
use crate::cli::workspace_root;
use crate::history::{HistoryCell, hydrate_history};
use deep_code_agent::i18n::{Lang, TextId, tr, tr_with};

mod completion;
mod editor;
mod selection;
mod session;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Default)]
pub struct LaunchConfig {
    pub resume: Option<SessionRecord>,
    /// Workspace override; `None` means the process working directory.
    /// Tests must set this — a launched runtime persists sessions and
    /// subagent state under `<workspace>/.deep-code`.
    pub workspace: Option<PathBuf>,
    /// Extra writable roots granted on the command line (`--add-dir`).
    pub extra_roots: Vec<PathBuf>,
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
    /// Prompts typed while a turn was streaming, queued in order. Drained and
    /// sent as one combined follow-up when the turn ends (see `submit` and the
    /// stream-end handlers). Cleared on cancel — Esc/Ctrl+C means "changed my
    /// mind", so nothing queued behind the cancelled turn should fire.
    pub(crate) steering_queue: Vec<String>,
    /// Set by the turn-end handler, consumed by `drain_stream_updates` once the
    /// drain loop is finished. The flush starts a new turn, so running it inside
    /// the loop would let a later `StreamFinished` from the turn that *just*
    /// ended tear down the new turn's receiver. Deferring keeps stream start-up
    /// out of the re-entrant path entirely.
    pub(crate) pending_steering_flush: bool,
    pub pending_approval: Option<ApprovalRequest>,
    pub last_checkpoint: Option<String>,
    /// Effective `--add-dir` grants (CLI ∪ resumed record); consulted by
    /// `/restore` to say honestly that these trees were not rolled back.
    pub(crate) extra_roots: Vec<PathBuf>,
    pub session_id: Option<String>,
    /// One-shot latch for the "session save failed" transcript warning; the
    /// status line keeps warning until a save succeeds again.
    pub(crate) save_error_notified: bool,
    /// Cells dropped from the front of `history` by the scrollback cap.
    pub(crate) trimmed_cells: usize,
    /// `/find` continuation state: (query, line index of the last match in
    /// the transcript snapshot). Repeating the same query searches upward.
    pub(crate) find_state: Option<(String, usize)>,
    pub scroll_offset: usize,
    pub approval_scroll_offset: usize,
    /// Currently highlighted approval option: 0 = y (approve), 1 = a (session),
    /// 2 = n (deny). Navigated with ↑/↓, acted on with Enter.
    pub approval_focus: usize,
    pub(crate) runtime: Arc<AgentRuntime>,
    pub(crate) backend_label: String,
    pub(crate) backend_offline: bool,
    pub(crate) subagent_manager: SharedSubAgentManager,
    subagent_shutdown: Option<Box<dyn Fn() + Send + Sync>>,
    /// Background-job store for the live runtime. Held so quitting or switching
    /// sessions can kill its whole process tree (`kill_on_drop` alone only
    /// reaps the direct child, leaving grandchildren — dev servers, watchers —
    /// holding ports). `None` only before the first runtime is adopted.
    job_store: Option<JobStore>,
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
    /// Resolved once at launch; every runtime swap and store access must
    /// reuse it instead of re-deriving from the process working directory.
    pub(crate) workspace: PathBuf,
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
    /// UI language, resolved from config at launch; `/lang` switches it live.
    pub(crate) lang: Lang,
    /// Session permission mode, shared (lock-free) with the runtime's approval
    /// gate. Shift+Tab cycles it; the status line shows it.
    pub(crate) permission_mode: deep_code_agent::SharedPermissionMode,
    /// One-shot latch: a first Shift+Tab that would enter Yolo arms this and
    /// shows a confirm; the next Shift+Tab confirms. Any other key disarms.
    pub(crate) yolo_armed: bool,
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
/// Cap on prompts queued behind one streaming turn. Reaching it keeps the text
/// in the composer rather than dropping either end of the queue — the whole
/// point of steering is that nothing typed gets silently discarded.
const STEERING_QUEUE_CAP: usize = 16;
/// Scrollback cap: transcript cells beyond this are dropped from the front
/// (multi-hour sessions would otherwise grow memory without bound).
pub(crate) const MAX_HISTORY_CELLS: usize = 2000;

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
    resumed_turns: Option<usize>,
    persistent: bool,
) -> HistoryCell {
    HistoryCell::Welcome {
        version: env!("CARGO_PKG_VERSION").to_string(),
        model: model.to_string(),
        reasoning: reasoning.to_string(),
        offline,
        workspace,
        resumed_turns,
        persistent,
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

/// Localized short label for a permission mode, for the status indicator.
pub(crate) fn perm_mode_label(lang: Lang, mode: deep_code_agent::PermissionMode) -> &'static str {
    use deep_code_agent::PermissionMode;
    let id = match mode {
        PermissionMode::Default => TextId::PermModeDefault,
        PermissionMode::AcceptEdits => TextId::PermModeAcceptEdits,
        PermissionMode::Auto => TextId::PermModeAuto,
        PermissionMode::Yolo => TextId::PermModeYolo,
    };
    tr(lang, id)
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
        let workspace = config.workspace.clone().unwrap_or_else(workspace_root);
        let workspace_files = deep_code_agent::list_workspace_files(&workspace, 2000);
        let loaded = AgentConfig::load(&workspace);
        let config_warnings = loaded.report.warnings.clone();
        let agent_config = loaded.config;
        let lang = Lang::from_env(&agent_config.language);
        let cost_currency = agent_config.cost_currency;
        let configured_model = agent_config.model.clone();
        let configured_reasoning = agent_config.reasoning_effort.as_setting().to_string();
        let workspace_display = home_relative(&workspace);
        let launched = launch_runtime(
            &agent_config,
            deep_code_agent::WorkspaceRoots::new(workspace.clone(), config.extra_roots.clone()),
            config.resume.clone(),
        );
        let extra_roots = launched.extra_roots;
        let runtime = launched.handle;
        let backend_label = launched.backend_label;
        let backend_offline = launched.offline;
        let session_id = launched.session_id;
        let subagent_manager = launched.subagent_manager;
        let permission_mode = launched.permission_mode;
        let subagent_shutdown = Some(launched.stop_hook);
        let job_store = Some(launched.job_store);
        let persistent = session_id.is_some();
        let resumed_turns = config.resume.as_ref().map(|record| {
            record
                .entries
                .iter()
                .filter(|entry| matches!(entry.kind, deep_code_agent::EntryKind::User { .. }))
                .count()
        });
        let mut history = vec![welcome_cell(
            &configured_model,
            &configured_reasoning,
            backend_offline,
            workspace_display,
            resumed_turns,
            persistent,
        )];

        if !config_warnings.is_empty() {
            history.push(HistoryCell::system(format!(
                "{}\n{}",
                tr(lang, TextId::ConfigWarningsHeader),
                config_warnings.join("\n")
            )));
        }

        // Name the effective grants (CLI ∪ resumed record) once at startup:
        // an invisible write boundary is indistinguishable from a bug when a
        // path outside it is denied — or quietly accepted.
        if !extra_roots.is_empty() {
            let dirs = extra_roots
                .iter()
                .map(|root| root.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            history.push(HistoryCell::system(tr_with(
                lang,
                TextId::ExtraRootsGrantedLabel,
                &[("dirs", &dirs)],
            )));
        }

        if let Some(record) = config.resume.as_ref() {
            history.extend(hydrate_history(record));
        }

        Self {
            input_cursor: 0,
            input: String::new(),
            history,
            active_turn: None,
            // Transient note only — `status_line()` owns the mode/backend/
            // session/telemetry frame, so seeding any of that here would show
            // it twice.
            status: String::new(),
            error: None,
            should_quit: false,
            ctrl_c_pending: false,
            is_streaming: false,
            steering_queue: Vec::new(),
            pending_steering_flush: false,
            pending_approval: None,
            last_checkpoint: None,
            extra_roots,
            session_id,
            save_error_notified: false,
            trimmed_cells: 0,
            find_state: None,
            scroll_offset: 0,
            approval_scroll_offset: 0,
            approval_focus: 0,
            runtime,
            backend_label,
            backend_offline,
            subagent_manager,
            subagent_shutdown,
            job_store,
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
            workspace,
            global_config_path: default_config_path(),
            streaming_since: None,
            pasted_blocks: Vec::new(),
            transcript: None,
            selection: None,
            resume_picker: None,
            lang,
            permission_mode,
            yolo_armed: false,
        }
    }

    /// Current session permission mode (for the status indicator).
    pub(crate) fn permission_mode(&self) -> deep_code_agent::PermissionMode {
        self.permission_mode.get()
    }

    /// Shift+Tab: advance the permission mode. Entering Yolo requires a second
    /// press (armed here) so the most permissive mode is never one keystroke
    /// away by accident.
    pub(crate) fn cycle_permission_mode(&mut self) {
        use deep_code_agent::PermissionMode;
        if self.yolo_armed {
            // Second consecutive Shift+Tab: confirm Yolo.
            self.yolo_armed = false;
            self.permission_mode.set(PermissionMode::Yolo);
            self.status = self.mode_switched_status(PermissionMode::Yolo);
            return;
        }
        let next = self.permission_mode.get().cycle();
        if next == PermissionMode::Yolo {
            // Arm instead of entering; the next press confirms.
            self.yolo_armed = true;
            self.status = self.tr(TextId::PermModeYoloArm).to_string();
            return;
        }
        self.permission_mode.set(next);
        self.status = self.mode_switched_status(next);
    }

    /// Disarm the pending-Yolo confirm (any key other than Shift+Tab).
    pub(crate) fn clear_yolo_arm(&mut self) {
        self.yolo_armed = false;
    }

    fn mode_switched_status(&self, mode: deep_code_agent::PermissionMode) -> String {
        self.tr_with(
            TextId::PermModeSwitched,
            &[("mode", perm_mode_label(self.lang, mode))],
        )
    }

    /// 当前语言下的文案(无参数)。
    pub(crate) fn tr(&self, id: TextId) -> &'static str {
        tr(self.lang, id)
    }

    /// 当前语言下的文案(带 `{name}` 插值)。
    pub(crate) fn tr_with(&self, id: TextId, args: &[(&str, &str)]) -> String {
        tr_with(self.lang, id, args)
    }

    /// Live activity label shown while a turn runs: an animated spinner plus
    /// elapsed seconds, so a long wait reads as activity rather than a frozen
    /// screen. While a tool is executing, the label names the tool and shows
    /// that call's own clock instead of the generic "generating".
    #[must_use]
    pub(crate) fn streaming_activity(&self) -> Option<String> {
        if !self.is_streaming {
            return None;
        }
        let elapsed = self.streaming_since?.elapsed();
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let frame = FRAMES[(elapsed.as_millis() / 120 % FRAMES.len() as u128) as usize];
        // A running tool owns the wait: "generating 180s" while a sub-agent or
        // a build runs for minutes reads as a hang. When several tools run in
        // a batch the newest is the one whose start the user just watched;
        // finished calls leave `tools`, so this falls back by itself.
        if let Some(tool) = self.active_turn.as_ref().and_then(|turn| turn.tools.last()) {
            return Some(format!(
                "{frame} {}",
                self.tr_with(
                    TextId::StreamingToolRunning,
                    &[
                        ("tool", &tool.tool_name),
                        ("secs", &tool.started_at.elapsed().as_secs().to_string()),
                    ],
                )
            ));
        }
        Some(format!(
            "{frame} {}",
            self.tr_with(
                TextId::StreamingGenerating,
                &[("secs", &elapsed.as_secs().to_string())],
            )
        ))
    }

    /// Test constructor: launch into a process-shared temp workspace so the
    /// persisted sessions/state land in the OS temp dir instead of littering
    /// the crate directory. The static intentionally never drops — the OS
    /// reclaims its temp storage.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn new() -> Self {
        static TEST_WORKSPACE: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        let workspace = TEST_WORKSPACE
            .get_or_init(|| tempfile::tempdir().expect("test workspace"))
            .path()
            .to_path_buf();
        let mut app = Self::launch(LaunchConfig {
            workspace: Some(workspace),
            ..LaunchConfig::default()
        });
        // Tests assert Chinese copy; pin the language so the developer's or
        // CI machine's LANG can never flake them.
        app.lang = Lang::Zh;
        app
    }

    /// Drop the oldest transcript cells beyond [`MAX_HISTORY_CELLS`]. Called
    /// once per frame before rendering; keeps multi-hour sessions bounded.
    pub(crate) fn enforce_history_cap(&mut self) {
        if self.history.len() <= MAX_HISTORY_CELLS {
            return;
        }
        let excess = self.history.len() - MAX_HISTORY_CELLS;
        self.history.drain(..excess);
        self.trimmed_cells += excess;
        // Snapshot line indices shifted; restart any /find continuation.
        self.find_state = None;
    }

    pub fn submit(&mut self) {
        // An approval is a modal decision — it must be answered, not typed
        // over — so the composer stays inert until it resolves.
        if self.pending_approval.is_some() {
            return;
        }
        self.close_completion();
        // The transcript is about to grow; drop any stale selection.
        self.clear_selection();

        // During editing, the composer shows compact `[粘贴 #N …]` chips so
        // long pasted blocks don't take over the input area.  At submit time
        // both the transcript and the model receive the **expanded** content
        // so the user can see what they actually sent.
        let display = self.input.trim().to_string();
        if display.is_empty() {
            self.status = self.tr(TextId::StatusEmptyPrompt).to_string();
            return;
        }
        let sent = self.expand_pasted(&display);
        // Never let the API key into the recallable prompt history.
        if !display.starts_with("/apikey") {
            self.remember_prompt(&sent);
        }

        // Slash commands are interactive directives, not conversation — they
        // run immediately (against the live UI) rather than queueing behind a
        // stream, in both idle and streaming states.
        if display.starts_with('/') && self.handle_slash_command(&display) {
            self.clear_input();
            return;
        }

        // Steering: a plain prompt typed mid-stream is queued, not dropped.
        // It's sent as a follow-up when the turn ends — the user no longer has
        // to wait for a long turn to finish before lining up the next message.
        // Not added to the transcript here: the streaming turn's own output
        // (held in `active_turn`) hasn't been flushed to `history` yet, so a
        // user cell pushed now would render ABOVE it. The cell is added when
        // the queue flushes, after the current turn's cells land.
        if self.is_streaming {
            if self.steering_queue.len() >= STEERING_QUEUE_CAP {
                // Leave the draft in the composer — losing it is worse than
                // refusing to take more.
                self.status = self.tr_with(
                    TextId::StatusSteeringQueueFull,
                    &[("count", &self.steering_queue.len().to_string())],
                );
                return;
            }
            self.steering_queue.push(sent);
            self.clear_input();
            self.status = self.tr_with(
                TextId::StatusSteeringQueued,
                &[("count", &self.steering_queue.len().to_string())],
            );
            return;
        }

        self.clear_input();
        self.error = None;
        // A turn that streamed content but never saw its terminal event (e.g.
        // the stream channel closed mid-approval) would be silently discarded
        // here; flush it into history like `record_error` does.
        self.flush_active_turn();
        self.scroll_offset = 0;
        self.approval_scroll_offset = 0;
        self.is_streaming = true;
        self.status = self.tr_with(
            TextId::StatusStreamingFrom,
            &[("backend", &self.backend_label)],
        );

        self.history.push(HistoryCell::user(sent.clone()));

        self.start_stream(StreamRequest::User(sent));
    }

    /// Send any prompts queued (steered) while the just-finished turn was
    /// streaming, as one combined follow-up. No-op when nothing is queued.
    ///
    /// Run only from `drain_stream_updates`, after the drain loop — never from
    /// an event handler, see `pending_steering_flush`. The combined user cell is
    /// pushed here rather than at queue time because the finished turn's own
    /// cells have only just landed in `history`; pushing earlier would render
    /// the user's message above output that was still streaming.
    pub(crate) fn flush_steering_queue(&mut self) {
        if self.steering_queue.is_empty() || self.is_streaming {
            return;
        }
        // Blank-line join so multiple steered messages read as separate turns
        // to the model rather than one run-on paragraph.
        let combined = std::mem::take(&mut self.steering_queue).join("\n\n");
        self.error = None;
        self.scroll_offset = 0;
        self.approval_scroll_offset = 0;
        self.is_streaming = true;
        self.status = self.tr_with(
            TextId::StatusStreamingFrom,
            &[("backend", &self.backend_label)],
        );
        // Added now (not at queue time): the just-finished turn's cells have
        // landed in `history`, so this renders after them, in order.
        self.history.push(HistoryCell::user(combined.clone()));
        self.start_stream(StreamRequest::User(combined));
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
            self.status = self.tr(TextId::StatusInputClearedEsc).to_string();
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
            self.status = self.tr(TextId::StatusInputClearedCtrlC).to_string();
        } else if self.ctrl_c_pending {
            self.should_quit = true;
        } else {
            self.ctrl_c_pending = true;
            self.status = self.tr(TextId::StatusCtrlCQuitConfirm).to_string();
        }
    }

    /// Any non-Ctrl+C key disarms the quit guard.
    pub fn clear_ctrl_c_guard(&mut self) {
        self.ctrl_c_pending = false;
    }

    fn cancel_streaming_turn(&mut self) {
        self.status = self.tr(TextId::StatusCancelling).to_string();
        // Cancel means "changed my mind", so drop the queue here and now rather
        // than waiting for `TurnCancelled` to do it: if the turn had already
        // finished and its `TurnFinished` is still sitting unread in the channel,
        // `cancel_turn` is a no-op on the idle runtime, no `TurnCancelled` ever
        // arrives, and the queue would be auto-sent despite the cancel.
        self.steering_queue.clear();
        self.pending_steering_flush = false;
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

        // Only put `rx` back when nothing claimed the slot while we were
        // draining. A successor's receiver must never be overwritten: the turn
        // behind it would keep running with nobody observing — tools execute,
        // files change, cost accrues, and an approval request parks forever with
        // no UI to answer it.
        if self.is_streaming && self.ui_rx.is_none() {
            self.ui_rx = Some(rx);
        }

        // Now that the loop is done (and any trailing `StreamFinished` from the
        // finished turn has been applied), it is safe to start the follow-up.
        if std::mem::take(&mut self.pending_steering_flush) {
            self.flush_steering_queue();
            applied = true;
        }
        applied
    }

    fn resolve_pending_tool(&mut self, decision: ApprovalDecision) {
        if self.pending_approval.take().is_none() {
            return;
        }

        let label = match decision {
            ApprovalDecision::Approved => self.tr(TextId::DecisionApproved),
            ApprovalDecision::ApprovedForSession => self.tr(TextId::DecisionApprovedSession),
            ApprovalDecision::Denied => self.tr(TextId::DecisionDenied),
        };
        self.approval_scroll_offset = 0;
        self.status = self.tr_with(TextId::StatusToolResolved, &[("decision", label)]);
        self.is_streaming = true;
        self.start_stream(StreamRequest::Approval(decision));
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
                // Reaching here with `is_streaming` still set means the channel
                // closed without any terminal event (runtime panic, or an
                // approval submitted against an already-cancelled turn): every
                // terminal handler clears the flag itself. There is no finished
                // turn for a follow-up to attach to, so drop the queue instead
                // of firing it at whatever unrelated turn comes next.
                if self.is_streaming {
                    self.steering_queue.clear();
                    self.pending_steering_flush = false;
                }
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
        self.status = self.tr(TextId::StatusAgentError).to_string();
        self.history.push(HistoryCell::system(format!(
            "{}{message}",
            self.tr(TextId::ErrorPrefix)
        )));
        self.is_streaming = false;
        // A failed turn is not a clean hand-off: don't auto-fire queued
        // prompts into a broken state (an API-key error would just re-error
        // each). They stay in the transcript / prompt history to resend.
        self.steering_queue.clear();
        self.clear_stream_receiver();
    }

    pub(crate) fn clear_stream_receiver(&mut self) {
        self.ui_rx = None;
    }

    #[must_use]
    pub fn status_line(&self) -> String {
        // CC-style minimal idle frame. The permission-mode chip is rendered
        // separately by `render_status` (always visible), and streaming/error
        // states have their own branches there — so this shows just the model
        // in use, context headroom, and any transient note. Session id and
        // cost detail live in `/status`, not on the always-on bar.
        //
        // The model is the *effective* one (auto-routing or a cascade can make
        // it differ from configured), so a switch to a pricier model is visible
        // here rather than only under `/status`. Falls back to the configured
        // model before the first turn produces telemetry.
        let model = self
            .last_telemetry
            .as_ref()
            .map_or(self.configured_model.as_str(), |value| {
                value.effective_model.as_str()
            });
        let ctx = self
            .last_telemetry
            .as_ref()
            .map(|value| format!("  ctx {}%", value.context_usage_percent))
            .unwrap_or_default();
        // `self.status` carries only the transient note (tool progress, command
        // feedback, the post-turn `/restore {id}` rollback hint).
        let note = if self.status.is_empty() {
            String::new()
        } else {
            format!("  {}", self.status)
        };
        format!("{model}{ctx}{note}")
    }

    pub async fn shutdown_runtime(&self) {
        if let Some(shutdown) = &self.subagent_shutdown {
            shutdown();
        }
        // Kill background jobs (and their whole process tree) on quit — the
        // TUI never routed through `LaunchedRuntime::shutdown`, so without this
        // grandchildren survived until the process exited.
        if let Some(job_store) = &self.job_store {
            job_store.shutdown();
        }
        self.runtime.shutdown().await;
    }
}
