use super::*;
use crate::{EntryKind, SessionEntry};
use std::collections::BTreeSet;

fn payload() -> String {
    let mut state = 0x1234_5678_9abc_def0u64;
    let mut text = String::with_capacity(10 * CHUNK_BYTES);
    for _ in 0..10 * CHUNK_BYTES {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        text.push(char::from(b'a' + (state % 26) as u8));
    }
    text
}

fn record(workspace: &Path, text: &str) -> SessionRecord {
    let mut record = SessionRecord::new(workspace.to_path_buf(), "system");
    update(&mut record, text, 0);
    record
}

fn update(record: &mut SessionRecord, text: &str, generation: usize) {
    let entry = Arc::new(SessionEntry::user(format!(
        "{}:{generation:02}:{text}",
        "p".repeat(generation)
    )));
    if record.entries.len() == 1 {
        record.entries.push(entry);
    } else {
        record.entries[1] = entry;
    }
    record.touch();
}

fn manifest_for(store: &JsonSessionStore, id: &SessionId) -> Manifest {
    serde_json::from_slice(&std::fs::read(store.path_for(id).unwrap()).unwrap()).unwrap()
}

fn chunk_names(store: &JsonSessionStore) -> BTreeSet<String> {
    match std::fs::read_dir(store.root.join(CHUNKS_DIR)) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().file_name().to_str().unwrap().to_owned())
            .filter(|name| name.strip_suffix(".bin").is_some_and(valid_hash))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
        Err(error) => panic!("cannot inspect chunks: {error}"),
    }
}

fn referenced_chunks(store: &JsonSessionStore, ids: &[&SessionId]) -> BTreeSet<String> {
    ids.iter()
        .flat_map(|id| manifest_for(store, id).chunks)
        .map(|hash| format!("{hash}.bin"))
        .collect()
}

fn assert_only_live_chunks(store: &JsonSessionStore, ids: &[&SessionId]) {
    assert_eq!(
        chunk_names(store),
        referenced_chunks(store, ids),
        "unpublished or superseded body chunks must not accumulate"
    );
}

fn copy_fork(store: &JsonSessionStore, record: &SessionRecord) -> SessionId {
    let fork = SessionId::parse("shared_gc_fork").unwrap();
    std::fs::copy(
        store.path_for(&record.id).unwrap(),
        store.path_for(&fork).unwrap(),
    )
    .unwrap();
    assert!(store.load(&fork).unwrap().entries == record.entries);
    fork
}

#[test]
fn prefix_updates_reclaim_superseded_chunks_instead_of_accumulating_history() {
    let workspace = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let text = payload();
    let mut current = record(workspace.path(), &text);
    for generation in 0..20 {
        update(&mut current, &text, generation);
        store.save(&mut current).unwrap();
        assert_only_live_chunks(&store, &[&current.id]);
        let bytes: u64 = chunk_names(&store)
            .iter()
            .map(|name| {
                std::fs::metadata(store.root.join(CHUNKS_DIR).join(name))
                    .unwrap()
                    .len()
            })
            .sum();
        assert!(bytes <= 12 * CHUNK_BYTES as u64);
    }
    assert!(store.load(&current.id).unwrap() == current);
    store.delete(&current.id).unwrap();
    assert!(chunk_names(&store).is_empty());
}

#[test]
fn collecting_multiple_orphan_batches_keeps_live_chunks_and_visits_every_candidate() {
    const ORPHANS: usize = 2 * 1024 + 9;
    let workspace = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let text = payload();
    let mut current = record(workspace.path(), &text);
    store.save(&mut current).unwrap();
    let live = referenced_chunks(&store, &[&current.id]);
    for index in 0..ORPHANS {
        let name = format!("{index:064x}.bin");
        assert!(!live.contains(&name));
        std::fs::write(store.root.join(CHUNKS_DIR).join(name), b"orphan").unwrap();
    }
    assert_eq!(chunk_names(&store).len(), live.len() + ORPHANS);
    store.save(&mut current).unwrap();
    assert_only_live_chunks(&store, &[&current.id]);
    assert!(store.load(&current.id).unwrap() == current);
    store.delete(&current.id).unwrap();
    assert!(chunk_names(&store).is_empty());
}

#[test]
fn copied_forks_keep_shared_chunks_until_the_last_referencing_session_is_deleted() {
    let workspace = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let text = payload();
    let mut current = record(workspace.path(), &text);
    store.save(&mut current).unwrap();
    let original_entries = current.entries.clone();
    let fork = copy_fork(&store, &current);
    assert_only_live_chunks(&store, &[&current.id, &fork]);

    update(&mut current, &text, 3);
    store.save(&mut current).unwrap();
    assert_only_live_chunks(&store, &[&current.id, &fork]);
    assert!(store.load(&fork).unwrap().entries == original_entries);
    store.delete(&current.id).unwrap();
    assert_only_live_chunks(&store, &[&fork]);
    assert!(store.load(&fork).unwrap().entries == original_entries);
    store.delete(&fork).unwrap();
    assert!(chunk_names(&store).is_empty());
}

#[test]
fn failed_index_publication_preserves_rollback_and_fork_snapshots() {
    let workspace = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let text = payload();
    let mut current = record(workspace.path(), &text);
    store.save(&mut current).unwrap();
    let previous = current.clone();
    let fork = copy_fork(&store, &current);
    let old_chunks = chunk_names(&store);
    let rollback = SessionId::parse("rollback_snapshot").unwrap();
    let original_index = store.path_for(&current.id).unwrap();
    let rollback_index = store.path_for(&rollback).unwrap();
    std::fs::rename(&original_index, &rollback_index).unwrap();
    std::fs::create_dir(&original_index).unwrap();
    std::fs::write(original_index.join("occupied"), "block index publication").unwrap();

    // A nonempty directory cannot be replaced by an atomic index-file rename
    // on any supported platform. The separately named signed snapshot keeps
    // the previous body recoverable throughout this injected failure.
    update(&mut current, &text, 7);
    assert!(store.save(&mut current).is_err());
    assert!(store.load(&rollback).unwrap().entries == previous.entries);
    assert!(store.load(&fork).unwrap().entries == previous.entries);
    for name in &old_chunks {
        assert!(store.root.join(CHUNKS_DIR).join(name).is_file());
    }
    std::fs::remove_dir_all(&original_index).unwrap();
    std::fs::rename(&rollback_index, &original_index).unwrap();
    assert!(store.load(&previous.id).unwrap() == previous);

    // A successful later update sweeps any uncommitted body left by the
    // failure, but must still respect the independently retained fork.
    update(&mut current, &text, 8);
    store.save(&mut current).unwrap();
    assert_only_live_chunks(&store, &[&current.id, &fork]);
    assert!(store.load(&fork).unwrap().entries == previous.entries);
    store.delete(&current.id).unwrap();
    assert_only_live_chunks(&store, &[&fork]);
    store.delete(&fork).unwrap();
    assert!(chunk_names(&store).is_empty());
}

#[test]
fn readers_and_independent_store_handles_observe_complete_snapshots_during_collection() {
    let workspace = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let text = Arc::new(payload());
    let mut initial = record(workspace.path(), &text);
    store.save(&mut initial).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let writer = std::thread::spawn({
        let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let text = Arc::clone(&text);
        let barrier = Arc::clone(&barrier);
        let mut current = initial.clone();
        move || {
            barrier.wait();
            for generation in 1..=4 {
                update(&mut current, &text, generation);
                store.save(&mut current).unwrap();
                std::thread::yield_now();
            }
            current
        }
    });
    let reader = std::thread::spawn({
        let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
        let id = initial.id.clone();
        let text = Arc::clone(&text);
        let barrier = Arc::clone(&barrier);
        move || {
            barrier.wait();
            for _ in 0..8 {
                let loaded = store.load(&id).unwrap();
                let EntryKind::User { content } = &loaded.entries[1].kind else {
                    panic!("user observation missing");
                };
                let body = content.text_blocks().next().unwrap();
                let mut parts = body.splitn(3, ':');
                let prefix = parts.next().unwrap();
                let generation: usize = parts.next().unwrap().parse().unwrap();
                assert!(generation <= 4);
                assert_eq!(prefix.len(), generation);
                assert!(parts.next().unwrap() == text.as_str());
                std::thread::yield_now();
            }
        }
    });
    barrier.wait();
    let latest = writer.join().unwrap();
    reader.join().unwrap();
    assert!(store.load(&latest.id).unwrap() == latest);
    assert_only_live_chunks(&store, &[&latest.id]);
}
