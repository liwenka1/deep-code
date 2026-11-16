use super::*;
use crate::model::ToolCallFunctionPayload;
use crate::tool::details::{PreparedPreview, serialization_count};
use serde_json::{Value, json};

fn call(id: &str) -> ToolCallPayload {
    ToolCallPayload {
        id: id.into(),
        call_type: "function".into(),
        function: ToolCallFunctionPayload {
            name: "apply_patch".into(),
            arguments: "{}".into(),
        },
    }
}

fn details(bytes: usize) -> Value {
    json!({
        "edit": {"path": "file", "diff": "x".repeat(bytes), "added": 1,
            "removed": 1, "truncated": false, "new_file": false},
        "details_storage": {"path": "full.json", "complete": true,
            "original_bytes": bytes, "error": null, "inline_truncated": false},
    })
}

fn completed(id: &str, details: Value) -> Arc<SessionEntry> {
    Arc::new(SessionEntry::assistant(
        "",
        None,
        vec![ToolExchange {
            call: call(id),
            result: Some(ExchangeResult {
                content: Content::from("done"),
                details: Some(details),
                status: ToolResultStatus::Success,
                asked: false,
            }),
        }],
    ))
}

fn result(session: &Session, id: &str) -> Value {
    session
        .entries()
        .iter()
        .find_map(|entry| match &entry.kind {
            EntryKind::Assistant { exchanges, .. } => exchanges
                .iter()
                .find(|exchange| exchange.call.id == id)
                .and_then(|exchange| exchange.result.as_ref())
                .and_then(|result| result.details.clone()),
            _ => None,
        })
        .unwrap()
}

fn append_prepared(session: &mut Session, id: &str, details: Value) {
    session.push_assistant(TurnId::new(), "", "", vec![call(id)]);
    let prepared = PreparedPreview::new(&details);
    let before = serialization_count();
    assert!(session.record_prepared_tool_result(
        id,
        Content::from("done"),
        ToolResultStatus::Success,
        Ask::Unasked,
        Some(details),
        prepared,
    ));
    assert_eq!(
        serialization_count(),
        before,
        "recording and eviction must use the prepared cost and compact value"
    );
}

#[test]
fn new_results_do_not_serialize_retained_or_archived_history_again() {
    let original = details(14_000);
    let entries = (0..256)
        .map(|index| completed(&format!("old_{index}"), original.clone()))
        .collect();
    let mut session = Session::from_entries(entries);
    assert!(result(&session, "old_0")["edit"].get("diff").is_none());
    assert_eq!(
        result(&session, "old_255")["edit"]["diff"],
        original["edit"]["diff"]
    );
    let before = serialization_count();
    for index in 0..8 {
        let id = format!("new_{index}");
        session.push_assistant(TurnId::new(), "", "", vec![call(&id)]);
        assert!(session.record_tool_result(
            &id,
            Content::from("done"),
            ToolResultStatus::Success,
            Ask::Unasked,
            Some(original.clone()),
        ));
    }
    assert_eq!(
        serialization_count() - before,
        8 * 2,
        "only each new details value and its compact representation are measured"
    );
    assert!(result(&session, "old_0")["edit"].get("diff").is_none());
    assert!(result(&session, "new_7")["edit"].get("diff").is_some());
}

#[test]
fn prepared_recording_evicts_without_serializing_any_details() {
    let original = details(14_000);
    let entries = (0..128)
        .map(|index| completed(&format!("old_{index}"), original.clone()))
        .collect();
    let mut session = Session::from_entries(entries);
    let first_retained = (0..128)
        .find(|index| {
            result(&session, &format!("old_{index}"))["edit"]
                .get("diff")
                .is_some()
        })
        .unwrap();
    append_prepared(&mut session, "new", original);
    assert!(
        result(&session, &format!("old_{first_retained}"))["edit"]
            .get("diff")
            .is_none()
    );
    assert!(result(&session, "new")["edit"].get("diff").is_some());
}

#[test]
fn preview_locations_survive_a_partial_insert_and_compaction_replacement() {
    let original = details(400_000);
    let anchor = Arc::new(SessionEntry::user("task"));
    let mut session = Session::from_entries(vec![
        Arc::new(SessionEntry::system("system")),
        anchor.clone(),
        completed("old_0", original.clone()),
        completed("old_1", original.clone()),
    ]);
    assert!(session.insert_partial_assistant_after(
        &anchor,
        TurnId::new(),
        "late partial reply".into(),
        String::new(),
    ));
    append_prepared(&mut session, "new_0", original.clone());
    assert!(result(&session, "old_0")["edit"].get("diff").is_none());
    assert!(result(&session, "old_1")["edit"].get("diff").is_some());
    assert!(matches!(&session.entries()[2].kind,
        EntryKind::Assistant {content, exchanges, ..}
            if content == "late partial reply" && exchanges.is_empty()
    ));

    let surviving = session.entries()[4..].to_vec();
    let mut compacted = vec![Arc::new(SessionEntry::compaction("summary", 3))];
    compacted.extend(surviving);
    session.replace_entries(compacted);
    append_prepared(&mut session, "new_1", original);
    assert!(result(&session, "old_1")["edit"].get("diff").is_none());
    assert!(result(&session, "new_0")["edit"].get("diff").is_some());
    assert!(result(&session, "new_1")["edit"].get("diff").is_some());
}
