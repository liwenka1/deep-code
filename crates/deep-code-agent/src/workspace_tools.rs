use std::fs;
use std::path::PathBuf;

use async_trait::async_trait;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::tool::{Tool, ToolCx, ToolError, ToolOutput, ToolRegistry, run_blocking};
use crate::workspace_policy::{WorkspacePolicy, contains_symlink, invalid, json_string};

const DEFAULT_READ_LINES: usize = 200;
const MAX_READ_LINES: usize = 500;
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const DEFAULT_GREP_RESULTS: usize = 100;
const MAX_GREP_RESULTS: usize = 500;
const DEFAULT_CONTEXT_LINES: usize = 2;

#[derive(Debug, Clone)]
pub struct WorkspaceTools {
    root: WorkspacePolicy,
}

impl WorkspaceTools {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, ToolError> {
        Ok(Self {
            root: WorkspacePolicy::new(root)?,
        })
    }

    pub fn into_registry(self) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(ReadFileTool::new(self.root.clone()));
        registry.register(ListDirTool::new(self.root.clone()));
        registry.register(GrepFilesTool::new(self.root.clone()));
        registry.register(WriteFileTool::new(self.root.clone()));
        registry.register(ApplyPatchTool::new(self.root));
        registry
    }
}

pub fn workspace_tool_registry(root: impl Into<PathBuf>) -> Result<ToolRegistry, ToolError> {
    Ok(WorkspaceTools::new(root)?.into_registry())
}

#[derive(Debug, Clone)]
struct ReadFileTool {
    root: WorkspacePolicy,
}

impl ReadFileTool {
    const NAME: &'static str = "read_file";

    fn new(root: WorkspacePolicy) -> Self {
        Self { root }
    }

    fn read_sync(&self, params: ReadFileParams) -> Result<ToolOutput, ToolError> {
        let start_line = params.start_line.unwrap_or(1);
        if start_line == 0 {
            return Err(invalid(Self::NAME, "start_line must be greater than 0"));
        }
        let max_lines = params
            .max_lines
            .unwrap_or(DEFAULT_READ_LINES)
            .clamp(1, MAX_READ_LINES);
        let path = self.root.resolve_existing(&params.path, Self::NAME)?;
        let metadata = fs::metadata(&path).map_err(|error| {
            ToolError::exec_failed(
                Self::NAME,
                format!("failed to read metadata for {}: {error}", path.display()),
            )
        })?;
        if !metadata.is_file() {
            return Err(invalid(Self::NAME, "path is not a file"));
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err(ToolError::exec_failed(
                Self::NAME,
                format!(
                    "{} is larger than the current 2 MiB read limit",
                    self.root.relative_display(&path)
                ),
            ));
        }
        let contents = fs::read_to_string(&path).map_err(|error| {
            ToolError::exec_failed(
                Self::NAME,
                format!("failed to read {} as UTF-8: {error}", path.display()),
            )
        })?;
        let lines = contents.lines().collect::<Vec<_>>();
        let total_lines = lines.len();
        let start_index = start_line.saturating_sub(1);
        let selected = lines
            .iter()
            .enumerate()
            .skip(start_index)
            .take(max_lines)
            .map(|(index, line)| json!({"line": index + 1, "text": line}))
            .collect::<Vec<_>>();
        let next_start_line = if start_index + selected.len() < total_lines {
            Some(start_index + selected.len() + 1)
        } else {
            None
        };
        Ok(ToolOutput::text(json_string(json!({
            "path": self.root.relative_display(&path),
            "total_lines": total_lines,
            "start_line": start_line,
            "max_lines": max_lines,
            "truncated": next_start_line.is_some(),
            "next_start_line": next_start_line,
            "lines": selected
        }))))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadFileParams {
    /// Workspace-relative file path
    path: String,
    /// 1-based line number, default 1
    start_line: Option<usize>,
    /// Maximum lines to return, default 200, max 500
    max_lines: Option<usize>,
}

#[async_trait]
impl Tool for ReadFileTool {
    type Params = ReadFileParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Read a UTF-8 file from the workspace. Supports start_line and max_lines for bounded reads."
    }

    async fn run(&self, params: ReadFileParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.read_sync(params)).await
    }
}

#[derive(Debug, Clone)]
struct ListDirTool {
    root: WorkspacePolicy,
}

impl ListDirTool {
    const NAME: &'static str = "list_dir";

    fn new(root: WorkspacePolicy) -> Self {
        Self { root }
    }

    fn list_sync(&self, params: ListDirParams) -> Result<ToolOutput, ToolError> {
        let path_arg = params.path.as_deref().unwrap_or(".");
        let path = self.root.resolve_existing(path_arg, Self::NAME)?;
        if !path.is_dir() {
            return Err(invalid(Self::NAME, "path is not a directory"));
        }
        let mut entries = fs::read_dir(&path)
            .map_err(|error| {
                ToolError::exec_failed(
                    Self::NAME,
                    format!("failed to list {}: {error}", path.display()),
                )
            })?
            .map(|entry| {
                let entry = entry.map_err(|error| {
                    ToolError::exec_failed(
                        Self::NAME,
                        format!("failed to read directory entry: {error}"),
                    )
                })?;
                let file_type = entry.file_type().map_err(|error| {
                    ToolError::exec_failed(
                        Self::NAME,
                        format!("failed to read entry type: {error}"),
                    )
                })?;
                let kind = if file_type.is_dir() {
                    "directory"
                } else if file_type.is_file() {
                    "file"
                } else if file_type.is_symlink() {
                    "symlink"
                } else {
                    "other"
                };
                let metadata = entry.metadata().ok();
                Ok(json!({
                    "name": entry.file_name().to_string_lossy(),
                    "path": self.root.relative_display(&entry.path()),
                    "kind": kind,
                    "size_bytes": metadata.as_ref().map(fs::Metadata::len),
                }))
            })
            .collect::<Result<Vec<_>, ToolError>>()?;
        entries.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
        Ok(ToolOutput::text(json_string(json!({
            "path": self.root.relative_display(&path),
            "entries": entries
        }))))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListDirParams {
    /// Workspace-relative directory path, default .
    path: Option<String>,
}

#[async_trait]
impl Tool for ListDirTool {
    type Params = ListDirParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "List a workspace directory with structured entries."
    }

    async fn run(&self, params: ListDirParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.list_sync(params)).await
    }
}

#[derive(Debug, Clone)]
struct GrepFilesTool {
    root: WorkspacePolicy,
}

impl GrepFilesTool {
    const NAME: &'static str = "grep_files";

    fn new(root: WorkspacePolicy) -> Self {
        Self { root }
    }

    fn grep_sync(&self, params: GrepFilesParams) -> Result<ToolOutput, ToolError> {
        let pattern = params.pattern.as_str();
        let path_arg = params.path.as_deref().unwrap_or(".");
        let context_lines = params.context_lines.unwrap_or(DEFAULT_CONTEXT_LINES);
        let case_insensitive = params.case_insensitive.unwrap_or(false);
        let max_results = params
            .max_results
            .unwrap_or(DEFAULT_GREP_RESULTS)
            .clamp(1, MAX_GREP_RESULTS);
        let regex = RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .build()
            .map_err(|error| invalid(Self::NAME, format!("invalid regex pattern: {error}")))?;
        let search_path = self.root.resolve_existing(path_arg, Self::NAME)?;
        let mut files_searched = 0usize;
        let mut matches = Vec::new();

        for entry in WalkBuilder::new(&search_path)
            .standard_filters(true)
            .follow_links(false)
            .build()
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if contains_symlink(path, Some(self.root.root())).unwrap_or(true) {
                continue;
            }
            let Ok(metadata) = fs::metadata(path) else {
                continue;
            };
            if metadata.len() > MAX_FILE_BYTES {
                continue;
            }
            let Ok(contents) = fs::read_to_string(path) else {
                continue;
            };
            files_searched += 1;
            let lines = contents.lines().collect::<Vec<_>>();
            for (index, line) in lines.iter().enumerate() {
                if !regex.is_match(line) {
                    continue;
                }
                let before_start = index.saturating_sub(context_lines);
                let after_end = (index + context_lines + 1).min(lines.len());
                matches.push(json!({
                    "path": self.root.relative_display(path),
                    "line_number": index + 1,
                    "line": line,
                    "context_before": (before_start..index)
                        .map(|line_index| json!({"line": line_index + 1, "text": lines[line_index]}))
                        .collect::<Vec<_>>(),
                    "context_after": ((index + 1)..after_end)
                        .map(|line_index| json!({"line": line_index + 1, "text": lines[line_index]}))
                        .collect::<Vec<_>>(),
                }));
                if matches.len() >= max_results {
                    break;
                }
            }
            if matches.len() >= max_results {
                break;
            }
        }

        Ok(ToolOutput::text(json_string(json!({
            "pattern": pattern,
            "path": self.root.relative_display(&search_path),
            "files_searched": files_searched,
            "truncated": matches.len() >= max_results,
            "matches": matches
        }))))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GrepFilesParams {
    /// Regex pattern
    pattern: String,
    /// Workspace-relative file or directory, default .
    path: Option<String>,
    /// Context lines before and after each match, default 2
    context_lines: Option<usize>,
    /// Case-insensitive search, default false
    case_insensitive: Option<bool>,
    /// Maximum matches, default 100, max 500
    max_results: Option<usize>,
}

#[async_trait]
impl Tool for GrepFilesTool {
    type Params = GrepFilesParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Search UTF-8 workspace files with a regex. Returns structured matches with file, line number, and context."
    }

    async fn run(&self, params: GrepFilesParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.grep_sync(params)).await
    }
}

#[derive(Debug, Clone)]
struct WriteFileTool {
    root: WorkspacePolicy,
}

impl WriteFileTool {
    const NAME: &'static str = "write_file";

    fn new(root: WorkspacePolicy) -> Self {
        Self { root }
    }

    fn write_sync(&self, params: WriteFileParams) -> Result<ToolOutput, ToolError> {
        let path = self.root.resolve_for_write(&params.path, Self::NAME)?;
        fs::write(&path, &params.content).map_err(|error| {
            ToolError::exec_failed(
                Self::NAME,
                format!("failed to write {}: {error}", path.display()),
            )
        })?;
        Ok(ToolOutput::text(json_string(json!({
            "path": self.root.relative_display(&path),
            "bytes_written": params.content.len()
        }))))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteFileParams {
    /// Workspace-relative file path
    path: String,
    /// Full file contents
    content: String,
}

#[async_trait]
impl Tool for WriteFileTool {
    type Params = WriteFileParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Create or overwrite a UTF-8 file inside the workspace. Requires approval."
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn run(&self, params: WriteFileParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.write_sync(params)).await
    }
}

#[derive(Debug, Clone)]
struct ApplyPatchTool {
    root: WorkspacePolicy,
}

impl ApplyPatchTool {
    const NAME: &'static str = "apply_patch";

    fn new(root: WorkspacePolicy) -> Self {
        Self { root }
    }

    fn patch_sync(&self, params: ApplyPatchParams) -> Result<ToolOutput, ToolError> {
        if params.old.is_empty() {
            return Err(invalid(Self::NAME, "old must not be empty"));
        }
        if params.old == params.new {
            return Err(invalid(
                Self::NAME,
                "old and new are identical; no change intended",
            ));
        }
        let path = self.root.resolve_existing(&params.path, Self::NAME)?;
        let contents = fs::read_to_string(&path).map_err(|error| {
            ToolError::exec_failed(
                Self::NAME,
                format!("failed to read {} as UTF-8: {error}", path.display()),
            )
        })?;
        let display = self.root.relative_display(&path);

        let located = locate_match(&contents, &params.old)
            .map_err(|error| invalid(Self::NAME, error.message(&display)))?;

        // Splice the matched *original* byte range out and drop `new` in: every
        // byte outside `[start, end)` is preserved verbatim, so CRLF, BOM, and
        // any untouched typographic characters elsewhere in the file survive a
        // fuzzy match unchanged (only the matched region is rewritten).
        let updated = format!(
            "{}{}{}",
            &contents[..located.start],
            params.new,
            &contents[located.end..]
        );
        fs::write(&path, updated).map_err(|error| {
            ToolError::exec_failed(
                Self::NAME,
                format!("failed to write {}: {error}", path.display()),
            )
        })?;
        Ok(ToolOutput::text(json_string(json!({
            "path": display,
            "replacements": 1,
            "match": located.kind.label(),
        }))))
    }
}

/// Which matching layer located the `old` text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchKind {
    /// Byte-for-byte match.
    Exact,
    /// Matched after ignoring each line's leading whitespace (indentation).
    Indent,
    /// Matched after normalizing typographic punctuation (smart quotes,
    /// dashes, non-breaking/wide spaces) to ASCII.
    Punct,
}

impl MatchKind {
    fn label(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Indent => "fuzzy-indent",
            Self::Punct => "fuzzy-punct",
        }
    }

    /// Clause appended to the "matched N places" error so the model knows which
    /// relaxation produced the ambiguity.
    fn ambiguity_qualifier(self) -> &'static str {
        match self {
            Self::Exact => "",
            Self::Indent => " after ignoring indentation",
            Self::Punct => " after normalizing punctuation",
        }
    }
}

/// The located byte range in the ORIGINAL contents plus the layer that found it.
struct Located {
    start: usize,
    end: usize,
    kind: MatchKind,
}

enum MatchError {
    NotFound,
    NonUnique { count: usize, kind: MatchKind },
}

impl MatchError {
    fn message(&self, path: &str) -> String {
        match self {
            Self::NotFound => format!(
                "old text not found in {path} (tried exact, indentation-insensitive, and \
                 punctuation-normalized matching). Recovery: call read_file on \"{path}\" and \
                 copy `old` verbatim from the current contents."
            ),
            Self::NonUnique { count, kind } => format!(
                "old text matched {count} places in {path}{}. Recovery: call read_file on \
                 \"{path}\" and extend `old` with surrounding lines so it is unique.",
                kind.ambiguity_qualifier()
            ),
        }
    }
}

/// Locate `old` in `contents` through a cascade of increasingly tolerant
/// layers — exact, then indentation-insensitive, then punctuation-normalized —
/// each requiring a UNIQUE match. Returns the range in the original bytes.
fn locate_match(contents: &str, old: &str) -> Result<Located, MatchError> {
    // 1. Exact.
    match contents.matches(old).count() {
        1 => {
            let start = contents.find(old).expect("counted one match");
            return Ok(Located {
                start,
                end: start + old.len(),
                kind: MatchKind::Exact,
            });
        }
        count if count > 1 => {
            return Err(MatchError::NonUnique {
                count,
                kind: MatchKind::Exact,
            });
        }
        _ => {}
    }

    // 2. Indentation-insensitive: strip each line's leading whitespace on both
    //    sides, then require a unique match; the line-start expansion lets `new`
    //    supply the replacement's indentation.
    let (hay_indent, indent_map) = strip_leading_ws_with_map(contents);
    let needle_indent = strip_leading_ws_with_map(old).0;
    match unique_range(&hay_indent, &indent_map, &needle_indent) {
        Ok((start, end)) => {
            return Ok(Located {
                start: expand_to_line_start(contents, start),
                end,
                kind: MatchKind::Indent,
            });
        }
        Err(Some(count)) => {
            return Err(MatchError::NonUnique {
                count,
                kind: MatchKind::Indent,
            });
        }
        Err(None) => {}
    }

    // 3. Punctuation-normalized: fold smart quotes / dashes / exotic spaces to
    //    ASCII on both sides, then require a unique match.
    let (hay_punct, punct_map) = normalize_punct_with_map(contents);
    let needle_punct = normalize_punct_with_map(old).0;
    match unique_range(&hay_punct, &punct_map, &needle_punct) {
        Ok((start, end)) => Ok(Located {
            start,
            end,
            kind: MatchKind::Punct,
        }),
        Err(Some(count)) => Err(MatchError::NonUnique {
            count,
            kind: MatchKind::Punct,
        }),
        Err(None) => Err(MatchError::NotFound),
    }
}

/// Find the unique occurrence of `needle` in the normalized `hay` and map its
/// bounds back to original byte offsets via `map` (`map[i]` = original offset of
/// normalized byte `i`, with a terminal entry for the end). `Err(None)` means no
/// match, `Err(Some(n))` means `n > 1` ambiguous matches.
fn unique_range(hay: &str, map: &[usize], needle: &str) -> Result<(usize, usize), Option<usize>> {
    if needle.is_empty() {
        return Err(None);
    }
    match hay.matches(needle).count() {
        1 => {
            let ns = hay.find(needle).expect("counted one match");
            let ne = ns + needle.len();
            Ok((map[ns], map[ne]))
        }
        0 => Err(None),
        count => Err(Some(count)),
    }
}

/// If the matched region begins partway into a line preceded only by
/// whitespace, extend the start back to the line start so the replacement text
/// (which carries its own indentation) supersedes the original indentation.
fn expand_to_line_start(contents: &str, start: usize) -> usize {
    let line_begin = contents[..start].rfind('\n').map_or(0, |index| index + 1);
    if contents[line_begin..start]
        .chars()
        .all(|ch| ch == ' ' || ch == '\t')
    {
        line_begin
    } else {
        start
    }
}

/// Drop each line's leading spaces/tabs, returning the normalized string and a
/// map from every normalized byte index to its original byte offset (plus a
/// terminal entry mapping the end).
fn strip_leading_ws_with_map(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len() + 1);
    let mut offset = 0usize;
    for line in s.split_inclusive('\n') {
        let ws_len = line.len() - line.trim_start_matches([' ', '\t']).len();
        let base = offset + ws_len;
        for (index, ch) in line[ws_len..].char_indices() {
            let origin = base + index;
            let before = out.len();
            out.push(ch);
            for _ in before..out.len() {
                map.push(origin);
            }
        }
        offset += line.len();
    }
    map.push(s.len());
    (out, map)
}

/// Fold typographic punctuation to ASCII, returning the normalized string and
/// the same normalized→original byte map as [`strip_leading_ws_with_map`].
fn normalize_punct_with_map(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len() + 1);
    for (index, ch) in s.char_indices() {
        let before = out.len();
        out.push(normalize_punct_char(ch));
        for _ in before..out.len() {
            map.push(index);
        }
    }
    map.push(s.len());
    (out, map)
}

fn normalize_punct_char(ch: char) -> char {
    match ch {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
        '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
        '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => ' ',
        other => other,
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ApplyPatchParams {
    /// Workspace-relative file path
    path: String,
    /// Text to replace; must occur exactly once
    old: String,
    /// Replacement text
    new: String,
}

#[async_trait]
impl Tool for ApplyPatchTool {
    type Params = ApplyPatchParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Replace `old` with `new` in one workspace file. `old` must identify a unique location; \
         it is matched exactly first, then falling back to ignoring indentation and normalizing \
         typographic punctuation. Include enough surrounding context to make `old` unique. \
         Requires approval."
    }

    fn requires_approval(&self) -> bool {
        true
    }

    async fn run(&self, params: ApplyPatchParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.patch_sync(params)).await
    }
}

#[cfg(test)]
#[path = "workspace_tools/tests.rs"]
mod tests;
