use super::*;

use crate::model::{ToolCallFunctionPayload, ToolCallPayload};
use crate::session_entry::{ExchangeResult, SessionEntry, ToolExchange};
use crate::tool::ToolResultStatus;
use serde_json::json;

fn source(workspace: &Path, relative: &Path, bytes: &[u8]) {
    let path = workspace.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn details_path() -> PathBuf {
    PathBuf::from(".deep-code")
        .join("spill")
        .join("tool-results")
        .join(format!("{}.txt", uuid::Uuid::new_v4()))
}

fn stream_path() -> PathBuf {
    PathBuf::from(".deep-code")
        .join("spill")
        .join("run-123-4-5")
        .join("job_owner_1.stdout.log")
}

fn add_result(record: &mut SessionRecord, text: String, details: Value) {
    record.entries.push(Arc::new(SessionEntry::assistant(
        "",
        None,
        vec![ToolExchange {
            call: ToolCallPayload {
                id: "call".into(),
                call_type: "function".into(),
                function: ToolCallFunctionPayload {
                    name: "shell".into(),
                    arguments: "{}".into(),
                },
            },
            result: Some(ExchangeResult {
                content: text.into(),
                details: Some(details),
                status: ToolResultStatus::Success,
                asked: false,
            }),
        }],
    )));
}

fn result(record: &SessionRecord) -> &ExchangeResult {
    record
        .entries
        .iter()
        .find_map(|entry| match &entry.kind {
            EntryKind::Assistant { exchanges, .. } => exchanges
                .first()
                .and_then(|exchange| exchange.result.as_ref()),
            _ => None,
        })
        .unwrap()
}

#[test]
fn a_bundle_survives_workspace_cleanup_with_images_full_output_and_nested_growing_streams() {
    let workspace = tempfile::tempdir().unwrap();
    let canonical = crate::paths::canonicalize(workspace.path()).unwrap();
    let store = JsonSessionStore::for_workspace(&canonical).unwrap();
    let asset = crate::image::snapshot_bytes(&canonical, b"GIF89a-observation").unwrap();
    let stream = stream_path();
    let stream_bytes = b"earlier output followed by later output";
    source(&canonical, &stream, stream_bytes);
    let full_output = details_path();
    source(
        &canonical,
        &full_output,
        b"the complete original tool output",
    );
    let full_details = json!({
        "job_id": "job", "status": "completed", "exit_code": 0,
        "command": format!("printf original-path-{}", canonical.display()),
        "stdout": {"text": "earlier output", "total_bytes": 7,
            "capture_complete": true, "spill": {"path": canonical.join(&stream), "bytes": 7, "capped": false}},
        "stderr": {"text": "", "total_bytes": 0, "capture_complete": true, "spill": null},
        "content_storage": {"path": full_output, "complete": true,
            "original_bytes": b"the complete original tool output".len(),
            "inline_truncated": true, "error": null},
    });
    let details_source = details_path();
    let details_bytes = serde_json::to_vec(&full_details).unwrap();
    source(&canonical, &details_source, &details_bytes);
    let mut preview = full_details.clone();
    preview["stdout"]["text"] = "preview".into();
    preview["details_storage"] = json!({"path": details_source, "complete": true,
        "original_bytes": details_bytes.len(), "error": null, "inline_truncated": true});
    let mut record = SessionRecord::new(canonical.clone(), "system")
        .with_extra_roots(vec![canonical.parent().unwrap().into()]);
    record.entries.push(Arc::new(SessionEntry::user_with_images(
        "observe",
        vec![asset.clone()],
    )));
    record
        .entries
        .push(Arc::new(SessionEntry::new(EntryKind::Compaction {
            summary: "archived observation".into(),
            archived_count: 1,
            has_archive: false,
            image_sources: vec![crate::CompactionImageSource {
                image: asset,
                role: crate::Role::Tool,
                call_id: Some("old".into()),
                tool_name: Some("read_file".into()),
            }],
        })));
    add_result(
        &mut record,
        format!(
            "[stdout truncated — complete stream saved: '{}' (1 KiB); read that file for the parts not shown]\n[Inline output shortened. Full output saved to {}; use read_file with next_start_byte for bounded continuation.]\n",
            canonical.join(&stream).display(),
            full_output.display()
        ),
        preview,
    );
    source(
        &canonical,
        Path::new(".deep-code/spill/unreferenced-private.txt"),
        b"must not travel",
    );
    source(
        &canonical,
        Path::new(".deep-code/checkpoints/large.bin"),
        b"checkpoint",
    );
    store.save(&mut record).unwrap();
    let out = tempfile::tempdir().unwrap();
    let destination = out.path().join("instance");
    let exported = store.export_bundle(&record.id, &destination).unwrap();
    drop(store);
    workspace.close().unwrap();
    assert!(!canonical.exists());

    let restored = JsonSessionStore::for_workspace(&destination)
        .unwrap()
        .load(&record.id)
        .unwrap();
    assert_eq!(restored, exported);
    assert!(restored.extra_roots.is_empty());
    assert!(restored.checkpoints.is_empty());
    for entry in &restored.entries {
        match &entry.kind {
            EntryKind::User { content } => {
                assert_eq!(
                    content.images().next().unwrap().read().unwrap(),
                    b"GIF89a-observation"
                );
            }
            EntryKind::Compaction { image_sources, .. } => {
                assert_eq!(
                    image_sources[0].image.read().unwrap(),
                    b"GIF89a-observation"
                );
            }
            _ => {}
        }
    }
    let observed = result(&restored);
    let text = observed.content.text();
    assert!(!text.contains(&format!("saved: '{}'", canonical.join(&stream).display())));
    let reported_stream = text
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("[stdout truncated — complete stream saved: '"))
        .and_then(|notice| notice.split_once("' ("))
        .map(|(path, _)| Path::new(path))
        .expect("the stream notice must identify its exported source");
    assert!(reported_stream.is_relative());
    assert_eq!(reported_stream, stream);
    assert_eq!(
        std::fs::read(destination.join(reported_stream)).unwrap(),
        stream_bytes
    );
    assert_eq!(
        std::fs::read(destination.join(&full_output)).unwrap(),
        b"the complete original tool output"
    );
    let preview = observed.details.as_ref().unwrap();
    assert_eq!(
        preview["stdout"]["spill"]["bytes"], 7,
        "historical capture counts do not grow with the file"
    );
    assert_eq!(preview["stdout"]["spill"]["path"], path_value(&stream));
    assert_eq!(
        std::fs::read(destination.join(&stream)).unwrap(),
        stream_bytes
    );
    let details =
        std::fs::read(destination.join(preview["details_storage"]["path"].as_str().unwrap()))
            .unwrap();
    assert_eq!(preview["details_storage"]["original_bytes"], details.len());
    let details: Value = serde_json::from_slice(&details).unwrap();
    assert_eq!(details["stdout"]["spill"]["path"], path_value(&stream));
    assert_eq!(
        details["command"], full_details["command"],
        "command provenance is not rewritten"
    );
    assert!(
        !destination
            .join(".deep-code/spill/unreferenced-private.txt")
            .exists()
    );
    assert!(!destination.join(".deep-code/checkpoints").exists());
}

#[test]
fn export_copies_and_verifies_large_images_referenced_by_failed_drafts() {
    use std::io::{Seek, SeekFrom};

    let workspace = tempfile::tempdir().unwrap();
    let source = workspace.path().canonicalize().unwrap();
    let input = source.join("large.gif");
    std::fs::write(&input, b"GIF89a-source").unwrap();
    let size = MAX_SOURCE_BYTES + 1;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&input)
        .unwrap()
        .set_len(size)
        .unwrap();
    let image = crate::image::snapshot_path(&source, &input).unwrap();
    let store = JsonSessionStore::for_workspace(&source).unwrap();
    let mut record = SessionRecord::new(source, "system");
    let mut draft = SessionEntry::user_with_images("draft", vec![image.clone()]);
    draft.unsent = Some(crate::UnsentMessage {
        error: Some("provider refused".into()),
        ..Default::default()
    });
    record.entries.push(Arc::new(draft));
    store.save(&mut record).unwrap();

    let out = tempfile::tempdir().unwrap();
    let destination = out.path().join("bundle");
    let exported = store.export_bundle(&record.id, &destination).unwrap();
    let entry = exported.entries.last().unwrap();
    assert!(entry.unsent.is_some());
    let EntryKind::User { content } = &entry.kind else {
        panic!("expected draft")
    };
    let copied = content.images().next().unwrap();
    assert_eq!(copied.bytes, size);
    assert_eq!(copied.digest, image.digest);
    assert_eq!(copied.workspace, destination.canonicalize().unwrap());
    copied.verify().unwrap();

    let mut corrupted = std::fs::OpenOptions::new()
        .write(true)
        .open(image.path())
        .unwrap();
    corrupted.seek(SeekFrom::End(-1)).unwrap();
    corrupted.write_all(&[0xff]).unwrap();
    drop(corrupted);
    let rejected = out.path().join("corrupt-bundle");
    assert!(
        store
            .export_bundle(&record.id, &rejected)
            .unwrap_err()
            .to_string()
            .contains("image asset content changed")
    );
    assert_eq!(
        std::fs::read_dir(rejected.join(".deep-code/images"))
            .unwrap()
            .count(),
        0
    );
    assert!(matches!(
        JsonSessionStore::for_workspace(rejected)
            .unwrap()
            .load(&record.id),
        Err(SessionStoreError::NotFound { .. })
    ));
}

#[test]
fn missing_and_outside_sources_fail_without_publishing_a_successful_export() {
    for outside in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        let canonical = crate::paths::canonicalize(workspace.path()).unwrap();
        let store = JsonSessionStore::for_workspace(&canonical).unwrap();
        let mut record = SessionRecord::new(canonical.clone(), "system");
        let image = crate::image::snapshot_bytes(&canonical, b"GIF89a-observation").unwrap();
        let image_relative = image.path().strip_prefix(&canonical).unwrap().to_path_buf();
        record.entries.push(Arc::new(SessionEntry::user_with_images(
            "observe",
            vec![image],
        )));
        let path = if outside {
            PathBuf::from("../outside-secret")
        } else {
            details_path()
        };
        add_result(
            &mut record,
            "done".into(),
            json!({"details_storage": {
                "path": path, "complete": true, "original_bytes": 4, "inline_truncated": true,
            }}),
        );
        store.save(&mut record).unwrap();
        let out = tempfile::tempdir().unwrap();
        let destination = out.path().join("instance");
        assert!(store.export_bundle(&record.id, &destination).is_err());
        assert!(
            destination.join(image_relative).is_file(),
            "an earlier source was already exported before the failure"
        );
        assert_eq!(
            std::fs::read_to_string(destination.join(".deep-code/.gitignore")).unwrap(),
            super::super::SELF_IGNORE_BODY,
            "partial observations must not become trackable after an export failure"
        );
        assert!(
            !destination
                .join(".deep-code/sessions")
                .join(format!("{}.json", record.id.as_str()))
                .exists()
        );
    }
}

#[cfg(unix)]
#[test]
fn source_and_destination_links_are_not_followed() {
    use std::os::unix::fs::symlink;
    let workspace = tempfile::tempdir().unwrap();
    let canonical = crate::paths::canonicalize(workspace.path()).unwrap();
    let store = JsonSessionStore::for_workspace(&canonical).unwrap();
    let secret = tempfile::tempdir().unwrap();
    std::fs::write(secret.path().join("secret"), b"sensitive").unwrap();
    let relative = details_path();
    std::fs::create_dir_all(canonical.join(relative.parent().unwrap())).unwrap();
    symlink(secret.path().join("secret"), canonical.join(&relative)).unwrap();
    let mut record = SessionRecord::new(canonical.clone(), "system");
    add_result(
        &mut record,
        "done".into(),
        json!({"details_storage": {
            "path": relative, "complete": true, "original_bytes": 9, "inline_truncated": true,
        }}),
    );
    store.save(&mut record).unwrap();
    let out = tempfile::tempdir().unwrap();
    assert!(
        store
            .export_bundle(&record.id, &out.path().join("source-link"))
            .is_err()
    );
    let destination = out.path().join("destination-link");
    symlink(secret.path(), &destination).unwrap();
    assert!(store.export_bundle(&record.id, &destination).is_err());
    assert!(!secret.path().join(".deep-code").exists());
}

#[test]
fn a_copied_signed_record_cannot_authorize_reading_another_workspaces_sources() {
    let workspace = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let store = JsonSessionStore::for_workspace(workspace.path()).unwrap();
    let mut record =
        SessionRecord::new(crate::paths::canonicalize(other.path()).unwrap(), "foreign");
    store.save(&mut record).unwrap();
    let out = tempfile::tempdir().unwrap();
    assert!(
        store
            .export_bundle(&record.id, &out.path().join("instance"))
            .is_err()
    );
}

#[test]
fn source_marker_examples_in_user_input_and_tool_source_code_are_not_export_references() {
    let workspace = tempfile::tempdir().unwrap();
    let canonical = crate::paths::canonicalize(workspace.path()).unwrap();
    let store = JsonSessionStore::for_workspace(&canonical).unwrap();
    let mut record = SessionRecord::new(canonical, "system");
    let user_text = "Explain this example:\n[Inline output shortened. Full output saved to not-a-captured-source; use read_file with next_start_byte for bounded continuation.]\n";
    record.entries.push(Arc::new(SessionEntry::user(user_text)));
    let stream = stream_path();
    source(store.workspace_path(), &stream, b"x");
    let tool_text = serde_json::to_string(&json!({"lines": [
        {"text": include_str!("../../runtime/tool_result.rs")},
        {"text": format!("[stdout truncated — complete stream saved: '{}' (1 byte); read that file for the parts not shown]", store.workspace_path().join(&stream).display())}
    ], "next_start_byte": null}))
    .unwrap();
    add_result(
        &mut record,
        tool_text.clone(),
        json!({"stdout": {"spill": {"path": store.workspace_path().join(stream), "bytes": 1, "capped": false}}}),
    );
    store.save(&mut record).unwrap();
    let out = tempfile::tempdir().unwrap();
    let exported = store
        .export_bundle(&record.id, &out.path().join("instance"))
        .unwrap();
    let EntryKind::User { content } = &exported.entries[1].kind else {
        panic!("missing user");
    };
    assert_eq!(content.text(), user_text);
    assert_eq!(result(&exported).content.text(), tool_text);
}

#[test]
fn short_agent_reports_with_complete_continuation_examples_do_not_create_sources() {
    let workspace = tempfile::tempdir().unwrap();
    let canonical = crate::paths::canonicalize(workspace.path()).unwrap();
    let store = JsonSessionStore::for_workspace(&canonical).unwrap();
    let unused = details_path();
    source(&canonical, &unused, b"not actually captured by the report");
    let examples = [
        "not-a-captured-source".to_string(),
        details_path().display().to_string(),
        unused.display().to_string(),
    ];
    for (index, path) in examples.iter().enumerate() {
        let report = format!(
            "Report example:\n[Inline output shortened. Full output saved to {path}; use read_file with next_start_byte for bounded continuation.]\n"
        );
        assert!(report.chars().count() < crate::runtime::tool_result::TOOL_OUTPUT_BUDGET);
        let mut record = SessionRecord::new(canonical.clone(), "system");
        add_result(
            &mut record,
            report.clone(),
            json!({"agent_id": "agent", "status": "completed"}),
        );
        let EntryKind::Assistant { exchanges, .. } =
            &mut Arc::make_mut(record.entries.last_mut().unwrap()).kind
        else {
            unreachable!();
        };
        exchanges[0].call.function.name = "agent".into();
        store.save(&mut record).unwrap();
        let out = tempfile::tempdir().unwrap();
        let destination = out.path().join(format!("example_{index}"));
        let exported = store.export_bundle(&record.id, &destination).unwrap();
        assert_eq!(result(&exported).content.text(), report);
        assert!(
            !destination.join(&unused).exists(),
            "text alone cannot establish a source reference"
        );
    }
}

#[test]
fn a_chunked_logical_session_exports_and_loads_after_source_cleanup() {
    let workspace = tempfile::tempdir().unwrap();
    let canonical = crate::paths::canonicalize(workspace.path()).unwrap();
    let store = JsonSessionStore::for_workspace(&canonical).unwrap();
    let mut record = SessionRecord::new(canonical.clone(), "system");
    for _ in 0..2 {
        record
            .entries
            .push(Arc::new(SessionEntry::user("x".repeat(4 * 1024 * 1024))));
    }
    store.save(&mut record).unwrap();
    assert!(
        std::fs::metadata(
            canonical
                .join(".deep-code/sessions")
                .join(format!("{}.json", record.id.as_str()))
        )
        .unwrap()
        .len()
            < super::super::MAX_SESSION_FILE_BYTES
    );
    let out = tempfile::tempdir().unwrap();
    let destination = out.path().join("instance");
    store.export_all_bundles(&destination).unwrap();
    drop(store);
    workspace.close().unwrap();
    let loaded = JsonSessionStore::for_workspace(&destination)
        .unwrap()
        .load(&record.id)
        .unwrap();
    assert_eq!(loaded.entries, record.entries);
}

#[test]
fn in_flight_sources_are_bounded_and_successful_siblings_release_their_budget() {
    let workspace = tempfile::tempdir().unwrap();
    let canonical = crate::paths::canonicalize(workspace.path()).unwrap();
    let path = details_path();
    source(&canonical, &path, b"data");
    let out = tempfile::tempdir().unwrap();
    let destination_path = crate::paths::canonicalize(out.path()).unwrap();
    let mut exporter = Exporter {
        source: crate::pinned_dir::open_canonical(&canonical).unwrap(),
        source_path: canonical,
        destination: crate::pinned_dir::open_canonical(&destination_path).unwrap(),
        destination_path,
        copied: HashMap::new(),
        copied_images: HashSet::new(),
        active: HashSet::new(),
        in_flight_bytes: super::super::MAX_SESSION_BODY_BYTES - 3,
        cancel: &CancellationToken::new(),
    };
    let error = exporter
        .spill(path.to_str().unwrap(), SourceKind::Content, None)
        .unwrap_err();
    assert!(error.to_string().contains("in-flight"));
    exporter.active.clear();
    exporter.in_flight_bytes = super::super::MAX_SESSION_BODY_BYTES - 4;
    exporter
        .spill(path.to_str().unwrap(), SourceKind::Content, None)
        .unwrap();
    assert_eq!(
        exporter.in_flight_bytes,
        super::super::MAX_SESSION_BODY_BYTES - 4
    );
    let sibling = details_path();
    source(&exporter.source_path, &sibling, b"more");
    exporter
        .spill(sibling.to_str().unwrap(), SourceKind::Content, None)
        .unwrap();
    assert_eq!(
        exporter.in_flight_bytes,
        super::super::MAX_SESSION_BODY_BYTES - 4
    );
}
