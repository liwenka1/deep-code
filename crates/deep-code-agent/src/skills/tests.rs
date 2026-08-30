/// A nested unreadable directory used to vanish with nothing said: the
/// note was gated on `depth == 0`, so a whole subtree of skills could fail
/// to load and look exactly like a subtree nobody had written. Same
/// "a refusal the caller cannot see is a lie" rule `grep_files` keeps four
/// skip ledgers for.
#[cfg(unix)]
#[test]
fn an_unreadable_nested_directory_is_reported() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("group");
    std::fs::create_dir_all(nested.join("real-skill")).unwrap();
    std::fs::write(
        nested.join("real-skill/SKILL.md"),
        "---\nname: real\ndescription: d\n---\nbody\n",
    )
    .unwrap();
    std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(&nested).is_ok() {
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755)).unwrap();
        return; // running as root; the refusal cannot be produced
    }

    let registry = SkillRegistry::discover(dir.path());
    let reported = registry
        .warnings()
        .iter()
        .any(|warning| warning.contains("group") && warning.contains("unreadable"));

    std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        reported,
        "an unreadable nested directory was skipped silently: {:?}",
        registry.warnings()
    );
}

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

/// `SkillRegistry::discover` is cross-platform, so its cycle termination
/// is tested cross-platform: a discovery walk that loops forever is not a
/// unix-only way to hang.
#[test]
fn symlink_cycles_terminate_without_duplicates() {
    let tmp = TempDir::new().unwrap();
    let nest = tmp.path().join("nest");
    plant_skill(&nest, "looped", "looped", "reachable through a cycle");
    if !crate::test_symlinks::symlink_dir_for_test(tmp.path(), &nest.join("back-up")) {
        return;
    }

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
