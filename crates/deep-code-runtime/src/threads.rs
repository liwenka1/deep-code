use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use deep_code_agent::{Message, Role, RuntimeEvent, SessionId, SessionRecord, TurnId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

/// Broadcast capacity for live thread event fan-out. Slow clients should reconnect
/// with `since_seq` when they receive a `stream.lagged` SSE event.
const LIVE_EVENT_CHANNEL_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeThread {
    pub thread_id: String,
    pub session_id: Option<SessionId>,
    pub title: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub last_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeTurn {
    pub thread_id: String,
    pub turn_id: TurnId,
    pub prompt: String,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeItem {
    pub thread_id: String,
    pub turn_id: Option<TurnId>,
    pub item_id: String,
    pub seq: u64,
    pub kind: String,
    pub created_at_ms: u64,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEnvelope {
    pub thread_id: String,
    pub seq: u64,
    pub timestamp: String,
    pub item: RuntimeItem,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeThreadDetail {
    pub thread: RuntimeThread,
    pub turns: Vec<RuntimeTurn>,
    pub items: Vec<RuntimeItem>,
    /// Session currently loaded in the shared in-process agent runtime, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_runtime_session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RuntimeThreadStore {
    inner: Arc<Mutex<ThreadState>>,
    next_thread: Arc<AtomicU64>,
    live_tx: broadcast::Sender<RuntimeEnvelope>,
}

#[derive(Debug, Default)]
struct ThreadState {
    threads: Vec<RuntimeThread>,
    turns: Vec<RuntimeTurn>,
    items: Vec<RuntimeItem>,
}

impl RuntimeThreadStore {
    #[must_use]
    pub fn new() -> Self {
        let (live_tx, _) = broadcast::channel(LIVE_EVENT_CHANNEL_CAPACITY);
        Self {
            inner: Arc::new(Mutex::new(ThreadState::default())),
            next_thread: Arc::new(AtomicU64::new(0)),
            live_tx,
        }
    }

    pub async fn create_thread(&self, title: Option<String>) -> RuntimeThread {
        let seq = self.next_thread.fetch_add(1, Ordering::Relaxed);
        let now = now_ms();
        let thread = RuntimeThread {
            thread_id: format!("thread_{now}_{seq}"),
            session_id: None,
            title,
            created_at_ms: now,
            updated_at_ms: now,
            last_seq: 0,
        };
        self.inner.lock().await.threads.push(thread.clone());
        thread
    }

    pub async fn ensure_thread(
        &self,
        thread_id: impl Into<String>,
        title: Option<String>,
    ) -> RuntimeThread {
        let thread_id = thread_id.into();
        let mut state = self.inner.lock().await;
        if let Some(thread) = state
            .threads
            .iter()
            .find(|thread| thread.thread_id == thread_id)
            .cloned()
        {
            return thread;
        }
        let now = now_ms();
        let thread = RuntimeThread {
            thread_id,
            session_id: None,
            title,
            created_at_ms: now,
            updated_at_ms: now,
            last_seq: 0,
        };
        state.threads.push(thread.clone());
        thread
    }

    pub async fn ensure_thread_with_session(
        &self,
        thread_id: impl Into<String>,
        title: Option<String>,
        session_id: SessionId,
    ) -> RuntimeThread {
        let thread_id = thread_id.into();
        let mut state = self.inner.lock().await;
        if let Some(thread) = state
            .threads
            .iter()
            .find(|thread| thread.thread_id == thread_id)
            .cloned()
        {
            return thread;
        }
        let now = now_ms();
        let thread = RuntimeThread {
            thread_id,
            session_id: Some(session_id),
            title,
            created_at_ms: now,
            updated_at_ms: now,
            last_seq: 0,
        };
        state.threads.push(thread.clone());
        thread
    }

    pub async fn hydrate_sessions(&self, sessions: Vec<SessionRecord>) {
        let mut state = self.inner.lock().await;
        for session in sessions {
            let thread_id = format!("session_{}", session.id.as_str());
            if state
                .threads
                .iter()
                .any(|thread| thread.thread_id == thread_id)
            {
                continue;
            }
            let (thread, turns, items) = project_session_record(&session);
            state.threads.push(thread);
            state.turns.extend(turns);
            state.items.extend(items);
        }
    }

    pub async fn list_threads(&self) -> Vec<RuntimeThread> {
        let mut threads = self.inner.lock().await.threads.clone();
        threads.sort_by(|left, right| right.updated_at_ms.cmp(&left.updated_at_ms));
        threads
    }

    pub async fn get_thread(&self, thread_id: &str) -> Option<RuntimeThreadDetail> {
        let state = self.inner.lock().await;
        let thread = state
            .threads
            .iter()
            .find(|thread| thread.thread_id == thread_id)?
            .clone();
        let turns = state
            .turns
            .iter()
            .filter(|turn| turn.thread_id == thread_id)
            .cloned()
            .collect();
        let items = state
            .items
            .iter()
            .filter(|item| item.thread_id == thread_id)
            .cloned()
            .collect();
        Some(RuntimeThreadDetail {
            thread,
            turns,
            items,
            active_runtime_session_id: None,
        })
    }

    pub fn with_active_runtime_session(
        detail: RuntimeThreadDetail,
        active_runtime_session_id: Option<String>,
    ) -> RuntimeThreadDetail {
        RuntimeThreadDetail {
            active_runtime_session_id,
            ..detail
        }
    }

    pub async fn update_thread_title(
        &self,
        thread_id: &str,
        title: Option<String>,
    ) -> Option<RuntimeThread> {
        let now = now_ms();
        let mut state = self.inner.lock().await;
        let thread = state
            .threads
            .iter_mut()
            .find(|thread| thread.thread_id == thread_id)?;
        thread.title = title;
        thread.updated_at_ms = now;
        Some(thread.clone())
    }

    pub async fn append_event(&self, thread_id: &str, event: &RuntimeEvent) -> RuntimeEnvelope {
        let now = now_ms();
        let mut state = self.inner.lock().await;
        if !state
            .threads
            .iter()
            .any(|thread| thread.thread_id == thread_id)
        {
            state.threads.push(RuntimeThread {
                thread_id: thread_id.to_string(),
                session_id: None,
                title: None,
                created_at_ms: now,
                updated_at_ms: now,
                last_seq: 0,
            });
        }
        let seq = state
            .threads
            .iter()
            .find(|thread| thread.thread_id == thread_id)
            .map_or(0, |thread| thread.last_seq)
            + 1;
        let turn_id = event_turn_id(event);
        if let RuntimeEvent::TurnStarted { turn_id, prompt } = event
            && !state.turns.iter().any(|turn| turn.turn_id == *turn_id)
        {
            state.turns.push(RuntimeTurn {
                thread_id: thread_id.to_string(),
                turn_id: turn_id.clone(),
                prompt: prompt.clone(),
                started_at_ms: now,
                finished_at_ms: None,
            });
        }
        if let RuntimeEvent::TurnFinished { turn_id, .. } | RuntimeEvent::TurnCancelled { turn_id } =
            event
            && let Some(turn) = state.turns.iter_mut().find(|turn| turn.turn_id == *turn_id)
        {
            turn.finished_at_ms = Some(now);
        }

        let kind = runtime_event_kind(event).to_string();
        let item = RuntimeItem {
            thread_id: thread_id.to_string(),
            turn_id,
            item_id: format!("{thread_id}_item_{seq}"),
            seq,
            kind,
            created_at_ms: now,
            payload: event_payload(event),
        };
        state.items.push(item.clone());
        if let Some(thread) = state
            .threads
            .iter_mut()
            .find(|thread| thread.thread_id == thread_id)
        {
            thread.updated_at_ms = now;
            thread.last_seq = seq;
            if let RuntimeEvent::SessionUpdated { session_id, .. } = event
                && session_id.is_some()
            {
                thread.session_id = session_id.clone();
            }
        }
        drop(state);

        let envelope = RuntimeEnvelope {
            thread_id: thread_id.to_string(),
            seq,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            item,
        };
        let _ = self.live_tx.send(envelope.clone());
        envelope
    }

    pub async fn append_manual_item(
        &self,
        thread_id: &str,
        kind: impl Into<String>,
        payload: Value,
    ) -> RuntimeEnvelope {
        let now = now_ms();
        let mut state = self.inner.lock().await;
        if !state
            .threads
            .iter()
            .any(|thread| thread.thread_id == thread_id)
        {
            state.threads.push(RuntimeThread {
                thread_id: thread_id.to_string(),
                session_id: None,
                title: None,
                created_at_ms: now,
                updated_at_ms: now,
                last_seq: 0,
            });
        }
        let seq = state
            .threads
            .iter()
            .find(|thread| thread.thread_id == thread_id)
            .map_or(0, |thread| thread.last_seq)
            + 1;
        let item = RuntimeItem {
            thread_id: thread_id.to_string(),
            turn_id: None,
            item_id: format!("{thread_id}_item_{seq}"),
            seq,
            kind: kind.into(),
            created_at_ms: now,
            payload,
        };
        state.items.push(item.clone());
        if let Some(thread) = state
            .threads
            .iter_mut()
            .find(|thread| thread.thread_id == thread_id)
        {
            thread.updated_at_ms = now;
            thread.last_seq = seq;
        }
        drop(state);

        let envelope = RuntimeEnvelope {
            thread_id: thread_id.to_string(),
            seq,
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            item,
        };
        let _ = self.live_tx.send(envelope.clone());
        envelope
    }

    pub async fn replay_since(&self, thread_id: &str, since_seq: u64) -> Vec<RuntimeEnvelope> {
        let state = self.inner.lock().await;
        state
            .items
            .iter()
            .filter(|item| item.thread_id == thread_id && item.seq > since_seq)
            .map(|item| RuntimeEnvelope {
                thread_id: thread_id.to_string(),
                seq: item.seq,
                timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                item: item.clone(),
            })
            .collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEnvelope> {
        self.live_tx.subscribe()
    }
}

impl Default for RuntimeThreadStore {
    fn default() -> Self {
        Self::new()
    }
}

#[must_use]
pub fn runtime_event_kind(event: &RuntimeEvent) -> &'static str {
    match event {
        RuntimeEvent::TurnStarted { .. } => "turn.started",
        RuntimeEvent::AssistantDelta { .. } => "assistant.delta",
        RuntimeEvent::ReasoningDelta { .. } => "reasoning.delta",
        RuntimeEvent::ToolCallStarted { .. } => "tool.started",
        RuntimeEvent::ToolCallUpdated { .. } => "tool.updated",
        RuntimeEvent::Provider(_) => "provider",
        RuntimeEvent::ApprovalRequired { .. } => "approval.required",
        RuntimeEvent::ApprovalResolved { .. } => "approval.resolved",
        RuntimeEvent::ToolResult { .. } => "tool.result",
        RuntimeEvent::ToolCallFinished { .. } => "tool.finished",
        RuntimeEvent::SessionUpdated { .. } => "session.updated",
        RuntimeEvent::TurnFinished { .. } => "turn.completed",
        RuntimeEvent::TurnCancelled { .. } => "turn.cancelled",
        RuntimeEvent::CheckpointCreated { .. } => "checkpoint.created",
        RuntimeEvent::WorkspaceRestored { .. } => "workspace.restored",
        RuntimeEvent::DiagnosticsUpdated { .. } => "diagnostics.updated",
        RuntimeEvent::CompactionApplied { .. } => "compaction.applied",
        RuntimeEvent::Error { .. } => "error",
    }
}

#[must_use]
pub fn event_payload(event: &RuntimeEvent) -> Value {
    match event {
        RuntimeEvent::Provider(agent) => {
            serde_json::json!({ "category": "provider", "provider": agent })
        }
        other => serde_json::to_value(other).unwrap_or_else(|_| serde_json::json!({})),
    }
}

fn event_turn_id(event: &RuntimeEvent) -> Option<TurnId> {
    match event {
        RuntimeEvent::TurnStarted { turn_id, .. }
        | RuntimeEvent::AssistantDelta { turn_id, .. }
        | RuntimeEvent::ReasoningDelta { turn_id, .. }
        | RuntimeEvent::ToolCallStarted { turn_id, .. }
        | RuntimeEvent::ToolCallUpdated { turn_id, .. }
        | RuntimeEvent::TurnFinished { turn_id, .. }
        | RuntimeEvent::TurnCancelled { turn_id } => Some(turn_id.clone()),
        RuntimeEvent::ApprovalRequired { turn_id, .. }
        | RuntimeEvent::ApprovalResolved { turn_id, .. }
        | RuntimeEvent::ToolCallFinished { turn_id, .. }
        | RuntimeEvent::Error { turn_id, .. } => turn_id.clone(),
        RuntimeEvent::Provider(_)
        | RuntimeEvent::ToolResult { .. }
        | RuntimeEvent::SessionUpdated { .. }
        | RuntimeEvent::CompactionApplied { .. }
        | RuntimeEvent::CheckpointCreated { .. }
        | RuntimeEvent::WorkspaceRestored { .. }
        | RuntimeEvent::DiagnosticsUpdated { .. } => None,
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn project_session_record(
    session: &SessionRecord,
) -> (RuntimeThread, Vec<RuntimeTurn>, Vec<RuntimeItem>) {
    let thread_id = format!("session_{}", session.id.as_str());
    let mut turns = Vec::new();
    let turn_ids: Vec<TurnId> = session
        .turns
        .iter()
        .enumerate()
        .map(|(index, turn)| {
            let turn_id = TurnId(format!("{thread_id}_turn_{}", index + 1));
            turns.push(RuntimeTurn {
                thread_id: thread_id.clone(),
                turn_id: turn_id.clone(),
                prompt: turn.user_prompt.clone(),
                started_at_ms: turn.started_at_ms,
                finished_at_ms: turn.finished_at_ms,
            });
            turn_id
        })
        .collect();

    let mut items = Vec::new();
    let mut seq = 0u64;
    let mut turn_index = 0usize;

    let mut push_item =
        |kind: &str, turn_id: Option<TurnId>, created_at_ms: u64, payload: Value| {
            seq += 1;
            items.push(RuntimeItem {
                thread_id: thread_id.clone(),
                turn_id,
                item_id: format!("{thread_id}_item_{seq}"),
                seq,
                kind: kind.to_string(),
                created_at_ms,
                payload,
            });
        };

    for message in &session.messages {
        match message.role {
            Role::User => {
                if turn_index > 0 {
                    append_hydrated_turn_checkpoints(
                        &mut push_item,
                        session,
                        turn_index - 1,
                        &turn_ids,
                    );
                    turn_index += 1;
                }
                let turn_id = turn_ids.get(turn_index).cloned();
                let created_at_ms = session
                    .turns
                    .get(turn_index)
                    .map(|turn| turn.started_at_ms)
                    .unwrap_or(session.updated_at_ms);
                push_item(
                    "user.message",
                    turn_id,
                    created_at_ms,
                    message_payload(message),
                );
            }
            Role::System => {}
            Role::Assistant => {
                let turn_id = turn_ids.get(turn_index).cloned();
                push_assistant_message_items(
                    &mut push_item,
                    message,
                    turn_id,
                    session.updated_at_ms,
                );
            }
            Role::Tool => {
                let turn_id = turn_ids.get(turn_index).cloned();
                push_item(
                    "tool.result",
                    turn_id,
                    session.updated_at_ms,
                    message_payload(message),
                );
            }
        }
    }

    if !turn_ids.is_empty() {
        append_hydrated_turn_checkpoints(&mut push_item, session, turn_index, &turn_ids);
    }

    if let Some(summary) = &session.summary {
        push_item(
            "compaction.applied",
            None,
            session.updated_at_ms,
            json!({
                "summary": summary,
                "compaction": session.compaction,
            }),
        );
    }

    let thread = RuntimeThread {
        thread_id: thread_id.clone(),
        session_id: Some(session.id.clone()),
        title: Some(session.preview()),
        created_at_ms: session.created_at_ms,
        updated_at_ms: session.updated_at_ms,
        last_seq: seq,
    };
    (thread, turns, items)
}

fn message_payload(message: &Message) -> Value {
    serde_json::to_value(message).unwrap_or_else(|_| json!({}))
}

fn push_assistant_message_items<F>(
    push_item: &mut F,
    message: &Message,
    turn_id: Option<TurnId>,
    created_at_ms: u64,
) where
    F: FnMut(&str, Option<TurnId>, u64, Value),
{
    if let Some(reasoning) = message
        .reasoning_content
        .as_ref()
        .filter(|text| !text.is_empty())
    {
        push_item(
            "reasoning.delta",
            turn_id.clone(),
            created_at_ms,
            json!({ "text": reasoning }),
        );
    }
    if !message.content.is_empty() || message.tool_calls.is_empty() {
        push_item(
            "assistant.message",
            turn_id.clone(),
            created_at_ms,
            message_payload(message),
        );
    }
    for call in &message.tool_calls {
        push_item(
            "tool.started",
            turn_id.clone(),
            created_at_ms,
            json!({
                "tool_call_id": call.id,
                "tool_name": call.function.name,
                "arguments": call.function.arguments,
            }),
        );
    }
}

fn append_hydrated_turn_checkpoints<F>(
    push_item: &mut F,
    session: &SessionRecord,
    turn_index: usize,
    turn_ids: &[TurnId],
) where
    F: FnMut(&str, Option<TurnId>, u64, Value),
{
    let Some(turn) = session.turns.get(turn_index) else {
        return;
    };
    let turn_id = turn_ids.get(turn_index).cloned();
    let window_end = session
        .turns
        .get(turn_index + 1)
        .map_or(u64::MAX, |next| next.started_at_ms);
    for checkpoint in &session.checkpoints {
        if checkpoint.created_at_ms >= turn.started_at_ms && checkpoint.created_at_ms < window_end {
            push_item(
                "checkpoint.created",
                turn_id.clone(),
                checkpoint.created_at_ms,
                serde_json::to_value(checkpoint).unwrap_or_else(|_| json!({})),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use deep_code_agent::RuntimeEvent;

    use super::*;

    #[tokio::test]
    async fn append_event_assigns_seq_and_replays_since() {
        let store = RuntimeThreadStore::new();
        let thread = store.create_thread(Some("test".to_string())).await;
        let turn_id = TurnId("turn_1".to_string());

        let first = store
            .append_event(
                &thread.thread_id,
                &RuntimeEvent::TurnStarted {
                    turn_id: turn_id.clone(),
                    prompt: "hi".to_string(),
                },
            )
            .await;
        let second = store
            .append_event(
                &thread.thread_id,
                &RuntimeEvent::TurnFinished {
                    turn_id,
                    usage: None,
                    telemetry: None,
                },
            )
            .await;

        assert_eq!(first.seq, 1);
        assert_eq!(second.seq, 2);
        let replay = store.replay_since(&thread.thread_id, 1).await;
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].seq, 2);
        let detail = store.get_thread(&thread.thread_id).await.unwrap();
        assert_eq!(detail.turns.len(), 1);
        assert!(detail.turns[0].finished_at_ms.is_some());
    }

    #[tokio::test]
    async fn hydrate_sessions_projects_saved_transcript() {
        let config = deep_code_agent::AgentConfig::default();
        let mut session = SessionRecord::new(
            std::path::PathBuf::from("/tmp/project"),
            &config,
            "system prompt",
        );
        session
            .turns
            .push(deep_code_agent::TurnRecord::new("hello"));
        session
            .messages
            .push(deep_code_agent::Message::user("hello"));
        session
            .messages
            .push(deep_code_agent::Message::assistant("world"));

        let thread_id = format!("session_{}", session.id.as_str());
        let store = RuntimeThreadStore::new();
        store.hydrate_sessions(vec![session.clone()]).await;

        let detail = store.get_thread(&thread_id).await.unwrap();
        assert_eq!(detail.thread.session_id, Some(session.id));
        assert_eq!(detail.turns.len(), 1);
        assert_eq!(
            detail
                .items
                .iter()
                .filter(|item| item.kind == "user.message")
                .count(),
            1
        );
        assert!(
            detail
                .items
                .iter()
                .any(|item| item.kind == "assistant.message")
        );
        assert!(
            detail
                .items
                .iter()
                .find(|item| item.kind == "user.message")
                .and_then(|item| item.turn_id.as_ref())
                .is_some()
        );
    }

    #[tokio::test]
    async fn hydrate_sessions_projects_reasoning_tools_and_compaction() {
        use deep_code_agent::{
            CheckpointId, CheckpointRecord, Message, ToolCallFunctionPayload, ToolCallPayload,
        };

        let config = deep_code_agent::AgentConfig::default();
        let mut session = SessionRecord::new(
            std::path::PathBuf::from("/tmp/project"),
            &config,
            "system prompt",
        );
        let mut turn = deep_code_agent::TurnRecord::new("run tool");
        turn.started_at_ms = 10;
        turn.finished_at_ms = Some(20);
        session.turns.push(turn);
        session.messages.push(Message::user("run tool"));
        session.messages.push(Message::assistant_turn(
            "calling",
            "thinking",
            vec![ToolCallPayload {
                id: "call_1".to_string(),
                call_type: "function".to_string(),
                function: ToolCallFunctionPayload {
                    name: "mock_echo".to_string(),
                    arguments: r#"{"message":"hi"}"#.to_string(),
                },
            }],
        ));
        session
            .messages
            .push(Message::tool("call_1", "mock_echo: hi"));
        session.summary = Some("older summary".to_string());
        session.compaction = Some("archived=2".to_string());
        let mut checkpoint = CheckpointRecord::new(CheckpointId("cp_1".to_string()), "snap");
        checkpoint.created_at_ms = 15;
        session.checkpoints.push(checkpoint);

        let thread_id = format!("session_{}", session.id.as_str());
        let store = RuntimeThreadStore::new();
        store.hydrate_sessions(vec![session]).await;

        let detail = store.get_thread(&thread_id).await.unwrap();
        assert!(
            detail
                .items
                .iter()
                .any(|item| item.kind == "reasoning.delta")
        );
        assert!(detail.items.iter().any(|item| item.kind == "tool.started"));
        assert!(detail.items.iter().any(|item| item.kind == "tool.result"));
        assert!(
            detail
                .items
                .iter()
                .any(|item| item.kind == "checkpoint.created")
        );
        assert!(
            detail
                .items
                .iter()
                .any(|item| item.kind == "compaction.applied")
        );
    }
}
