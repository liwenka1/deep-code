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
    use crate::subagent::roles::SubAgentRole;
    use crate::subagent::types::SubAgentStatus;
    use crate::tool::{ApprovalDecision, ApprovalRequest, ToolCall, ToolCx, ToolRegistry};

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

    /// Never produces anything: exercises the wall-clock timeout path.
    #[derive(Clone)]
    struct StuckClient;

    impl LlmClient for StuckClient {
        fn provider_name(&self) -> &'static str {
            "stuck"
        }

        fn model(&self) -> &str {
            "stuck-model"
        }

        async fn stream_chat(&self, _request: ChatRequest) -> AgentResult<AgentEventStream> {
            let stream = try_stream! {
                futures_util::future::pending::<()>().await;
                yield AgentEvent::Done { usage: None };
            };
            Ok(Box::pin(stream))
        }
    }

    fn agent_call(id: &str, task: &str, role: &str) -> ToolCall {
        ToolCall::new(id, "agent", json!({"task": task, "role": role}))
    }

    #[tokio::test]
    async fn agent_call_blocks_until_report_and_finalizes_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(SummaryClient);
        let cancel = CancellationToken::new();
        let mut registry = ToolRegistry::new();
        let services = attach_subagent_tools(
            &mut registry,
            client,
            AgentConfig::default(),
            dir.path().to_path_buf(),
            cancel,
        );

        let result = registry
            .run_tool_call(agent_call("call_1", "summarize lib.rs", "explore"), None)
            .await
            .unwrap()
            .into_result()
            .unwrap();

        assert_eq!(result.status, crate::tool::ToolResultStatus::Success);
        assert!(
            result.content.contains("Mapped the crate root"),
            "tool result must be the child's report, got: {}",
            result.content
        );

        let manager = services.manager.read().unwrap();
        let records = manager.list_current_session();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].status, SubAgentStatus::Completed);
        assert!(
            records[0]
                .structured
                .as_ref()
                .is_some_and(|report| report.summary.contains("Mapped")),
            "ledger must hold the parsed structured report"
        );
    }

    #[tokio::test]
    async fn panicking_child_returns_soft_error_and_finalizes_failed() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(PanicClient);
        let cancel = CancellationToken::new();
        let mut registry = ToolRegistry::new();
        let services = attach_subagent_tools(
            &mut registry,
            client,
            AgentConfig::default(),
            dir.path().to_path_buf(),
            cancel,
        );

        let result = registry
            .run_tool_call(agent_call("call_1", "explode", "explore"), None)
            .await
            .unwrap()
            .into_result()
            .unwrap();

        assert_eq!(result.status, crate::tool::ToolResultStatus::Error);
        assert!(
            result.content.contains("sub-agent failed"),
            "model must read the failure, got: {}",
            result.content
        );
        let manager = services.manager.read().unwrap();
        assert_eq!(
            manager.list_current_session()[0].status,
            SubAgentStatus::Failed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_calls_beyond_cap_error_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(SlowClient);
        let cancel = CancellationToken::new();
        let mut registry = ToolRegistry::new();
        let services = attach_subagent_tools(
            &mut registry,
            client,
            AgentConfig::default(),
            dir.path().to_path_buf(),
            cancel,
        );
        {
            let mut manager = services.manager.write().unwrap();
            let _ = manager.set_max_concurrent(1);
        }

        // Both calls poll concurrently: the first inserts its Running record
        // before its first await; the second hits the cap.
        let (first, second) = tokio::join!(
            registry.run_tool_call(agent_call("call_1", "slow one", "explore"), None),
            registry.run_tool_call(agent_call("call_2", "slow two", "explore"), None),
        );
        assert!(first.is_ok(), "first child must run: {first:?}");
        let error = second.expect_err("second child must hit the cap");
        assert!(
            error.to_string().contains("concurrency limit"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn session_shutdown_cancels_inflight_child() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(SlowClient);
        let cancel = CancellationToken::new();
        let mut registry = ToolRegistry::new();
        let services = attach_subagent_tools(
            &mut registry,
            client,
            AgentConfig::default(),
            dir.path().to_path_buf(),
            cancel,
        );

        let call = registry.run_tool_call(agent_call("call_1", "slow", "explore"), None);
        tokio::pin!(call);
        // Let the child start, then pull the session-wide plug.
        tokio::select! {
            biased;
            _ = &mut call => panic!("child must still be running"),
            () = tokio::task::yield_now() => {}
        }
        services.cancel_all_running();

        let result = call.await.unwrap().into_result().unwrap();
        assert_eq!(result.status, crate::tool::ToolResultStatus::Error);
        assert!(result.content.contains("cancelled"));
        let manager = services.manager.read().unwrap();
        assert_eq!(
            manager.list_current_session()[0].status,
            SubAgentStatus::Cancelled
        );
    }

    #[tokio::test(start_paused = true)]
    async fn turn_cancel_token_cancels_child() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(SlowClient);
        let cancel = CancellationToken::new();
        let mut registry = ToolRegistry::new();
        let services = attach_subagent_tools(
            &mut registry,
            client,
            AgentConfig::default(),
            dir.path().to_path_buf(),
            cancel,
        );

        let turn_cancel = CancellationToken::new();
        let call = agent_call("call_1", "slow", "explore");
        let plan = registry.evaluate_tool(&call);
        let cx = ToolCx::new().with_cancel(turn_cancel.clone());
        let run = registry.run_tool_call_with_plan(&call, None, plan, cx);
        tokio::pin!(run);
        tokio::select! {
            biased;
            _ = &mut run => panic!("child must still be running"),
            () = tokio::task::yield_now() => {}
        }
        turn_cancel.cancel();

        let result = run.await.unwrap().into_result().unwrap();
        assert!(result.content.contains("cancelled"));
        let manager = services.manager.read().unwrap();
        assert_eq!(
            manager.list_current_session()[0].status,
            SubAgentStatus::Cancelled
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stuck_child_hits_wall_clock_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(StuckClient);
        let cancel = CancellationToken::new();
        let mut registry = ToolRegistry::new();
        // Widen the stream guards past the agent wall clock so this test
        // exercises the tool's own ceiling, not the transport watchdog.
        let config = AgentConfig {
            stream_chunk_timeout: Duration::from_secs(7200),
            stream_total_timeout: Duration::from_secs(7200),
            ..AgentConfig::default()
        };
        let services = attach_subagent_tools(
            &mut registry,
            client,
            config,
            dir.path().to_path_buf(),
            cancel,
        );

        let result = registry
            .run_tool_call(agent_call("call_1", "hang forever", "explore"), None)
            .await
            .unwrap()
            .into_result()
            .unwrap();

        assert_eq!(result.status, crate::tool::ToolResultStatus::Error);
        assert!(
            result.content.contains("timeout"),
            "must surface the wall-clock timeout, got: {}",
            result.content
        );
        let manager = services.manager.read().unwrap();
        assert_eq!(
            manager.list_current_session()[0].status,
            SubAgentStatus::Failed
        );
    }

    #[tokio::test]
    async fn approval_posture_allows_writes_only_for_writing_roles() {
        let runtime = AgentRuntime::new(SummaryClient, ToolRegistry::new());
        let write_request = ApprovalRequest {
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
        // Dispatching a writing role is the write authorization.
        assert_eq!(
            runtime.subagent_approval_decision(&write_request, SubAgentRole::Implementer),
            ApprovalDecision::Approved
        );
        assert_eq!(
            runtime.subagent_approval_decision(&write_request, SubAgentRole::General),
            ApprovalDecision::Approved
        );
        // Read-only roles stay read-only.
        assert_eq!(
            runtime.subagent_approval_decision(&write_request, SubAgentRole::Explore),
            ApprovalDecision::Denied
        );
        // The posture never extends past file writes: untrusted shell is
        // denied even for implementer.
        let shell_request = ApprovalRequest {
            call_id: "call_2".to_string(),
            tool_name: "shell".to_string(),
            description: "run".to_string(),
            arguments: json!({"command": "curl https://example.com"}),
            risk_level: crate::execution_policy::RiskLevel::High,
            requires_sandbox: false,
            read_only: false,
            matched_rule: None,
            preview: None,
            safety_reasons: Vec::new(),
            safety_suggestions: Vec::new(),
        };
        assert_eq!(
            runtime.subagent_approval_decision(&shell_request, SubAgentRole::Implementer),
            ApprovalDecision::Denied
        );
    }

    /// Real end-to-end smoke: a live DeepSeek child runs the `agent` tool
    /// against a scratch workspace and must return the structured report.
    /// Confirms the machinery (child runtime, workspace tools, output
    /// contract) works against the actual model. Whether the *parent* chooses
    /// to delegate given the system-prompt guidance is a prompt-quality
    /// question for dogfooding, not an assertion. Run manually:
    /// `DEEPSEEK_API_KEY=... cargo test -p deep-code-agent subagent -- --ignored`
    #[tokio::test]
    #[ignore = "requires DEEPSEEK_API_KEY and network"]
    async fn real_deepseek_child_returns_structured_report() {
        let api_key = std::env::var("DEEPSEEK_API_KEY")
            .ok()
            .filter(|key| !key.trim().is_empty())
            .expect("set DEEPSEEK_API_KEY to run this smoke test");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .unwrap();
        let config = AgentConfig {
            api_key: Some(api_key),
            ..AgentConfig::builtin()
        };
        let client = Arc::new(crate::client::DeepSeekClient::new(config.clone()).expect("client"));
        let mut registry = ToolRegistry::new();
        attach_subagent_tools(
            &mut registry,
            client,
            config,
            dir.path().to_path_buf(),
            CancellationToken::new(),
        );

        let result = registry
            .run_tool_call(
                agent_call(
                    "call_1",
                    "Read lib.rs in the workspace and report what its single function does. Read-only.",
                    "explore",
                ),
                None,
            )
            .await
            .unwrap()
            .into_result()
            .unwrap();

        assert_eq!(
            result.status,
            crate::tool::ToolResultStatus::Success,
            "child run failed: {}",
            result.content
        );
        assert!(
            result.content.contains("SUMMARY"),
            "report must carry the structured contract, got: {}",
            result.content
        );
    }

    trait ToolOutcomeExt {
        fn into_result(self) -> Option<crate::tool::ToolResult>;
    }

    impl ToolOutcomeExt for crate::tool::ToolRunOutcome {
        fn into_result(self) -> Option<crate::tool::ToolResult> {
            match self {
                crate::tool::ToolRunOutcome::Result { result } => Some(result),
                _ => None,
            }
        }
    }
}
