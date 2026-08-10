//! Lightweight workspace overview for long-context system prompts.

use std::fs;
use std::path::Path;

const MAX_ENTRIES: usize = 24;

/// Build a short top-level workspace listing for prompt injection, plus the
/// extra granted roots when any exist. Naming the extras here is what makes
/// the grant usable: the model is otherwise trained toward workspace-relative
/// paths and would never try an absolute path into a sibling repo.
#[must_use]
pub fn build_workspace_summary(workspace: &Path, extra_roots: &[std::path::PathBuf]) -> String {
    let summary = primary_summary(workspace);
    if extra_roots.is_empty() {
        return summary;
    }
    let extras = extra_roots
        .iter()
        .map(|root| format!("- {}", root.display()))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{summary}\n附加可写目录 / additional writable roots (use absolute paths; \
         relative paths always resolve against the workspace):\n{extras}"
    )
}

fn primary_summary(workspace: &Path) -> String {
    let Ok(read_dir) = fs::read_dir(workspace) else {
        return format!("工作区: {} (不可读)", workspace.display());
    };

    // Collect → filter → sort → truncate: `read_dir` order is arbitrary, and
    // this summary lands in the system prompt, so an unstable listing would
    // change the prompt prefix between runs and forfeit provider prompt-cache
    // hits. Sorting also makes the truncation deterministic instead of
    // keeping whichever entries the OS happened to yield first.
    let mut entries: Vec<(String, bool)> = read_dir
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && name != ".git" {
                return None;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            Some((name, is_dir))
        })
        .collect();
    entries.sort();
    entries.truncate(MAX_ENTRIES);
    let entries: Vec<String> = entries
        .into_iter()
        .map(|(name, is_dir)| format!("{name} ({})", if is_dir { "dir" } else { "file" }))
        .collect();

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
            // Normalize to forward slashes so the listing is identical across
            // platforms (the model and `@` completion expect `/`).
            files.push(relative.to_string_lossy().replace('\\', "/"));
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
        let summary = build_workspace_summary(dir.path(), &[]);
        assert!(summary.contains("README.md"));
        assert!(summary.contains("src (dir)"));
        assert!(
            !summary.contains("additional writable roots"),
            "no extras section without grants: {summary}"
        );
    }

    #[test]
    fn extra_roots_are_named_in_the_summary() {
        let dir = TempDir::new().unwrap();
        let extra = TempDir::new().unwrap();
        let summary = build_workspace_summary(
            dir.path(),
            std::slice::from_ref(&extra.path().to_path_buf()),
        );
        assert!(summary.contains("additional writable roots"), "{summary}");
        assert!(
            summary.contains(&extra.path().display().to_string()),
            "extras must be listed verbatim: {summary}"
        );
    }

    /// The summary feeds the system prompt: entries must come out sorted so
    /// the prompt prefix is byte-stable across runs (provider cache hits).
    #[test]
    fn summary_entries_are_sorted() {
        let dir = TempDir::new().unwrap();
        for name in ["zeta.txt", "alpha.txt", "midway.txt"] {
            std::fs::write(dir.path().join(name), "x").unwrap();
        }
        let summary = build_workspace_summary(dir.path(), &[]);
        let alpha = summary.find("alpha.txt").unwrap();
        let midway = summary.find("midway.txt").unwrap();
        let zeta = summary.find("zeta.txt").unwrap();
        assert!(
            alpha < midway && midway < zeta,
            "entries must be sorted: {summary}"
        );
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
