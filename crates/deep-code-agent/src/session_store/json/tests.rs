use super::*;

#[test]
fn a_cancelled_session_scan_returns_an_error_instead_of_an_empty_list() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        store.list_cancellable(&cancel),
        Err(SessionStoreError::Cancelled)
    ));

    let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
    store.save(&mut record).unwrap();
    assert!(matches!(
        store.list_cancellable(&cancel),
        Err(SessionStoreError::Cancelled)
    ));
    assert_eq!(store.list().unwrap(), vec![record]);
}

#[tokio::test]
async fn cancelling_a_running_scan_stops_between_large_invalid_files() {
    use std::time::Duration;

    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    // The reported regression: 300 directory entries sharing 7 MiB of invalid
    // JSON still cause about 2 GiB of reads/parsing if cancellation is ignored.
    let source = store.root.join("invalid_0.json");
    let mut invalid = vec![b' '; 7 * 1024 * 1024];
    *invalid.last_mut().unwrap() = b'{';
    fs::write(&source, invalid).unwrap();
    for index in 1..300 {
        fs::hard_link(&source, store.root.join(format!("invalid_{index}.json"))).unwrap();
    }

    let cancel = tokio_util::sync::CancellationToken::new();
    let worker_cancel = cancel.clone();
    let worker_store = store.clone();
    let worker = tokio::task::spawn_blocking(move || worker_store.list_cancellable(&worker_cancel));
    // Observe real progress before cancelling: a pre-cancelled token alone
    // cannot catch a missing cancellation check inside the file loop.
    tokio::time::timeout(Duration::from_secs(10), async {
        while store.take_diagnostics().is_empty() {
            assert!(
                !worker.is_finished(),
                "scan never reported its invalid input"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result, Err(SessionStoreError::Cancelled)));
}

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
fn json_store_migrates_an_authenticated_legacy_schema() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let id = SessionId("legacy_session".to_string());
    let legacy = LegacySessionRecord {
        schema_version: LEGACY_SESSION_SCHEMA_VERSION,
        id: id.clone(),
        workspace: dir.path().to_path_buf(),
        extra_roots: Vec::new(),
        extra_roots_mac: None,
        record_mac: None,
        created_at_ms: 1,
        updated_at_ms: 2,
        entries: vec![
            std::sync::Arc::new(LegacySessionEntry {
                kind: crate::session_entry::EntryKind::System {
                    content: "system".to_string(),
                },
            }),
            std::sync::Arc::new(LegacySessionEntry {
                kind: crate::session_entry::EntryKind::User {
                    content: "resume me".to_string(),
                    images: Vec::new(),
                },
            }),
            std::sync::Arc::new(LegacySessionEntry {
                kind: crate::session_entry::EntryKind::Assistant {
                    content: "saved".to_string(),
                    reasoning: None,
                    exchanges: Vec::new(),
                },
            }),
        ],
        turns: vec![LegacyTurnRecord { started_at_ms: 1 }],
        checkpoints: vec![LegacyCheckpointRecord {
            id: crate::checkpoint::CheckpointId("checkpoint_1".to_string()),
            label: "before_turn".to_string(),
            created_at_ms: 1,
        }],
        summary: None,
        compaction: None,
        session_cost: crate::pricing::CostEstimate::default(),
        session_cache_hit_tokens: 0,
        session_cache_miss_tokens: 0,
        session_cache_savings: crate::pricing::CostEstimate::default(),
    };
    let mut legacy = legacy;
    legacy.record_mac = Some(crate::session_integrity::sign_serialized(&legacy).unwrap());
    fs::write(
        store.path_for(&id).unwrap(),
        serde_json::to_vec_pretty(&legacy).unwrap(),
    )
    .unwrap();

    let loaded = store.load(&id).unwrap();
    assert_eq!(loaded.id, id);
    assert_eq!(loaded.entries.len(), 3);
    assert_eq!(loaded.turns.len(), 1);
    assert_eq!(loaded.checkpoints.len(), 1);
    assert_eq!(
        loaded.entries[1].turn_id.as_ref(),
        Some(&loaded.turns[0].id)
    );
    assert_eq!(
        loaded.entries[2].turn_id.as_ref(),
        Some(&loaded.turns[0].id)
    );
    assert_eq!(loaded.checkpoints[0].turn_id, loaded.turns[0].id);
    assert!(store.list().unwrap().iter().any(|record| record.id == id));
}

#[test]
fn json_store_rejects_an_unsigned_published_schema_without_rewriting_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let id = SessionId("published_legacy_session".to_string());
    let legacy = LegacySessionRecord {
        schema_version: PUBLISHED_SESSION_SCHEMA_VERSION,
        id: id.clone(),
        workspace: dir.path().to_path_buf(),
        extra_roots: Vec::new(),
        extra_roots_mac: None,
        record_mac: None,
        created_at_ms: 1,
        updated_at_ms: 2,
        entries: vec![
            std::sync::Arc::new(LegacySessionEntry {
                kind: crate::session_entry::EntryKind::System {
                    content: "system".to_string(),
                },
            }),
            std::sync::Arc::new(LegacySessionEntry {
                kind: crate::session_entry::EntryKind::User {
                    content: "continue this".to_string(),
                    images: Vec::new(),
                },
            }),
        ],
        turns: vec![LegacyTurnRecord { started_at_ms: 1 }],
        checkpoints: Vec::new(),
        summary: None,
        compaction: None,
        session_cost: crate::pricing::CostEstimate::default(),
        session_cache_hit_tokens: 0,
        session_cache_miss_tokens: 0,
        session_cache_savings: crate::pricing::CostEstimate::default(),
    };
    let path = store.path_for(&id).unwrap();
    let original = serde_json::to_vec_pretty(&legacy).unwrap();
    fs::write(&path, &original).unwrap();

    assert!(matches!(
        store.load(&id),
        Err(SessionStoreError::AuthenticationFailed { .. })
    ));
    assert!(store.list().unwrap().is_empty());
    assert!(
        store
            .take_diagnostics()
            .iter()
            .any(|line| line.contains("failed integrity verification"))
    );
    assert_eq!(fs::read(&path).unwrap(), original);
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
            // verification rejects the signature the writer just generated.
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
fn json_store_restores_recorded_grants() {
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
fn list_finds_the_latest_session_beyond_the_former_entry_scan_limit() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    for index in 0..257 {
        let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
        record.updated_at_ms = index;
        record.entries.push(std::sync::Arc::new(
            crate::session_entry::SessionEntry::user("continue me"),
        ));
        store.save(&mut record).unwrap();
    }

    // Pick the actual last directory entry, rather than assuming filesystem
    // enumeration follows creation order or sorts the filenames.
    let last = store.directory.entries().unwrap().last().unwrap().unwrap();
    let path = store.root.join(last.file_name());
    let mut newest = store.read_file(&path).unwrap();
    newest.updated_at_ms = 1_000;
    let snapshot = super::super::prepare_record(&mut newest).unwrap();
    // Overwrite in place to keep the directory entries unchanged.
    fs::write(&path, snapshot.json()).unwrap();

    let listed = store.list().unwrap();
    assert_eq!(listed.len(), MAX_SESSION_LIST);
    assert_eq!(listed[0], newest);
    assert!(store.take_diagnostics().is_empty());
}

#[test]
fn list_finds_the_latest_session_beyond_the_former_byte_scan_limit() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    // Legal trailing JSON whitespace keeps this fixture's parsed records
    // small while exercising over 64 MiB of real filesystem input.
    let write_padded = |record: &mut SessionRecord| {
        let mut json = super::super::prepare_record(record)
            .unwrap()
            .json()
            .to_string();
        json.push_str(&" ".repeat(7 * 1024 * 1024));
        assert!((json.len() as u64) < MAX_SESSION_FILE_BYTES);
        fs::write(store.path_for(&record.id).unwrap(), json).unwrap();
    };
    for index in 0..10 {
        let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
        record.updated_at_ms = index;
        record.entries.push(std::sync::Arc::new(
            crate::session_entry::SessionEntry::user("continue me"),
        ));
        write_padded(&mut record);
    }
    let last = store.directory.entries().unwrap().last().unwrap().unwrap();
    let mut newest = store.read_file(&store.root.join(last.file_name())).unwrap();
    newest.updated_at_ms = 1_000;
    write_padded(&mut newest);

    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 10);
    assert_eq!(listed[0], newest);
    assert!(store.take_diagnostics().is_empty());
}

#[test]
fn list_bounds_diagnostics_without_hiding_valid_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    for index in 0..300 {
        fs::write(store.root.join(format!("invalid_{index}.json")), "{").unwrap();
    }
    let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
    store.save(&mut record).unwrap();

    assert_eq!(store.list().unwrap(), vec![record.clone()]);
    let diagnostics = store.take_diagnostics();
    assert_eq!(diagnostics.len(), MAX_SESSION_DIAGNOSTICS + 1);
    assert!(diagnostics.last().unwrap().contains("diagnostics omitted"));
    assert!(store.take_diagnostics().is_empty());
    // Repeated scans without a drain must also remain bounded.
    for _ in 0..2 {
        assert_eq!(store.list().unwrap(), vec![record.clone()]);
    }
    assert_eq!(store.take_diagnostics().len(), MAX_SESSION_DIAGNOSTICS + 1);
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
    assert!(exported.contains("\"entries\""));
    assert!(exported.contains('\n'));
}

/// Unknown fields must not be discarded and then overwritten by a resumed save.
#[test]
fn an_unknown_field_is_rejected_without_modifying_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
    store.save(&mut record).unwrap();
    let path = store.path_for(&record.id).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["unexpected_field"] = serde_json::json!({"nested": true});
    let original = serde_json::to_string_pretty(&value).unwrap();
    fs::write(&path, &original).unwrap();

    assert!(matches!(
        store.load(&record.id),
        Err(SessionStoreError::Serialization { .. })
    ));
    assert!(store.list().unwrap().is_empty());
    assert!(
        store
            .take_diagnostics()
            .iter()
            .any(|line| line.contains("unexpected_field"))
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

/// A missing required field is a parse error, never an empty session to save.
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
    let original = serde_json::to_string_pretty(&value).unwrap();
    fs::write(&path, &original).unwrap();

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
    assert_eq!(fs::read_to_string(&path).unwrap(), original);
}

#[test]
fn unauthenticated_records_are_refused_without_rewriting_or_exporting_them() {
    let dir = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(dir.path()).unwrap();
    for mutation in ["transcript", "missing_tag", "invalid_tag"] {
        let mut record = SessionRecord::new(dir.path().to_path_buf(), "system");
        record.entries.push(std::sync::Arc::new(
            crate::session_entry::SessionEntry::user("preserve this conversation"),
        ));
        store.save(&mut record).unwrap();
        let path = store.path_for(&record.id).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        match mutation {
            "transcript" => value["entries"] = serde_json::json!([]),
            "missing_tag" => {
                value.as_object_mut().unwrap().remove("record_mac");
            }
            _ => value["record_mac"] = serde_json::json!("invalid"),
        }
        let original = serde_json::to_string_pretty(&value).unwrap();
        fs::write(&path, &original).unwrap();
        assert!(
            matches!(
                store.load(&record.id),
                Err(SessionStoreError::AuthenticationFailed { .. })
            ),
            "{mutation}"
        );
        assert!(
            matches!(
                store.export(&record.id),
                Err(SessionStoreError::AuthenticationFailed { .. })
            ),
            "{mutation}"
        );
        assert!(store.list().unwrap().is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }
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
            .any(|line| line.contains("failed integrity verification")),
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
