use deep_code_agent::{EntryKind, SessionRecord, ToolResultStatus};

use deep_code_agent::i18n::{Lang, TextId, tr, tr_with};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolApprovalState {
    NotRequired,
    Required,
    Approved,
    Denied,
    /// The call was recorded before the session format carried an approval, so
    /// this transcript does not know whether the gate ever asked.
    ///
    /// Distinct from [`Self::NotRequired`] on purpose: "never asked" is a fact
    /// that lets a call fold, while "unknown" is not — a resumed transcript must
    /// not hide a call a human may have authorised. It carries no badge, because
    /// a badge would be a claim this transcript cannot make.
    Unknown,
}

impl ToolApprovalState {
    #[must_use]
    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::NotRequired | Self::Unknown => "",
            Self::Required => tr(lang, TextId::BadgeRequired),
            Self::Approved => tr(lang, TextId::BadgeApproved),
            Self::Denied => tr(lang, TextId::BadgeDenied),
        }
    }
}

/// Welcome 卡的会话概要行:恢复(带轮数)/新会话(是否持久化)。
/// 供 `ui::cell_lines` 的 Welcome 渲染使用。
#[must_use]
pub(crate) fn session_summary(
    lang: Lang,
    resumed_turns: Option<usize>,
    persistent: bool,
) -> String {
    match resumed_turns {
        Some(turns) => tr_with(
            lang,
            TextId::SessionResumed,
            &[("turns", &turns.to_string())],
        ),
        None if persistent => tr(lang, TextId::SessionNewPersistent).to_string(),
        None => tr(lang, TextId::SessionNewEphemeral).to_string(),
    }
}

/// One completed call inside a [`HistoryCell::ToolBatch`].
///
/// Deliberately carries no approval state and no result status: a batch only
/// ever admits calls that were neither gated by a human nor failed, so both
/// would be constants. `folded_entry` is the only constructor, which is what
/// keeps that true — relax the policy there and this struct grows the field it
/// then needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolBatchEntry {
    pub tool_name: String,
    pub arguments: String,
    pub summary: String,
}

/// Whether a call that is *still running* draws no row of its own.
///
/// The same test as [`folded_entry`] minus the outcome, which is not known yet:
/// a call that turns out to have failed shows its args — and its failure — the
/// moment it does, because it is then no longer quiet.
///
/// Used by the live preview, which is why it must stay separate from the
/// outcome-dependent half: showing a command and then condensing it away is the
/// flicker this whole policy exists to avoid, so a quiet call never gets a row
/// *to* flicker.
#[must_use]
pub(crate) fn quiet_while_running(tool_name: &str, approval: ToolApprovalState) -> bool {
    approval == ToolApprovalState::NotRequired && tool_log_label(tool_name).is_some()
}

/// The folded-batch entry a finished call contributes, or `None` when the call
/// must stay visible in full.
///
/// This is the entire "what may be hidden from the transcript" policy, and it
/// is deliberately a whitelist:
///
/// * **only a success** — a failure is the thing a reader most needs to see,
///   and it is never folded away;
/// * **only an un-gated call** — a call a human answered is that human's
///   decision, part of the record of what was authorised, not noise;
/// * **only a tool with a verb of its own** — see [`tool_log_label`], which
///   excludes everything that writes or that a user watches.
///
/// A call that fails the test is not merely left unfolded: it is pushed as an
/// ordinary cell, which ends the run it interrupted, so the calls before and
/// after it fold into two honest nodes instead of one that would have to
/// mention a call it is hiding.
#[must_use]
pub(crate) fn folded_entry(
    tool_name: &str,
    arguments: &str,
    approval: ToolApprovalState,
    status: &ToolResultStatus,
    summary: &str,
) -> Option<ToolBatchEntry> {
    (quiet_while_running(tool_name, approval) && *status == ToolResultStatus::Success).then(|| {
        ToolBatchEntry {
            tool_name: tool_name.to_string(),
            arguments: arguments.to_string(),
            summary: summary.to_string(),
        }
    })
}

/// Tools the policy engine can never ask a human about.
///
/// Only consulted for exchanges whose record predates
/// `ExchangeResult::asked` (in the agent crate): for those, `Unknown` is the
/// honest reading, and this is what keeps a resumed old session from either
/// hiding a call a human answered or badging one it cannot vouch for. Newer
/// records answer from the record itself and never reach this list.
///
/// These three are the ones `execution_policy::evaluate_tool` hardcodes
/// `requires_approval: false` for, and the `Tool` trait's default is `false`
/// with overrides only on the editing and dispatch tools — so no rule, mode or
/// standing consent can ever put one of them in front of a human. Everything
/// else in [`tool_log_label`] (`shell`, `web_search`, `fetch_url`) can be, and so
/// never folds on an unknown record: the only direction that cannot hide a
/// decision.
///
/// A whitelist, deliberately: a tool nobody listed simply does not fold, so a
/// stale list costs a noisier transcript rather than a hidden approval.
#[must_use]
fn never_gated(tool_name: &str) -> bool {
    matches!(tool_name, "read_file" | "list_dir" | "grep_files")
}

/// The one-line verb a folded batch counts a tool by, or `None` for a tool that
/// must stay visible.
///
/// Narrower than the tool registry on purpose. `job` and `agent` are
/// long-lived things a reader watches rather than noise; `write_file`,
/// `apply_patch` and `request_write_root` change the workspace, and the
/// transcript is the audit trail for exactly that.
///
/// A tool that is not listed — **including one added to the registry later** —
/// has no verb, so it cannot fold and stays visible. The failure mode of
/// forgetting to update this list is a noisier transcript, never a silent one.
///
/// Listing `shell` here is narrower than it looks, and the direction is worth
/// stating because the opposite is easy to assume: **nothing about the gate
/// makes a call visible.**
///
/// A standing consent — `auto_allow`, a session-remembered command (`a`),
/// AcceptEdits, Auto, Yolo — resolves the gate *before* anything is put in front
/// of a human: the runtime emits `ApprovalResolved` and never `ApprovalRequired`
/// (see `auto_approval_granted` and its caller), and both the UI's badge and the
/// call's visibility are driven by the latter. So under those modes every
/// successful `shell` folds, and draws nothing at all while it runs — the normal
/// case there, not an exception.
///
/// That is the intended reading, and it is what `ExchangeResult::asked` records
/// as `false`, so a resumed transcript agrees: the consent was the human's
/// decision, taken earlier and once ("stop asking me"), and re-showing every
/// command it covers is the noise those modes exist to remove.
///
/// In an ordinary session this list removes reads and searches. Under Yolo it
/// removes most of the transcript.
#[must_use]
pub(crate) fn tool_log_label(tool_name: &str) -> Option<TextId> {
    Some(match tool_name {
        "read_file" => TextId::ToolLogReadFiles,
        "list_dir" => TextId::ToolLogListDirs,
        "grep_files" => TextId::ToolLogSearches,
        "shell" => TextId::ToolLogCommands,
        "web_search" => TextId::ToolLogWebSearches,
        "fetch_url" => TextId::ToolLogFetches,
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryCell {
    /// The startup header: a compact, styled welcome card (rendered specially
    /// in `cell_lines`). It scrolls away naturally after the first message.
    /// Holds raw data, not preformatted text, so a `/lang` switch re-renders
    /// it in the new language on the next frame.
    Welcome {
        version: String,
        /// Raw model id, e.g. "deepseek-flash".
        model: String,
        /// Raw reasoning setting, e.g. "medium".
        reasoning: String,
        offline: bool,
        /// Home-relative workspace path, left-truncated at render time.
        workspace: String,
        /// `Some(turns)` when resumed; `None` for a fresh session.
        resumed_turns: Option<usize>,
        /// Whether the fresh session is persisted (ignored when resumed).
        persistent: bool,
    },
    System {
        text: String,
    },
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    /// 模型思考过程。默认折叠成一行 header:流式时它有用,写完就基本没用,而
    /// 全文会把真正的回答顶出屏幕。
    ///
    /// `expanded` 是用户对这一块的独立选择(点击 header 切换),它放在 **cell
    /// 里**而不是旁边一张索引表,因为 `enforce_history_cap` 会从头部 `drain`
    /// 掉旧 cell —— 任何按下标记录的展开状态一裁剪就指向了别的 cell。放在 cell
    /// 里还顺带让 `CachedCellLines::matches`(比较 cell 全值)自动失效缓存。
    Reasoning {
        text: String,
        expanded: bool,
    },
    ToolCall {
        tool_name: String,
        arguments: String,
        approval: ToolApprovalState,
        /// Seconds this call has been running — `Some` only in the live
        /// transcript preview (re-computed each frame), `None` once flushed.
        /// Distinguishes several parallel `agent` calls that would otherwise
        /// all look identically frozen.
        running_for_secs: Option<u64>,
    },
    /// The result line renders status and summary only; the tool's name is on
    /// the `ToolCall` cell directly above it, so it is not carried twice.
    ToolResult {
        status: ToolResultStatus,
        summary: String,
    },
    /// Live output tail of a still-running tool (streaming shell). Preview
    /// only: it is never flushed into the persistent transcript — the final
    /// ToolResult summary replaces it.
    ToolStream {
        text: String,
    },
    /// A run of quiet, successful calls, folded to one summary row.
    ///
    /// A batch is "a maximal run of tool calls between two pieces of prose",
    /// and the transcript already yields that for free: the merge rule is
    /// *`history`'s last cell is a `ToolBatch`*, so anything else pushed in
    /// between — prose, reasoning, a diagnostic, another tool's call, or a call
    /// that did not qualify — closes the run by construction. No boundaries are
    /// recorded anywhere, and none can drift from the transcript's real order.
    ///
    /// Every entry is a complete call+result pair decided together at
    /// `ToolCallFinished` (the result is in hand at that point), so there is no
    /// half-known entry and no call-id pairing to get wrong. And because only
    /// qualifying calls ever get in, the entries are all quiet successes by
    /// construction — see [`folded_entry`] for what qualifies, which is where
    /// the whole "what may be hidden" policy lives.
    ToolBatch {
        entries: Vec<ToolBatchEntry>,
        /// The reader's choice, per batch — the same rule as `Reasoning`, and
        /// for the same two reasons: it survives the scrollback cap (which
        /// drops cells from the front and would slide any index-keyed state),
        /// and it invalidates the render memo by simply being part of the cell.
        expanded: bool,
    },
    Diagnostics {
        summary: String,
        rendered: String,
    },
    Checkpoint {
        id: String,
        label: String,
    },
    /// A compaction summary, as rendered.
    ///
    /// Typed rather than a preformatted `metadata: String`, which is exactly how
    /// this cell came to print a raw `archived=2` into a Chinese UI — no unit,
    /// and nothing the renderer could localize.
    Compaction {
        /// SessionEntries folded into the summary. Not messages: one assistant
        /// entry carries a whole tool batch, so the two counts diverge precisely
        /// on the sessions where compaction fires.
        archived_entries: usize,
        /// Estimated context tokens either side of the fold. `None` when the
        /// compaction did not measure itself (a cell replayed from a session
        /// record cannot — the record stores no token counts).
        context_tokens: Option<(u32, u32)>,
        summary: String,
    },
}

impl HistoryCell {
    #[must_use]
    pub fn system(text: impl Into<String>) -> Self {
        Self::System { text: text.into() }
    }

    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self::User { text: text.into() }
    }

    #[must_use]
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::Assistant { text: text.into() }
    }

    #[must_use]
    pub fn lines(&self, lang: Lang) -> Vec<String> {
        match self {
            // Welcome renders exclusively through `ui::cell_lines` (it needs
            // the styled header/rule/intro), so its plain-text form is never
            // requested — no duplicate formatting kept here.
            Self::Welcome { .. } => Vec::new(),
            Self::System { text } | Self::User { text } | Self::Assistant { text } => {
                vec![text.clone()]
            }
            // `/copy` hands over what the model actually produced — the whole
            // reasoning, not the one-line header the transcript happens to be
            // showing. Folding is a display choice, not a data one.
            Self::Reasoning { text, .. } => vec![text.clone()],
            // Compact single line: detailed risk/sandbox/rule live in the
            // approval panel; here we only show name + args, plus an approval
            // badge when the call was actually gated.
            Self::ToolCall {
                tool_name,
                arguments,
                approval,
                running_for_secs,
                ..
            } => {
                let args = truncate_chars(&collapse_whitespace(arguments), 72);
                let badge = match approval {
                    // `Unknown` carries no badge: this transcript does not know
                    // whether the gate ever asked, and a badge is a claim.
                    ToolApprovalState::NotRequired | ToolApprovalState::Unknown => String::new(),
                    other => format!(" [{}]", other.label(lang)),
                };
                let clock = running_for_secs
                    .map(|secs| format!(" · {secs}s"))
                    .unwrap_or_default();
                vec![format!("{tool_name}  {args}{clock}{badge}")]
            }
            Self::ToolResult {
                status, summary, ..
            } => {
                vec![format!(
                    "{} {}",
                    tool_result_word(status),
                    truncate_chars(&collapse_whitespace(summary), 88)
                )]
            }
            Self::ToolStream { text } => text.lines().map(str::to_string).collect(),
            // `/copy` gets what the model actually ran, not the folded summary
            // — the same rule as reasoning. The entries are rendered through
            // the standalone variants' own formatters so the two can never
            // describe one call two ways.
            Self::ToolBatch { entries, .. } => entries
                .iter()
                .flat_map(|entry| {
                    let call = Self::ToolCall {
                        tool_name: entry.tool_name.clone(),
                        arguments: entry.arguments.clone(),
                        approval: ToolApprovalState::NotRequired,
                        running_for_secs: None,
                    };
                    let result = Self::ToolResult {
                        status: ToolResultStatus::Success,
                        summary: entry.summary.clone(),
                    };
                    call.lines(lang).into_iter().chain(result.lines(lang))
                })
                .collect(),
            Self::Diagnostics { summary, rendered } => {
                if rendered.is_empty() {
                    vec![summary.clone()]
                } else {
                    vec![summary.clone(), truncate_chars(rendered, 600)]
                }
            }
            Self::Checkpoint { id, label } => vec![
                tr_with(lang, TextId::CheckpointLabel, &[("label", label)]),
                format!("ID: {id}"),
                tr_with(lang, TextId::CheckpointRestoreHint, &[("id", id)]),
            ],
            Self::Compaction {
                archived_entries,
                context_tokens,
                summary,
            } => {
                let count = archived_entries.to_string();
                let title = match context_tokens {
                    Some((before, after)) => tr_with(
                        lang,
                        TextId::CompactionTitleMeasured,
                        &[
                            ("count", &count),
                            ("before", &before.to_string()),
                            ("after", &after.to_string()),
                        ],
                    ),
                    None => tr_with(lang, TextId::CompactionTitle, &[("count", &count)]),
                };
                // The middle line is the point of the whole cell. The transcript
                // above still shows the originals, so without it this block reads
                // as "the conversation was rewritten into the text below" — which
                // is not what happened. What happened is that the model stopped
                // seeing them, and only this line says so.
                vec![
                    title,
                    tr(lang, TextId::CompactionWhatModelSees).to_string(),
                    summary.clone(),
                ]
            }
        }
    }
}

/// Rebuild the transcript of a resumed session.
///
/// Turns are identified by their user entry, and each one closes with the
/// checkpoints taken inside it. The subtlety is which `record.turns` entry a
/// user entry belongs to: `turns` is append-only for the life of the session
/// while `entries` is TRIMMED by compaction, so the two line up at their END,
/// never at their start. Counting user entries from the front — as this used to
/// — made a compacted session render the session's OLDEST checkpoints against
/// its NEWEST turns. The ids printed were the ones the snapshot cap had long
/// since pruned from disk, each carrying a `/restore <id>` hint that either
/// failed outright or, while the snapshot still existed, rewound the workspace
/// to the start of the session instead of to the turn it was printed beside.
/// Anchoring at the end is right whether or not a compaction ever happened.
pub(crate) fn hydrate_history(record: &SessionRecord) -> Vec<HistoryCell> {
    let user_entries = record
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, EntryKind::User { .. }))
        .count();
    // Saturating: a session interrupted mid-turn has one more user entry than
    // it has finished turn records, and that trailing turn simply has no record
    // to key off (`append_turn_checkpoints` returns early for it).
    let turn_offset = record.turns.len().saturating_sub(user_entries);

    let mut cells = Vec::new();
    let mut current_turn: Vec<HistoryCell> = Vec::new();
    // Which turn (0-based among the user entries present) is open, if any.
    let mut open_turn: Option<usize> = None;
    let mut next_turn = 0usize;

    for entry in &record.entries {
        match &entry.kind {
            EntryKind::User { content, .. } => {
                // Close the turn this entry ends. Cells accumulated before the
                // FIRST user entry — a compaction banner heading the retained
                // tail — belong to no turn and are emitted without checkpoints.
                cells.append(&mut current_turn);
                if let Some(index) = open_turn {
                    append_turn_checkpoints(&mut cells, record, turn_offset + index);
                }
                open_turn = Some(next_turn);
                next_turn += 1;
                current_turn.push(HistoryCell::user(content.clone()));
            }
            EntryKind::System { .. } => {}
            EntryKind::Assistant {
                content,
                reasoning,
                exchanges,
            } => {
                if let Some(reasoning) = reasoning.as_ref().filter(|text| !text.is_empty()) {
                    // A resumed session starts folded, like a live one: the
                    // expansion is a reading choice, not part of the record.
                    current_turn.push(HistoryCell::Reasoning {
                        text: reasoning.clone(),
                        expanded: false,
                    });
                }
                if !content.is_empty() {
                    current_turn.push(HistoryCell::assistant(content.clone()));
                }
                for exchange in exchanges {
                    // Resume rebuilds the same batches the live path builds,
                    // from the same predicate: one assistant entry's exchanges
                    // already ARE one batch, so this needs no segmentation of
                    // its own — a non-qualifying exchange simply breaks the run
                    // by being pushed between the qualifying ones.
                    let summary = exchange
                        .result
                        .as_ref()
                        .map(|result| summarize_tool_result(&result.content));
                    // The approval the record kept, mapped back onto what the
                    // live view showed. `asked` is that predicate exactly — the
                    // live badge appears iff the request was put in front of a
                    // human — so this reproduces the session instead of
                    // approximating it.
                    let approval = match &exchange.result {
                        // `asked == Some(true)`: a human answered, and the
                        // status carries which way (a refusal is the one
                        // outcome it records).
                        Some(result) if result.asked == Some(true) => {
                            if result.status == ToolResultStatus::Denied {
                                ToolApprovalState::Denied
                            } else {
                                ToolApprovalState::Approved
                            }
                        }
                        // Nobody was asked: the policy never raised the gate, a
                        // standing consent resolved it, it was refused outright
                        // (a hard `PolicyVerdict::Deny` never reaches a human —
                        // so no badge, exactly as live), or the wait was
                        // cancelled. All quiet, all folded like live.
                        Some(result) if result.asked == Some(false) => {
                            ToolApprovalState::NotRequired
                        }
                        // Recorded before the field existed: unknown, and
                        // unknown is NOT "nobody asked", so only what the policy
                        // can never ask about may still fold (see `never_gated`).
                        Some(_) if never_gated(&exchange.call.function.name) => {
                            ToolApprovalState::NotRequired
                        }
                        _ => ToolApprovalState::Unknown,
                    };
                    let folded = match (&exchange.result, &summary) {
                        (Some(result), Some(summary)) => folded_entry(
                            &exchange.call.function.name,
                            &exchange.call.function.arguments,
                            approval,
                            &result.status,
                            summary,
                        ),
                        _ => None,
                    };
                    if let Some(entry) = folded {
                        match current_turn.last_mut() {
                            Some(HistoryCell::ToolBatch { entries, .. }) => entries.push(entry),
                            _ => current_turn.push(HistoryCell::ToolBatch {
                                entries: vec![entry],
                                expanded: false,
                            }),
                        }
                        continue;
                    }
                    current_turn.push(HistoryCell::ToolCall {
                        tool_name: exchange.call.function.name.clone(),
                        arguments: exchange.call.function.arguments.clone(),
                        approval,
                        running_for_secs: None,
                    });
                    // Pending exchanges (interrupted before a result) render
                    // the call only — no fabricated result line.
                    if let (Some(result), Some(summary)) = (&exchange.result, summary) {
                        current_turn.push(HistoryCell::ToolResult {
                            status: result.status,
                            summary,
                        });
                    }
                }
            }
            EntryKind::Compaction {
                summary,
                archived_count,
            } => {
                current_turn.push(HistoryCell::Compaction {
                    archived_entries: *archived_count,
                    // The record carries no token counts, so a replayed cell can
                    // say what was folded but not what it bought.
                    context_tokens: None,
                    summary: summary.clone(),
                });
            }
        }
    }
    cells.append(&mut current_turn);
    if let Some(index) = open_turn {
        append_turn_checkpoints(&mut cells, record, turn_offset + index);
    }

    cells
}

fn append_turn_checkpoints(
    cells: &mut Vec<HistoryCell>,
    record: &SessionRecord,
    turn_index: usize,
) {
    let Some(turn) = record.turns.get(turn_index) else {
        return;
    };
    let window_end = record
        .turns
        .get(turn_index + 1)
        .map_or(u64::MAX, |next| next.started_at_ms);
    for checkpoint in &record.checkpoints {
        if checkpoint.created_at_ms >= turn.started_at_ms && checkpoint.created_at_ms < window_end {
            cells.push(HistoryCell::Checkpoint {
                id: checkpoint.id.0.clone(),
                label: checkpoint.label.clone(),
            });
        }
    }
}

pub(crate) fn summarize_tool_result(content: &str) -> String {
    const MAX_CHARS: usize = 300;

    if content.contains("<diagnostics file=")
        && let Some(block_start) = content.find("<diagnostics file=")
    {
        let prefix = content[..block_start].trim();
        let diagnostics = &content[block_start..];
        let diag_summary = diagnostics
            .lines()
            .find(|line| line.starts_with("  ERROR") || line.starts_with("  WARNING"))
            .map(|line| line.trim().to_string())
            .unwrap_or_else(|| "diagnostics attached".to_string());
        if prefix.is_empty() {
            return truncate_chars(&diag_summary, MAX_CHARS);
        }
        return truncate_chars(&format!("{prefix} | {diag_summary}"), MAX_CHARS);
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(content)
        && let Some(summary) = summarize_json_tool_result(&value)
    {
        return summary;
    }

    truncate_chars(&collapse_whitespace(content), MAX_CHARS)
}

fn summarize_json_tool_result(value: &serde_json::Value) -> Option<String> {
    let object = value.as_object()?;
    // Empty counts as absent: a grep of the workspace root has its root prefix
    // stripped to "", and `unwrap_or` only catches a missing key, so the line
    // opened with a bare colon.
    let path = object
        .get("path")
        .and_then(serde_json::Value::as_str)
        .filter(|path| !path.is_empty())
        .unwrap_or(".");

    if let Some(entries) = object.get("entries").and_then(serde_json::Value::as_array) {
        return Some(format!("{path}: {} entries", entries.len()));
    }

    if let Some(lines) = object.get("lines").and_then(serde_json::Value::as_array) {
        let total_lines = object
            .get("total_lines")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(lines.len() as u64);
        let truncated = object
            .get("truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        return Some(format!(
            "{path}: {} lines shown of {total_lines} (truncated={truncated})",
            lines.len()
        ));
    }

    if let Some(matches) = object.get("matches").and_then(serde_json::Value::as_array) {
        let files_searched = object
            .get("files_searched")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let truncated = object
            .get("truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        // The refusal ledgers ride along or the human is lied to: grep counts
        // files it refused to search, and a summary reading "0 matches across
        // 5 files" while three were skipped is exactly the "searched
        // everything, found nothing" misread the counts exist to prevent —
        // surfaced to the model in the JSON, so the person watching the panel
        // deserves the same honesty.
        //
        // ALL of them. This list has to track the tool's ledgers or a newly
        // split-out bucket silently stops reaching the line — which is what
        // splitting binary/symlink out of "unreadable" would otherwise have
        // done, quietly shrinking the number the human is shown.
        //
        // Broken out by cause rather than summed. The model is told which
        // ledger each refusal landed in; collapsing them back into one integer
        // for the human re-merged the exact distinction the split was for, and
        // now that boundary refusals are counted too, one number mixes "grep
        // could not read it" with "the boundary said no" — different problems
        // with different fixes.
        let causes = [
            ("oversized", "skipped_oversized"),
            ("binary", "skipped_binary"),
            ("symlinks", "skipped_symlinks"),
            ("unreadable", "skipped_unreadable"),
        ]
        .iter()
        .filter_map(|(label, key)| {
            let count = object.get(*key).and_then(serde_json::Value::as_u64)?;
            (count > 0).then(|| format!("{label}={count}"))
        })
        .collect::<Vec<_>>();
        // The tool's own note carries the "at least" hedge whenever the walk
        // did not finish; without it the human reads a floor as a census.
        let floor = object
            .get("note")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|note| note.contains("at least"));
        let skipped = if causes.is_empty() {
            String::new()
        } else {
            format!(
                ", skipped {}{}",
                causes.join(" "),
                if floor { " (at least)" } else { "" }
            )
        };
        return Some(format!(
            "{path}: {} matches across {files_searched} files (truncated={truncated}{skipped})",
            matches.len()
        ));
    }

    if let Some(bytes_written) = object
        .get("bytes_written")
        .and_then(serde_json::Value::as_u64)
    {
        return Some(format!("{path}: wrote {bytes_written} bytes"));
    }

    if let Some(replacements) = object
        .get("replacements")
        .and_then(serde_json::Value::as_u64)
    {
        return Some(format!("{path}: {replacements} replacements"));
    }

    if let Some(command) = object.get("command").and_then(serde_json::Value::as_str) {
        let status = object
            .get("status")
            .or_else(|| object.get("tool_status"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let cwd = object
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(".");
        if let Some(job_id) = object.get("job_id").and_then(serde_json::Value::as_str) {
            return Some(format!("{job_id}: {status} in {cwd} ({command})"));
        }
        if object.contains_key("stdout") || object.contains_key("stderr") {
            let exit = object
                .get("exit_code")
                .and_then(serde_json::Value::as_i64)
                .map_or("none".to_string(), |code| code.to_string());
            return Some(format!("{status} exit={exit} in {cwd} ({command})"));
        }
    }

    if let Some(job_id) = object.get("job_id").and_then(serde_json::Value::as_str) {
        let status = object
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        return Some(format!("{job_id}: {status}"));
    }

    None
}

/// Collapse all runs of whitespace (incl. newlines) to single spaces so a
/// multi-line JSON argument or tool output renders on one line. The agent
/// crate's spelling, re-exported so the TUI's callers keep their short name.
pub(crate) use deep_code_agent::collapse_whitespace;

#[must_use]
pub(crate) fn tool_result_word(status: &ToolResultStatus) -> &'static str {
    match status {
        ToolResultStatus::Success => "✓",
        ToolResultStatus::Error => "✗",
        ToolResultStatus::Denied => "⊘",
    }
}

/// Truncate to at most `max_chars` characters, appending ` (truncated)` when
/// anything was cut. Returns the input unchanged when it already fits, so the
/// marker only ever means a real system cut. An explicit word (not a bare `…`)
/// because these strings — tool args, diff previews, diagnostics — otherwise
/// read as if the ellipsis were authored content.
pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut truncated = String::new();
    for _ in 0..max_chars {
        let Some(ch) = chars.next() else {
            return text.to_string();
        };
        truncated.push(ch);
    }
    if chars.next().is_some() {
        truncated.push_str(" (truncated)");
    }
    truncated
}

/// Truncate to at most `max_cols` terminal **columns**, appending
/// ` (truncated)` when anything was cut.
///
/// The column/character distinction is not cosmetic where the result is laid
/// out into a fixed number of rows: a cap of 240 *characters* is up to 480
/// columns of CJK, which wraps to twice the rows the caller budgeted for. On
/// the approval panel that arithmetic decided whether the resolved grant target
/// stayed on screen, so model-influenced text is capped by the same unit the
/// layout spends. Counted per grapheme, so a combining mark or an emoji
/// sequence is measured (and kept) whole.
pub(crate) fn truncate_display_width(text: &str, max_cols: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    // Columns alone do not bound the string. A grapheme cluster carries any
    // number of combining marks and still measures one column, so
    // `"r" + U+0301 × 20000` passes a 240-column cap completely untouched:
    // 40 KB in one terminal cell, re-emitted on every redraw, which terminals
    // answer either by stacking the marks over neighbouring rows — the
    // resolved-target line among them — or by stalling. `justification` is an
    // unvalidated model-supplied string, so this is directly reachable. Four
    // marks is more than any legitimate script stacks.
    const MAX_MARKS_PER_CLUSTER: usize = 4;

    let mut truncated = String::new();
    let mut used = 0_usize;
    let mut clipped_a_cluster = false;
    for grapheme in text.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme);
        if used + width > max_cols {
            return format!("{truncated} (truncated)");
        }
        if grapheme.chars().count() > MAX_MARKS_PER_CLUSTER + 1 {
            clipped_a_cluster = true;
            truncated.extend(grapheme.chars().take(MAX_MARKS_PER_CLUSTER + 1));
        } else {
            truncated.push_str(grapheme);
        }
        used += width;
    }
    if clipped_a_cluster {
        return format!("{truncated} (truncated)");
    }
    // Consumed the whole input without exceeding the cap.
    text.to_string()
}

#[cfg(test)]
mod width_tests {
    use super::{truncate_chars, truncate_display_width};
    use unicode_width::UnicodeWidthStr;

    /// A column cap is not a length cap: one grapheme cluster carries any
    /// number of combining marks and still measures a single column, so
    /// without a per-cluster bound `"r" + U+0301 × 20000` walked through a
    /// 240-column cap untouched — 40 KB in one terminal cell, redrawn every
    /// frame, stacking marks over the rows around it (the approval panel's
    /// resolved-target line among them).
    #[test]
    fn a_column_cap_also_bounds_marks_inside_one_cluster() {
        let zalgo = format!("r{}", "\u{301}".repeat(20_000));
        let capped = truncate_display_width(&zalgo, 240);
        assert!(
            capped.chars().count() < 40,
            "one cluster kept {} chars through a 240-column cap",
            capped.chars().count()
        );
        assert!(capped.contains("(truncated)"), "and must say it was cut");
        // Legitimate stacking is untouched.
        let vietnamese = "ế";
        assert_eq!(truncate_display_width(vietnamese, 240), vietnamese);
    }

    /// The cap is columns, and a double-width script must not be able to spend
    /// twice the budget the caller reserved.
    ///
    /// This is the arithmetic that decided whether the approval panel's
    /// resolved grant target stayed on screen: capping *characters* let 240 CJK
    /// characters claim 480 columns — seven rows at 80 columns — where the
    /// caller had budgeted for at most 240.
    #[test]
    fn a_column_cap_is_not_a_character_cap() {
        let wide = "构".repeat(240);
        assert_eq!(
            UnicodeWidthStr::width(truncate_chars(&wide, 240).as_str()),
            480,
            "the character cap is what allowed a double-width overrun"
        );
        let capped = truncate_display_width(&wide, 240);
        assert!(
            UnicodeWidthStr::width(capped.as_str()) <= 240 + " (truncated)".len(),
            "columns must stay within the cap, got {}",
            UnicodeWidthStr::width(capped.as_str())
        );
        assert!(capped.ends_with(" (truncated)"), "a real cut is announced");
    }

    #[test]
    fn text_that_fits_is_returned_unchanged() {
        assert_eq!(truncate_display_width("/tmp/x", 240), "/tmp/x");
        // Exactly at the cap is not a cut.
        let exact = "构".repeat(5);
        assert_eq!(truncate_display_width(&exact, 10), exact);
    }

    /// A grapheme is never split down the middle: a cap landing inside a
    /// double-width glyph drops it whole rather than emitting half of it.
    #[test]
    fn a_cap_inside_a_wide_glyph_drops_it_whole() {
        let capped = truncate_display_width("构构构", 5);
        assert_eq!(capped, "构构 (truncated)");
    }
}

#[cfg(test)]
mod tests;
