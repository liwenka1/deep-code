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

mod approval;
mod completion;
mod editor;
mod selection;
mod session;
mod stream;

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
    /// 2 = n (deny). Navigated with ↑/↓, acted on with Enter. A root grant
    /// offers only y/n and starts focused on deny — see
    /// [`Self::park_approval`].
    pub approval_focus: usize,
    /// Whether the pending panel has been drawn at least once.
    ///
    /// The run loop applies runtime events, may skip the frame (there is a
    /// minimum interval between draws while a turn streams), and then reads
    /// whatever key is already queued. A key typed *before* the approval
    /// existed would otherwise be dispatched against it: a `y` meant as the
    /// first letter of a steering message resolved a boundary prompt the user
    /// had never seen. Decision keys are ignored until the panel has actually
    /// been on screen for a frame; scrolling and Ctrl-C are not gated.
    pub(crate) approval_armed: bool,
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
    /// Startup-degradation warnings from the most recent runtime swap, parked
    /// by `adopt_runtime` until its caller has finished rebuilding the
    /// transcript. See `App::flush_launch_warnings`.
    pub(crate) pending_launch_warnings: Vec<String>,
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

/// Localized short label for a permission mode, for the status indicator. The
/// mode→key mapping lives on the enum (`PermissionMode::text_id`); the accent
/// colour stays in `render_status`, which owns ratatui.
pub(crate) fn perm_mode_label(lang: Lang, mode: deep_code_agent::PermissionMode) -> &'static str {
    tr(lang, mode.text_id())
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
        // Exhaustive destructuring, not field-by-field moves: the previous
        // shape read nine fields and silently ignored `warnings`, and nothing
        // — not the compiler, not review — pointed at the gap. Naming every
        // field means a new one must be given a home here, and dropping one
        // has to be spelled `field: _` on purpose.
        let deep_code_agent::LaunchedRuntime {
            handle: runtime,
            backend_label,
            session_id,
            subagent_manager,
            job_store,
            stop_hook,
            offline: backend_offline,
            warnings: launch_warnings,
            permission_mode,
            extra_roots,
        } = launched;
        let subagent_shutdown = Some(stop_hook);
        let job_store = Some(job_store);
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

        // Hydrate FIRST, then warn. `switch_session` already orders it this way
        // ("after the rebuild, so the clear above cannot erase them"); this
        // path did the opposite, so on `-c`/`-r` the warnings were pushed above
        // a whole restored transcript. The viewport is bottom-anchored, so on
        // any session longer than a screen the degradation notice started off
        // screen — the same "written somewhere nobody looks" failure the
        // staging fix exists to prevent, on the more common resume entry point.
        if let Some(record) = config.resume.as_ref() {
            history.extend(hydrate_history(record));
        }

        // Config warnings obey the same ordering, and for the same reason: they
        // were pushed BEFORE the hydrate, so on `-c`/`-r` into a session longer
        // than a screen `CfgGlobalKeyPerms` ("the global config holding your
        // plaintext API key is group/world readable") opened off screen. The
        // reorder that fixed the launch warnings left this block, twenty lines
        // above it, behind.
        if !config_warnings.is_empty() {
            history.push(HistoryCell::system(format!(
                "{}\n{}",
                tr(lang, TextId::ConfigWarningsHeader),
                config_warnings.join("\n")
            )));
        }

        // `LaunchedRuntime::warnings` documents "the consumer must surface
        // these" — the library cannot write to stderr because raw mode owns
        // the screen. Only `adopt_runtime` (the runtime-SWAP path: /clear,
        // /resume, /model, /add-dir) was ever wired, so at startup — the one
        // moment a dead `auto_allow` entry, a disabled tool group, an
        // unavailable session store or a checkpoint failure is actually
        // decided — every warning was dropped on the floor. Same rendering as
        // the swap path, so a warning reads identically whenever it arrives.
        for warning in &launch_warnings {
            history.push(HistoryCell::system(tr_with(
                lang,
                TextId::SystemWarning,
                &[("message", warning)],
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
            approval_armed: false,
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
            pending_launch_warnings: Vec::new(),
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

    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(3);
    }

    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(3);
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
