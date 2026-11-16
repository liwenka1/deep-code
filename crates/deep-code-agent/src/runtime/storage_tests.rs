use super::*;

#[tokio::test]
async fn a_long_write_turn_restores_every_argument_and_completed_result() {
    const STEPS: usize = 150;
    const CALLS: usize = STEPS * 4;
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().canonicalize().unwrap();
    let payload = "x".repeat(15_000);
    let mut scripts: Vec<_> = (0..STEPS)
        .map(|step| {
            let mut events: Vec<_> = (0..4)
                .map(|slot| AgentEvent::ToolCallDelta {
                    delta: indexed_tool_call_delta(
                        slot,
                        &format!("write_{}", step * 4 + slot as usize),
                        "write_file",
                        &serde_json::json!({"path": "payload.txt", "content": payload}).to_string(),
                    ),
                })
                .collect();
            events.push(AgentEvent::Done { usage: None });
            events
        })
        .collect();
    scripts.push(ok_turn());
    let config = AgentConfig {
        model: DEEPSEEK_V4_PRO.into(),
        ..AgentConfig::builtin()
    };
    let runtime = AgentRuntime::with_new_session(
        ScriptedClient::new(scripts),
        crate::workspace_tools::workspace_tool_registry(workspace.clone()).unwrap(),
        "system",
        &workspace,
        &config,
    )
    .unwrap()
    .with_permission_mode(crate::execution_policy::SharedPermissionMode::new(
        crate::execution_policy::PermissionMode::AcceptEdits,
    ));
    let mut receiver = runtime.submit_user("write the payloads in one turn").await;
    let (mut completed, mut finished) = (0, false);
    while let Some(event) = receiver.recv().await {
        match event {
            RuntimeEvent::ToolCallFinished { result, .. } => {
                assert_eq!(result.status, ToolResultStatus::Success);
                completed += 1;
            }
            RuntimeEvent::SessionUpdated { save_error, .. } => {
                assert!(save_error.is_none(), "{save_error:?}");
            }
            RuntimeEvent::Error { message, .. } => panic!("{message}"),
            RuntimeEvent::TurnFinished { .. } => finished = true,
            _ => {}
        }
    }
    assert_eq!(completed, CALLS);
    assert!(finished);
    assert_eq!(runtime.flush_session().await, None);
    runtime.shutdown().await;
    assert_eq!(runtime.last_save_error(), None);
    let store = crate::JsonSessionStore::for_workspace(&workspace).unwrap();
    let id = runtime.session_id().await.unwrap();
    let saved = store.load(&id).unwrap();
    let exchanges: Vec<_> = saved
        .entries
        .iter()
        .flat_map(|entry| match &entry.kind {
            crate::EntryKind::Assistant { exchanges, .. } => exchanges.as_slice(),
            _ => &[],
        })
        .collect();
    assert_eq!(exchanges.len(), CALLS);
    for (index, exchange) in exchanges.iter().enumerate() {
        assert_eq!(exchange.call.id, format!("write_{index}"));
        let args: serde_json::Value =
            serde_json::from_str(&exchange.call.function.arguments).unwrap();
        assert_eq!(args["path"], "payload.txt");
        assert_eq!(args["content"], payload);
        assert_eq!(
            exchange.result.as_ref().unwrap().status,
            ToolResultStatus::Success
        );
    }
    let full = serde_json::to_vec_pretty(&saved).unwrap();
    assert!(full.len() as u64 > crate::session_store::MAX_SESSION_FILE_BYTES);
    let index_path = workspace
        .join(".deep-code")
        .join("sessions")
        .join(format!("{}.json", id.as_str()));
    assert!(
        std::fs::metadata(index_path).unwrap().len() < crate::session_store::MAX_SESSION_FILE_BYTES
    );
    let resumed = AgentRuntime::from_session_record(
        ScriptedClient::new(Vec::new()),
        ToolRegistry::default(),
        saved,
        store.clone(),
        config,
    );
    assert_eq!(
        resumed.session_messages().await,
        runtime.session_messages().await
    );
    resumed.shutdown().await;
    assert_eq!(resumed.last_save_error(), None);
    assert_eq!(
        store.load(&id).unwrap().entries,
        runtime.live_snapshot().await.unwrap().record.entries
    );
}
