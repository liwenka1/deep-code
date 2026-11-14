//! Bounded change and process-input previews for gated tool calls.
//!
//! An approval prompt that only shows raw JSON arguments asks the user to
//! sign off blind on large rewrites or interpreter input. Mutation previews
//! derive a diff from the workspace; process previews show the exact stdin
//! byte count, bounded content, target and pipe/EOF semantics. Both attach to
//! [`crate::tool::ApprovalRequest`] for UI and HTTP consumers.

use std::path::Path;

use serde_json::Value;
use similar::TextDiff;

use crate::i18n::{Lang, TextId, tr, tr_with};
use crate::tool::ToolCall;
use crate::workspace_policy::WorkspacePolicy;

const MAX_PREVIEW_LINES: usize = 40;
const MAX_PREVIEW_BYTES: usize = 4 * 1024;
/// Maximum existing source read for a mutation diff preview. This does not
/// limit paged reads by read_file.
const MAX_SOURCE_BYTES: u64 = crate::workspace_tools::MAX_FILE_BYTES;
const NEW_FILE_HEAD_LINES: usize = 20;
const STDIN_FRAGMENT_BYTES: usize = 1536;
const STDIN_FRAGMENT_LINES: usize = 16;

/// Build a preview against the live policy already used by the runtime. A
/// runtime must not reconstruct a policy after a root path has been swapped:
/// doing so could make the preview read from a new directory the session never
/// approved. Callers that only have a `WorkspaceRoots` value (tests, isolated
/// tooling) build the policy themselves and pass it here.
pub(crate) fn build_approval_preview_with_policy(
    call: &ToolCall,
    policy: &WorkspacePolicy,
    lang: Lang,
) -> Option<String> {
    match call.name.as_str() {
        "apply_patch" => {
            let old = string_arg(&call.arguments, "old")?;
            let new = string_arg(&call.arguments, "new")?;
            Some(clamp_preview(&unified_diff(old, new), lang))
        }
        "write_file" => {
            let rel = string_arg(&call.arguments, "path")?;
            let content = string_arg(&call.arguments, "content")?;
            // Resolve through the same policy (all granted roots) the tool
            // itself uses, so the preview never reads paths the execution
            // would reject — and never rejects paths the execution would
            // accept, which is what a single-root policy would do to an
            // `--add-dir` write.
            let path = policy.resolve_for_write(rel, "write_file").ok()?;
            Some(write_file_preview(policy, &path, content, lang))
        }
        "shell" => string_arg(&call.arguments, "stdin")
            .map(|input| process_input_preview(input, None, false, lang)),
        "job" => match string_arg(&call.arguments, "action")? {
            "start" => {
                let interactive = call
                    .arguments
                    .get("interactive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let input = string_arg(&call.arguments, "stdin").unwrap_or_default();
                Some(process_input_preview(input, None, interactive, lang))
            }
            "input" => Some(process_input_preview(
                string_arg(&call.arguments, "stdin")?,
                Some(string_arg(&call.arguments, "job_id")?),
                true,
                lang,
            )),
            "close_stdin" => Some(process_input_preview(
                "",
                Some(string_arg(&call.arguments, "job_id")?),
                false,
                lang,
            )),
            _ => None,
        },
        _ => None,
    }
}

fn process_input_preview(input: &str, job_id: Option<&str>, keep_open: bool, lang: Lang) -> String {
    let mut out = match lang {
        Lang::En => format!(
            "stdin: {} UTF-8 bytes; no newline is appended\n",
            input.len()
        ),
        Lang::Zh => format!("stdin：{} 个 UTF-8 字节；不会自动添加换行\n", input.len()),
    };
    if let Some(job_id) = job_id {
        let target = crate::text_util::truncate_chars(job_id, 160);
        out.push_str(&match lang {
            Lang::En => format!("target job_id: {target}\n"),
            Lang::Zh => format!("目标 job_id：{target}\n"),
        });
    }
    out.push_str(match (lang, keep_open) {
        (Lang::En, true) => "pipe remains open; EOF is not sent (pipe, not a PTY)\n",
        (Lang::Zh, true) => "保留输入管道；不发送 EOF（管道，不是 PTY）\n",
        (Lang::En, false) => {
            "stdin closes with EOF; process completion must be checked separately\n"
        }
        (Lang::Zh, false) => "关闭 stdin 并发送 EOF；进程是否完成需另行检查\n",
    });
    if input.is_empty() {
        out.push_str(match lang {
            Lang::En => "[stdin is empty]",
            Lang::Zh => "[stdin 为空]",
        });
        return out;
    }
    let mut head_end = input.len().min(STDIN_FRAGMENT_BYTES);
    while !input.is_char_boundary(head_end) {
        head_end -= 1;
    }
    if let Some((index, _)) = input.match_indices('\n').nth(STDIN_FRAGMENT_LINES - 1) {
        head_end = head_end.min(index + 1);
    }
    let mut tail_start = input.len().saturating_sub(STDIN_FRAGMENT_BYTES);
    while !input.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    if let Some((index, _)) = input.rmatch_indices('\n').nth(STDIN_FRAGMENT_LINES - 1) {
        tail_start = tail_start.max(index + 1);
    }
    if head_end >= tail_start {
        out.push_str("[stdin]\n");
        out.push_str(input);
    } else {
        out.push_str("[stdin head]\n");
        out.push_str(&input[..head_end]);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        let omitted = tail_start - head_end;
        out.push_str(&match lang {
            Lang::En => format!("[stdin middle omitted: {omitted} bytes]\n"),
            Lang::Zh => format!("[stdin 中间已省略：{omitted} 字节]\n"),
        });
        out.push_str("[stdin tail]\n");
        out.push_str(&input[tail_start..]);
    }
    out
}

fn write_file_preview(policy: &WorkspacePolicy, path: &Path, content: &str, lang: Lang) -> String {
    let handle = match policy.open_existing_path(path, "write_file") {
        Ok(handle) => handle,
        Err(_) if path.symlink_metadata().is_err() => {
            let total = content.lines().count();
            let mut out = format!(
                "{}\n",
                tr_with(
                    lang,
                    TextId::PreviewNewFile,
                    &[("total", &total.to_string())]
                )
            );
            for line in content.lines().take(NEW_FILE_HEAD_LINES) {
                out.push_str("+ ");
                out.push_str(line);
                out.push('\n');
            }
            if total > NEW_FILE_HEAD_LINES {
                out.push_str(&tr_with(
                    lang,
                    TextId::PreviewMoreLines,
                    &[("count", &(total - NEW_FILE_HEAD_LINES).to_string())],
                ));
            }
            return clamp_preview(&out, lang);
        }
        Err(_) => return tr(lang, TextId::PreviewReadFail).to_string(),
    };
    match handle.no_follow_metadata() {
        Ok(metadata) if metadata.len() > MAX_SOURCE_BYTES => {
            // The displayed limit comes from this mutation-preview guard.
            return tr_with(
                lang,
                TextId::PreviewFileTooBig,
                &[("limit", &crate::workspace_tools::MAX_FILE_MIB.to_string())],
            );
        }
        Err(_) => return tr(lang, TextId::PreviewReadFail).to_string(),
        Ok(_) => {}
    }
    match handle.read_to_string() {
        Ok(existing) => {
            let diff = unified_diff(&existing, content);
            if diff.trim().is_empty() {
                tr(lang, TextId::PreviewNoChange).to_string()
            } else {
                clamp_preview(&diff, lang)
            }
        }
        Err(_) => tr(lang, TextId::PreviewNotUtf8).to_string(),
    }
}

/// Cap on the diff algorithm's runtime. This preview is built *synchronously*
/// inside the async approval flow (see `approval_flow::approval_preview`), so a
/// slow diff blocks the executor — and the cancel with it. `similar`'s line
/// diff is Myers, whose worst case is O((N+M)·D): two large, wholly-dissimilar
/// files (e.g. the model rewriting a big generated file, `existing` up to
/// `MAX_SOURCE_BYTES` against an equally large `content`) drove it into a
/// tens-of-seconds freeze. The output is clamped to 40 lines anyway, so an
/// optimal diff is not needed; on timeout `similar` returns a coarser but valid
/// diff, which previews just as well.
const DIFF_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

pub(crate) fn unified_diff(old: &str, new: &str) -> String {
    TextDiff::configure()
        .timeout(DIFF_TIMEOUT)
        .diff_lines(old, new)
        .unified_diff()
        .context_radius(2)
        .to_string()
}

fn string_arg<'a>(arguments: &'a Value, field: &str) -> Option<&'a str> {
    arguments.get(field).and_then(Value::as_str)
}

/// Bound the preview for UI display: line cap first, then byte cap.
fn clamp_preview(preview: &str, lang: Lang) -> String {
    let lines: Vec<&str> = preview.lines().collect();
    let mut out = String::new();
    let mut shown = 0usize;
    for line in &lines {
        if shown >= MAX_PREVIEW_LINES || out.len() + line.len() + 1 > MAX_PREVIEW_BYTES {
            break;
        }
        out.push_str(line);
        out.push('\n');
        shown += 1;
    }
    if shown < lines.len() {
        out.push_str(&tr_with(
            lang,
            TextId::PreviewMoreLines,
            &[("count", &(lines.len() - shown).to_string())],
        ));
    } else if out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_policy::WorkspaceRoots;
    use serde_json::json;
    use std::fs;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall::new("call_1", name, arguments)
    }

    /// Preview against a freshly built policy. These tests are about WHAT the
    /// preview shows; the runtime path — which must reuse the live policy
    /// rather than rebuild one — is covered in `runtime/integration_tests.rs`.
    fn preview(call: &ToolCall, roots: &WorkspaceRoots, lang: Lang) -> Option<String> {
        let policy = WorkspacePolicy::new(roots.clone()).ok()?;
        build_approval_preview_with_policy(call, &policy, lang)
    }

    #[test]
    fn apply_patch_preview_shows_unified_diff() {
        let workspace = tempfile::tempdir().unwrap();
        let preview = preview(
            &call(
                "apply_patch",
                json!({"path": "a.rs", "old": "fn old() {}", "new": "fn new() {}"}),
            ),
            &WorkspaceRoots::from(workspace.path()),
            Lang::Zh,
        )
        .unwrap();
        assert!(preview.contains("-fn old() {}"), "{preview}");
        assert!(preview.contains("+fn new() {}"), "{preview}");
    }

    #[test]
    fn write_file_preview_diffs_against_existing_content() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("note.txt"), "one\ntwo\n").unwrap();
        let preview = preview(
            &call(
                "write_file",
                json!({"path": "note.txt", "content": "one\nthree\n"}),
            ),
            &WorkspaceRoots::from(workspace.path()),
            Lang::Zh,
        )
        .unwrap();
        assert!(preview.contains("-two"), "{preview}");
        assert!(preview.contains("+three"), "{preview}");
    }

    /// Mutation-preview refusals name the source guard actually enforced.
    #[test]
    fn write_file_preview_reports_the_limit_from_the_constant() {
        let workspace = tempfile::tempdir().unwrap();
        fs::write(
            workspace.path().join("big.bin"),
            vec![b'x'; usize::try_from(MAX_SOURCE_BYTES).unwrap() + 1],
        )
        .unwrap();
        let preview = preview(
            &call("write_file", json!({"path": "big.bin", "content": "hi\n"})),
            &WorkspaceRoots::from(workspace.path()),
            Lang::En,
        )
        .unwrap();
        assert!(
            preview.contains(&format!("{} MiB", crate::workspace_tools::MAX_FILE_MIB)),
            "the preview must name the limit it actually enforced: {preview}"
        );
        // ...and the assertion above, alone, is a tautology at today's value:
        // `contains("2 MiB")` is equally true of a locale string with "2 MiB"
        // hardcoded, so reverting the derivation outright left this test — and
        // all eight in this module — green. What actually distinguishes the two
        // is whether the pack still carries the placeholder, so assert on that
        // directly, in both languages, rather than on a rendering that cannot
        // tell them apart until someone bumps the constant.
        for lang in [Lang::En, Lang::Zh] {
            assert!(
                crate::i18n::tr(lang, crate::i18n::TextId::PreviewFileTooBig).contains("{limit}"),
                "{lang:?} must take the limit as a parameter, not spell it out"
            );
        }
    }

    /// Rendering an approval prompt must not touch the workspace: the mkdir
    /// that write execution needs used to live inside path RESOLUTION, so
    /// previewing `src/new_mod/thing.rs` planted `src/new_mod/` before the
    /// user decided — and a denial left it behind. The preview still renders
    /// (the user sees the new-file summary); the disk stays as it was.
    #[test]
    fn write_file_preview_creates_no_directories() {
        let workspace = tempfile::tempdir().unwrap();
        let preview = preview(
            &call(
                "write_file",
                json!({"path": "src/new_mod/thing.rs", "content": "hi\n"}),
            ),
            &WorkspaceRoots::from(workspace.path()),
            Lang::Zh,
        );
        assert!(preview.is_some(), "nested new-file preview must render");
        assert!(
            !workspace.path().join("src").exists(),
            "previewing an approval must not mkdir into the workspace"
        );
    }

    #[test]
    fn write_file_preview_summarizes_new_files() {
        let workspace = tempfile::tempdir().unwrap();
        let preview = preview(
            &call(
                "write_file",
                json!({"path": "fresh.txt", "content": "hello\nworld\n"}),
            ),
            &WorkspaceRoots::from(workspace.path()),
            Lang::Zh,
        )
        .unwrap();
        assert!(preview.starts_with("新文件 · 2 行"), "{preview}");
        assert!(preview.contains("+ hello"), "{preview}");
    }

    #[test]
    fn preview_is_bounded_for_huge_changes() {
        let workspace = tempfile::tempdir().unwrap();
        let content = (0..500)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = preview(
            &call("write_file", json!({"path": "big.txt", "content": content})),
            &WorkspaceRoots::from(workspace.path()),
            Lang::Zh,
        )
        .unwrap();
        assert!(preview.lines().count() <= MAX_PREVIEW_LINES + 2);
        assert!(preview.contains("未显示"), "{preview}");
    }

    #[test]
    fn traversal_paths_produce_no_preview() {
        let workspace = tempfile::tempdir().unwrap();
        assert!(
            preview(
                &call(
                    "write_file",
                    json!({"path": "../outside.txt", "content": "x"}),
                ),
                &WorkspaceRoots::from(workspace.path()),
                Lang::Zh,
            )
            .is_none()
        );
    }

    #[test]
    fn unknown_tools_produce_no_preview() {
        let workspace = tempfile::tempdir().unwrap();
        assert!(
            preview(
                &call("unknown_tool", json!({"command": "ls"})),
                &WorkspaceRoots::from(workspace.path()),
                Lang::Zh
            )
            .is_none()
        );
    }

    #[test]
    fn shell_preview_exposes_interpreter_stdin_and_exact_byte_count() {
        let workspace = tempfile::tempdir().unwrap();
        let source = "from pathlib import Path\nPath('result.txt').write_text('你好')\n";
        let shown = preview(
            &call("shell", json!({ "command": "python3 -", "stdin": source })),
            &WorkspaceRoots::from(workspace.path()),
            Lang::En,
        )
        .unwrap();
        assert!(
            shown.contains(&format!("{} UTF-8 bytes", source.len())),
            "{shown}"
        );
        assert!(shown.contains(source), "{shown}");
        assert!(shown.contains("no newline is appended"));
        assert!(shown.contains("closes with EOF"));
        assert!(!shown.contains("pipe remains open"));
        assert!(!workspace.path().join("result.txt").exists());
    }

    #[test]
    fn long_utf8_single_line_input_keeps_both_ends_with_explicit_middle_omission() {
        let workspace = tempfile::tempdir().unwrap();
        let input = format!("HEAD{}xxTAIL", "界".repeat((64 * 1024 - 10) / 3));
        assert_eq!(input.len(), 64 * 1024);
        let shown = preview(
            &call("shell", json!({ "command": "python3 -", "stdin": input })),
            &WorkspaceRoots::from(workspace.path()),
            Lang::En,
        )
        .unwrap();
        assert!(shown.contains(&format!("{} UTF-8 bytes", input.len())));
        assert!(shown.contains("HEAD") && shown.ends_with("TAIL"));
        assert!(shown.contains("[stdin head]") && shown.contains("[stdin tail]"));
        assert!(shown.contains("stdin middle omitted:") && shown.contains("bytes]"));
        assert!(
            shown.len() <= MAX_PREVIEW_BYTES,
            "{} preview bytes",
            shown.len()
        );
        assert!(!shown.contains('�'));
    }

    #[test]
    fn many_line_stdin_keeps_a_bounded_head_and_tail() {
        let input = (0..1_000)
            .map(|line| format!("line-{line}\n"))
            .collect::<String>();
        let shown = process_input_preview(&input, None, false, Lang::En);
        assert!(shown.contains("line-0\n") && shown.ends_with("line-999\n"));
        assert!(shown.contains("stdin middle omitted:"));
        assert!(shown.lines().count() <= MAX_PREVIEW_LINES);
        assert!(shown.len() <= MAX_PREVIEW_BYTES);
    }

    #[test]
    fn job_previews_name_input_targets_and_pipe_or_eof_semantics() {
        let workspace = tempfile::tempdir().unwrap();
        let roots = WorkspaceRoots::from(workspace.path());
        let interactive = preview(
            &call(
                "job",
                json!({
                    "action": "start", "command": "python3 -i", "interactive": true,
                    "stdin": "print('ready')\n"
                }),
            ),
            &roots,
            Lang::En,
        )
        .unwrap();
        assert!(interactive.contains("pipe remains open; EOF is not sent"));
        assert!(interactive.contains("pipe, not a PTY"));
        assert!(interactive.contains("print('ready')"));
        let finite = preview(
            &call(
                "job",
                json!({
                    "action": "start", "command": "cat", "stdin": "done"
                }),
            ),
            &roots,
            Lang::En,
        )
        .unwrap();
        assert!(finite.contains("4 UTF-8 bytes") && finite.contains("closes with EOF"));
        let empty_interactive = preview(
            &call(
                "job",
                json!({
                    "action": "start", "command": "cat", "interactive": true
                }),
            ),
            &roots,
            Lang::En,
        )
        .unwrap();
        assert!(
            empty_interactive.contains("0 UTF-8 bytes")
                && empty_interactive.contains("pipe remains open")
        );
        let input = preview(
            &call(
                "job",
                json!({
                    "action": "input", "job_id": "job_target", "stdin": "你好\n"
                }),
            ),
            &roots,
            Lang::En,
        )
        .unwrap();
        assert!(input.contains("target job_id: job_target"));
        assert!(input.contains("7 UTF-8 bytes") && input.contains("你好\n"));
        assert!(input.contains("EOF is not sent"));
        let eof = preview(
            &call(
                "job",
                json!({
                    "action": "close_stdin", "job_id": "job_target"
                }),
            ),
            &roots,
            Lang::En,
        )
        .unwrap();
        assert!(eof.contains("target job_id: job_target"));
        assert!(eof.contains("0 UTF-8 bytes") && eof.contains("closes with EOF"));
        assert!(eof.contains("process completion must be checked separately"));
    }
}
