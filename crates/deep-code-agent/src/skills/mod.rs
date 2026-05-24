use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const MAX_SKILL_DESCRIPTION_CHARS: usize = 280;
const MAX_AVAILABLE_SKILLS_CHARS: usize = 12_000;

/// Parsed representation of a `SKILL.md` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub path: PathBuf,
}

/// Collection of discovered skills.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillRegistry {
    skills: Vec<Skill>,
    warnings: Vec<String>,
}

impl SkillRegistry {
    const MAX_DISCOVERY_DEPTH: usize = 8;

    #[must_use]
    pub fn discover(dir: &Path) -> Self {
        let mut registry = Self::default();
        let Ok(canonical_dir) = fs::canonicalize(dir) else {
            return registry;
        };
        if !canonical_dir.is_dir() {
            return registry;
        }
        let mut visited = HashSet::new();
        Self::discover_recursive(dir, 0, &mut registry, &mut visited);
        registry
            .skills
            .sort_by(|left, right| left.name.cmp(&right.name).then_with(|| left.path.cmp(&right.path)));
        registry
    }

    fn discover_recursive(
        dir: &Path,
        depth: usize,
        registry: &mut Self,
        visited: &mut HashSet<PathBuf>,
    ) {
        if depth > Self::MAX_DISCOVERY_DEPTH {
            return;
        }
        if !Self::mark_discovered_dir(dir, visited) {
            return;
        }
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                if depth == 0 {
                    registry.push_warning(format!(
                        "Failed to read skills directory {}: {error}",
                        dir.display()
                    ));
                }
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'))
            {
                continue;
            }
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_dir() {
                continue;
            }
            let skill_path = path.join("SKILL.md");
            match fs::read_to_string(&skill_path) {
                Ok(content) => match Self::parse_skill(&content) {
                    Ok(mut skill) => {
                        if !Self::mark_discovered_dir(&path, visited) {
                            continue;
                        }
                        skill.path = skill_path;
                        registry.skills.push(skill);
                        continue;
                    }
                    Err(reason) => {
                        if Self::mark_discovered_dir(&path, visited) {
                            registry.push_warning(format!(
                                "Failed to parse {}: {reason}",
                                skill_path.display()
                            ));
                        }
                        continue;
                    }
                },
                Err(error) if skill_path.exists() => {
                    if Self::mark_discovered_dir(&path, visited) {
                        registry.push_warning(format!(
                            "Failed to read {}: {error}",
                            skill_path.display()
                        ));
                    }
                    continue;
                }
                Err(_) => {}
            }
            Self::discover_recursive(&path, depth + 1, registry, visited);
        }
    }

    fn mark_discovered_dir(dir: &Path, visited: &mut HashSet<PathBuf>) -> bool {
        let key = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        visited.insert(key)
    }

    fn push_warning(&mut self, warning: String) {
        self.warnings.push(warning);
    }

    fn parse_skill(content: &str) -> Result<Skill, String> {
        let trimmed = content.trim_start();
        if trimmed.starts_with("---") {
            let start = content
                .find("---")
                .ok_or_else(|| "missing frontmatter opening delimiter".to_string())?;
            let rest = &content[start + 3..];
            let end = rest
                .find("---")
                .ok_or_else(|| "missing frontmatter closing delimiter".to_string())?;
            let frontmatter = &rest[..end];
            let body = &rest[end + 3..];
            let mut metadata = HashMap::new();
            for raw in frontmatter.lines() {
                let line = raw.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((key, value)) = line.split_once(':') {
                    let value = value.trim();
                    let unquoted = if (value.starts_with('"') && value.ends_with('"') && value.len() >= 2)
                        || (value.starts_with('\'') && value.ends_with('\'') && value.len() >= 2)
                    {
                        &value[1..value.len() - 1]
                    } else {
                        value
                    };
                    metadata.insert(key.trim().to_ascii_lowercase(), unquoted.to_string());
                }
            }
            let name = metadata
                .get("name")
                .filter(|name| !name.is_empty())
                .cloned()
                .ok_or_else(|| "missing required frontmatter field: name".to_string())?;
            let description = metadata.get("description").cloned().unwrap_or_default();
            return Ok(Skill {
                name,
                description,
                body: body.trim().to_string(),
                path: PathBuf::new(),
            });
        }
        Err("skills require YAML frontmatter with name and description".to_string())
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    #[must_use]
    pub fn list(&self) -> &[Skill] {
        &self.skills
    }

    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.skills.len()
    }
}

#[must_use]
pub fn global_skills_dir() -> PathBuf {
    home_dir()
        .map(|home| home.join(".deep-code").join("skills"))
        .unwrap_or_else(|| PathBuf::from(".deep-code/skills"))
}

#[must_use]
pub fn workspace_skills_dir(workspace: &Path) -> PathBuf {
    workspace.join("skills")
}

#[must_use]
pub fn skills_directories(workspace: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![
        workspace_skills_dir(workspace),
        workspace.join(".deep-code").join("skills"),
        global_skills_dir(),
    ];
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for path in candidates.drain(..) {
        let Ok(canonical) = fs::canonicalize(&path) else {
            continue;
        };
        if canonical.is_dir() && seen.insert(canonical) {
            out.push(path);
        }
    }
    out
}

#[must_use]
pub fn discover_in_workspace(workspace: &Path) -> SkillRegistry {
    let mut merged = SkillRegistry::default();
    for dir in skills_directories(workspace) {
        let registry = SkillRegistry::discover(&dir);
        for skill in registry.skills {
            if !merged.skills.iter().any(|existing| existing.name == skill.name) {
                merged.skills.push(skill);
            }
        }
        merged.warnings.extend(registry.warnings);
    }
    merged
}

#[must_use]
pub fn render_skills_block(registry: &SkillRegistry) -> Option<String> {
    if registry.is_empty() {
        return None;
    }
    let mut out = String::new();
    out.push_str("## Skills\n");
    out.push_str(
        "Local skills are available in this session. Each entry includes a name, description, \
and file path. Open the listed `SKILL.md` when a skill is relevant.\n\n",
    );
    out.push_str("### Available skills\n");
    let mut omitted = 0usize;
    for skill in registry.list() {
        let description = truncate_for_prompt(&skill.description, MAX_SKILL_DESCRIPTION_CHARS);
        let line = if description.is_empty() {
            format!("- {}: (file: {})\n", skill.name, skill.path.display())
        } else {
            format!(
                "- {}: {} (file: {})\n",
                skill.name,
                description,
                skill.path.display()
            )
        };
        if out.chars().count() + line.chars().count() > MAX_AVAILABLE_SKILLS_CHARS {
            omitted += 1;
        } else {
            out.push_str(&line);
        }
    }
    if omitted > 0 {
        out.push_str(&format!(
            "- ... {omitted} additional skills omitted from this prompt budget.\n"
        ));
    }
    if !registry.warnings().is_empty() {
        out.push_str("\n### Skill load warnings\n");
        for warning in registry.warnings().iter().take(8) {
            out.push_str("- ");
            out.push_str(&truncate_for_prompt(warning, MAX_SKILL_DESCRIPTION_CHARS));
            out.push('\n');
        }
    }
    Some(out)
}

#[must_use]
pub fn build_system_prompt(base: &str, workspace: &Path) -> String {
    let registry = discover_in_workspace(workspace);
    let Some(skills) = render_skills_block(&registry) else {
        return base.to_string();
    };
    format!("{base}\n\n{skills}")
}

fn truncate_for_prompt(value: &str, max_chars: usize) -> String {
    let single_line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= max_chars {
        return single_line;
    }
    let mut truncated = single_line
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_skill(dir: &Path, name: &str, description: &str) {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nDo the thing.\n"),
        )
        .unwrap();
    }

    #[test]
    fn discover_loads_workspace_skills() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        write_skill(&workspace.join("skills"), "workspace-skill", "local");
        let registry = SkillRegistry::discover(&workspace.join("skills"));
        assert!(registry.get("workspace-skill").is_some());
    }

    #[test]
    fn build_system_prompt_includes_skills_block() {
        let tmp = TempDir::new().unwrap();
        write_skill(&tmp.path().join("skills"), "demo", "demo skill");
        let prompt = build_system_prompt("You are deep-code.", tmp.path());
        assert!(prompt.contains("You are deep-code."));
        assert!(prompt.contains("demo skill"));
        assert!(prompt.contains("## Skills"));
    }
}
