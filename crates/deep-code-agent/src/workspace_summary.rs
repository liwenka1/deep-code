//! Lightweight workspace overview for long-context system prompts.

use std::fs;
use std::path::Path;

const MAX_ENTRIES: usize = 24;

/// Build a short top-level workspace listing for prompt injection.
#[must_use]
pub fn build_workspace_summary(workspace: &Path) -> String {
    let Ok(read_dir) = fs::read_dir(workspace) else {
        return format!("工作区: {} (不可读)", workspace.display());
    };

    let mut entries = Vec::new();
    for entry in read_dir.flatten().take(MAX_ENTRIES + 4) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && name != ".git" {
            continue;
        }
        let kind = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            "dir"
        } else {
            "file"
        };
        entries.push(format!("{name} ({kind})"));
        if entries.len() >= MAX_ENTRIES {
            break;
        }
    }

    if entries.is_empty() {
        return format!("工作区: {} (空目录)", workspace.display());
    }

    format!(
        "工作区概览 / workspace summary ({}):\n{}",
        workspace.display(),
        entries
            .iter()
            .map(|entry| format!("- {entry}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// Recursive workspace file listing (gitignore-aware, files only, relative
/// paths, sorted, capped at `max`). Used by the TUI's `@` file completion.
#[must_use]
pub fn list_workspace_files(workspace: &Path, max: usize) -> Vec<String> {
    let mut files = Vec::new();
    let walker = ignore::WalkBuilder::new(workspace)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if let Ok(relative) = entry.path().strip_prefix(workspace) {
            files.push(relative.display().to_string());
            if files.len() >= max {
                break;
            }
        }
    }
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn lists_workspace_entries() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("README.md"), "hi").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let summary = build_workspace_summary(dir.path());
        assert!(summary.contains("README.md"));
        assert!(summary.contains("src (dir)"));
    }

    #[test]
    fn file_listing_respects_gitignore_and_cap() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::create_dir(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(dir.path().join("target/junk.o"), "x").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "/target\n").unwrap();
        // gitignore applies inside git repos; mark the dir as one.
        std::fs::create_dir(dir.path().join(".git")).unwrap();

        let files = list_workspace_files(dir.path(), 100);
        assert!(files.contains(&"src/main.rs".to_string()));
        assert!(files.contains(&"Cargo.toml".to_string()));
        assert!(
            !files.iter().any(|file| file.starts_with("target")),
            "gitignored paths must be excluded: {files:?}"
        );

        let capped = list_workspace_files(dir.path(), 1);
        assert_eq!(capped.len(), 1);
    }
}
