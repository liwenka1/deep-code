//! Project instructions: an `AGENTS.md` read once per session and injected
//! into the system prompt.
//!
//! Two files are read, in this order: the user's own
//! `~/.deep-code/AGENTS.md` (global — conventions that follow the person) and
//! the workspace root's `AGENTS.md` (workspace — conventions that follow the
//! repository). Neither is required. They are read when a runtime is launched
//! — at startup, on `/clear`, and on resume — and not again: the prompt is a
//! persisted record entry and the head of the provider's cached prefix, so an
//! edit takes effect in the next conversation. That is also what the other
//! terminal agents do; none of them watches the file mid-session.
//!
//! Instructions are context, not permission. Nothing here reaches the
//! execution policy, the approval gate or the sandbox: a repository can tell
//! the model how to work and cannot widen what it may do — the same posture
//! as the skills index, which also puts repository text in the prompt. The
//! auto-mode judge builds its own prompt and never sees this block.
//!
//! The workspace copy is repository content, and is distrusted on one axis
//! beyond that: a symlinked `AGENTS.md` is refused. A checkout can carry
//! `AGENTS.md -> ~/.ssh/id_rsa`, and reading through it would put the target
//! into the system prompt and send it to the provider on the first request.
//! The global copy lives in the user's own directory and may be a symlink
//! (dotfiles repositories do that).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The file name looked for in both locations.
pub const INSTRUCTIONS_FILE: &str = "AGENTS.md";

/// Total instruction characters kept across both files. Everything past it is
/// dropped, with a note in the prompt and a launch warning for the human. Sized
/// so a long, hand-written file fits whole and a mistaken paste of a whole
/// manual does not take the prompt with it; what the other agents keep sits in
/// the same range (a 32 KiB cap on one side, a "stay under 200 lines" rule on
/// the other).
pub const MAX_INSTRUCTIONS_CHARS: usize = 24_000;

/// Cap on the compaction instructions pulled out of the files. They ride on
/// every handover request; a page of them is a page the summarizer reads
/// before the transcript.
pub const MAX_COMPACT_INSTRUCTIONS_CHARS: usize = 4_000;

/// Bytes read from either file before giving up on the rest. Well above the
/// character cap in any encoding, so a file this large is cut by the cap
/// anyway; this only keeps a multi-gigabyte path from being read whole.
const MAX_READ_BYTES: u64 = 256 * 1024;

const BLOCK_HEADING: &str = "# Project instructions (AGENTS.md)";
const GLOBAL_HEADING: &str = "## Global instructions (~/.deep-code/AGENTS.md)";
const WORKSPACE_HEADING: &str = "## Workspace instructions (AGENTS.md at the workspace root)";
const TRUNCATION_NOTE: &str = "[AGENTS.md truncated: the instructions exceed the 24000-character \
limit, and the rest was not loaded]";

/// Which of the two locations a source came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionScope {
    Global,
    Workspace,
}

/// One file that contributed to the block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionSource {
    pub scope: InstructionScope,
    pub path: PathBuf,
    /// Lines actually kept (after truncation, if any) — what a welcome card
    /// can honestly report as loaded.
    pub lines: usize,
}

/// The loaded instructions: the prompt block, where it came from, and what
/// went wrong on the way. `Default` is "no instructions", which is also what
/// [`Self::load`] returns when neither file exists.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectInstructions {
    block: Option<String>,
    pub sources: Vec<InstructionSource>,
    /// Some content was dropped at [`MAX_INSTRUCTIONS_CHARS`].
    pub truncated: bool,
    /// Human-facing problems: a file that could not be read, a refused
    /// symlink, the truncation. Launch surfaces them with its other warnings.
    pub warnings: Vec<String>,
    /// The `## Compact Instructions` (or `## 压缩说明`) sections of the files,
    /// global then workspace, for the compaction summarizer: what this project
    /// must not lose when history is folded. `None` when no file has one.
    pub compact_instructions: Option<String>,
}

impl ProjectInstructions {
    /// Read `~/.deep-code/AGENTS.md` and `<workspace>/AGENTS.md`.
    #[must_use]
    pub fn load(workspace: &Path) -> Self {
        let global = crate::paths::home_dir().map(|home| {
            home.join(crate::paths::DEEP_CODE_DIR)
                .join(INSTRUCTIONS_FILE)
        });
        Self::load_from(global.as_deref(), &workspace.join(INSTRUCTIONS_FILE))
    }

    /// [`Self::load`] with both paths explicit, so tests do not depend on the
    /// developer's own home directory.
    pub(crate) fn load_from(global: Option<&Path>, workspace_file: &Path) -> Self {
        let mut out = Self::default();
        let mut budget = MAX_INSTRUCTIONS_CHARS;
        let mut sections = Vec::new();
        let mut compact: Vec<String> = Vec::new();
        let candidates = global
            .map(|path| (InstructionScope::Global, path.to_path_buf()))
            .into_iter()
            .chain(std::iter::once((
                InstructionScope::Workspace,
                workspace_file.to_path_buf(),
            )));
        for (scope, path) in candidates {
            let body = match read_instructions(&path, scope) {
                Ok(Some(body)) => body,
                Ok(None) => continue,
                Err(warning) => {
                    out.warnings.push(warning);
                    continue;
                }
            };
            if let Some(section) = compact_instructions_section(&body) {
                compact.push(section);
            }
            let total = body.chars().count();
            let kept: String = body.chars().take(budget).collect();
            let kept_chars = kept.chars().count();
            let cut = kept_chars < total;
            budget -= kept_chars;
            if !kept.trim().is_empty() {
                out.sources.push(InstructionSource {
                    scope,
                    path: path.clone(),
                    lines: kept.lines().count(),
                });
                sections.push(render_section(scope, kept.trim_end(), cut));
            }
            if cut {
                out.truncated = true;
                out.warnings.push(format!(
                    "{}: instructions exceed the {MAX_INSTRUCTIONS_CHARS}-character limit \
                     ({total} characters in total across AGENTS.md files); the rest was not loaded",
                    path.display()
                ));
            }
        }
        if !sections.is_empty() {
            out.block = Some(format!(
                "{BLOCK_HEADING}\n\n{sections}",
                sections = sections.join("\n\n")
            ));
        }
        if !compact.is_empty() {
            out.compact_instructions = Some(crate::text_util::truncate_chars(
                &compact.join("\n\n"),
                MAX_COMPACT_INSTRUCTIONS_CHARS,
            ));
        }
        out
    }

    /// The prompt block, or `None` when no file contributed anything.
    #[must_use]
    pub fn block(&self) -> Option<&str> {
        self.block.as_deref()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.block.is_none()
    }
}

/// The body of a `Compact Instructions` section: from the heading line
/// (`#`–`######`, title `Compact Instructions`, `压缩说明` or `压缩指令`,
/// case-insensitive) to the next heading of the same or a higher level, or
/// the end of the file. `None` when there is no such heading or the section
/// is blank. The section stays in the prompt block too — it is part of the
/// file — this is the copy the summarizer gets on its own.
///
/// Lines inside fenced code blocks are code, not structure: a `# comment` in
/// a shell example must not end the section early, and an example of the
/// heading itself must not start one.
fn compact_instructions_section(body: &str) -> Option<String> {
    fn heading_level(line: &str) -> Option<(usize, &str)> {
        let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
        ((1..=6).contains(&hashes) && line[hashes..].starts_with(' '))
            .then(|| (hashes, line[hashes..].trim()))
    }
    let is_title = |title: &str| {
        let title = title.trim().trim_end_matches(':').trim();
        title.eq_ignore_ascii_case("compact instructions")
            || title == "压缩说明"
            || title == "压缩指令"
    };
    let lines: Vec<&str> = body.lines().collect();
    let mut in_fence = false;
    let headings: Vec<Option<(usize, &str)>> = lines
        .iter()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                in_fence = !in_fence;
                return None;
            }
            (!in_fence).then(|| heading_level(line)).flatten()
        })
        .collect();
    let start = headings
        .iter()
        .position(|heading| heading.is_some_and(|(_, title)| is_title(title)))?;
    let level = headings[start]?.0;
    let end = headings[start + 1..]
        .iter()
        .position(|heading| heading.is_some_and(|(found, _)| found <= level))
        .map_or(lines.len(), |offset| start + 1 + offset);
    let section = lines[start + 1..end].join("\n");
    let section = section.trim();
    (!section.is_empty()).then(|| section.to_string())
}

fn render_section(scope: InstructionScope, body: &str, cut: bool) -> String {
    let heading = match scope {
        InstructionScope::Global => GLOBAL_HEADING,
        InstructionScope::Workspace => WORKSPACE_HEADING,
    };
    if cut {
        format!("{heading}\n\n{body}\n\n{TRUNCATION_NOTE}")
    } else {
        format!("{heading}\n\n{body}")
    }
}

/// The file's text, normalized (CRLF → LF, outer whitespace trimmed); `None`
/// when it does not exist or is empty; `Err` with the human-facing reason when
/// it exists but must not or cannot be read.
fn read_instructions(path: &Path, scope: InstructionScope) -> Result<Option<String>, String> {
    let entry = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "{}: cannot read ({error}); ignored",
                path.display()
            ));
        }
    };
    if entry.file_type().is_symlink() && scope == InstructionScope::Workspace {
        return Err(format!(
            "{}: is a symbolic link; a workspace AGENTS.md must be a regular file (ignored)",
            path.display()
        ));
    }
    let target = match fs::metadata(path) {
        Ok(metadata) => metadata,
        // A dangling global symlink: there is nothing to read, and the user
        // put the link there, so say so rather than pretend the file is absent.
        Err(error) => {
            return Err(format!(
                "{}: cannot read ({error}); ignored",
                path.display()
            ));
        }
    };
    if !target.is_file() {
        return Err(format!("{}: not a regular file; ignored", path.display()));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| file.take(MAX_READ_BYTES).read_to_end(&mut bytes))
        .map_err(|error| format!("{}: cannot read ({error}); ignored", path.display()))?;
    let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn load(dir: &TempDir, global: bool) -> ProjectInstructions {
        let global_path = dir.path().join("home").join(INSTRUCTIONS_FILE);
        ProjectInstructions::load_from(
            global.then_some(global_path.as_path()),
            &dir.path().join("ws").join(INSTRUCTIONS_FILE),
        )
    }

    fn write(dir: &TempDir, which: &str, text: &str) -> PathBuf {
        let path = dir.path().join(which).join(INSTRUCTIONS_FILE);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn no_files_means_no_block_and_no_noise() {
        let dir = TempDir::new().unwrap();
        let loaded = load(&dir, true);
        assert_eq!(loaded, ProjectInstructions::default());
        assert!(loaded.is_empty());
        assert_eq!(loaded.block(), None);
    }

    #[test]
    fn a_workspace_file_is_loaded_normalized_under_its_heading() {
        let dir = TempDir::new().unwrap();
        let path = write(&dir, "ws", "\n\n# Rules\r\n- run cargo test\r\n\n\n");
        let loaded = load(&dir, true);
        assert_eq!(
            loaded.block(),
            Some(
                "# Project instructions (AGENTS.md)\n\n\
                 ## Workspace instructions (AGENTS.md at the workspace root)\n\n\
                 # Rules\n- run cargo test"
            )
        );
        assert_eq!(
            loaded.sources,
            vec![InstructionSource {
                scope: InstructionScope::Workspace,
                path,
                lines: 2,
            }]
        );
        assert!(!loaded.truncated);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    }

    /// The person's conventions come first, the repository's second — the
    /// later text wins the model's attention on a conflict, and the
    /// repository is the more specific of the two.
    #[test]
    fn global_comes_before_workspace() {
        let dir = TempDir::new().unwrap();
        write(&dir, "home", "always answer in English");
        write(&dir, "ws", "tests live in tests.rs");
        let loaded = load(&dir, true);
        let block = loaded.block().unwrap();
        let global_at = block.find("## Global instructions").unwrap();
        let workspace_at = block.find("## Workspace instructions").unwrap();
        assert!(global_at < workspace_at, "{block}");
        assert!(block.find("always answer in English").unwrap() < workspace_at);
        assert_eq!(
            loaded
                .sources
                .iter()
                .map(|source| source.scope)
                .collect::<Vec<_>>(),
            [InstructionScope::Global, InstructionScope::Workspace]
        );

        // Without a global location at all (no home dir) the workspace file
        // alone is loaded.
        let alone = load(&dir, false);
        assert_eq!(alone.sources.len(), 1);
        assert!(!alone.block().unwrap().contains("Global"));
    }

    #[test]
    fn an_empty_or_blank_file_is_not_a_source() {
        let dir = TempDir::new().unwrap();
        write(&dir, "ws", " \n\t\r\n");
        let loaded = load(&dir, true);
        assert!(loaded.is_empty());
        assert!(loaded.sources.is_empty());
        assert!(loaded.warnings.is_empty());
    }

    /// The cap is one budget across both files: whatever crosses it is cut
    /// there, the block says so where the cut happened, the report flags it,
    /// and the human gets a warning naming the file. The kept line count is
    /// the truth after the cut.
    #[test]
    fn the_character_cap_is_shared_and_announced_everywhere() {
        let dir = TempDir::new().unwrap();
        let global_line = "g".repeat(99) + "\n";
        write(&dir, "home", &global_line.repeat(200)); // 20_000 chars
        let workspace_line = "w".repeat(99) + "\n";
        let workspace = write(&dir, "ws", &workspace_line.repeat(100)); // 10_000 chars
        let loaded = load(&dir, true);
        assert!(loaded.truncated);
        let block = loaded.block().unwrap();
        assert_eq!(block.matches(TRUNCATION_NOTE).count(), 1);
        assert!(
            block.find(TRUNCATION_NOTE).unwrap() > block.find("## Workspace").unwrap(),
            "the note belongs to the file that was cut"
        );
        // 20_000 global + 10_000 workspace: the workspace keeps 4_000 chars of
        // its 10_000 (the final newline of the 20_000 is trimmed, so one more
        // char remains for it).
        let kept: usize = block
            .split("## Workspace instructions (AGENTS.md at the workspace root)\n\n")
            .nth(1)
            .unwrap()
            .split("\n\n[AGENTS.md truncated")
            .next()
            .unwrap()
            .chars()
            .count();
        assert!((3_990..=4_001).contains(&kept), "kept {kept}");
        assert_eq!(loaded.sources.len(), 2);
        assert_eq!(loaded.sources[0].lines, 200);
        assert!(
            (40..=41).contains(&loaded.sources[1].lines),
            "{:?}",
            loaded.sources[1]
        );
        assert_eq!(loaded.warnings.len(), 1);
        assert!(
            loaded.warnings[0].contains(&workspace.display().to_string())
                && loaded.warnings[0].contains("24000"),
            "{:?}",
            loaded.warnings
        );

        // A single oversized file: cut, noted, and still a source.
        let dir = TempDir::new().unwrap();
        write(&dir, "ws", &"x".repeat(MAX_INSTRUCTIONS_CHARS + 1));
        let loaded = load(&dir, true);
        assert!(loaded.truncated);
        assert_eq!(loaded.sources.len(), 1);
        assert!(loaded.block().unwrap().ends_with(TRUNCATION_NOTE));

        // Exactly at the cap: whole, and nothing said.
        let dir = TempDir::new().unwrap();
        write(&dir, "ws", &"x".repeat(MAX_INSTRUCTIONS_CHARS));
        let loaded = load(&dir, true);
        assert!(!loaded.truncated);
        assert!(loaded.warnings.is_empty());
    }

    /// A global file that already spends the whole budget leaves nothing for
    /// the workspace file: it is reported as not loaded, not silently absent.
    #[test]
    fn a_workspace_file_with_no_budget_left_is_reported_not_dropped() {
        let dir = TempDir::new().unwrap();
        write(&dir, "home", &"g".repeat(MAX_INSTRUCTIONS_CHARS));
        let workspace = write(&dir, "ws", "important rule");
        let loaded = load(&dir, true);
        assert!(loaded.truncated);
        assert_eq!(loaded.sources.len(), 1, "{:?}", loaded.sources);
        assert!(!loaded.block().unwrap().contains("important rule"));
        assert!(
            loaded
                .warnings
                .iter()
                .any(|warning| warning.contains(&workspace.display().to_string())),
            "{:?}",
            loaded.warnings
        );
    }

    #[test]
    fn a_directory_named_like_the_file_warns_and_is_ignored() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join("ws").join(INSTRUCTIONS_FILE)).unwrap();
        let loaded = load(&dir, true);
        assert!(loaded.is_empty());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(
            loaded.warnings[0].contains("not a regular file"),
            "{:?}",
            loaded.warnings
        );
    }

    /// A workspace `AGENTS.md` that is a symlink is refused — a checkout can
    /// point it at anything on the machine, and the target would go to the
    /// provider in the system prompt. The global one is the user's own and
    /// may be a link.
    #[test]
    fn a_symlinked_workspace_file_is_refused_but_a_global_link_is_followed() {
        let dir = TempDir::new().unwrap();
        let secret = dir.path().join("secret.txt");
        fs::write(&secret, "PRIVATE KEY MATERIAL").unwrap();
        fs::create_dir_all(dir.path().join("ws")).unwrap();
        fs::create_dir_all(dir.path().join("home")).unwrap();
        let workspace_link = dir.path().join("ws").join(INSTRUCTIONS_FILE);
        if !crate::test_symlinks::symlink_file_for_test(&secret, &workspace_link) {
            return;
        }
        let global_link = dir.path().join("home").join(INSTRUCTIONS_FILE);
        assert!(crate::test_symlinks::symlink_file_for_test(
            &secret,
            &global_link
        ));
        let loaded = load(&dir, true);
        let block = loaded.block().unwrap();
        assert!(
            block.contains("## Global instructions") && block.contains("PRIVATE KEY MATERIAL"),
            "the user's own link is followed: {block}"
        );
        assert!(
            !block.contains("## Workspace instructions"),
            "the workspace link must not be read: {block}"
        );
        assert_eq!(loaded.sources.len(), 1);
        assert_eq!(loaded.warnings.len(), 1);
        assert!(
            loaded.warnings[0].contains("symbolic link")
                && loaded.warnings[0].contains(&workspace_link.display().to_string()),
            "{:?}",
            loaded.warnings
        );
    }

    /// The `Compact Instructions` section is lifted out for the summarizer —
    /// from either file, in order, under any heading level or spelling the
    /// docs name — and nothing else of the file comes with it.
    #[test]
    fn compact_instructions_are_extracted_from_both_files() {
        let dir = TempDir::new().unwrap();
        write(
            &dir,
            "home",
            "# Me\nterse answers\n\n### compact instructions:\n- keep my vetoes\n\n### Other\nnope",
        );
        write(
            &dir,
            "ws",
            "# Repo\nrules\n\n## 压缩说明\n- 保留未推送的 commit 列表\n- keep failing test names\n\n## Commands\ncargo test\n",
        );
        let loaded = load(&dir, true);
        assert_eq!(
            loaded.compact_instructions.as_deref(),
            Some("- keep my vetoes\n\n- 保留未推送的 commit 列表\n- keep failing test names")
        );
        // Still part of the prompt block: it is text in the file like any other.
        assert!(loaded.block().unwrap().contains("保留未推送的 commit 列表"));

        let dir = TempDir::new().unwrap();
        write(&dir, "ws", "# Repo\n## Compact Instructions\n\n## Next\nx");
        assert_eq!(
            load(&dir, true).compact_instructions,
            None,
            "a blank section is none"
        );
        let dir = TempDir::new().unwrap();
        write(
            &dir,
            "ws",
            "# Repo\n```md\n## Compact Instructions\nan example, not the section\n```\n\
             ## Compact Instructions\n- keep this\n```sh\n# the gate, not a heading\n\
             scripts/preflight.sh\n```\n## Next\nx\n",
        );
        assert_eq!(
            load(&dir, true).compact_instructions.as_deref(),
            Some("- keep this\n```sh\n# the gate, not a heading\nscripts/preflight.sh\n```"),
            "fenced code is neither a heading nor the end of the section"
        );
        let dir = TempDir::new().unwrap();
        write(
            &dir,
            "ws",
            "# Repo\nmentions compact instructions in prose\n",
        );
        assert_eq!(
            load(&dir, true).compact_instructions,
            None,
            "prose is not a heading"
        );
        let dir = TempDir::new().unwrap();
        write(
            &dir,
            "ws",
            &format!(
                "## Compact Instructions\n{}",
                "k".repeat(MAX_COMPACT_INSTRUCTIONS_CHARS + 10)
            ),
        );
        let long = load(&dir, true).compact_instructions.unwrap();
        assert_eq!(
            long.chars().count(),
            MAX_COMPACT_INSTRUCTIONS_CHARS + 1,
            "capped, with the ellipsis"
        );
    }
}
