//! The `skill` tool: the model asks for a skill's full instructions by name.
//!
//! The skills index in the system prompt names each skill and says when it
//! applies; the body stays out of the prompt until the model asks for it. It
//! used to ask by reading the `SKILL.md` path with `read_file` — which works
//! for a skill inside the workspace and for nothing else: `~/.deep-code/skills`
//! is outside every write root, `read_file` refuses absolute paths outside the
//! roots, `request_write_root` refuses `~/.deep-code` on its floor, and on
//! macOS the sandbox denies shell reads of that directory too. The documented
//! global location was unreachable. This tool reads through the registry the
//! launch built instead, so no fence has to move: the registry is the
//! allow-list, and the fences stay exactly as strict as they are.
//!
//! The file is re-read at call time (a skill edited since launch is served as
//! it is now), but only while it still resolves to the place it resolved to at
//! discovery. The workspace is model-writable, and a `SKILL.md` swapped for a
//! symlink to a credential after launch would otherwise come back labelled as
//! instructions. `read_file` refuses symlinks for the same reason; this is its
//! equivalent for the one read that bypasses the roots.

use std::fs;
use std::io::Read;
use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

use super::SkillRegistry;
use crate::i18n::{SharedLang, TextId, tr, tr_with};
use crate::runtime::tool_result::TOOL_OUTPUT_BUDGET;
use crate::tool::{Tool, ToolCx, ToolError, ToolOutput, run_blocking};

/// Chars held back from [`TOOL_OUTPUT_BUDGET`] for the header and the
/// continuation note, so a chunk never trips the runtime's head/tail cut.
const WRAPPER_RESERVE: usize = 400;
/// Floor on the chunk size, so every call makes progress whatever the header.
const MIN_CHUNK_CHARS: usize = 1_000;
/// Bytes read per call. Instructions past this are not instructions anyone
/// follows; it only keeps a runaway file from being read whole.
const MAX_READ_BYTES: u64 = 256 * 1024;
/// Names listed in the unknown-skill error before an ellipsis.
const LISTED_NAMES: usize = 20;

const DESCRIPTION: &str = "Return a skill's full instructions (the body of its SKILL.md) by name; \
the available skills are listed under '## Skills' in the system prompt. Call this before acting on \
a skill. Long instructions come back in chunks: when a result ends with a truncation note, call \
again with the `offset` it names.";

#[derive(Debug, Clone)]
pub struct SkillTool {
    registry: Arc<SkillRegistry>,
    lang: SharedLang,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillParams {
    /// The skill's `name`, exactly as listed under "## Skills".
    name: String,
    /// Character offset to continue long instructions from, as named by a
    /// previous result's truncation note. Omit to start from the beginning.
    offset: Option<usize>,
}

impl SkillTool {
    pub const NAME: &'static str = "skill";

    #[must_use]
    pub fn new(registry: Arc<SkillRegistry>, lang: SharedLang) -> Self {
        Self { registry, lang }
    }

    fn read_sync(&self, params: SkillParams) -> Result<ToolOutput, ToolError> {
        let lang = self.lang.get();
        let name = params.name.trim();
        if name.is_empty() {
            return Err(ToolError::InvalidArguments {
                name: Self::NAME.to_string(),
                message: "missing string field 'name'".to_string(),
            });
        }
        let Some(skill) = self.registry.get(name) else {
            let names: Vec<&str> = self
                .registry
                .list()
                .iter()
                .map(|skill| skill.name.as_str())
                .collect();
            let mut available = names[..names.len().min(LISTED_NAMES)].join(", ");
            if names.len() > LISTED_NAMES {
                available.push_str(", …");
            }
            return Ok(ToolOutput::soft_error(tr_with(
                lang,
                TextId::SkillNotFound,
                &[("name", name), ("available", &available)],
            )));
        };
        // The same file as at discovery, or nothing: see the module docs.
        match fs::canonicalize(&skill.path) {
            Ok(now) if now == skill.identity => {}
            _ => {
                return Ok(ToolOutput::soft_error(tr_with(
                    lang,
                    TextId::SkillChanged,
                    &[("name", name)],
                )));
            }
        }
        let mut bytes = Vec::new();
        if let Err(error) = fs::File::open(&skill.path)
            .and_then(|file| file.take(MAX_READ_BYTES).read_to_end(&mut bytes))
        {
            return Ok(ToolOutput::soft_error(tr_with(
                lang,
                TextId::SkillUnreadable,
                &[("name", name), ("error", &error.to_string())],
            )));
        }
        let text = String::from_utf8_lossy(&bytes);
        // A file that lost its metadata block since launch is still served —
        // whole, so the model sees what is actually there — rather than
        // refused as "not a skill any more".
        let body = match super::frontmatter::read_skill_file(&text) {
            Ok(parsed) => parsed.body,
            Err(_) => text.trim().to_string(),
        };

        let offset = params.offset.unwrap_or(0);
        let total = body.chars().count();
        if offset > total {
            return Ok(ToolOutput::soft_error(tr_with(
                lang,
                TextId::SkillOffsetBeyondEnd,
                &[
                    ("offset", &offset.to_string()),
                    ("total", &total.to_string()),
                ],
            )));
        }
        let header = format!("# Skill: {}\n{}\n\n", skill.name, skill.path.display());
        let budget = TOOL_OUTPUT_BUDGET
            .saturating_sub(WRAPPER_RESERVE)
            .saturating_sub(header.chars().count())
            .max(MIN_CHUNK_CHARS);
        let end = offset.saturating_add(budget).min(total);
        let chunk: String = body.chars().skip(offset).take(end - offset).collect();
        let mut out = header;
        out.push_str(&chunk);
        if end < total {
            out.push_str("\n\n");
            out.push_str(&tr_with(
                lang,
                TextId::SkillContinue,
                &[
                    ("start", &offset.to_string()),
                    ("end", &end.to_string()),
                    ("total", &total.to_string()),
                    ("next_offset", &end.to_string()),
                ],
            ));
        } else if offset > 0 {
            out.push_str("\n\n");
            out.push_str(tr(lang, TextId::SkillEnd));
        }
        Ok(ToolOutput::text(out))
    }
}

#[async_trait]
impl Tool for SkillTool {
    type Params = SkillParams;

    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    async fn run(&self, params: SkillParams, _cx: &ToolCx) -> Result<ToolOutput, ToolError> {
        let this = self.clone();
        run_blocking(Self::NAME, move || this.read_sync(params)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    use serde_json::{Value, json};
    use tempfile::TempDir;

    use crate::i18n::Lang;
    use crate::skills::discover_in_workspace;
    use crate::tool::{ErasedTool, ToolCall, ToolResultStatus};

    fn plant(root: &Path, name: &str, body: &str) -> PathBuf {
        let dir = root.join("skills").join(name);
        fs::create_dir_all(&dir).unwrap();
        let manifest = dir.join("SKILL.md");
        fs::write(
            &manifest,
            format!("---\nname: {name}\ndescription: about {name}\n---\n{body}\n"),
        )
        .unwrap();
        manifest
    }

    fn tool_for(workspace: &Path) -> SkillTool {
        SkillTool::new(
            Arc::new(discover_in_workspace(workspace)),
            SharedLang::new(Lang::Zh),
        )
    }

    async fn call(tool: &SkillTool, arguments: Value) -> crate::tool::ToolResult {
        tool.execute(
            &ToolCall::new("call_1", SkillTool::NAME, arguments),
            &ToolCx::default(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn serves_the_body_under_a_header_and_lists_names_for_a_miss() {
        let dir = TempDir::new().unwrap();
        let manifest = plant(dir.path(), "review", "1. read the diff\n2. run the tests");
        plant(dir.path(), "release", "run the workflow");
        let tool = tool_for(dir.path());

        let result = call(&tool, json!({ "name": "review" })).await;
        assert_eq!(
            result.status,
            ToolResultStatus::Success,
            "{}",
            result.content
        );
        assert_eq!(
            result.content,
            format!(
                "# Skill: review\n{}\n\n1. read the diff\n2. run the tests",
                manifest.display()
            )
        );

        let miss = call(&tool, json!({ "name": "nope" })).await;
        assert_eq!(miss.status, ToolResultStatus::Error);
        assert!(
            miss.content.contains("nope") && miss.content.contains("release, review"),
            "{}",
            miss.content
        );
    }

    /// The body is served as it is NOW, in chunks that tile it exactly.
    #[tokio::test]
    async fn re_reads_the_file_and_pages_long_bodies() {
        let dir = TempDir::new().unwrap();
        let manifest = plant(dir.path(), "long", "short at launch");
        let tool = tool_for(dir.path());
        let body: String = (0..30_000)
            .map(|i| char::from_u32(0x4e00 + (i % 300) as u32).unwrap())
            .collect();
        fs::write(
            &manifest,
            format!("---\nname: long\ndescription: d\n---\n{body}\n"),
        )
        .unwrap();

        let mut reassembled = String::new();
        let mut offset = 0usize;
        let mut chunks = 0usize;
        loop {
            let mut arguments = json!({ "name": "long" });
            if offset > 0 {
                arguments["offset"] = json!(offset);
            }
            let result = call(&tool, arguments).await;
            assert_eq!(
                result.status,
                ToolResultStatus::Success,
                "{}",
                result.content
            );
            assert!(result.content.chars().count() <= TOOL_OUTPUT_BUDGET);
            let (_, rest) = result.content.split_once("\n\n").unwrap();
            chunks += 1;
            match rest.rsplit_once("\n\n[") {
                Some((chunk, note)) if note.contains("offset=") => {
                    reassembled.push_str(chunk);
                    offset = note
                        .split("offset=")
                        .nth(1)
                        .and_then(|digits| digits.split(|c: char| !c.is_ascii_digit()).next())
                        .and_then(|digits| digits.parse().ok())
                        .unwrap();
                }
                Some((chunk, note)) => {
                    assert!(note.contains("结束"), "{note}");
                    reassembled.push_str(chunk);
                    break;
                }
                None => panic!("a multi-chunk body always carries a note: {rest:?}"),
            }
        }
        assert!(chunks >= 3, "{chunks}");
        assert_eq!(reassembled, body);

        let beyond = call(&tool, json!({ "name": "long", "offset": 40_000 })).await;
        assert_eq!(beyond.status, ToolResultStatus::Error);
        assert!(beyond.content.contains("40000"), "{}", beyond.content);
    }

    /// Swapping the file for a symlink after launch — the one thing a
    /// model-writable workspace lets a model do to it — is refused, and the
    /// target never reaches the output. Deleting it is reported, not served
    /// from the launch-time copy.
    #[tokio::test]
    async fn a_file_that_moved_since_discovery_is_refused() {
        let dir = TempDir::new().unwrap();
        let manifest = plant(dir.path(), "swap", "original");
        let tool = tool_for(dir.path());
        let secret = dir.path().join("secret");
        fs::write(&secret, "PRIVATE KEY MATERIAL").unwrap();
        fs::remove_file(&manifest).unwrap();
        if !crate::test_symlinks::symlink_file_for_test(&secret, &manifest) {
            return;
        }
        let result = call(&tool, json!({ "name": "swap" })).await;
        assert_eq!(result.status, ToolResultStatus::Error);
        assert!(
            !result.content.contains("PRIVATE KEY") && result.content.contains("swap"),
            "{}",
            result.content
        );

        fs::remove_file(&manifest).unwrap();
        let gone = call(&tool, json!({ "name": "swap" })).await;
        assert_eq!(gone.status, ToolResultStatus::Error);
        assert!(!gone.content.contains("original"), "{}", gone.content);
    }

    /// A skill whose directory is itself a symlink (a supported layout) keeps
    /// working: its identity was taken through the link at discovery.
    #[tokio::test]
    async fn a_symlinked_skill_directory_stays_readable() {
        let dir = TempDir::new().unwrap();
        let real = dir.path().join("elsewhere").join("linked");
        fs::create_dir_all(&real).unwrap();
        fs::write(
            real.join("SKILL.md"),
            "---\nname: linked\ndescription: d\n---\nbody via link\n",
        )
        .unwrap();
        let skills = dir.path().join("skills");
        fs::create_dir_all(&skills).unwrap();
        if !crate::test_symlinks::symlink_dir_for_test(&real, &skills.join("linked")) {
            return;
        }
        let tool = tool_for(dir.path());
        let result = call(&tool, json!({ "name": "linked" })).await;
        assert_eq!(
            result.status,
            ToolResultStatus::Success,
            "{}",
            result.content
        );
        assert!(result.content.ends_with("body via link"));
    }

    #[test]
    fn the_skill_tool_is_read_only_and_ungated() {
        use crate::execution_policy::{ExecPolicy, PolicyVerdict, ToolKind};
        assert_eq!(
            ExecPolicy::classify_tool(SkillTool::NAME),
            ToolKind::ReadOnlyFile
        );
        let plan = ExecPolicy::new().evaluate_tool(SkillTool::NAME, &json!({ "name": "x" }));
        assert!(matches!(plan.verdict, PolicyVerdict::Allow));
        assert!(!plan.requires_approval && plan.read_only);
    }
}
