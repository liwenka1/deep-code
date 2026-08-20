#[cfg(test)]
mod integration {
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    use async_stream::try_stream;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::client::{AgentEventStream, LlmClient};
    use crate::config::AgentConfig;
    use crate::error::AgentResult;
    use crate::event::AgentEvent;
    use crate::model::{ChatRequest, FunctionCallDelta, ToolCallDelta};
    use crate::runtime::AgentRuntime;
    use crate::subagent::registry::attach_subagent_tools;
    use crate::subagent::roles::SubAgentRole;
    use crate::subagent::types::SubAgentStatus;
    use crate::tool::{ApprovalDecision, ApprovalRequest, ToolCall, ToolCx, ToolRegistry};

    #[derive(Clone)]
    struct SummaryClient;

    #[async_trait::async_trait]
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

    /// Streams the canonical report while recording each request's model id,
    /// so tests can assert which tier a child actually ran on.
    struct ModelRecordingClient {
        models: StdMutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl LlmClient for ModelRecordingClient {
        fn provider_name(&self) -> &'static str {
            "model-recording"
        }

        fn model(&self) -> &str {
            "model-recording"
        }

        async fn stream_chat(&self, request: ChatRequest) -> AgentResult<AgentEventStream> {
            self.models.lock().unwrap().push(request.model.clone());
            let stream = try_stream! {
                yield AgentEvent::TextDelta {
                    text: "### SUMMARY\nok\n\n### EVIDENCE\nNone.\n\n### CHANGES\nNone.\n\n### RISKS\nNone.\n\n### BLOCKERS\nNone.\n".to_string(),
                };
                yield AgentEvent::Done { usage: None };
            };
            Ok(Box::pin(stream))
        }
    }

    /// First request: one read-only tool call; afterwards: the canonical
    /// report. Exercises a child that does real tool work before reporting.
    struct OneCallThenReportClient {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LlmClient for OneCallThenReportClient {
        fn provider_name(&self) -> &'static str {
            "one-call"
        }

        fn model(&self) -> &str {
            "one-call"
        }

        async fn stream_chat(&self, _request: ChatRequest) -> AgentResult<AgentEventStream> {
            let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
            let stream = try_stream! {
                if first {
                    yield AgentEvent::ToolCallDelta {
                        delta: ToolCallDelta {
                            index: Some(0),
                            id: Some("probe_call_1".to_string()),
                            call_type: Some("function".to_string()),
                            function: Some(FunctionCallDelta {
                                name: Some("list_dir".to_string()),
                                arguments: Some(r#"{"path":"."}"#.to_string()),
                            }),
                        },
                    };
                } else {
                    yield AgentEvent::TextDelta {
                        text: "### SUMMARY\nok\n\n### EVIDENCE\nNone.\n\n### CHANGES\nNone.\n\n### RISKS\nNone.\n\n### BLOCKERS\nNone.\n".to_string(),
                    };
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

    #[async_trait::async_trait]
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

    #[async_trait::async_trait]
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

    #[async_trait::async_trait]
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

    /// Emits a read-only tool call on every turn, so an uninterrupted child
    /// loops until max_steps. Counts model calls to prove that cancellation
    /// halts the child's own loop, not merely the parent's wait.
    #[derive(Clone)]
    struct LoopingClient {
        model_calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl LlmClient for LoopingClient {
        fn provider_name(&self) -> &'static str {
            "looping"
        }

        fn model(&self) -> &str {
            "looping-model"
        }

        async fn stream_chat(&self, _request: ChatRequest) -> AgentResult<AgentEventStream> {
            let n = self.model_calls.fetch_add(1, Ordering::SeqCst) + 1;
            let stream = try_stream! {
                // A small per-call delay opens a window for the test to cancel
                // mid-loop instead of the loop finishing in one logical instant.
                tokio::time::sleep(Duration::from_millis(50)).await;
                yield AgentEvent::ToolCallDelta {
                    delta: ToolCallDelta {
                        index: Some(0),
                        id: Some(format!("loop_call_{n}")),
                        call_type: Some("function".to_string()),
                        function: Some(FunctionCallDelta {
                            name: Some("list_dir".to_string()),
                            arguments: Some(r#"{"path":"."}"#.to_string()),
                        }),
                    },
                };
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

    /// Reconnaissance roles are pinned to the flash tier; writing/planning
    /// roles inherit the parent's configured model (the point of dispatching
    /// them is their output quality).
    #[tokio::test]
    async fn reconnaissance_roles_pin_flash_and_implementer_inherits() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(ModelRecordingClient {
            models: StdMutex::new(Vec::new()),
        });
        let cancel = CancellationToken::new();
        let mut registry = ToolRegistry::new();
        let services = attach_subagent_tools(
            &mut registry,
            client.clone(),
            AgentConfig::default(),
            dir.path().to_path_buf(),
            cancel,
        );

        registry
            .run_tool_call(agent_call("call_1", "map the crate", "explore"), None)
            .await
            .unwrap();
        // A writing role's spawn is the write authorization, so it prompts
        // (asserted in the policy tests); approve it here to reach the child.
        let implementer = agent_call("call_2", "land the fix", "implementer");
        assert!(registry.evaluate_tool(&implementer).requires_approval);
        registry
            .run_tool_call(implementer, Some(ApprovalDecision::Approved))
            .await
            .unwrap();

        let models = client.models.lock().unwrap().clone();
        assert_eq!(models.len(), 2, "one child request per call: {models:?}");
        assert_eq!(
            models[0],
            crate::model_registry::DEEPSEEK_V4_FLASH,
            "explore child must run on the flash tier"
        );
        assert_eq!(
            models[1], services.agent_config.model,
            "implementer child must inherit the parent's configured model"
        );
    }

    /// A child's tool calls stream one progress line each into the parent's
    /// ToolCallProgress channel, so a long child run shows live activity.
    #[tokio::test]
    async fn child_tool_calls_stream_progress_to_parent() {
        let dir = tempfile::tempdir().unwrap();
        let client = Arc::new(OneCallThenReportClient {
            calls: AtomicUsize::new(0),
        });
        let cancel = CancellationToken::new();
        let mut registry = ToolRegistry::new();
        let _services = attach_subagent_tools(
            &mut registry,
            client,
            AgentConfig::default(),
            dir.path().to_path_buf(),
            cancel,
        );

        let updates: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
        let update_fn: crate::tool::ToolUpdateFn = {
            let updates = Arc::clone(&updates);
            Arc::new(move |update| updates.lock().unwrap().push(update.text))
        };
        let call = agent_call("call_1", "list the workspace", "explore");
        let plan = registry.evaluate_tool(&call);
        let result = registry
            .run_tool_call_with_plan(&call, None, plan, ToolCx::new().with_update_fn(update_fn))
            .await
            .unwrap()
            .into_result()
            .unwrap();

        assert_eq!(result.status, crate::tool::ToolResultStatus::Success);
        let updates = updates.lock().unwrap().clone();
        assert!(
            updates.iter().any(|line| {
                line.starts_with("[explore] +")
                    && line.contains(" step 1/")
                    && line.ends_with(": list_dir")
            }),
            "child tool call must stream a role/elapsed/step progress line, got: {updates:?}"
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
            manager.set_max_concurrent(1);
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

    /// Regression pin for the orphan bug (F1): cancelling the parent turn must
    /// stop the child's own run_loop, not merely abandon the wait. An
    /// uninterrupted `LoopingClient` child would run to max_steps; a properly
    /// cancelled one freezes its model-call count. Real time (not paused) so
    /// the per-call delay creates a genuine mid-loop cancel window.
    #[tokio::test]
    async fn cancel_halts_child_loop_not_just_the_wait() {
        let dir = tempfile::tempdir().unwrap();
        let model_calls = Arc::new(AtomicUsize::new(0));
        let client = Arc::new(LoopingClient {
            model_calls: Arc::clone(&model_calls),
        });
        let mut registry = ToolRegistry::new();
        attach_subagent_tools(
            &mut registry,
            client,
            AgentConfig::default(),
            dir.path().to_path_buf(),
            CancellationToken::new(),
        );

        let turn_cancel = CancellationToken::new();
        let call = agent_call("call_1", "loop over the workspace", "explore");
        let plan = registry.evaluate_tool(&call);
        let cx = ToolCx::new().with_cancel(turn_cancel.clone());

        let canceller = {
            let token = turn_cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                token.cancel();
            })
        };

        let result = registry
            .run_tool_call_with_plan(&call, None, plan, cx)
            .await
            .unwrap()
            .into_result()
            .unwrap();
        canceller.await.unwrap();

        assert!(
            result.content.contains("cancelled"),
            "expected cancellation, got: {}",
            result.content
        );
        // It did start working before the cancel...
        let at_cancel = model_calls.load(Ordering::SeqCst);
        assert!(at_cancel >= 1, "child never ran before cancel");
        assert!(
            at_cancel < 25,
            "child ran to near max_steps despite cancel: {at_cancel} model calls"
        );
        // ...and — the real orphan detector — a killed run_loop makes no
        // further model calls. A surviving orphan would keep calling every
        // ~50ms, so the count would climb across this window.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            model_calls.load(Ordering::SeqCst),
            at_cancel,
            "model calls kept climbing after cancel — child run_loop is an orphan"
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
            network: false,
            call_id: "call_1".to_string(),
            tool_name: "write_file".to_string(),
            description: "write".to_string(),
            arguments: json!({"path": "a.txt", "content": "x"}),
            risk_level: crate::execution_policy::RiskLevel::Medium,
            requires_sandbox: false,
            read_only: false,
            matched_rule: None,
            justification: None,
            resolved_target: None,
            preview: None,
            safety_notes: Vec::new(),
        };
        // Explicitly dispatching `implementer` is the write authorization.
        assert_eq!(
            runtime.subagent_approval_decision(&write_request, SubAgentRole::Implementer),
            ApprovalDecision::Approved
        );
        // The default role (general) and every read-only role stay read-only,
        // so a bare `agent(task=...)` call cannot write unattended.
        assert_eq!(
            runtime.subagent_approval_decision(&write_request, SubAgentRole::General),
            ApprovalDecision::Denied
        );
        assert_eq!(
            runtime.subagent_approval_decision(&write_request, SubAgentRole::Explore),
            ApprovalDecision::Denied
        );
        // The posture never extends past file writes: untrusted shell is
        // denied even for implementer.
        let shell_request = ApprovalRequest {
            network: false,
            call_id: "call_2".to_string(),
            tool_name: "shell".to_string(),
            description: "run".to_string(),
            arguments: json!({"command": "curl https://example.com"}),
            risk_level: crate::execution_policy::RiskLevel::High,
            requires_sandbox: false,
            read_only: false,
            matched_rule: None,
            justification: None,
            resolved_target: None,
            preview: None,
            safety_notes: Vec::new(),
        };
        assert_eq!(
            runtime.subagent_approval_decision(&shell_request, SubAgentRole::Implementer),
            ApprovalDecision::Denied
        );
        // A root grant is auto-denied for every role — widening the boundary
        // is a parent-loop conversation with the human, and a child's
        // approvals never reach one.
        let grant_request = ApprovalRequest {
            network: false,
            call_id: "call_3".to_string(),
            tool_name: "request_write_root".to_string(),
            description: "widen".to_string(),
            arguments: json!({"path": "/tmp/x", "justification": "y"}),
            risk_level: crate::execution_policy::RiskLevel::High,
            requires_sandbox: false,
            read_only: false,
            matched_rule: None,
            justification: Some("y".to_string()),
            resolved_target: Some("/tmp/x".to_string()),
            preview: None,
            safety_notes: Vec::new(),
        };
        assert_eq!(
            runtime.subagent_approval_decision(&grant_request, SubAgentRole::Implementer),
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
