use std::fs;

use async_trait::async_trait;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::tool::{Tool, ToolCx, ToolError, ToolOutput, ToolRegistry, run_blocking};
#[cfg(test)]
use crate::workspace_policy::WorkspaceRoots;
use crate::workspace_policy::{WorkspacePolicy, contains_symlink, invalid, json_string};

const DEFAULT_READ_LINES: usize = 200;
const MAX_READ_LINES: usize = 500;
/// Size cap for reading/searching a single file. `pub(crate)` so every
/// user-facing mention of the limit (read_file's error, grep's note, the
/// approval preview's source guard) derives from this one number instead of
/// hardcoding "2 MiB" — a bumped constant must not leave the prose lying.
pub(crate) const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const DEFAULT_GREP_RESULTS: usize = 100;
const MAX_GREP_RESULTS: usize = 500;
const DEFAULT_CONTEXT_LINES: usize = 2;
/// Cap on each skipped-path list in grep results: enough to act on, small
/// enough that a tree full of oversized artifacts cannot flood the output.
const SKIPPED_PATHS_LISTED: usize = 10;

/// The size cap in MiB, for prose. The compile-time assert keeps the division
/// exact: a cap that stops being a MiB multiple would otherwise truncate into
/// understating the limit.
pub(crate) const MAX_FILE_MIB: u64 = {
    assert!(MAX_FILE_BYTES.is_multiple_of(1024 * 1024));
    MAX_FILE_BYTES / (1024 * 1024)
};

#[derive(Debug, Clone)]
pub struct WorkspaceTools {
    root: WorkspacePolicy,
}

impl WorkspaceTools {
    /// Test convenience: own-policy construction. Production launches build
    /// ONE shared policy and use [`Self::with_policy`].
    #[cfg(test)]
    pub fn new(roots: impl Into<WorkspaceRoots>) -> Result<Self, ToolError> {
        Ok(Self::with_policy(WorkspacePolicy::new(roots)?))
    }

    /// Build on an existing (shared) boundary policy — see
    /// [`crate::shell_tools::ShellTools::with_policy`] for why launches share
    /// one policy across tool groups.
    pub(crate) fn with_policy(root: WorkspacePolicy) -> Self {
        Self { root }
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

/// Test convenience wrapper over [`workspace_tool_registry_from`].
#[cfg(test)]
pub fn workspace_tool_registry(
    roots: impl Into<WorkspaceRoots>,
) -> Result<ToolRegistry, ToolError> {
    Ok(WorkspaceTools::new(roots)?.into_registry())
}

/// Registry from a shared boundary policy (see [`WorkspaceTools::with_policy`]).
pub(crate) fn workspace_tool_registry_from(policy: WorkspacePolicy) -> ToolRegistry {
    WorkspaceTools::with_policy(policy).into_registry()
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
                    "{} is larger than the current {MAX_FILE_MIB} MiB read limit",
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
    /// Workspace-relative file path (absolute allowed only inside a granted root)
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
    /// Workspace-relative directory path, default . (absolute allowed only inside a granted root)
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
        // Refusals are counted, not silently dropped: a file this loop refuses
        // to search is a place a match may be hiding, and "0 matches" without
        // the counts reads as "searched everything, found nothing". Two
        // ledgers — the size cap, and anything that could not be read (an IO
        // error, non-UTF-8 content, or a directory the walk itself could not
        // open, which takes its whole subtree with it). Paths are listed
        // (capped) so the caller can actually go look.
        //
        // Two deliberate non-ledgers, for different reasons. Symlinked paths:
        // the boundary refuses them on every tool, grep is not special.
        // `standard_filters` exclusions (hidden files, .gitignore/.ignore):
        // these are the LARGEST class of unsearched files by far, and counting
        // them per call would drown the real refusals in thousands of
        // `target/` entries — so they are declared once, in `description()`,
        // where the caller learns the rule instead of re-reading a census.
        let mut skipped_oversized = 0usize;
        let mut skipped_oversized_paths: Vec<String> = Vec::new();
        let mut skipped_unreadable = 0usize;
        let mut skipped_unreadable_paths: Vec<String> = Vec::new();
        let mut matches = Vec::new();
        // Boundary snapshot taken once: `granted_roots()` locks and clones per
        // call, and this loop visits every file in the tree — per-file calls
        // would be thousands of lock+clone rounds for a boundary that cannot
        // change within one tool call.
        let granted_roots = self.root.granted_roots();

        for entry in WalkBuilder::new(&search_path)
            .standard_filters(true)
            .follow_links(false)
            .build()
        {
            let entry = match entry {
                Ok(entry) => entry,
                // A DIRECTORY the walk could not read (EACCES, a subtree that
                // vanished mid-walk) surfaces here, and dropping it silently
                // discarded everything beneath it — the same "searched
                // everything, found nothing" lie the per-file ledgers exist to
                // prevent, one level up and far larger. Counted as unreadable
                // like any other refusal; `ignore` carries the offending path
                // on the error, so the caller still gets somewhere to look.
                Err(error) => {
                    skipped_unreadable += 1;
                    push_capped(
                        &mut skipped_unreadable_paths,
                        match &error {
                            ignore::Error::WithPath { path, .. } => {
                                self.root.relative_display(path)
                            }
                            // No path on the error (a loop or a partial read):
                            // the message is all there is, and it beats silence.
                            other => other.to_string(),
                        },
                    );
                    continue;
                }
            };
            let path = entry.path();
            // From the walker's readdir data — `path.is_file()` would re-stat
            // the full path (following symlinks, at odds with
            // `follow_links(false)`) once per entry; `ListDirTool` already
            // reads the entry the same way. A symlink never counts as a file
            // here, which is where the boundary wants it anyway.
            let Some(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_file() {
                continue;
            }
            match contains_symlink(path, &granted_roots) {
                // Boundary policy, not a coverage gap (see the ledger note).
                Ok(true) => continue,
                Ok(false) => {}
                // An lstat that itself failed: fail closed AND on the record.
                Err(_) => {
                    skipped_unreadable += 1;
                    push_capped(
                        &mut skipped_unreadable_paths,
                        self.root.relative_display(path),
                    );
                    continue;
                }
            }
            // `entry.metadata()` reuses the walker's own stat instead of a
            // third follow-the-path traversal. It is an lstat for the tree
            // walk; the one exception is a search rooted AT a file, where
            // `WalkBuilder` forces `follow_links` on regardless of our
            // setting. Harmless either way — the two checks above already
            // established this entry is a plain file, and `search_path` came
            // out of `resolve_existing` symlink-free.
            let Ok(metadata) = entry.metadata() else {
                skipped_unreadable += 1;
                push_capped(
                    &mut skipped_unreadable_paths,
                    self.root.relative_display(path),
                );
                continue;
            };
            if metadata.len() > MAX_FILE_BYTES {
                skipped_oversized += 1;
                push_capped(
                    &mut skipped_oversized_paths,
                    self.root.relative_display(path),
                );
                continue;
            }
            let Ok(contents) = fs::read_to_string(path) else {
                skipped_unreadable += 1;
                push_capped(
                    &mut skipped_unreadable_paths,
                    self.root.relative_display(path),
                );
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

        let mut result = json!({
            "pattern": pattern,
            "path": self.root.relative_display(&search_path),
            "files_searched": files_searched,
            "skipped_oversized": skipped_oversized,
            "skipped_unreadable": skipped_unreadable,
            "truncated": matches.len() >= max_results,
            "matches": matches
        });
        if !skipped_oversized_paths.is_empty() {
            result["skipped_oversized_paths"] = json!(skipped_oversized_paths);
        }
        if !skipped_unreadable_paths.is_empty() {
            result["skipped_unreadable_paths"] = json!(skipped_unreadable_paths);
        }
        if skipped_oversized > 0 || skipped_unreadable > 0 {
            let mut parts = Vec::new();
            if skipped_oversized > 0 {
                parts.push(format!(
                    "{skipped_oversized} file(s) over the {MAX_FILE_MIB} MiB search limit"
                ));
            }
            if skipped_unreadable > 0 {
                parts.push(format!("{skipped_unreadable} unreadable file(s)"));
            }
            // Truncation stops the walk mid-tree, so what was counted so far is
            // a floor, not a census. Saying "not searched: 3" when the walk
            // never finished would be a different flavour of the same lie.
            let census = if matches.len() >= max_results {
                "at least "
            } else {
                ""
            };
            // No promise the caller cannot keep: shell grep needs approval in
            // gated contexts, so the paths themselves are the reliable part of
            // this note — enough to report the gap even when no tool can close
            // it.
            result["note"] = json!(format!(
                "not searched: {census}{} — see the skipped_*_paths lists (first {} each); \
                 the shell tool can grep them where approval allows",
                parts.join(" and "),
                SKIPPED_PATHS_LISTED
            ));
        }
        Ok(ToolOutput::text(json_string(result)))
    }
}

/// `fs::write` with `O_NOFOLLOW` where the platform has it: a symlink at the
/// final component fails the open instead of being written through.
///
/// Windows has no equivalent flag, and `create_new` is not usable here because
/// overwriting an existing file is the tool's normal job. There the guarantee
/// is the resolve-time `symlink_metadata` check alone, as before.
fn write_no_follow(path: &std::path::Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }

    options.open(path)?.write_all(content)
}

/// [`write_no_follow`]'s failure, mapped to a message that names the rule.
///
/// `O_NOFOLLOW` refuses a symlinked final component with `ELOOP`, whose stock
/// text is "Too many levels of symbolic links" — for a *single* link. Handed
/// that bare, the model reads a loop it must have built rather than the
/// boundary it just hit, and retries into word salad. This is the same
/// unreadable-errno problem [`WorkspacePolicy::prepare_for_write`] grew a
/// diagnosis for; the write leaf gets the matching half.
///
/// Only the last component is in question: resolution already refused a
/// symlink that was there beforehand, and an intermediate loop would have
/// failed the canonicalize before this open. So `ELOOP` here means the leaf,
/// and it means the race — which is exactly what the caller needs told.
fn write_failed(tool_name: &str, path: &std::path::Path, error: &std::io::Error) -> ToolError {
    #[cfg(unix)]
    let diagnosis = if error.raw_os_error() == Some(libc::ELOOP) {
        " (the final path component is a symlink; symlinks are never written through)"
    } else {
        ""
    };
    #[cfg(not(unix))]
    let diagnosis = "";
    ToolError::exec_failed(
        tool_name,
        format!("failed to write {}: {error}{diagnosis}", path.display()),
    )
}

/// Append to a skipped-path list unless it already holds
/// [`SKIPPED_PATHS_LISTED`] entries (the sibling counter keeps the true
/// total; the list is a sample to act on, not the census).
fn push_capped(paths: &mut Vec<String>, path: String) {
    if paths.len() < SKIPPED_PATHS_LISTED {
        paths.push(path);
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GrepFilesParams {
    /// Regex pattern
    pattern: String,
    /// Workspace-relative file or directory, default . (absolute allowed only inside a granted root)
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

    /// The exclusions belong in the CONTRACT, not just in the result. The
    /// ledgers in the output cover files the loop refused one at a time, but
    /// the walker's standard filters drop far more before the loop ever sees
    /// them — and those never appear in any count, so a caller reading
    /// "0 matches" has no way to learn that `.gitignore` hid the answer.
    /// Saying so here is the only place that gap can be closed.
    fn description(&self) -> &str {
        "Search UTF-8 workspace files with a regex. Returns structured matches with file, line \
         number, and context. Hidden files and anything excluded by .gitignore/.ignore are NOT \
         searched and are not counted anywhere — use the shell tool if you need those. Files \
         refused individually (over the size limit, or unreadable) are reported in \
         skipped_oversized/skipped_unreadable with their paths."
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
        // `prepare_`, not `resolve_`: execution is the one place allowed to
        // create missing parent directories — preview/approval resolve the
        // same path side-effect-free.
        let path = self.root.prepare_for_write(&params.path, Self::NAME)?;
        // `fs::write` opens `O_CREAT|O_WRONLY|O_TRUNC` and FOLLOWS a symlink at
        // the final component. Resolution rejects a symlink that is already
        // there, but it cannot rule out one planted in the window between that
        // check and this open — and planting it is an ordinary permitted write
        // inside a granted root, while these tools run in-process where no
        // sandbox sees them. `O_NOFOLLOW` states the refusal to the kernel, so
        // the leaf half of that race closes regardless of timing. The same
        // lock `jobs::create_spill_file` already uses.
        //
        // It does NOT cover a directory symlink planted at an intermediate
        // segment — `O_NOFOLLOW` only inspects the last component — which
        // stays the residue described in `prepare_for_write`.
        write_no_follow(&path, params.content.as_bytes())
            .map_err(|error| write_failed(Self::NAME, &path, &error))?;
        Ok(ToolOutput::text(json_string(json!({
            "path": self.root.relative_display(&path),
            "bytes_written": params.content.len()
        }))))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteFileParams {
    /// Workspace-relative file path (absolute allowed only inside a granted root)
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

        // The model writes `new` with LF (that is how `read_file` showed it), so
        // in a CRLF file splice in the file's own convention — otherwise a
        // successful patch leaves a block of LF-only lines in an otherwise CRLF
        // file and every later diff shows the whole region as changed.
        let replacement = if contents.contains("\r\n") && !params.new.contains('\r') {
            to_crlf(&params.new)
        } else {
            params.new.clone()
        };

        // Splice the matched *original* byte range out and drop `new` in: every
        // byte outside `[start, end)` is preserved verbatim, so CRLF, BOM, and
        // any untouched typographic characters elsewhere in the file survive a
        // fuzzy match unchanged (only the matched region is rewritten).
        let updated = format!(
            "{}{}{}",
            &contents[..located.start],
            replacement,
            &contents[located.end..]
        );
        // `write_no_follow`, not `fs::write`, for the reason spelled out at
        // `WriteFileTool::write_sync` — and here the window is WIDER, not
        // narrower: `resolve_existing` above is followed by a read, a fuzzy
        // `locate_match`, and a CRLF pass before this open, all of which a
        // concurrent `job` can outlive. `apply_patch` is the same
        // `ToolKind::WriteFile` as `write_file` and auto-approves under
        // `accept_edits`, so leaving it on the following open made the lock on
        // its twin decorative.
        write_no_follow(&path, updated.as_bytes())
            .map_err(|error| write_failed(Self::NAME, &path, &error))?;
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

/// Convert lone `\n` to `\r\n`, leaving any existing `\r\n` untouched.
fn to_crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
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

    // 1b. CRLF: `read_file` splits with `str::lines`, which drops the `\r`, so
    // the model faithfully copies LF-only text out of a CRLF file and then no
    // layer here can ever match it — exact sees `\n` vs `\r\n`, the indentation
    // layer only strips *leading* whitespace, and punctuation folding does not
    // touch `\r`. The failure told the model to "copy `old` verbatim from
    // read_file", which is exactly what it had just done: an unrecoverable retry
    // loop on every multi-line edit in a CRLF repo. Re-matching with the needle
    // converted to the file's own line ending keeps the range in original
    // coordinates, so the surrounding CRLF is preserved rather than normalized.
    if contents.contains("\r\n") && !old.contains('\r') && old.contains('\n') {
        let crlf_old = to_crlf(old);
        match contents.matches(&crlf_old).count() {
            1 => {
                let start = contents.find(&crlf_old).expect("counted one match");
                return Ok(Located {
                    start,
                    end: start + crlf_old.len(),
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
    /// Workspace-relative file path (absolute allowed only inside a granted root)
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
