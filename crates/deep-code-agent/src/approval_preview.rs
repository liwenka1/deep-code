//! Change previews for gated file-mutating tool calls.
//!
//! An approval prompt that only shows raw JSON arguments asks the user to
//! sign off blind on large rewrites. For `apply_patch` and `write_file` we
//! can do better: derive a bounded unified diff from the call arguments and
//! the current workspace state, and attach it to the
//! [`crate::tool::ApprovalRequest`] before it reaches the UI.

use std::fs;
use std::path::Path;

use serde_json::Value;
use similar::TextDiff;

use crate::i18n::{Lang, TextId, tr, tr_with};
use crate::tool::ToolCall;
use crate::workspace_policy::{WorkspacePolicy, WorkspaceRoots};

const MAX_PREVIEW_LINES: usize = 40;
const MAX_PREVIEW_BYTES: usize = 4 * 1024;
/// Mirrors the read tool's source-size guard.
const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;
const NEW_FILE_HEAD_LINES: usize = 20;

/// Best-effort preview: `None` means "nothing to add beyond the arguments"
/// (unknown tool, unresolvable path, malformed args) — approval proceeds with
/// the raw-argument display as before.
pub(crate) fn build_approval_preview(
    call: &ToolCall,
    roots: &WorkspaceRoots,
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
            let policy = WorkspacePolicy::new(roots.clone()).ok()?;
            let path = policy.resolve_for_write(rel, "write_file").ok()?;
            Some(write_file_preview(&path, content, lang))
        }
        _ => None,
    }
}

fn write_file_preview(path: &Path, content: &str, lang: Lang) -> String {
    if !path.exists() {
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
    match fs::metadata(path) {
        Ok(metadata) if metadata.len() > MAX_SOURCE_BYTES => {
            return tr(lang, TextId::PreviewFileTooBig).to_string();
        }
        Err(_) => return tr(lang, TextId::PreviewReadFail).to_string(),
        Ok(_) => {}
    }
    match fs::read_to_string(path) {
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

fn unified_diff(old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new)
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
    use serde_json::json;

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall::new("call_1", name, arguments)
    }

    #[test]
    fn apply_patch_preview_shows_unified_diff() {
        let workspace = tempfile::tempdir().unwrap();
        let preview = build_approval_preview(
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
        let preview = build_approval_preview(
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

    /// Rendering an approval prompt must not touch the workspace: the mkdir
    /// that write execution needs used to live inside path RESOLUTION, so
    /// previewing `src/new_mod/thing.rs` planted `src/new_mod/` before the
    /// user decided — and a denial left it behind. The preview still renders
    /// (the user sees the new-file summary); the disk stays as it was.
    #[test]
    fn write_file_preview_creates_no_directories() {
        let workspace = tempfile::tempdir().unwrap();
        let preview = build_approval_preview(
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
        let preview = build_approval_preview(
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
        let preview = build_approval_preview(
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
            build_approval_preview(
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
            build_approval_preview(
                &call("shell", json!({"command": "ls"})),
                &WorkspaceRoots::from(workspace.path()),
                Lang::Zh
            )
            .is_none()
        );
    }
}
