//! Post-edit path extraction for LSP diagnostics.

use std::path::{Path, PathBuf};

use serde_json::Value;

const EDIT_TOOLS: &[&str] = &["write_file", "apply_patch"];

#[must_use]
pub fn is_edit_tool(tool_name: &str) -> bool {
    EDIT_TOOLS.contains(&tool_name)
}

#[must_use]
pub fn edited_paths_for_tool(tool_name: &str, input: &Value) -> Vec<PathBuf> {
    match tool_name {
        "write_file" | "apply_patch" => input
            .get("path")
            .and_then(Value::as_str)
            .map(|path| vec![PathBuf::from(path)])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[must_use]
pub fn resolve_edit_paths(workspace: &Path, relative_paths: &[PathBuf]) -> Vec<PathBuf> {
    relative_paths
        .iter()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                workspace.join(path)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_write_file_path() {
        let paths = edited_paths_for_tool("write_file", &json!({"path": "src/main.rs"}));
        assert_eq!(paths, vec![PathBuf::from("src/main.rs")]);
    }

    #[test]
    fn ignores_non_edit_tools() {
        assert!(edited_paths_for_tool("read_file", &json!({"path": "a.rs"})).is_empty());
    }
}
