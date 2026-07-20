//! Workspace skill discovery and prompt exposure.
//!
//! A *skill* is a directory containing a `SKILL.md` whose fenced metadata
//! names it and says when it applies (see [`frontmatter`]). deep-code scans a
//! small set of well-known roots, gathers every skill it can load, and folds
//! a compact index into the system prompt so the model knows what exists and
//! can open the full instructions only when needed.

mod frontmatter;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Longest description rendered into the prompt index. The index only needs
/// enough text to *choose* a skill — detailed guidance belongs in the skill
/// body — and one tweet-sized line (280 chars) covers that without inflating
/// every request.
const DESCRIPTION_BUDGET: usize = 280;

/// Character ceiling for the whole rendered skills block: roughly 3-4k
/// tokens, enough for dozens of skills while guaranteeing a huge skill
/// library can never crowd out the actual conversation.
const INDEX_BUDGET: usize = 12_000;

/// How many load warnings the prompt surfaces before going quiet; past a
/// handful they stop being actionable and just burn tokens.
const WARNING_DISPLAY_CAP: usize = 8;

/// Directory levels scanned below a skills root. Genuine layouts nest a
/// little (`<root>/<vendor>/<skill>/SKILL.md`); anything deeper is almost
/// always a misconfigured root (someone pointing it at `$HOME`), so cap the
/// walk at a generous eight levels.
const MAX_WALK_DEPTH: usize = 8;

/// One skill loaded from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Identifier from the metadata block; how the model refers to it.
    pub name: String,
    /// Short "when to use me" summary from the metadata block. May be empty.
    pub description: String,
    /// Full instructions: everything after the metadata block, trimmed.
    pub body: String,
    /// Where the `SKILL.md` actually lives. The directory name is allowed to
    /// differ from `name`, so consumers must use this path as-is.
    pub path: PathBuf,
}

/// Every skill found under one or more roots, plus any problems hit while
/// loading them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillRegistry {
    entries: Vec<Skill>,
    warnings: Vec<String>,
}

impl SkillRegistry {
    /// Scan `dir` for skills.
    ///
    /// The walk visits directories breadth-agnostically from a worklist: a
    /// child directory that contains a `SKILL.md` is loaded as one skill and
    /// treated as a leaf (files under it are that skill's own material, not
    /// further skills); a child without one is queued for scanning. Hidden
    /// children (`.git`, `.cache`, ...) are never entered. Symlinks are
    /// followed, with canonical-path bookkeeping plus [`MAX_WALK_DEPTH`]
    /// keeping cyclic layouts finite.
    #[must_use]
    pub fn discover(dir: &Path) -> Self {
        let mut registry = Self::default();
        let Ok(root_identity) = fs::canonicalize(dir) else {
            return registry;
        };
        if !root_identity.is_dir() {
            return registry;
        }

        let mut seen: HashSet<PathBuf> = HashSet::from([root_identity]);
        let mut pending: Vec<(PathBuf, usize)> = vec![(dir.to_path_buf(), 0)];
        while let Some((current, depth)) = pending.pop() {
            registry.scan_level(&current, depth, &mut seen, &mut pending);
        }

        registry.entries.sort_by(|x, y| {
            (x.name.as_str(), x.path.as_path()).cmp(&(y.name.as_str(), y.path.as_path()))
        });
        registry
    }

    /// Examine the immediate children of `dir`, loading skills and queueing
    /// plain subdirectories onto `pending`.
    fn scan_level(
        &mut self,
        dir: &Path,
        depth: usize,
        seen: &mut HashSet<PathBuf>,
        pending: &mut Vec<(PathBuf, usize)>,
    ) {
        let children = match fs::read_dir(dir) {
            Ok(iter) => iter,
            Err(err) => {
                // An unreadable root is misconfiguration the user should see;
                // unreadable nested directories are usually just noise.
                if depth == 0 {
                    self.note(format!(
                        "skills root {} is unreadable: {err}",
                        dir.display()
                    ));
                }
                return;
            }
        };

        for entry in children.flatten() {
            let child = entry.path();
            if is_hidden(&child) {
                continue;
            }
            if !fs::metadata(&child)
                .map(|meta| meta.is_dir())
                .unwrap_or(false)
            {
                continue;
            }

            let manifest = child.join("SKILL.md");
            match fs::read_to_string(&manifest) {
                Ok(text) => {
                    // Loaded or not, this directory is spoken for: never
                    // descend into a skill directory, and never load the
                    // same physical directory twice via a symlink alias.
                    if !claim(&child, seen) {
                        continue;
                    }
                    match frontmatter::read_skill_file(&text) {
                        Ok(parsed) => self.entries.push(Skill {
                            name: parsed.name,
                            description: parsed.description,
                            body: parsed.body,
                            path: manifest,
                        }),
                        Err(why) => {
                            self.note(format!("skipping {}: {why}", manifest.display()));
                        }
                    }
                }
                Err(err) if manifest.exists() => {
                    if claim(&child, seen) {
                        self.note(format!("cannot read {}: {err}", manifest.display()));
                    }
                }
                Err(_) => {
                    // No manifest here — keep walking downward, depth permitting.
                    if depth < MAX_WALK_DEPTH && claim(&child, seen) {
                        pending.push((child, depth + 1));
                    }
                }
            }
        }
    }

    fn note(&mut self, warning: String) {
        self.warnings.push(warning);
    }

    /// Look up a skill by its metadata name.
    #[cfg(test)]
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.entries.iter().find(|skill| skill.name == name)
    }

    /// All loaded skills, sorted by name (then path).
    #[must_use]
    pub fn list(&self) -> &[Skill] {
        &self.entries
    }

    /// Problems encountered while loading (unreadable roots, malformed
    /// metadata, ...). Empty on a clean scan.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// True when no skills were loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of loaded skills.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Record a directory's canonical identity in `seen`. Returns `false` when
/// the same physical directory was already handled — the mechanism that cuts
/// off symlink cycles and aliased duplicates.
fn claim(dir: &Path, seen: &mut HashSet<PathBuf>) -> bool {
    let identity = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    seen.insert(identity)
}

/// A path is hidden when its final component starts with a dot.
fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|component| component.to_str())
        .is_some_and(|component| component.starts_with('.'))
}

/// The user-level skills root: `~/.deep-code/skills`. Falls back to a
/// relative path when `$HOME` is unset (containers, stripped-down CI).
#[must_use]
pub fn global_skills_dir() -> PathBuf {
    match crate::paths::home_dir() {
        Some(home) => home.join(".deep-code").join("skills"),
        None => PathBuf::from(".deep-code/skills"),
    }
}

/// The project-local skills root: `<workspace>/skills`.
#[must_use]
pub fn workspace_skills_dir(workspace: &Path) -> PathBuf {
    workspace.join("skills")
}

/// The skills roots consulted for a workspace, most specific first:
///
/// 1. `<workspace>/skills`
/// 2. `<workspace>/.deep-code/skills`
/// 3. `~/.deep-code/skills`
///
/// Only roots that exist on disk are returned, deduplicated by canonical
/// path so a symlinked alias is not scanned twice.
#[must_use]
pub fn skills_directories(workspace: &Path) -> Vec<PathBuf> {
    let candidates = [
        workspace_skills_dir(workspace),
        workspace.join(".deep-code").join("skills"),
        global_skills_dir(),
    ];
    let mut resolved = HashSet::new();
    let mut roots = Vec::new();
    for candidate in candidates {
        let Ok(real) = fs::canonicalize(&candidate) else {
            continue;
        };
        if real.is_dir() && resolved.insert(real) {
            roots.push(candidate);
        }
    }
    roots
}

/// Scan every root from [`skills_directories`] and merge the results into a
/// single registry. When two roots define the same skill name, the earlier
/// (more specific) root wins; warnings from all roots accumulate.
#[must_use]
pub fn discover_in_workspace(workspace: &Path) -> SkillRegistry {
    let mut combined = SkillRegistry::default();
    for root in skills_directories(workspace) {
        let found = SkillRegistry::discover(&root);
        combined.warnings.extend(found.warnings);
        for skill in found.entries {
            let name_taken = combined.entries.iter().any(|held| held.name == skill.name);
            if !name_taken {
                combined.entries.push(skill);
            }
        }
    }
    combined
}

/// Render the prompt-facing skills index, or `None` when there is nothing
/// to show. Entries are one line each; the block as a whole is capped at
/// [`INDEX_BUDGET`] characters, with a trailing note counting anything that
/// did not fit.
#[must_use]
pub fn render_skills_block(registry: &SkillRegistry) -> Option<String> {
    if registry.is_empty() {
        return None;
    }

    let mut block = String::from("## Skills\n");
    block.push_str(
        "Reusable instruction sets found on this machine. Each line names a skill, \
summarizes when it applies, and points at its SKILL.md; read that file before \
acting on a skill.\n\n### Available skills\n",
    );

    let mut used = block.chars().count();
    let mut dropped = 0usize;
    for skill in registry.list() {
        let summary = clamp_line(&skill.description, DESCRIPTION_BUDGET);
        let row = if summary.is_empty() {
            format!("- {} -> {}\n", skill.name, skill.path.display())
        } else {
            format!(
                "- {} — {} -> {}\n",
                skill.name,
                summary,
                skill.path.display()
            )
        };
        let cost = row.chars().count();
        if used + cost > INDEX_BUDGET {
            dropped += 1;
        } else {
            used += cost;
            block.push_str(&row);
        }
    }
    if dropped > 0 {
        block.push_str(&format!(
            "({dropped} more skills exist but were left out to keep this prompt small.)\n"
        ));
    }

    if !registry.warnings().is_empty() {
        block.push_str("\n### Skill load issues\n");
        for warning in registry.warnings().iter().take(WARNING_DISPLAY_CAP) {
            block.push_str("- ");
            block.push_str(&clamp_line(warning, DESCRIPTION_BUDGET));
            block.push('\n');
        }
    }

    Some(block)
}

/// Append the rendered skills index for `workspace` to a base system prompt.
/// Returns `base` untouched when no skills are installed.
#[must_use]
pub fn build_system_prompt(base: &str, workspace: &Path) -> String {
    match render_skills_block(&discover_in_workspace(workspace)) {
        Some(block) => format!("{base}\n\n{block}"),
        None => base.to_string(),
    }
}

/// Flatten `text` to a single whitespace-normalized line capped at `limit`
/// characters, appending `…` when anything was cut.
fn clamp_line(text: &str, limit: usize) -> String {
    let mut flat = String::with_capacity(text.len().min(limit + 8));
    for word in text.split_whitespace() {
        if !flat.is_empty() {
            flat.push(' ');
        }
        flat.push_str(word);
    }
    if flat.chars().count() <= limit {
        return flat;
    }
    let keep = limit.saturating_sub(1);
    let cut_at = flat
        .char_indices()
        .nth(keep)
        .map_or(flat.len(), |(byte, _)| byte);
    flat.truncate(cut_at);
    flat.push('…');
    flat
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Drop a minimal skill (`<root>/<dir_name>/SKILL.md`) on disk.
    fn plant_skill(root: &Path, dir_name: &str, skill_name: &str, summary: &str) {
        let home = root.join(dir_name);
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("SKILL.md"),
            format!("---\nname: {skill_name}\ndescription: {summary}\n---\ninstructions here\n"),
        )
        .unwrap();
    }

    #[test]
    fn discover_finds_direct_and_nested_skills() {
        let tmp = TempDir::new().unwrap();
        plant_skill(tmp.path(), "top", "top", "sits at the root");
        plant_skill(
            &tmp.path().join("vendor").join("acme"),
            "deep",
            "deep",
            "two levels down",
        );

        let registry = SkillRegistry::discover(tmp.path());
        assert_eq!(registry.len(), 2);
        assert!(registry.get("top").is_some());
        let deep = registry.get("deep").expect("nested skill found");
        assert!(deep.path.ends_with("vendor/acme/deep/SKILL.md"));
        assert!(registry.warnings().is_empty());
    }

    #[test]
    fn discover_returns_sorted_entries() {
        let tmp = TempDir::new().unwrap();
        plant_skill(tmp.path(), "zzz", "zeta", "last alphabetically");
        plant_skill(tmp.path(), "aaa", "alpha", "first alphabetically");

        let registry = SkillRegistry::discover(tmp.path());
        let names: Vec<&str> = registry
            .list()
            .iter()
            .map(|skill| skill.name.as_str())
            .collect();
        assert_eq!(names, ["alpha", "zeta"]);
    }

    #[test]
    fn skill_directories_are_leaves() {
        let tmp = TempDir::new().unwrap();
        plant_skill(tmp.path(), "outer", "outer", "the real skill");
        // A SKILL.md nested inside a skill directory is fixture material,
        // not an independently loadable skill.
        plant_skill(
            &tmp.path().join("outer"),
            "fixture",
            "inner",
            "must stay invisible",
        );

        let registry = SkillRegistry::discover(tmp.path());
        assert!(registry.get("outer").is_some());
        assert!(registry.get("inner").is_none());
    }

    #[test]
    fn hidden_directories_are_never_entered() {
        let tmp = TempDir::new().unwrap();
        plant_skill(
            &tmp.path().join(".git"),
            "hooks",
            "ghost",
            "inside vcs metadata",
        );
        plant_skill(tmp.path(), "real", "real", "visible");

        let registry = SkillRegistry::discover(tmp.path());
        assert!(registry.get("ghost").is_none());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn missing_root_yields_empty_registry() {
        let tmp = TempDir::new().unwrap();
        let registry = SkillRegistry::discover(&tmp.path().join("nope"));
        assert!(registry.is_empty());
        assert!(registry.warnings().is_empty());
    }

    #[test]
    fn malformed_manifest_warns_and_is_skipped() {
        let tmp = TempDir::new().unwrap();
        let bad = tmp.path().join("broken");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("SKILL.md"), "no fence at all\n").unwrap();

        let registry = SkillRegistry::discover(tmp.path());
        assert!(registry.is_empty());
        assert_eq!(registry.warnings().len(), 1);
        assert!(registry.warnings()[0].contains("SKILL.md"));
    }

    #[test]
    fn walk_stops_at_the_depth_ceiling() {
        let tmp = TempDir::new().unwrap();
        // Skill at depth MAX_WALK_DEPTH (scannable) and one a level below it.
        let mut within = tmp.path().to_path_buf();
        for level in 0..MAX_WALK_DEPTH {
            within = within.join(format!("lvl{level}"));
        }
        plant_skill(&within, "edge", "edge", "at the limit");
        plant_skill(
            &within.join("lvl-extra"),
            "past",
            "past",
            "beyond the limit",
        );

        let registry = SkillRegistry::discover(tmp.path());
        assert!(
            registry.get("edge").is_some(),
            "depth-limit skill must load"
        );
        assert!(
            registry.get("past").is_none(),
            "too-deep skill must not load"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cycles_terminate_without_duplicates() {
        let tmp = TempDir::new().unwrap();
        let nest = tmp.path().join("nest");
        plant_skill(&nest, "looped", "looped", "reachable through a cycle");
        std::os::unix::fs::symlink(tmp.path(), nest.join("back-up")).unwrap();

        let registry = SkillRegistry::discover(tmp.path());
        assert_eq!(registry.len(), 1, "cycle must not multiply skills");
    }

    #[test]
    fn workspace_merge_prefers_the_more_specific_root() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        plant_skill(
            &ws.join("skills"),
            "dc-test-dup",
            "dc-test-dup",
            "from workspace root",
        );
        plant_skill(
            &ws.join(".deep-code").join("skills"),
            "dc-test-dup",
            "dc-test-dup",
            "from dot dir",
        );
        plant_skill(
            &ws.join(".deep-code").join("skills"),
            "dc-test-solo",
            "dc-test-solo",
            "only in dot dir",
        );

        let merged = discover_in_workspace(ws);
        assert_eq!(
            merged.get("dc-test-dup").unwrap().description,
            "from workspace root",
            "workspace root must shadow .deep-code on name conflicts"
        );
        assert!(merged.get("dc-test-solo").is_some());
    }

    #[test]
    fn skills_directories_lists_only_existing_roots_in_order() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path();
        fs::create_dir_all(ws.join("skills")).unwrap();
        fs::create_dir_all(ws.join(".deep-code").join("skills")).unwrap();

        let roots = skills_directories(ws);
        assert_eq!(roots.first(), Some(&ws.join("skills")));
        assert_eq!(roots.get(1), Some(&ws.join(".deep-code").join("skills")));
    }

    #[test]
    fn rendered_block_lists_name_summary_and_path() {
        let tmp = TempDir::new().unwrap();
        plant_skill(tmp.path(), "review", "review", "check the diff carefully");

        let registry = SkillRegistry::discover(tmp.path());
        let block = render_skills_block(&registry).expect("non-empty registry renders");
        assert!(block.starts_with("## Skills\n"));
        assert!(block.contains("- review — check the diff carefully -> "));
        assert!(
            block.contains(
                &tmp.path()
                    .join("review")
                    .join("SKILL.md")
                    .display()
                    .to_string()
            )
        );
    }

    #[test]
    fn rendered_block_is_none_for_empty_registry() {
        assert!(render_skills_block(&SkillRegistry::default()).is_none());
    }

    #[test]
    fn rendered_block_clamps_long_descriptions() {
        let tmp = TempDir::new().unwrap();
        plant_skill(tmp.path(), "wordy", "wordy", &"w".repeat(1_000));

        let block = render_skills_block(&SkillRegistry::discover(tmp.path())).expect("renders");
        assert!(block.contains('…'), "clamped text carries an ellipsis");
        assert!(!block.contains(&"w".repeat(DESCRIPTION_BUDGET + 1)));
    }

    #[test]
    fn rendered_block_respects_the_index_budget() {
        let tmp = TempDir::new().unwrap();
        let filler = "f".repeat(DESCRIPTION_BUDGET - 10);
        for index in 0..120 {
            let name = format!("bulk-{index:03}");
            plant_skill(tmp.path(), &name, &name, &filler);
        }

        let block = render_skills_block(&SkillRegistry::discover(tmp.path())).expect("renders");
        assert!(block.contains("more skills exist but were left out"));
        assert!(
            block.chars().count() < INDEX_BUDGET + 2_000,
            "block must stay near the budget, got {}",
            block.chars().count()
        );
    }

    #[test]
    fn rendered_block_surfaces_load_warnings() {
        let tmp = TempDir::new().unwrap();
        plant_skill(tmp.path(), "fine", "fine", "loads normally");
        let bad = tmp.path().join("bad");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("SKILL.md"), "not a skill file").unwrap();

        let block = render_skills_block(&SkillRegistry::discover(tmp.path())).expect("renders");
        assert!(block.contains("### Skill load issues"));
    }

    #[test]
    fn system_prompt_gains_skills_block_when_skills_exist() {
        let tmp = TempDir::new().unwrap();
        plant_skill(
            &tmp.path().join("skills"),
            "prompt-demo",
            "prompt-demo",
            "appears in the prompt",
        );

        let prompt = build_system_prompt("Base instructions.", tmp.path());
        assert!(prompt.starts_with("Base instructions."));
        assert!(prompt.contains("prompt-demo"));
        assert!(prompt.contains("appears in the prompt"));
    }

    #[test]
    fn clamp_line_collapses_whitespace_and_caps_length() {
        assert_eq!(clamp_line("  a\t b\n  c ", 100), "a b c");
        let clamped = clamp_line(&"x".repeat(50), 10);
        assert_eq!(clamped.chars().count(), 10);
        assert!(clamped.ends_with('…'));
    }
}
