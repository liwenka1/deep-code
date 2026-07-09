#[cfg(test)]
mod integration {
    use std::sync::Arc;
    use std::time::Duration;

    use async_stream::try_stream;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use crate::client::{AgentEventStream, LlmClient};
    use crate::config::AgentConfig;
    use crate::error::AgentResult;
    use crate::event::AgentEvent;
    use crate::model::ChatRequest;
    use crate::runtime::AgentRuntime;
    use crate::subagent::registry::attach_subagent_tools;
    use crate::subagent::types::SubAgentStatus;
    use crate::tool::{ApprovalDecision, ApprovalRequest, ToolCall, ToolRegistry};

    #[derive(Clone)]
    struct SummaryClient;

    impl LlmClient for SummaryClient {
        fn provider_name(&self) -> &'static str {
            "summary"
        }

        fn model(&self) -> &str {
            "summary-model"
        }

        async fn stream_chat(&self, _request: ChatRequest) -> AgentResult<AgentEventStream> {
            let text = r#"### SUMMARY
Mapped the crate root.

### EVIDENCE
- src/lib.rs:1-10

### CHANGES
None.

### RISKS
None observed.

### BLOCKERS
None.
"#;
            let stream = try_stream! {
                for token in text.split_inclusive('\n') {
                    yield AgentEvent::TextDelta { text: token.to_string() };
                }
                yield AgentEvent::Done { usage: None };
            };
            Ok(Box::pin(stream))
        }
    }

    /// Panics while streaming — the child's run_loop task dies and the
    /// runner must map the broken event channel to a Failed record.
    #[derive(Clone)]
    struct PanicClient;

    impl LlmClient for PanicClient {
        fn provider_name(&self) -> &'static str {
            "panic"
        }

        fn model(&self) -> &str {
            "panic-model"
        }

        async fn stream_chat(&self, _request: ChatRequest) -> AgentResult<AgentEventStream> {
            panic!("synthetic client panic");
        }
    }

    #[derive(Clone)]
    struct SlowClient;

    impl LlmClient for SlowClient {
        fn provider_name(&self) -> &'static str {
            "slow"
        }

        fn model(&self) -> &str {
            "slow-model"
        }

        async fn stream_chat(&self, _request: ChatRequest) -> AgentResult<AgentEventStream> {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let stream = try_stream! {
                yield AgentEvent::TextDelta { text: "late".to_string() };
                yield AgentEvent::Done { usage: None };
            };
            Ok(Box::pin(stream))
        }
    }

    #[tokio::test]
    async fn panicking_child_finalizes_as_failed_not_running_forever() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(PanicClient);
        let cancel = CancellationToken::new();
        let mut registry = ToolRegistry::new();
        let services = attach_subagent_tools(
            &mut registry,
            Arc::clone(&client),
            AgentConfig::default(),
            dir.path().to_path_buf(),
            cancel,
        );

        let open = ToolCall::new(
            "call_1",
            "agent_open",
            json!({"prompt": "explode", "type": "explore", "name": "boom"}),
        );
        let open_result = registry
            .run_tool_call(open, None)
            .await
            .unwrap()
            .into_result()
            .unwrap();
        let opened: serde_json::Value = serde_json::from_str(&open_result.content).unwrap();
        let agent_id = opened["agent_id"].as_str().expect("agent_id").to_string();

        let mut terminal_status = None;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let manager = services.manager.read().unwrap();
            if let Some(record) = manager.get(&agent_id)
                && record.status.is_terminal()
            {
                terminal_status = Some((record.status, record.error.clone()));
                break;
            }
        }

        let (status, error) = terminal_status.expect("record must reach a terminal state");
        assert_eq!(status, SubAgentStatus::Failed);
        assert!(
            error.is_some_and(|message| message.contains("event stream ended")),
            "failure must carry the broken-channel reason"
        );
    }

    #[tokio::test]
    async fn agent_open_respects_concurrency_cap() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(SummaryClient);
        let cancel = CancellationToken::new();
        let mut registry = ToolRegistry::new();
        let services = attach_subagent_tools(
            &mut registry,
            Arc::clone(&client),
            AgentConfig::default(),
            dir.path().to_path_buf(),
            cancel,
        );
        {
            let mut manager = services.manager.write().unwrap();
            let _ = manager.set_max_concurrent(1);
        }

        let open = ToolCall::new(
            "call_1",
            "agent_open",
            json!({"prompt": "explore lib.rs", "type": "explore", "name": "first"}),
        );
        let second = ToolCall::new(
            "call_2",
            "agent_open",
            json!({"prompt": "explore mod.rs", "type": "explore", "name": "second"}),
        );

        let first = registry.run_tool_call(open, None).await.unwrap();
        assert!(first.is_result_success(), "open failed: {first:?}");
        let second = registry.run_tool_call(second, None).await;
        assert!(
            second.is_err(),
            "expected concurrency error, got {second:?}"
        );
        assert!(
            second
                .unwrap_err()
                .to_string()
                .contains("concurrency limit"),
            "unexpected error"
        );
    }

    #[tokio::test]
    async fn subagent_runs_to_structured_completion() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(SummaryClient);
        let cancel = CancellationToken::new();
        let mut registry = ToolRegistry::new();
        let services = attach_subagent_tools(
            &mut registry,
            Arc::clone(&client),
            AgentConfig::default(),
            dir.path().to_path_buf(),
            cancel,
        );

        let open = ToolCall::new(
            "call_1",
            "agent_open",
            json!({"prompt": "summarize lib.rs", "type": "explore", "name": "worker"}),
        );
        let open_result = registry
            .run_tool_call(open, None)
            .await
            .unwrap()
            .into_result()
            .unwrap();
        let opened: serde_json::Value = serde_json::from_str(&open_result.content).unwrap();
        let agent_id = opened["agent_id"].as_str().expect("agent_id").to_string();

        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let manager = services.manager.read().unwrap();
            if manager
                .get(&agent_id)
                .is_some_and(|record| record.status.is_terminal())
            {
                break;
            }
        }

        let eval = ToolCall::new(
            "call_2",
            "agent_eval",
            json!({"agent_id": agent_id, "wait": false}),
        );
        let eval_result = registry
            .run_tool_call(eval, None)
            .await
            .unwrap()
            .into_result()
            .unwrap();
        let projection: serde_json::Value = serde_json::from_str(&eval_result.content).unwrap();
        assert_eq!(projection["status"], "completed");
        assert!(
            projection["snapshot"]["structured"]["summary"]
                .as_str()
                .unwrap()
                .contains("Mapped")
        );
    }

    #[tokio::test]
    async fn agent_close_cancels_running_agent() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(SlowClient);
        let cancel = CancellationToken::new();
        let mut registry = ToolRegistry::new();
        let services = attach_subagent_tools(
            &mut registry,
            Arc::clone(&client),
            AgentConfig::default(),
            dir.path().to_path_buf(),
            cancel,
        );

        let open = ToolCall::new(
            "call_1",
            "agent_open",
            json!({"prompt": "slow task", "type": "explore", "name": "slow-worker"}),
        );
        let open_result = registry
            .run_tool_call(open, None)
            .await
            .unwrap()
            .into_result()
            .unwrap();
        let opened: serde_json::Value = serde_json::from_str(&open_result.content).unwrap();
        let agent_id = opened["agent_id"].as_str().expect("agent_id").to_string();

        let close = ToolCall::new("call_2", "agent_close", json!({"agent_id": agent_id}));
        let close_result = registry
            .run_tool_call(close, None)
            .await
            .unwrap()
            .into_result()
            .unwrap();
        let projection: serde_json::Value = serde_json::from_str(&close_result.content).unwrap();
        assert_eq!(projection["status"], "cancelled");

        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let manager = services.manager.read().unwrap();
            if manager
                .get(&agent_id)
                .is_some_and(|record| record.status.is_terminal())
            {
                break;
            }
        }
        let manager = services.manager.read().unwrap();
        assert_eq!(
            manager.get(&agent_id).map(|record| record.status),
            Some(SubAgentStatus::Cancelled)
        );
    }

    #[tokio::test]
    async fn parent_cancel_stops_background_subagent() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(SlowClient);
        let cancel = CancellationToken::new();
        let mut registry = ToolRegistry::new();
        let services = attach_subagent_tools(
            &mut registry,
            Arc::clone(&client),
            AgentConfig::default(),
            dir.path().to_path_buf(),
            cancel.clone(),
        );

        let open = ToolCall::new(
            "call_1",
            "agent_open",
            json!({"prompt": "slow", "type": "explore", "name": "bg"}),
        );
        let open_result = registry
            .run_tool_call(open, None)
            .await
            .unwrap()
            .into_result()
            .unwrap();
        let agent_id =
            serde_json::from_str::<serde_json::Value>(&open_result.content).unwrap()["agent_id"]
                .as_str()
                .expect("agent_id")
                .to_string();

        services.cancel_all_running();

        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let manager = services.manager.read().unwrap();
            if manager
                .get(&agent_id)
                .is_some_and(|record| record.status.is_terminal())
            {
                break;
            }
        }
        let manager = services.manager.read().unwrap();
        assert_eq!(
            manager.get(&agent_id).map(|record| record.status),
            Some(SubAgentStatus::Cancelled)
        );
    }

    #[tokio::test]
    async fn agent_eval_wait_timeout_sets_timed_out() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(SlowClient);
        let cancel = CancellationToken::new();
        let mut registry = ToolRegistry::new();
        attach_subagent_tools(
            &mut registry,
            client,
            AgentConfig::default(),
            dir.path().to_path_buf(),
            cancel,
        );

        let open = ToolCall::new(
            "call_1",
            "agent_open",
            json!({"prompt": "slow", "type": "explore", "name": "slow"}),
        );
        let open_result = registry
            .run_tool_call(open, None)
            .await
            .unwrap()
            .into_result()
            .unwrap();
        let agent_id =
            serde_json::from_str::<serde_json::Value>(&open_result.content).unwrap()["agent_id"]
                .as_str()
                .expect("agent_id")
                .to_string();

        let eval = ToolCall::new(
            "call_2",
            "agent_eval",
            json!({"agent_id": agent_id, "wait": true, "timeout_ms": 100}),
        );
        let eval_result = registry
            .run_tool_call(eval, None)
            .await
            .unwrap()
            .into_result()
            .unwrap();
        let projection: serde_json::Value = serde_json::from_str(&eval_result.content).unwrap();
        assert_eq!(projection["timed_out"], true);
        assert_eq!(projection["status"], "running");
    }

    #[tokio::test]
    async fn subagent_denies_write_tool_approval() {
        let runtime = AgentRuntime::new(SummaryClient, ToolRegistry::new());
        let request = ApprovalRequest {
            call_id: "call_1".to_string(),
            tool_name: "write_file".to_string(),
            description: "write".to_string(),
            arguments: json!({"path": "a.txt", "content": "x"}),
            risk_level: crate::execution_policy::RiskLevel::Medium,
            requires_sandbox: false,
            read_only: false,
            matched_rule: None,
            preview: None,
            safety_reasons: Vec::new(),
            safety_suggestions: Vec::new(),
        };
        assert_eq!(
            runtime.subagent_approval_decision(&request),
            ApprovalDecision::Denied
        );
    }

    trait ToolOutcomeExt {
        fn is_result_success(&self) -> bool;
        fn into_result(self) -> Option<crate::tool::ToolResult>;
    }

    impl ToolOutcomeExt for crate::tool::ToolRunOutcome {
        fn is_result_success(&self) -> bool {
            matches!(
                self,
                crate::tool::ToolRunOutcome::Result { result }
                    if result.status == crate::tool::ToolResultStatus::Success
            )
        }

        fn into_result(self) -> Option<crate::tool::ToolResult> {
            match self {
                crate::tool::ToolRunOutcome::Result { result } => Some(result),
                _ => None,
            }
        }
    }
}
