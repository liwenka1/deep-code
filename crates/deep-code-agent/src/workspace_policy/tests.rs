use super::*;

fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let canonical = dir.path().canonicalize().unwrap();
    (dir, canonical)
}

#[test]
fn contains_symlink_walks_canonical_path_without_error() {
    // `canonicalize` yields a verbatim `\\?\D:\...` path on Windows, whose
    // first component is the bare disk prefix. Statting it directly fails
    // with ERROR_INVALID_FUNCTION; the walk must skip Prefix/RootDir.
    let (_dir, root) = canonical_tempdir();
    let file = root.join("note.txt");
    fs::write(&file, "x").unwrap();
    assert!(!contains_symlink(&file, std::slice::from_ref(&root)).unwrap());
}

// Skip/triage policy (unix bugs panic, Windows may lack the privilege,
// DEEPCODE_REQUIRE_SYMLINKS hardens CI) lives in `crate::test_symlinks`.
use crate::test_symlinks::symlink_dir_for_test;

#[test]
fn contains_symlink_still_detects_a_symlink_segment() {
    let (_dir, root) = canonical_tempdir();
    let target = root.join("real");
    fs::create_dir(&target).unwrap();
    let link = root.join("link");
    if !symlink_dir_for_test(&target, &link) {
        return;
    }
    assert!(contains_symlink(&link.join("inner"), std::slice::from_ref(&root)).unwrap());
}

#[test]
fn absolute_path_inside_primary_is_accepted() {
    let (_dir, root) = canonical_tempdir();
    fs::write(root.join("note.txt"), "x").unwrap();
    let policy = WorkspacePolicy::new(root.clone()).unwrap();
    let resolved = policy
        .resolve_existing(&root.join("note.txt").to_string_lossy(), "read_file")
        .unwrap();
    assert_eq!(resolved, root.join("note.txt"));
}

#[test]
fn absolute_path_inside_extra_root_is_accepted() {
    let (_a, primary) = canonical_tempdir();
    let (_b, extra) = canonical_tempdir();
    fs::write(extra.join("host.ts"), "x").unwrap();
    let policy = WorkspacePolicy::new(WorkspaceRoots::new(primary, vec![extra.clone()])).unwrap();
    let resolved = policy
        .resolve_existing(&extra.join("host.ts").to_string_lossy(), "read_file")
        .unwrap();
    assert_eq!(resolved, extra.join("host.ts"));
}

#[test]
fn prepare_for_write_creates_missing_parents_inside_extra_root() {
    let (_a, primary) = canonical_tempdir();
    let (_b, extra) = canonical_tempdir();
    let policy = WorkspacePolicy::new(WorkspaceRoots::new(primary, vec![extra.clone()])).unwrap();
    let target = extra.join("src/new_mod/thing.rs");
    let resolved = policy
        .prepare_for_write(&target.to_string_lossy(), "write_file")
        .unwrap();
    assert_eq!(resolved, target);
    assert!(extra.join("src/new_mod").is_dir());
}

/// The `starts_with` filter on the fast-forward is load-bearing and had
/// nothing pinning it: deleting it — so `start` becomes the deepest root
/// granted, covering this path or not — left the whole lib suite green
/// while making `contains_symlink` skip every segment under a SHALLOWER
/// root. With `--add-dir` granting something deeper than the primary
/// workspace, that is the boundary silently ceasing to detect symlinks in
/// the workspace itself.
///
/// The off-by-one direction was already covered (`start + 1` fails three
/// tests); this is the other axis, and it needs two roots at different
/// depths to show up at all.
#[test]
fn contains_symlink_still_checks_a_shallow_root_when_a_deeper_one_is_granted() {
    let (_a, primary) = canonical_tempdir();
    let (_b, base) = canonical_tempdir();
    // An extra root several levels deeper than the primary, as `--add-dir`
    // into a nested project directory would produce.
    let deeper = base.join("a/b/c/d");
    fs::create_dir_all(&deeper).unwrap();
    let deeper = deeper.canonicalize().unwrap();

    let target = primary.join("real");
    fs::create_dir(&target).unwrap();
    let link = primary.join("link");
    if !symlink_dir_for_test(&target, &link) {
        return;
    }

    let roots = [primary, deeper];
    assert!(
        contains_symlink(&link.join("inner"), &roots).unwrap(),
        "a symlink under the shallow root must still be caught while a deeper root is granted"
    );
}

/// A dangling symlink posing as a missing PARENT segment.
///
/// It used to get past resolution entirely — the ancestor walk asked
/// `exists()`, which FOLLOWS links, so a dangling one read as "absent" and
/// the walk climbed straight past it — and trip only inside
/// `create_dir_all`, as a bare "File exists" for a directory the tool was
/// told to create. Both halves now refuse it: the walk asks
/// `symlink_metadata` and stops AT the link, so `contains_symlink` judges
/// it at resolve time, and the prepare-side diagnosis stays as the second
/// line of defence for a link planted after resolution. Either way the
/// failure names the rule, or the model retries into word salad instead of
/// learning it.
#[test]
fn prepare_for_write_names_the_symlink_when_mkdir_hits_one() {
    let (_dir, root) = canonical_tempdir();
    let (_out, outside) = canonical_tempdir();
    let policy = WorkspacePolicy::new(root.clone()).unwrap();
    if !symlink_dir_for_test(&outside.join("gone"), &root.join("src")) {
        return;
    }
    let error = policy
        .prepare_for_write("src/new_mod/thing.rs", "write_file")
        .expect_err("a symlinked parent segment must fail the write");
    let message = format!("{error:?}");
    assert!(
        message.contains("symlinks in the destination path are not allowed"),
        "the failure must teach the rule: {message}"
    );
}

/// The OTHER errno-17 cause: an ordinary file sitting where a directory
/// has to go. `create_dir_all` reports it as the same bare "File exists"
/// the symlink case produces, and the two need completely different fixes,
/// so the message has to say which one happened.
#[test]
fn prepare_for_write_names_the_file_blocking_the_directory() {
    let (_dir, root) = canonical_tempdir();
    let policy = WorkspacePolicy::new(root.clone()).unwrap();
    fs::write(root.join("src"), "not a directory").unwrap();
    let error = policy
        .prepare_for_write("src/new_mod/thing.rs", "write_file")
        .expect_err("a file blocking the destination directory must fail the write");
    let message = format!("{error:?}");
    assert!(
        message.contains("a file already exists on that path"),
        "the failure must name the blocker, not just echo the errno: {message}"
    );
    assert!(
        !message.contains("symlinks in the destination path"),
        "a plain file must not be reported as a symlink: {message}"
    );
}

/// Resolution must be a pure question. It runs at preview/approval time,
/// before the human decides, and a denied write must leave no trace —
/// creating `src/new_mod` while merely RENDERING the approval panel is a
/// side effect the user never consented to. (Execution creates parents
/// via `prepare_for_write`, pinned by the test above.)
#[test]
fn resolve_for_write_leaves_the_disk_untouched() {
    let (_dir, root) = canonical_tempdir();
    let policy = WorkspacePolicy::new(root.clone()).unwrap();
    let resolved = policy
        .resolve_for_write("src/new_mod/thing.rs", "write_file")
        .unwrap();
    assert_eq!(resolved, root.join("src/new_mod/thing.rs"));
    assert!(
        !root.join("src").exists(),
        "resolving a write must not create its parent directories"
    );
}

#[test]
fn absolute_path_outside_all_roots_is_rejected() {
    let (_a, primary) = canonical_tempdir();
    let (_b, extra) = canonical_tempdir();
    let (_c, outside) = canonical_tempdir();
    fs::write(outside.join("secret.txt"), "x").unwrap();
    let policy = WorkspacePolicy::new(WorkspaceRoots::new(primary, vec![extra])).unwrap();
    let raw = outside.join("secret.txt");
    let read = policy.resolve_existing(&raw.to_string_lossy(), "read_file");
    assert!(read.is_err(), "read outside all roots must be rejected");
    let write = policy.resolve_for_write(&raw.to_string_lossy(), "write_file");
    assert!(write.is_err(), "write outside all roots must be rejected");
    // The rejection teaches the remedy: a model that meant to touch a
    // sibling repo must learn the grant channels, not retry blind.
    let message = write.unwrap_err().to_string();
    assert!(
        message.contains("/add-dir"),
        "rejection must name the grant channel: {message}"
    );
}

#[test]
fn absolute_path_with_parent_traversal_is_rejected() {
    let (_dir, root) = canonical_tempdir();
    let policy = WorkspacePolicy::new(root.clone()).unwrap();
    // Canonically inside the root, but spelled with `..` — still refused.
    let sneaky = format!("{}/sub/../note.txt", root.display());
    assert!(policy.resolve_for_write(&sneaky, "write_file").is_err());
}

#[test]
fn relative_paths_still_resolve_against_primary_only() {
    let (_a, primary) = canonical_tempdir();
    let (_b, extra) = canonical_tempdir();
    fs::write(extra.join("only-here.txt"), "x").unwrap();
    let policy = WorkspacePolicy::new(WorkspaceRoots::new(primary.clone(), vec![extra])).unwrap();
    // The extra root never becomes a fallback base for relative paths;
    // it is addressable by absolute path alone.
    assert!(
        policy
            .resolve_existing("only-here.txt", "read_file")
            .is_err()
    );
    let resolved = policy.resolve_for_write("fresh.txt", "write_file").unwrap();
    assert_eq!(resolved, primary.join("fresh.txt"));
}

#[test]
fn symlink_segment_under_extra_root_is_rejected() {
    let (_a, primary) = canonical_tempdir();
    let (_b, extra) = canonical_tempdir();
    let (_c, outside) = canonical_tempdir();
    fs::write(outside.join("secret.txt"), "x").unwrap();
    let link = extra.join("link");
    if !symlink_dir_for_test(&outside, &link) {
        return;
    }
    let policy = WorkspacePolicy::new(WorkspaceRoots::new(primary, vec![extra])).unwrap();
    let raw = link.join("secret.txt");
    assert!(
        policy
            .resolve_existing(&raw.to_string_lossy(), "read_file")
            .is_err(),
        "symlinked segment under an extra root must be rejected"
    );
}

#[test]
fn extras_are_deduped_and_primary_is_not_repeated() {
    let (_a, primary) = canonical_tempdir();
    let (_b, extra) = canonical_tempdir();
    let policy = WorkspacePolicy::new(WorkspaceRoots::new(
        primary.clone(),
        vec![extra.clone(), extra.clone(), primary.clone()],
    ))
    .unwrap();
    assert_eq!(policy.granted_roots(), &[primary, extra]);
}

#[test]
fn missing_extra_root_fails_construction() {
    let (_a, primary) = canonical_tempdir();
    let missing = primary.join("does-not-exist");
    let result = WorkspacePolicy::new(WorkspaceRoots::new(primary, vec![missing]));
    assert!(result.is_err(), "an unresolvable grant must refuse launch");
}

/// The load-bearing property of the shared boundary: a grant lands in
/// every clone taken BEFORE it — that is what lets a mid-session
/// `request_write_root` reach tools registered at launch without any
/// registry rebuild.
#[test]
fn grant_extra_is_visible_through_prior_clones() {
    let (_a, primary) = canonical_tempdir();
    let (_b, extra) = canonical_tempdir();
    fs::write(extra.join("host.ts"), "x").unwrap();
    let policy = WorkspacePolicy::new(primary.clone()).unwrap();
    let tool_held_clone = policy.clone(); // what a registered tool holds
    assert!(
        tool_held_clone
            .resolve_existing(&extra.join("host.ts").to_string_lossy(), "read_file")
            .is_err(),
        "not granted yet"
    );

    let outcome = policy.grant_extra(&extra).unwrap();
    assert!(matches!(
        outcome,
        RootGrantOutcome::Granted { ref canonical } if *canonical == extra
    ));
    assert_eq!(
        tool_held_clone.granted_roots(),
        vec![primary, extra.clone()]
    );
    assert!(
        tool_held_clone
            .resolve_existing(&extra.join("host.ts").to_string_lossy(), "read_file")
            .is_ok(),
        "the clone taken before the grant must see it"
    );
}

#[test]
fn grant_extra_reports_covered_paths_without_recording() {
    let (_a, primary) = canonical_tempdir();
    let policy = WorkspacePolicy::new(primary.clone()).unwrap();
    // Inside the primary (including the primary itself): already granted.
    let sub = primary.join("src");
    fs::create_dir(&sub).unwrap();
    for covered in [&primary, &sub] {
        assert!(
            matches!(
                policy.grant_extra(covered).unwrap(),
                RootGrantOutcome::AlreadyGranted { .. }
            ),
            "{} is covered",
            covered.display()
        );
    }
    assert_eq!(policy.granted_roots(), vec![primary], "nothing recorded");
}

#[test]
fn grant_extra_fails_closed_on_bad_paths() {
    let (_a, primary) = canonical_tempdir();
    let policy = WorkspacePolicy::new(primary.clone()).unwrap();
    // Relative: ambiguous about its base — refused outright.
    assert!(policy.grant_extra(Path::new("relative/dir")).is_err());
    // Nonexistent: nothing to canonicalize.
    assert!(policy.grant_extra(&primary.join("nope-missing")).is_err());
    // A file is not a containment zone.
    let file = primary.join("f.txt");
    fs::write(&file, "x").unwrap();
    assert!(policy.grant_extra(&file).is_err());
    assert_eq!(policy.granted_roots(), vec![primary], "all refused");
}

/// The request channel refuses the home directory, every ancestor of it
/// (each would cover the whole home), and the filesystem root — the tool
/// description promises the model never to ask for those, and this makes
/// the promise enforced rather than advisory. `--add-dir` stays the
/// human's own call and is deliberately not subject to this floor.
#[test]
fn grant_extra_refuses_home_and_its_ancestors() {
    let (_a, primary) = canonical_tempdir();
    let policy = WorkspacePolicy::new(primary.clone()).unwrap();
    let Some(home) = crate::paths::home_dir().and_then(|home| home.canonicalize().ok()) else {
        eprintln!("no resolvable home dir on this host; skipping");
        return;
    };
    if home.starts_with(&primary) || primary.starts_with(&home) {
        // A workspace inside (or above) home would legitimately cover it.
        eprintln!("tempdir overlaps home on this host; skipping");
        return;
    }
    for target in home.ancestors() {
        assert!(
            policy.grant_extra(target).is_err(),
            "{} must be refused: it covers the home directory",
            target.display()
        );
    }
    assert_eq!(policy.granted_roots(), vec![primary], "nothing granted");
}

/// A directory whose name embeds control characters is refused before
/// anyone is prompted: the panel could not display it faithfully (an
/// embedded newline or escape byte fabricates panel lines), and a prompt
/// the human cannot read is not an approval. The TUI additionally
/// sanitizes what it renders — this is the fail-closed layer underneath.
#[cfg(unix)]
#[test]
fn grant_extra_refuses_names_with_control_characters() {
    let (_a, primary) = canonical_tempdir();
    let policy = WorkspacePolicy::new(primary.clone()).unwrap();
    for name in ["evil\ndir", "evil\x1b[2Kdir"] {
        let evil = primary.join(name);
        fs::create_dir(&evil).unwrap();
        let Err(error) = policy.grant_extra(&evil) else {
            panic!("control characters in the name must refuse: {name:?}");
        };
        assert!(
            error.to_string().contains("control characters"),
            "the reason must name the problem: {error}"
        );
    }
    assert_eq!(policy.granted_roots(), vec![primary], "nothing granted");
}

/// The credential floor's rule, pinned without needing real secret
/// directories on the host: overlap in EITHER direction is refused, and an
/// unrelated sibling is not.
#[test]
fn sensitive_overlap_refuses_both_directions() {
    let secrets = [PathBuf::from("/home/u/.ssh"), PathBuf::from("/home/u/.aws")];
    // The candidate IS the store, or sits inside it.
    for inside in ["/home/u/.ssh", "/home/u/.ssh/keys"] {
        assert_eq!(
            sensitive_overlap(Path::new(inside), &secrets),
            Some(Path::new("/home/u/.ssh")),
            "{inside} overlaps a credential store"
        );
    }
    // The candidate is an ancestor that would cover a store.
    assert_eq!(
        sensitive_overlap(Path::new("/home/u"), &secrets),
        Some(Path::new("/home/u/.ssh"))
    );
    // Unrelated siblings stay grantable — the floor must not swallow
    // ordinary project directories.
    for outside in ["/home/u/projects", "/home/u/.sshfoo", "/srv/build"] {
        assert_eq!(
            sensitive_overlap(Path::new(outside), &secrets),
            None,
            "{outside} must remain grantable"
        );
    }
}

/// The wiring half: the shared list really does name the credential stores
/// and deep-code's own config directory, so the rule above is applied to
/// the paths that matter. Pinned separately from the sandbox's use of the
/// same constant — that is what keeps the kernel fence and this fence from
/// drifting apart.
#[test]
fn sensitive_paths_cover_the_credential_stores_and_deep_code_home() {
    let Some(home) = crate::paths::home_dir().and_then(|home| home.canonicalize().ok()) else {
        eprintln!("no resolvable home dir on this host; skipping");
        return;
    };
    let secrets = crate::paths::sensitive_paths();
    // Enumerated from the list itself, plus deep-code's own directory, so
    // that adding an entry to `CREDENTIAL_ENTRIES` is pinned by this test
    // automatically. A hand-written subset was not a guard: it named four
    // entries, so a new one could be added to the list and dropped from
    // `sensitive_paths` without anything going red.
    for entry in crate::paths::CREDENTIAL_ENTRIES
        .iter()
        .copied()
        .chain(std::iter::once(crate::paths::DEEP_CODE_DIR))
    {
        assert!(
            secrets.contains(&home.join(entry)),
            "{entry} must be refused by the request channel: {secrets:?}"
        );
    }
    // The cloud trio in particular: `.aws` alone was the inconsistency —
    // GCP and Azure credentials are the same category and the same risk.
    for entry in [".aws", ".config/gcloud", ".azure"] {
        assert!(
            secrets.contains(&home.join(entry)),
            "{entry} must be covered: {secrets:?}"
        );
    }
}

/// End-to-end through the real resolver: deep-code's own config directory
/// holds the plaintext API key and the only `auto_allow` layer that is
/// honoured, and `read_file`/`write_file` never meet the sandbox that
/// denies it — so the request channel must refuse it outright. Runs where
/// that directory exists (any real install); skips otherwise.
#[test]
fn grant_extra_refuses_deep_code_home() {
    let (_a, primary) = canonical_tempdir();
    let policy = WorkspacePolicy::new(primary.clone()).unwrap();
    let Some(home) = crate::paths::home_dir().and_then(|home| home.canonicalize().ok()) else {
        eprintln!("no resolvable home dir on this host; skipping");
        return;
    };
    let config_dir = home.join(crate::paths::DEEP_CODE_DIR);
    if !config_dir.is_dir() || primary.starts_with(&config_dir) {
        eprintln!("no ~/.deep-code on this host; skipping");
        return;
    }
    let Err(error) = policy.grant_extra(&config_dir) else {
        panic!("granting {} must be refused", config_dir.display());
    };
    assert!(
        error.to_string().contains("credential store"),
        "the reason must name the problem: {error}"
    );
    assert_eq!(policy.granted_roots(), vec![primary], "nothing granted");
}

/// macOS gives the home directory two canonical spellings for one inode —
/// `/Users/x` and the firmlinked `/System/Volumes/Data/Users/x` — and
/// `realpath(3)` collapses neither, so each resolves to itself. Every
/// floor here is a `starts_with` on canonical paths, so the Data spelling
/// used to walk through all of them: not "inside home", not "overlapping
/// a credential store", not `~/.deep-code` — while a write through it
/// lands on exactly those files. Seatbelt is no backstop, because
/// `read_file`/`write_file` are in-process and never meet it.
#[cfg(target_os = "macos")]
#[test]
fn the_firmlink_spelling_resolves_into_the_namespace_the_floors_use() {
    let Some(home) = crate::paths::home_dir().and_then(|home| home.canonicalize().ok()) else {
        eprintln!("no resolvable home dir on this host; skipping");
        return;
    };
    let data_home = Path::new("/System/Volumes/Data").join(home.strip_prefix("/").unwrap());
    if !data_home.is_dir() {
        eprintln!("no firmlinked data volume on this host; skipping");
        return;
    }

    // The normalization itself: both spellings must land on one path, or
    // no prefix-based floor downstream can be sound.
    assert_eq!(
        crate::paths::canonicalize(&data_home).unwrap(),
        home,
        "the firmlink spelling must resolve into the same namespace as home"
    );

    // And the floor that consumes it. Home itself is always present; the
    // credential entries are only asserted where the host has them.
    let mut checked = vec![data_home.clone()];
    for entry in [".ssh", crate::paths::DEEP_CODE_DIR] {
        if data_home.join(entry).is_dir() {
            checked.push(data_home.join(entry));
        }
    }
    for candidate in checked {
        let canonical = crate::paths::canonicalize(&candidate).unwrap();
        assert!(
            refuse_as_unattended_root(&canonical).is_some(),
            "the firmlink spelling {} walked through the floor",
            candidate.display()
        );
    }
}

/// A symlink to a directory canonicalizes to its target — the resolution
/// step speaks only canonical paths, so the prompt displays the real
/// target and the grant records that same value. (That prompt-vs-grant
/// equality is enforced by the runtime's re-resolve-and-compare; pinned
/// in the runtime integration tests.)
#[test]
fn grant_extra_grants_the_canonical_target_of_a_symlink() {
    let (_a, primary) = canonical_tempdir();
    let (_b, target) = canonical_tempdir();
    let link = primary.join("link");
    if !symlink_dir_for_test(&target, &link) {
        return;
    }
    let outcome = policy_grant(&primary, &link);
    assert!(
        matches!(outcome, RootGrantOutcome::Granted { ref canonical } if *canonical == target),
        "the grant must be the resolved target, not the link spelling"
    );
}

fn policy_grant(primary: &Path, requested: &Path) -> RootGrantOutcome {
    WorkspacePolicy::new(primary.to_path_buf())
        .unwrap()
        .grant_extra(requested)
        .unwrap()
}

/// `first_file_segment` names the plain file blocking a `create_dir_all` —
/// the segment that EXISTS and is NOT a directory, exactly that one. All
/// three collapses survived the suite: returning the first existing segment
/// (the guard widened to `true`), returning the first DIRECTORY (the `!`
/// deleted), and returning `Some("")` (the whole body replaced) all went
/// unnoticed because nothing pinned the diagnostic's subject.
#[test]
fn first_file_segment_names_the_blocking_file_exactly() {
    // Canonical base: production callers hand this fn canonical paths, and on
    // macOS a raw tempdir path starts with the `/var` SYMLINK — which this
    // very function (correctly) reports as the first non-directory segment.
    let temp = tempfile::TempDir::new().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let blocker = base.join("blocker");
    std::fs::write(&blocker, b"plain file").unwrap();
    let attempted = blocker.join("sub").join("dir");
    assert_eq!(first_file_segment(&attempted), Some(blocker));
    // A path whose existing prefix is all directories blocks nothing.
    let clean = base.join("not-yet").join("made");
    assert_eq!(first_file_segment(&clean), None);
}

/// A symlink DEEP in the path must be found with no skip roots in play: the
/// fast-forward comparison (`index < start`) inverted to `>` checks only the
/// first segment and waves the rest through — precisely the segments the
/// caller asked to have checked.
#[cfg(unix)]
#[test]
fn deep_symlink_is_found_with_no_skip_roots() {
    // Canonical base for the same reason as above: macOS `/var` is a symlink.
    let temp = tempfile::TempDir::new().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let real = base.join("real");
    std::fs::create_dir(&real).unwrap();
    let link = base.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    // The link is the DEEP segment here (everything above it is canonical),
    // and the probe must not need to exist past it: the walk returns at the
    // first symlink segment. The negative probe is an existing, fully
    // canonical path — a missing segment would be a stat error, not a "no".
    assert!(contains_symlink(&link, &[]).unwrap());
    assert!(!contains_symlink(&real, &[]).unwrap());
}
