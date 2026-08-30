use super::*;

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
        crate::session_entry::EntryKind::User { content } if content == "hello"
    ));
}

#[test]
fn json_store_round_trips_extra_roots_and_defaults_them_on_old_files() {
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

    // A v2 file written before the field existed loads with no grants
    // (serde default), not an error.
    let mut legacy = serde_json::to_value(&record).unwrap();
    legacy.as_object_mut().unwrap().remove("extra_roots");
    legacy["id"] = serde_json::json!("session_7_0");
    let path = dir
        .path()
        .join(".deep-code/sessions")
        .join("session_7_0.json");
    std::fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();
    let pre_field = store.load(&SessionId("session_7_0".to_string())).unwrap();
    assert!(pre_field.extra_roots.is_empty());
}

#[test]
fn json_store_migrates_v1_files_on_load() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let v1_json = serde_json::json!({
        "schema_version": 1,
        "id": "session_1_0",
        "workspace": dir.path(),
        "created_at_ms": 1,
        "updated_at_ms": 2,
        "config": {
            "base_url": "https://api.deepseek.com/beta",
            "model": "deepseek-v4-pro",
            "timeout_secs": 60,
            "api_key_present": false
        },
        "messages": [
            {"role": "system", "content": "sys"},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "", "tool_calls": [{
                "id": "c1", "type": "function",
                "function": {"name": "shell", "arguments": "{}"}
            }]}
            // interrupted: no tool message — the migration leaves a
            // pending exchange instead of requiring a repair pass
        ],
        "turns": []
    });
    let path = dir
        .path()
        .join(".deep-code/sessions")
        .join("session_1_0.json");
    std::fs::write(&path, serde_json::to_string_pretty(&v1_json).unwrap()).unwrap();

    let mut loaded = store.load(&SessionId("session_1_0".to_string())).unwrap();
    assert_eq!(loaded.schema_version, super::SESSION_SCHEMA_VERSION);
    assert_eq!(loaded.entries.len(), 3);
    assert_eq!(loaded.preview(), "hi");

    // Saving writes the file back as v2; reloading stays stable.
    store.save(&mut loaded).unwrap();
    let reloaded = store.load(&loaded.id).unwrap();
    assert_eq!(reloaded.entries, loaded.entries);
}

#[test]
fn json_store_rejects_future_schema() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let future = serde_json::json!({"schema_version": 3, "id": "session_9_0"});
    let path = dir
        .path()
        .join(".deep-code/sessions")
        .join("session_9_0.json");
    std::fs::write(&path, future.to_string()).unwrap();

    assert!(matches!(
        store.load(&SessionId("session_9_0".to_string())),
        Err(SessionStoreError::UnsupportedSchema { found: 3, .. })
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
