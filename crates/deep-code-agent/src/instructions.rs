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

/// The body of a `Compact Instructions` section: from its heading (any level,
/// ATX `## Title` or setext `Title` over `===`/`---`, title `Compact
/// Instructions`, `压缩说明` or `压缩指令`, case-insensitive, an ASCII or
/// full-width colon allowed) to the next heading of the same or a higher
/// level, or the end of the file. `None` when there is no such heading or the
/// section is blank. The section stays in the prompt block too — it is part of
/// the file — this is the copy the summarizer gets on its own.
///
/// Headings are read the way a Markdown renderer reads them, because the only
/// symptom of a miss is a missing `· 含压缩说明` on the welcome card: up to
/// three spaces of indentation, a tab after the `#`s, a closing `##` run.
/// Lines inside fenced code blocks are code, not structure — a `# comment` in
/// a shell example must not end the section early, an example of the heading
/// must not start one, and a fence only closes on its own character at least
/// as long as it opened, so a ```` block may quote a ``` one.
fn compact_instructions_section(body: &str) -> Option<String> {
    let is_title = |title: &str| {
        let title = title.trim().trim_end_matches([':', '：']).trim();
        title.eq_ignore_ascii_case("compact instructions")
            || title == "压缩说明"
            || title == "压缩指令"
    };
    let lines: Vec<&str> = body.lines().collect();
    let headings = markdown_headings(&lines);
    let (at, section) = headings
        .iter()
        .enumerate()
        .find(|(_, heading)| is_title(&heading.title))?;
    let end = headings[at + 1..]
        .iter()
        .find(|heading| heading.level <= section.level)
        .map_or(lines.len(), |heading| heading.first_line);
    let section = lines[section.last_line + 1..end].join("\n");
    let section = section.trim();
    (!section.is_empty()).then(|| section.to_string())
}

/// A heading and the lines it spans: one for an ATX heading, the paragraph
/// and its underline for a setext one, whose title is the whole paragraph.
#[derive(Debug)]
struct Heading {
    first_line: usize,
    last_line: usize,
    level: usize,
    title: String,
}

/// The file's headings, read the way a Markdown renderer reads them.
///
/// A setext underline turns the paragraph above it into a heading only when
/// that paragraph is not inside a list item or a quote: under one, `---` is a
/// rule, and taking it for a heading cut the section off there. A list item's
/// continuation lines — indented, lazy, or a paragraph after a blank line
/// indented to the item's content column — stay inside the item; a quote does
/// not reach past a blank line. An HTML comment or a link reference
/// definition is a block of its own, not part of the paragraph below it.
fn markdown_headings(lines: &[&str]) -> Vec<Heading> {
    let mut headings = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    let mut in_comment = false;
    // The open paragraph: its first line, and whether it is inside a list
    // item or a quote.
    let mut paragraph: Option<(usize, bool)> = None;
    // The content column of the open list item, if a list is open.
    let mut list_content: Option<usize> = None;
    for (index, line) in lines.iter().enumerate() {
        if let Some((marker, length)) = fence {
            if closes_fence(line, marker, length) {
                fence = None;
            }
            continue;
        }
        if in_comment {
            in_comment = !line.contains("-->");
            continue;
        }
        if line.trim().is_empty() {
            paragraph = None;
            continue;
        }
        // Four spaces or a tab: a paragraph's continuation, or indented code.
        let Some(rest) = block_indent(line) else {
            continue;
        };
        let indent = line.len() - rest.len();
        // A block starting left of the open item's content ends the list.
        let top_level = list_content.is_none_or(|column| indent < column);
        let ends_list = |list_content: &mut Option<usize>| {
            if top_level {
                *list_content = None;
            }
        };
        if let Some(open) = opens_fence(line) {
            fence = Some(open);
            paragraph = None;
            ends_list(&mut list_content);
            continue;
        }
        if let Some(comment) = rest.strip_prefix("<!--") {
            in_comment = !comment.contains("-->");
            paragraph = None;
            ends_list(&mut list_content);
            continue;
        }
        if paragraph.is_none() && is_link_definition(rest) {
            continue;
        }
        if let Some((level, title)) = atx_heading(line) {
            headings.push(Heading {
                first_line: index,
                last_line: index,
                level,
                title: title.to_string(),
            });
            paragraph = None;
            ends_list(&mut list_content);
            continue;
        }
        if let Some(level) = setext_underline(line) {
            match paragraph {
                Some((first, false)) => {
                    let title = lines[first..index]
                        .iter()
                        .map(|line| line.trim())
                        .collect::<Vec<_>>()
                        .join(" ");
                    headings.push(Heading {
                        first_line: first,
                        last_line: index,
                        level,
                        title,
                    });
                    paragraph = None;
                    list_content = None;
                    continue;
                }
                // `---` with nothing it may underline is a rule; a lone `===`
                // is text and falls through.
                _ if level == 2 => {
                    paragraph = None;
                    ends_list(&mut list_content);
                    continue;
                }
                _ => {}
            }
        }
        if is_thematic_break(rest) {
            paragraph = None;
            ends_list(&mut list_content);
            continue;
        }
        let item = list_item_content(rest);
        let quote = rest.starts_with('>');
        if let Some(offset) = item {
            list_content = Some(indent + offset);
        } else if paragraph.is_none() && top_level {
            list_content = None;
        }
        paragraph = Some(match paragraph {
            Some((first, contained)) => (first, contained || item.is_some() || quote),
            None => (index, item.is_some() || quote || !top_level),
        });
    }
    headings
}

/// The line past at most three spaces of indentation; `None` for four or
/// more (or a leading tab), which make an indented code line, not structure.
fn block_indent(line: &str) -> Option<&str> {
    let spaces = line.bytes().take_while(|byte| *byte == b' ').count();
    (spaces <= 3 && !line[spaces..].starts_with('\t')).then(|| &line[spaces..])
}

/// A fence opener: three or more backticks or tildes (a backtick fence's info
/// string may not contain a backtick). Its marker byte and length.
fn opens_fence(line: &str) -> Option<(u8, usize)> {
    let rest = block_indent(line)?;
    let marker = *rest.as_bytes().first()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let length = rest.bytes().take_while(|byte| *byte == marker).count();
    if length < 3 || (marker == b'`' && rest[length..].contains('`')) {
        return None;
    }
    Some((marker, length))
}

/// The closer for a fence opened with `length` × `marker`: the same marker,
/// at least as many, nothing after them but spaces.
fn closes_fence(line: &str, marker: u8, length: usize) -> bool {
    let Some(rest) = block_indent(line) else {
        return false;
    };
    let run = rest.bytes().take_while(|byte| *byte == marker).count();
    run >= length && rest[run..].trim().is_empty()
}

/// An ATX heading's level and title, the closing `#` run removed.
fn atx_heading(line: &str) -> Option<(usize, &str)> {
    let rest = block_indent(line)?;
    let hashes = rest.bytes().take_while(|byte| *byte == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let after = &rest[hashes..];
    if !after.is_empty() && !after.starts_with([' ', '\t']) {
        return None;
    }
    let title = after.trim();
    let unclosed = title.trim_end_matches('#');
    let title = if unclosed.is_empty() {
        ""
    } else if unclosed.ends_with([' ', '\t']) {
        unclosed.trim_end()
    } else {
        title
    };
    Some((hashes, title))
}

/// A setext underline's level: a run of `=` (1) or `-` (2), nothing else.
fn setext_underline(line: &str) -> Option<usize> {
    let rest = block_indent(line)?.trim_end();
    let marker = rest.chars().next()?;
    let level = match marker {
        '=' => 1,
        '-' => 2,
        _ => return None,
    };
    rest.chars().all(|ch| ch == marker).then_some(level)
}

/// Where a list item's content starts, counted from its marker: the marker,
/// then the spaces after it (one, when the item is empty or the spaces run to
/// five or more). `None` when the line opens no list item.
fn list_item_content(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    let marker = if matches!(bytes.first(), Some(b'-' | b'*' | b'+')) {
        1
    } else {
        let digits = bytes
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if !(1..=9).contains(&digits) || !matches!(bytes.get(digits), Some(b'.' | b')')) {
            return None;
        }
        digits + 1
    };
    let after = &rest[marker..];
    if after.trim().is_empty() {
        return Some(marker + 1);
    }
    let spaces = after
        .bytes()
        .take_while(|byte| matches!(byte, b' ' | b'\t'))
        .count();
    match spaces {
        0 => None,
        1..=4 => Some(marker + spaces),
        _ => Some(marker + 1),
    }
}

/// A link reference definition's first line (`[label]: destination`): a
/// block of its own when it does not continue a paragraph.
fn is_link_definition(rest: &str) -> bool {
    rest.strip_prefix('[')
        .and_then(|after| after.split_once("]:"))
        .is_some_and(|(label, destination)| {
            !label.trim().is_empty() && !label.contains(']') && !destination.trim().is_empty()
        })
}

/// A thematic break: three or more `*`, `-` or `_`, the same one, with only
/// spaces or tabs between them.
fn is_thematic_break(rest: &str) -> bool {
    let Some(first) = rest.chars().find(|ch| !matches!(ch, ' ' | '\t')) else {
        return false;
    };
    matches!(first, '*' | '-' | '_')
        && rest
            .chars()
            .all(|ch| ch == first || ch == ' ' || ch == '\t')
        && rest.chars().filter(|ch| *ch == first).count() >= 3
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
    // An editor's byte-order mark is not text: left in, it sat in front of a
    // first-line heading and hid it from the section finder.
    let text = text.trim_start_matches('\u{feff}').trim();
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

    /// Headings are read the way a Markdown renderer reads them. Every case
    /// here used to drop the section, or cut it, without a word — the only
    /// symptom a missing `· 含压缩说明` on the welcome card.
    #[test]
    fn compact_instructions_headings_are_read_like_markdown() {
        let cases: [(&str, &str, Option<&str>); 12] = [
            (
                "a byte-order mark before a first-line heading",
                "\u{feff}## Compact Instructions\n- keep X\n## Next\ny",
                Some("- keep X"),
            ),
            (
                "a full-width colon",
                "# R\n## 压缩说明：\n- keep X\n## Next\ny",
                Some("- keep X"),
            ),
            (
                "an indented heading",
                "# R\n   ## Compact Instructions\n- keep X\n## Next\ny",
                Some("- keep X"),
            ),
            (
                "four spaces make code, not a heading",
                "# R\n    ## Compact Instructions\n- keep X\n",
                None,
            ),
            (
                "a closing hash run",
                "# R\n## Compact Instructions ##\n- keep X\n## Next\ny",
                Some("- keep X"),
            ),
            (
                "a tab after the hashes",
                "# R\n##\tCompact Instructions\n- keep X\n## Next\ny",
                Some("- keep X"),
            ),
            (
                "a longer fence quoting a shorter one",
                "# R\n## Compact Instructions\n- keep X\n````md\n```sh\n# not a heading\n```\n````\n- keep Y\n## Next\ny",
                Some("- keep X\n````md\n```sh\n# not a heading\n```\n````\n- keep Y"),
            ),
            (
                "a tilde line inside a backtick fence",
                "# R\n## Compact Instructions\n- keep X\n```md\n~~~\n# not a heading\n~~~\n```\n- keep Y\n## Next\ny",
                Some("- keep X\n```md\n~~~\n# not a heading\n~~~\n```\n- keep Y"),
            ),
            (
                "a setext heading after the section ends it",
                "# R\n## Compact Instructions\n- keep X\n\nOther\n=====\nsecret stuff",
                Some("- keep X"),
            ),
            (
                "a setext heading can be the section's own",
                "Compact Instructions\n---\n- keep X\n\nNext\n----\ny",
                Some("- keep X"),
            ),
            (
                "a rule under a list item is not a heading",
                "## Compact Instructions\n- keep X\n---\n- keep Y\n## Next\ny",
                Some("- keep X\n---\n- keep Y"),
            ),
            (
                "a rule after a blank line is not a heading",
                "## Compact Instructions\n- keep X\n\n---\n- keep Y\n## Next\ny",
                Some("- keep X\n\n---\n- keep Y"),
            ),
        ];
        for (case, body, expected) in cases {
            let dir = TempDir::new().unwrap();
            write(&dir, "ws", body);
            assert_eq!(
                load(&dir, true).compact_instructions.as_deref(),
                expected,
                "{case}"
            );
        }
    }

    /// What is NOT inside a list item or a quote is not taken for it: a quote
    /// ends at a blank line, a list item reaches only as far as its content
    /// column, and a fence or a rule at the top level ends the list. Each of
    /// these used to let the section run past the next real heading and hand
    /// the following section to the summarizer as standing instructions. An
    /// HTML comment or a link definition above a setext title is not part of
    /// its paragraph.
    #[test]
    fn a_heading_after_a_list_or_quote_still_ends_the_section() {
        let cases: [(&str, &str, Option<&str>); 7] = [
            (
                "a quote, a blank line, an indented heading",
                "## Compact Instructions\n> keep X\n\n  Next\n  ----\nsecret",
                Some("> keep X"),
            ),
            (
                "an ordered item, a heading indented short of its content",
                "## Compact Instructions\n1. keep X\n\n  Next\n  ----\nsecret",
                Some("1. keep X"),
            ),
            (
                "a bullet item, a heading indented short of its content",
                "## Compact Instructions\n- keep X\n\n Next\n ----\nsecret",
                Some("- keep X"),
            ),
            (
                "a list closed by a top-level fence",
                "## Compact Instructions\n- keep X\n```sh\nls\n```\n  Next\n  ----\nsecret",
                Some("- keep X\n```sh\nls\n```"),
            ),
            (
                "a list closed by a top-level rule",
                "## Compact Instructions\n- keep X\n\n* * *\n\n  Next\n  ----\nsecret",
                Some("- keep X\n\n* * *"),
            ),
            (
                "an HTML comment above a setext title",
                "<!-- keep -->\nCompact Instructions\n---\n- keep X",
                Some("- keep X"),
            ),
            (
                "a link definition above a setext title",
                "[ref]: https://example.com\nCompact Instructions\n---\n- keep X",
                Some("- keep X"),
            ),
        ];
        for (case, body, expected) in cases {
            let dir = TempDir::new().unwrap();
            write(&dir, "ws", body);
            assert_eq!(
                load(&dir, true).compact_instructions.as_deref(),
                expected,
                "{case}"
            );
        }
    }

    /// A `---` under a list item's or a quote's text is a rule, not a heading
    /// that ends the section, and a setext heading's title is its whole
    /// paragraph — the last line alone made a false section.
    #[test]
    fn a_rule_inside_a_list_or_quote_does_not_end_the_section() {
        let cases: [(&str, &str, Option<&str>); 5] = [
            (
                "an indented continuation line",
                "## Compact Instructions\n- keep the failing test names,\n  and the commands that reproduce them\n---\n- keep Y\n## Next\ny",
                Some(
                    "- keep the failing test names,\n  and the commands that reproduce them\n---\n- keep Y",
                ),
            ),
            (
                "a lazy continuation line",
                "## Compact Instructions\n- keep X\nand its reason\n---\n- keep Y\n## Next\ny",
                Some("- keep X\nand its reason\n---\n- keep Y"),
            ),
            (
                "a quote's continuation line",
                "## Compact Instructions\n> keep X\nand its reason\n---\n- keep Y\n## Next\ny",
                Some("> keep X\nand its reason\n---\n- keep Y"),
            ),
            (
                "an item's indented paragraph after a blank line",
                "## Compact Instructions\n- keep X\n\n  more about X\n---\n- keep Y\n## Next\ny",
                Some("- keep X\n\n  more about X\n---\n- keep Y"),
            ),
            (
                "a two-line paragraph over a rule",
                "# R\nSome intro\nCompact Instructions\n---\n- keep X\n",
                None,
            ),
        ];
        for (case, body, expected) in cases {
            let dir = TempDir::new().unwrap();
            write(&dir, "ws", body);
            assert_eq!(
                load(&dir, true).compact_instructions.as_deref(),
                expected,
                "{case}"
            );
        }
    }
}
