use super::*;

#[test]
fn a_live_store_cannot_be_redirected_to_another_workspaces_sessions() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    let outside = root.path().join("outside");
    fs::create_dir_all(&workspace).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let store = JsonSessionStore::for_workspace(&workspace).unwrap();
    let external = JsonSessionStore::for_workspace(&outside).unwrap();
    let mut record = SessionRecord::new(workspace.clone(), "original");
    store.save(&mut record).unwrap();
    let mut foreign = record.clone();
    foreign.workspace = outside.clone();
    external.save(&mut foreign).unwrap();
    let foreign_path = outside
        .join(".deep-code/sessions")
        .join(format!("{}.json", record.id.as_str()));
    let original_bytes = fs::read(&foreign_path).unwrap();
    let moved = crate::test_symlinks::move_open_dir_for_redirect_test(
        &workspace,
        &root.path().join("previous"),
    );
    if moved && !crate::test_symlinks::symlink_dir_for_test(&outside, &workspace) {
        return;
    }
    record.touch();
    store.save(&mut record).unwrap();
    assert_eq!(fs::read(&foreign_path).unwrap(), original_bytes);
    assert_eq!(store.load(&record.id).unwrap().workspace, record.workspace);
    store.delete(&record.id).unwrap();
    assert_eq!(fs::read(foreign_path).unwrap(), original_bytes);
}

#[test]
fn session_reads_refuse_leaf_symlinks() {
    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let mut record = SessionRecord::new(workspace.path().to_path_buf(), "original");
    store.save(&mut record).unwrap();
    let filename = store.path_for(&record.id).unwrap();
    let external = outside.path().join("session.json");
    fs::rename(&filename, &external).unwrap();
    if !crate::test_symlinks::symlink_file_for_test(&external, &filename) {
        return;
    }
    assert!(store.load(&record.id).is_err());
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn json_store_rejects_invalid_session_id() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    assert!(SessionId::parse("../evil").is_err());
    assert!(matches!(
        store.load(&SessionId("../evil".to_string())),
        Err(SessionStoreError::InvalidId { .. })
    ));
    assert!(matches!(
        store.delete(&SessionId("..".to_string())),
        Err(SessionStoreError::InvalidId { .. })
    ));
}

#[test]
fn json_store_round_trips_session() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
    record.entries.push(std::sync::Arc::new(
        crate::session_entry::SessionEntry::user("hello"),
    ));
    record.touch();

    store.save(&mut record).unwrap();
    let loaded = store.load(&record.id).unwrap();

    assert_eq!(loaded.id, record.id);
    assert_eq!(loaded.entries.len(), 2);
    assert!(matches!(
        &loaded.entries[1].kind,
        crate::session_entry::EntryKind::User { content, .. } if content == "hello"
    ));
}

#[test]
fn session_signatures_survive_real_cost_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
    record.entries.push(std::sync::Arc::new(
        crate::session_entry::SessionEntry::user("keep this conversation"),
    ));

    for model in [
        crate::model_registry::DEEPSEEK_FLASH,
        crate::model_registry::DEEPSEEK_V4_PRO,
    ] {
        for input in [22, 1_001, 12_345] {
            // Flash's first case produces USD 0.00012660000000000001.
            // Without float_roundtrip, parsing rounds it to 0.0001266 and
            // save_serialized rejects the signature we just generated.
            let cost = crate::pricing::calculate_turn_cost(
                model,
                &crate::model::Usage {
                    prompt_tokens: Some(input),
                    completion_tokens: Some(100),
                    ..Default::default()
                },
            );
            let savings = crate::pricing::cache_savings(model, input);
            record.session_cost.usd += cost.usd;
            record.session_cost.cny += cost.cny;
            record.session_cache_savings.usd += savings.usd;
            record.session_cache_savings.cny += savings.cny;

            store.save(&mut record).unwrap();
            let mut loaded = store.load(&record.id).unwrap();
            assert_eq!(loaded, record, "{model}, input={input}");
            assert!(crate::session_integrity::verify_record(&mut loaded));
            assert_eq!(store.list().unwrap(), vec![record.clone()]);
            assert!(store.take_diagnostics().is_empty());
        }
    }
}

/// A session file copied to a new name (a "fork") must load under the NEW id and
/// save back to its own file — never to the id stored inside it, which would
/// clobber the original session.
#[test]
fn a_copied_session_file_does_not_write_back_to_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let mut original = SessionRecord::new(dir.path().to_path_buf(), "system");
    store.save(&mut original).unwrap();

    // Copy original.json -> fork.json (the file still holds original's id).
    let original_path = dir
        .path()
        .join(".deep-code/sessions")
        .join(format!("{}.json", original.id.as_str()));
    let fork_id = SessionId("session_fork_copy".to_string());
    let fork_path = dir
        .path()
        .join(".deep-code/sessions")
        .join(format!("{}.json", fork_id.as_str()));
    std::fs::copy(&original_path, &fork_path).unwrap();

    // Loading the fork yields the fork's id, not the one baked into the file.
    let loaded = store.load(&fork_id).unwrap();
    assert_eq!(
        loaded.id, fork_id,
        "a copied session must adopt its filename's id"
    );
    assert_ne!(loaded.id, original.id);
}

#[test]
fn json_store_rejects_old_schema_files() {
    let dir = tempfile::tempdir().unwrap();
    let extra = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();

    // Grants survive a save/load cycle — this is what lets `-c` restore
    // the same write boundary the session was working under.
    let mut record = SessionRecord::new(dir.path().to_path_buf(), "system")
        .with_extra_roots(vec![extra.path().to_path_buf()]);
    store.save(&mut record).unwrap();
    let loaded = store.load(&record.id).unwrap();
    assert_eq!(loaded.extra_roots, vec![extra.path().to_path_buf()]);

    // Records from the previous layout are rejected by the hard schema
    // boundary.
    let mut old_schema = serde_json::to_value(&record).unwrap();
    old_schema.as_object_mut().unwrap().remove("extra_roots");
    old_schema.as_object_mut().unwrap().remove("record_mac");
    old_schema["schema_version"] = serde_json::json!(2);
    old_schema["id"] = serde_json::json!("session_7_0");
    let path = dir
        .path()
        .join(".deep-code/sessions")
        .join("session_7_0.json");
    std::fs::write(&path, serde_json::to_string_pretty(&old_schema).unwrap()).unwrap();
    assert!(matches!(
        store.load(&SessionId("session_7_0".to_string())),
        Err(SessionStoreError::UnsupportedSchema { found: 2, .. })
    ));
}

#[test]
fn json_store_rejects_future_schema() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let future = serde_json::json!({"schema_version": 4, "id": "session_9_0"});
    let path = dir
        .path()
        .join(".deep-code/sessions")
        .join("session_9_0.json");
    std::fs::write(&path, future.to_string()).unwrap();

    assert!(matches!(
        store.load(&SessionId("session_9_0".to_string())),
        Err(SessionStoreError::UnsupportedSchema { found: 4, .. })
    ));
}

#[test]
fn json_store_list_sorts_by_updated_at_desc() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();

    let mut older = SessionRecord::new(dir.path().to_path_buf(), "system");
    older.updated_at_ms = 1;
    store.save(&mut older).unwrap();

    let mut newer = SessionRecord::new(dir.path().to_path_buf(), "system");
    newer.updated_at_ms = 2;
    store.save(&mut newer).unwrap();

    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, newer.id);
}

#[test]
fn json_store_list_skips_a_record_with_a_forged_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
    store.save(&mut record).unwrap();
    let path = store.path_for(&record.id).unwrap();
    let mut json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    json["updated_at_ms"] = serde_json::json!(u64::MAX);
    fs::write(&path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();

    assert!(
        store.list().unwrap().is_empty(),
        "a timestamp edit must not make a forged session the latest"
    );
}

#[test]
fn json_store_refuses_an_oversized_session_before_parsing() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let id = SessionId("session_oversized".to_string());
    let path = store.path_for(&id).unwrap();
    fs::write(
        &path,
        vec![b'x'; usize::try_from(MAX_SESSION_FILE_BYTES + 1).unwrap()],
    )
    .unwrap();

    assert!(matches!(
        store.load(&id),
        Err(SessionStoreError::TooLarge { .. })
    ));
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn json_store_list_keeps_only_the_newest_bounded_window() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    for index in 0..(MAX_SESSION_LIST + 8) {
        let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
        record.updated_at_ms = index as u64;
        store.save(&mut record).unwrap();
    }

    let listed = store.list().unwrap();
    assert_eq!(listed.len(), MAX_SESSION_LIST);
    assert_eq!(listed[0].updated_at_ms, (MAX_SESSION_LIST + 7) as u64);
    assert_eq!(listed.last().unwrap().updated_at_ms, 8);
}

#[test]
fn json_store_list_keeps_an_older_resumable_session_ahead_of_newer_empty_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();

    let mut resumable = SessionRecord::new(dir.path().to_path_buf(), "system");
    resumable.updated_at_ms = 1;
    resumable.entries.push(std::sync::Arc::new(
        crate::session_entry::SessionEntry::user("continue me"),
    ));
    let resumable_id = resumable.id.clone();
    store.save(&mut resumable).unwrap();

    for index in 0..MAX_SESSION_LIST {
        let mut empty = SessionRecord::new(dir.path().to_path_buf(), "system");
        empty.updated_at_ms = (index + 2) as u64;
        store.save(&mut empty).unwrap();
    }

    let listed = store.list().unwrap();
    assert_eq!(listed.len(), MAX_SESSION_LIST);
    assert!(listed.iter().any(|record| record.id == resumable_id));
}

#[test]
fn json_store_rejects_unresumable_writes_without_replacing_the_previous_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
    store.save(&mut record).unwrap();
    let path = store.path_for(&record.id).unwrap();
    let previous = fs::read(&path).unwrap();

    for _ in 0..=MAX_SESSION_ENTRIES {
        record.entries.push(std::sync::Arc::new(
            crate::session_entry::SessionEntry::user("entry"),
        ));
    }
    assert!(matches!(
        store.save(&mut record),
        Err(SessionStoreError::TooManyEntries { .. })
    ));
    assert_eq!(fs::read(path).unwrap(), previous);
}

/// The state directory holds full conversation transcripts and the log, and
/// it is created *inside* the user's repository — so it must exclude itself
/// from git rather than rely on the user (or the agent's own `git add -A`)
/// noticing.
#[test]
fn opening_the_store_makes_the_state_dir_self_ignoring() {
    let dir = tempfile::tempdir().unwrap();
    JsonSessionStore::for_workspace(dir.path()).unwrap();

    let marker = dir.path().join(".deep-code").join(".gitignore");
    let body = fs::read_to_string(&marker).expect(".gitignore must be written");
    assert!(body.lines().any(|line| line.trim() == "*"), "{body:?}");

    // Never clobber a file the user customized.
    fs::write(&marker, "# mine\n").unwrap();
    JsonSessionStore::for_workspace(dir.path()).unwrap();
    assert_eq!(fs::read_to_string(&marker).unwrap(), "# mine\n");
}

/// The complement of the regular-file case above: a DANGLING symlink is an
/// existing directory entry too, but `Path::exists()` follows it and says
/// "absent", so the old `exists()`-then-`fs::write` created the link's
/// target instead — outside the workspace, from the unsandboxed parent
/// process, and reachable by nothing more than opening a repository that
/// ships this path as a symlink.
///
/// Uses the shared helper rather than a raw `#[cfg(unix)]` symlink: the bug
/// is real on Windows too (`fs::write` follows a file symlink there as
/// well), so a unix-only test cannot see a Windows-only regression, and the
/// helper is what carries `DEEPCODE_REQUIRE_SYMLINKS` so a runner that
/// quietly loses the privilege turns red instead of sweeping this green.
#[test]
fn a_dangling_marker_symlink_is_not_written_through() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let victim = outside.path().join("victim.txt");
    let state_dir = dir.path().join(".deep-code");
    fs::create_dir_all(&state_dir).unwrap();
    let marker = state_dir.join(".gitignore");
    if !crate::test_symlinks::symlink_file_for_test(&victim, &marker) {
        return;
    }

    JsonSessionStore::for_workspace(dir.path()).unwrap();

    assert!(
        !victim.exists(),
        "the marker write followed a dangling symlink out of the workspace"
    );
    assert!(
        fs::symlink_metadata(&marker).is_ok_and(|meta| meta.file_type().is_symlink()),
        "the link itself must be left alone, not replaced by a marker file"
    );
}

/// The level above the leaf. `write_self_ignore` refuses a link at
/// `.gitignore`, which buys nothing if `.deep-code` itself is a link: the
/// `create_dir_all` that ran first would already have followed it and put
/// `sessions/` — every transcript of every conversation — outside the
/// workspace, in the unsandboxed parent process.
#[test]
fn a_symlinked_state_dir_does_not_relocate_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join(".deep-code");
    if !crate::test_symlinks::symlink_dir_for_test(outside.path(), &state_dir) {
        return;
    }

    let refused = JsonSessionStore::for_workspace(dir.path());

    assert!(
        refused.is_err(),
        "a symlinked .deep-code must be refused, not followed"
    );
    assert!(
        !outside.path().join("sessions").exists(),
        "session storage was created outside the workspace: {}",
        outside.path().display()
    );
}

#[test]
fn json_store_delete_removes_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
    let id = record.id.clone();
    store.save(&mut record).unwrap();
    store.delete(&id).unwrap();
    assert!(matches!(
        store.load(&id),
        Err(SessionStoreError::NotFound { .. })
    ));
}

#[test]
fn json_store_export_is_pretty_json() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
    store.save(&mut record).unwrap();
    let exported = store.export(&record.id).unwrap();
    assert!(exported.contains("\"schema_version\""));
    assert!(exported.contains('\n'));
}

/// The fields of [`SessionRecord`] are part of the signed bytes:
/// `session_integrity::record_message` signs a re-serialization of the parsed
/// struct, so adding or removing a field changes the tag for every existing
/// record. `list()` would then treat those files as forged and `/resume` would
/// discard their transcripts.
///
/// Adding a field is therefore allowed only together with a
/// `SESSION_SCHEMA_VERSION` bump. This list is the tripwire that makes the bump
/// deliberate: if you are reading this because the assertion failed, bump the
/// version (and update the list) rather than deleting the test.
#[test]
fn the_signed_field_set_is_pinned() {
    let mut record = SessionRecord::new(PathBuf::from("/ws"), "system");
    // Populate every optional field too, so no key hides behind its
    // `skip_serializing_if`.
    record.extra_roots = vec![PathBuf::from("/extra")];
    record.extra_roots_mac = Some("tag".to_string());
    record.record_mac = Some("tag".to_string());
    record.summary = Some("summary".to_string());
    record.compaction = Some("archived=1".to_string());

    let value = serde_json::to_value(&record).unwrap();
    let mut keys: Vec<&str> = value
        .as_object()
        .expect("a record is a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "checkpoints",
            "compaction",
            "created_at_ms",
            "entries",
            "extra_roots",
            "extra_roots_mac",
            "id",
            "record_mac",
            "schema_version",
            "session_cache_hit_tokens",
            "session_cache_miss_tokens",
            "session_cache_savings",
            "session_cost",
            "summary",
            "turns",
            "updated_at_ms",
            "workspace",
        ],
        "the record's shape is part of the host tag: bump SESSION_SCHEMA_VERSION \
         when it changes, and update this list"
    );
}

/// A key this build does not know is dropped at parse time, so it never reaches
/// the signed message. That is what makes a forward-compatible writer safe: the
/// record stays verifiable and `list()` still offers it.
#[test]
fn an_unknown_field_does_not_make_a_record_look_forged() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
    store.save(&mut record).unwrap();
    let path = store.path_for(&record.id).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["a_field_from_a_newer_build"] = serde_json::json!({"nested": true});
    fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();

    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 1, "an unknown key must not hide the session");
    assert_eq!(listed[0].id, record.id);
}

/// The other direction: a *required* field that disappears is a parse error, not
/// an empty session. The distinction matters because the version check is what
/// lets a schema bump reject old files explicitly; a parser that filled the gap
/// with a default would hand an unverifiable record to `/resume`, which then
/// clears the transcript and writes the empty result back.
#[test]
fn a_record_missing_a_required_field_is_refused_rather_than_emptied() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
    store.save(&mut record).unwrap();
    let path = store.path_for(&record.id).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("session_cost")
        .expect("session_cost is persisted");
    fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();

    assert!(matches!(
        store.load(&record.id),
        Err(SessionStoreError::Serialization { .. })
    ));
    assert!(store.list().unwrap().is_empty());
    let diagnostics = store.take_diagnostics();
    assert!(
        diagnostics
            .iter()
            .any(|line| line.contains("unreadable session")),
        "{diagnostics:?}"
    );
}

/// A record that fails its tag is skipped by `list()` — and the reason is
/// handed to the caller instead of written to stderr, where the TUI's log
/// redirect swallowed it and `-c` said "No saved sessions." over a directory
/// full of files.
#[test]
fn list_hands_its_skip_reasons_to_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
    store.save(&mut record).unwrap();
    let path = store.path_for(&record.id).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    // `updated_at_ms` is inside the tag.
    value["updated_at_ms"] = serde_json::json!(1);
    fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();

    assert!(store.list().unwrap().is_empty());
    let diagnostics = store.take_diagnostics();
    assert!(
        diagnostics
            .iter()
            .any(|line| line.contains("unauthenticated session")),
        "{diagnostics:?}"
    );
    assert!(
        store.take_diagnostics().is_empty(),
        "taking the diagnostics must drain them"
    );
}

/// A copied session file is a fork, and the two tags deliberately disagree
/// about what that means: the conversation travels (the record tag is re-issued
/// for the new id, so `/resume` does not treat a legitimately copied file as a
/// forged one), and the write grants do not (the grant tag names the id and
/// workspace they were approved under, so `runtime_launch` drops them with a
/// warning).
///
/// Pinning both halves keeps the next reader from "fixing" one into the other:
/// re-signing the grants would carry one workspace's authority into another,
/// and not re-signing the record would throw away the transcript of a fork.
#[test]
fn a_forked_session_keeps_its_transcript_and_loses_its_grants() {
    let dir = tempfile::tempdir().unwrap();
    let extra = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
    record.extra_roots = vec![extra.path().to_path_buf()];
    record.entries.push(std::sync::Arc::new(
        crate::session_entry::SessionEntry::user("keep me"),
    ));
    store.save(&mut record).unwrap();

    let fork = SessionId("session_forked_1".to_string());
    fs::copy(
        store.path_for(&record.id).unwrap(),
        store.path_for(&fork).unwrap(),
    )
    .unwrap();

    let mut loaded = store.load(&fork).unwrap();
    assert_eq!(loaded.id, fork, "the filename wins over the stored id");
    assert_eq!(
        loaded.entries.len(),
        record.entries.len(),
        "a fork keeps the conversation"
    );
    assert!(
        crate::session_integrity::verify_record(&mut loaded),
        "the fork's re-issued record tag must verify, or `/resume` would \
         discard the transcript it just kept"
    );
    assert!(
        !crate::session_integrity::verify_roots(
            loaded.id.as_str(),
            &loaded.workspace,
            &loaded.extra_roots,
            loaded.extra_roots_mac.as_deref(),
        ),
        "a fork must not inherit the original session's write grants"
    );
}
