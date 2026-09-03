use deep_code_agent::{EntryKind, SessionRecord, ToolResultStatus};

use deep_code_agent::i18n::{Lang, TextId, tr, tr_with};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolApprovalState {
    NotRequired,
    Required,
    Approved,
    Denied,
}

impl ToolApprovalState {
    #[must_use]
    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::NotRequired => "",
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryCell {
    /// The startup header: a compact, styled welcome card (rendered specially
    /// in `cell_lines`). It scrolls away naturally after the first message.
    /// Holds raw data, not preformatted text, so a `/lang` switch re-renders
    /// it in the new language on the next frame.
    Welcome {
        version: String,
        /// Raw model id, e.g. "deepseek-chat".
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
    Reasoning {
        text: String,
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
    Diagnostics {
        summary: String,
        rendered: String,
    },
    Checkpoint {
        id: String,
        label: String,
    },
    Compaction {
        metadata: Option<String>,
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
            Self::System { text }
            | Self::User { text }
            | Self::Assistant { text }
            | Self::Reasoning { text } => vec![text.clone()],
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
                    ToolApprovalState::NotRequired => String::new(),
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
            Self::Compaction { metadata, summary } => {
                let title = metadata.as_deref().map_or_else(
                    || tr(lang, TextId::CompactionSummaryTitle).to_string(),
                    |value| tr_with(lang, TextId::CompactionSummaryTitleMeta, &[("meta", value)]),
                );
                vec![title, summary.clone()]
            }
        }
    }
}

pub(crate) fn hydrate_history(record: &SessionRecord) -> Vec<HistoryCell> {
    let mut cells = Vec::new();
    let mut turn_index = 0usize;
    let mut current_turn = Vec::new();

    for entry in &record.entries {
        match &entry.kind {
            EntryKind::User { content } => {
                if !current_turn.is_empty() {
                    cells.append(&mut current_turn);
                    append_turn_checkpoints(&mut cells, record, turn_index);
                    turn_index += 1;
                }
                current_turn.push(HistoryCell::user(content.clone()));
            }
            EntryKind::System { .. } => {}
            EntryKind::Assistant {
                content,
                reasoning,
                exchanges,
            } => {
                if let Some(reasoning) = reasoning.as_ref().filter(|text| !text.is_empty()) {
                    current_turn.push(HistoryCell::Reasoning {
                        text: reasoning.clone(),
                    });
                }
                if !content.is_empty() {
                    current_turn.push(HistoryCell::assistant(content.clone()));
                }
                for exchange in exchanges {
                    current_turn.push(HistoryCell::ToolCall {
                        tool_name: exchange.call.function.name.clone(),
                        arguments: exchange.call.function.arguments.clone(),
                        approval: ToolApprovalState::NotRequired,
                        running_for_secs: None,
                    });
                    // Pending exchanges (interrupted before a result) render
                    // the call only — no fabricated result line.
                    if let Some(result) = &exchange.result {
                        current_turn.push(HistoryCell::ToolResult {
                            status: result.status,
                            summary: summarize_tool_result(&result.content),
                        });
                    }
                }
            }
            EntryKind::Compaction {
                summary,
                archived_count,
            } => {
                current_turn.push(HistoryCell::Compaction {
                    metadata: Some(format!("archived={archived_count}")),
                    summary: summary.clone(),
                });
            }
        }
    }
    if !current_turn.is_empty() {
        cells.extend(current_turn);
        append_turn_checkpoints(&mut cells, record, turn_index);
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

    let flattened = content.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&flattened, MAX_CHARS)
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
/// multi-line JSON argument or tool output renders on one line.
pub(crate) fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

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
