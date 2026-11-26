use super::*;
use crate::session_store::{SessionStore, TurnRecord};

fn save_point(
    record: &mut SessionRecord,
    turn: &TurnId,
    label: &str,
    number: usize,
) -> CheckpointId {
    let id = CheckpointId(format!("{label}_{number}"));
    std::fs::create_dir_all(record.workspace.join(".deep-code/checkpoints").join(&id.0)).unwrap();
    record
        .checkpoints
        .push(CheckpointRecord::new(id.clone(), turn.clone(), label));
    id
}

fn fold(store: &JsonSessionStore, record: &mut SessionRecord) {
    let plan = crate::compaction::plan_compaction(&record.entries, None).unwrap();
    let mut result = crate::compaction::apply_compaction(&record.entries, &plan, "handover".into());
    let summary = Arc::make_mut(&mut result.entries[1]);
    let EntryKind::Compaction { has_archive, .. } = &mut summary.kind else {
        panic!("expected compaction summary");
    };
    let entries = plan
        .archived(&record.entries)
        .iter()
        .filter(|entry| entry.unsent.is_none())
        .cloned()
        .collect();
    store.save_compaction_archive(&summary.id, entries).unwrap();
    *has_archive = true;
    record.entries = result.entries;
}

#[test]
fn session_at_checkpoint_uses_the_round_boundary_even_inside_archives() {
    let workspace = tempfile::tempdir().unwrap();
    let workspace = crate::canonicalize(workspace.path()).unwrap();
    let store = JsonSessionStore::for_pinned_workspace(&workspace).unwrap();
    let mut source = SessionRecord::new(workspace, "system");
    let mut before_ten = None;
    for round in 1..=12 {
        let turn = TurnId::new();
        source.turns.push(TurnRecord::new(turn.clone()));
        source.entries.push(Arc::new(
            SessionEntry::user(format!("question {round}")).with_turn_id(turn.clone()),
        ));
        if round == 10 {
            before_ten = Some(save_point(&mut source, &turn, "before_turn", round));
            source.entries.push(Arc::new(
                SessionEntry::user("steering within 10").with_turn_id(turn.clone()),
            ));
        }
        source.entries.push(Arc::new(
            SessionEntry::assistant(format!("answer {round}"), None, Vec::new())
                .with_turn_id(turn.clone()),
        ));
        save_point(&mut source, &turn, "after_turn", round);
        if round == 2 || round == 12 {
            let id = TurnId::new();
            source.turns.push(TurnRecord::new(id.clone()));
            let mut draft = SessionEntry::user("rejected attachment").with_turn_id(id);
            draft.unsent = Some(crate::UnsentMessage::default());
            source.entries.push(Arc::new(draft));
        }
        if round == 6 || round == 12 {
            fold(&store, &mut source);
        }
    }
    store.save(&mut source).unwrap();
    let cancel = CancellationToken::new();
    let before_ten = before_ten.unwrap();
    let projections = [("3", 3), ("10", 10), (before_ten.0.as_str(), 9)];
    for (selector, expected) in projections {
        let (point, fork) = store
            .session_at_checkpoint(&source.id, selector, &cancel)
            .unwrap();
        assert_ne!(source.id, fork.id);
        assert_eq!(point.round, expected + usize::from(point.before_turn));
        assert_eq!(fork.turns.len(), expected);
        assert!(fork.checkpoints.is_empty());
        let transcript = store.transcript_entries(&fork, &cancel).unwrap();
        let prompts: Vec<_> = transcript
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::User { content } => Some(content.text()),
                _ => None,
            })
            .collect();
        let mut expected_prompts: Vec<_> = (1..=expected)
            .map(|round| format!("question {round}"))
            .collect();
        if expected == 10 {
            expected_prompts.push("steering within 10".into());
        }
        assert_eq!(prompts, expected_prompts);
        assert!(matches!(&transcript.last().unwrap().kind,
            EntryKind::Assistant { content, .. } if content == &format!("answer {expected}")));
    }
    assert_eq!(store.load(&source.id).unwrap(), source);
}

#[test]
fn pruned_and_legacy_summary_only_restore_points_are_reported() {
    let workspace = tempfile::tempdir().unwrap();
    let root = crate::canonicalize(workspace.path()).unwrap();
    let store = JsonSessionStore::for_pinned_workspace(&root).unwrap();
    let turn = TurnId::new();
    let mut record = SessionRecord::new(root.clone(), "system");
    record.turns.push(TurnRecord::new(turn.clone()));
    record
        .entries
        .push(Arc::new(SessionEntry::compaction("old summary", 5)));
    record.entries.push(Arc::new(
        SessionEntry::user("hi").with_turn_id(turn.clone()),
    ));
    record.entries.push(Arc::new(
        SessionEntry::assistant("hello", None, Vec::new()).with_turn_id(turn.clone()),
    ));
    let id = save_point(&mut record, &turn, "after_turn", 1);
    store.save(&mut record).unwrap();
    let cancel = CancellationToken::new();
    assert!(
        store
            .session_at_checkpoint(&record.id, "1", &cancel)
            .unwrap_err()
            .to_string()
            .contains("legacy compaction")
    );
    std::fs::remove_dir(root.join(".deep-code/checkpoints").join(id.0)).unwrap();
    assert!(
        store
            .restore_points(&record.id, &cancel)
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .session_at_checkpoint(&record.id, "1", &cancel)
            .unwrap_err()
            .to_string()
            .contains("unavailable")
    );
}

#[test]
fn preparing_or_abandoning_a_restore_never_publishes_a_partial_session() {
    let parent = tempfile::tempdir().unwrap();
    let workspace = parent.path().join("original");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = crate::canonicalize(&workspace).unwrap();
    let store = JsonSessionStore::for_pinned_workspace(&workspace).unwrap();
    let mut record = SessionRecord::new(workspace.clone(), "system");
    let turn = TurnId::new();
    let bytes = b"GIF89a-saved-attachment";
    let image = crate::snapshot_bytes(&workspace, bytes).unwrap();
    record.turns.push(TurnRecord::new(turn.clone()));
    record.entries.push(Arc::new(
        SessionEntry::user_with_images("question", vec![image.clone()]).with_turn_id(turn.clone()),
    ));
    record.entries.push(Arc::new(
        SessionEntry::assistant("done", None, Vec::new()).with_turn_id(turn.clone()),
    ));
    save_point(&mut record, &turn, "after_turn", 1);
    store.save(&mut record).unwrap();
    let cancel = CancellationToken::new();

    let prepared = store.prepare_restore(&record.id, "1", &cancel).unwrap();
    let fork = prepared.record().workspace.clone();
    assert!(fork.is_dir());
    drop(prepared);
    assert!(!fork.exists());

    std::fs::remove_file(image.path()).unwrap();
    let error = store
        .prepare_restore(&record.id, "1", &cancel)
        .err()
        .unwrap();
    assert!(error.to_string().contains(".deep-code/images"));
    cancel.cancel();
    assert!(matches!(
        store.prepare_restore(&record.id, "1", &cancel),
        Err(SessionStoreError::Cancelled)
    ));
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 1);
    assert_eq!(store.load(&record.id).unwrap(), record);
}
