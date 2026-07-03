import { useCallback, useMemo, useState } from "react";
import {
  deleteSession,
  extractApprovalRequest,
  extractCheckpointCreated,
  extractDiagnostics,
  extractErrorMessage,
  extractProviderText,
  extractToolResult,
  getSession,
  newSession,
  resumeSession,
  streamPrompt,
  submitApproval,
  toRuntimeAuth,
} from "../api/runtime";
import type {
  ChatMessage,
  PendingApproval,
  RuntimeInfo,
  SessionEntry,
  SessionSummary,
} from "../api/types";
import { sessionIdString } from "../api/types";
import { useDoctor, useTasksPanel } from "../hooks/useTasksPanel";
import { ApprovalDialog } from "./ApprovalDialog";
import { ChatPanel } from "./ChatPanel";
import { SessionList } from "./SessionList";
import { SettingsPage } from "./SettingsPage";
import { TasksPanel } from "./TasksPanel";

interface AppShellProps {
  runtime: RuntimeInfo;
  sessions: SessionSummary[];
  onRefreshSessions: () => Promise<void>;
}

function nextId(prefix: string): string {
  return `${prefix}-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
}

function recordToMessages(entries: SessionEntry[]): ChatMessage[] {
  const messages: ChatMessage[] = [];
  for (const entry of entries) {
    switch (entry.type) {
      case "user":
        messages.push({ id: entry.id, role: "user", content: entry.content });
        break;
      case "assistant":
        for (const exchange of entry.exchanges ?? []) {
          messages.push({
            id: exchange.call.id,
            role: "tool",
            content: exchange.result?.content ?? "",
            toolName: exchange.call.function.name,
            toolStatus: exchange.result?.status,
          });
        }
        if (entry.content) {
          messages.push({
            id: entry.id,
            role: "assistant",
            content: entry.content,
          });
        }
        break;
      default:
        // System and compaction entries are not rendered in the chat log.
        break;
    }
  }
  return messages;
}

function upsertToolMessage(
  messages: ChatMessage[],
  callId: string,
  patch: Omit<ChatMessage, "id">,
): ChatMessage[] {
  const index = messages.findIndex((message) => message.id === callId);
  if (index >= 0) {
    const updated = [...messages];
    updated[index] = { ...updated[index], ...patch, id: callId };
    return updated;
  }
  return [...messages, { id: callId, ...patch }];
}

export function AppShell({
  runtime,
  sessions,
  onRefreshSessions,
}: AppShellProps) {
  const auth = useMemo(() => toRuntimeAuth(runtime), [runtime]);
  const [activeSessionId, setActiveSessionId] = useState<string | null>(null);
  const [backendLabel, setBackendLabel] = useState<string | null>(null);
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [streamingText, setStreamingText] = useState("");
  const [prompt, setPrompt] = useState("");
  const [isStreaming, setIsStreaming] = useState(false);
  const [status, setStatus] = useState("Ready");
  const [error, setError] = useState<string | null>(null);
  const [pendingApproval, setPendingApproval] =
    useState<PendingApproval | null>(null);
  const [approvalBusy, setApprovalBusy] = useState(false);
  const [showSettings, setShowSettings] = useState(false);
  const [tasksVisible, setTasksVisible] = useState(true);

  const tasks = useTasksPanel(auth, tasksVisible);
  const doctor = useDoctor(auth, showSettings);

  const loadSession = useCallback(
    async (sessionId: string) => {
      setError(null);
      setIsStreaming(false);
      setPendingApproval(null);
      try {
        const active = await resumeSession(auth, sessionId);
        const record = await getSession(auth, sessionId);
        setActiveSessionId(active.session_id ?? sessionId);
        setBackendLabel(active.backend_label);
        setMessages(recordToMessages(record.entries));
        setStatus(`Resumed session ${sessionIdString(record.id)}`);
        void tasks.refresh();
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : String(cause));
      }
    },
    [auth, tasks],
  );

  const handleNewChat = async () => {
    setError(null);
    setIsStreaming(false);
    setPendingApproval(null);
    try {
      const active = await newSession(auth);
      setActiveSessionId(active.session_id);
      setBackendLabel(active.backend_label);
      setMessages([]);
      setStreamingText("");
      setStatus(
        active.session_id
          ? `New session ${active.session_id}`
          : "New session started",
      );
      await onRefreshSessions();
      void tasks.refresh();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  };

  const handleDeleteSession = async (sessionId: string) => {
    if (!window.confirm(`Delete session "${sessionId}"?`)) {
      return;
    }
    try {
      await deleteSession(auth, sessionId);
      if (activeSessionId === sessionId) {
        setActiveSessionId(null);
        setMessages([]);
        setBackendLabel(null);
        setStatus("Session deleted");
      }
      await onRefreshSessions();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  };

  const handleApprovalDecision = async (decision: "approved" | "denied") => {
    if (!pendingApproval) {
      return;
    }
    setApprovalBusy(true);
    try {
      await submitApproval(auth, pendingApproval.callId, decision);
      setPendingApproval(null);
      setStatus(decision === "approved" ? "Tool approved" : "Tool denied");
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setApprovalBusy(false);
    }
  };

  const handleSubmit = async () => {
    const text = prompt.trim();
    if (!text || isStreaming) {
      return;
    }

    setPrompt("");
    setError(null);
    setIsStreaming(true);
    setStreamingText("");
    setStatus("Streaming…");
    setMessages((current) => [
      ...current,
      { id: nextId("user"), role: "user", content: text },
    ]);

    let assistantText = "";

    try {
      for await (const envelope of streamPrompt(auth, text)) {
        const delta = extractProviderText(envelope);
        if (delta) {
          assistantText += delta;
          setStreamingText(assistantText);
        }

        const approval = extractApprovalRequest(envelope);
        if (approval) {
          setPendingApproval({
            callId: approval.call_id,
            toolName: approval.tool_name,
            description: approval.description,
            arguments: approval.arguments,
            riskLevel: approval.risk_level,
            requiresSandbox: approval.requires_sandbox,
          });
          setMessages((current) =>
            upsertToolMessage(current, approval.call_id, {
              role: "tool",
              toolName: approval.tool_name,
              toolStatus: "pending",
              content: approval.description,
              meta: JSON.stringify(approval.arguments, null, 2),
            }),
          );
          setStatus("Waiting for tool approval…");
        }

        const toolResult = extractToolResult(envelope);
        if (toolResult) {
          setMessages((current) =>
            upsertToolMessage(current, toolResult.callId, {
              role: "tool",
              toolName: toolResult.toolName,
              toolStatus: toolResult.status,
              content: toolResult.content,
            }),
          );
        }

        const diagnostics = extractDiagnostics(envelope);
        if (diagnostics) {
          tasks.addDiagnostic({
            id: nextId("diag"),
            summary: diagnostics.summary,
            rendered: diagnostics.rendered,
            timestamp: envelope.timestamp,
          });
          setStatus(`Diagnostics: ${diagnostics.summary}`);
        }

        const checkpointId = extractCheckpointCreated(envelope);
        if (checkpointId) {
          setStatus(`Checkpoint created: ${checkpointId}`);
          void tasks.refresh();
        }

        const errMsg = extractErrorMessage(envelope);
        if (errMsg) {
          setError(errMsg);
        }

        if (envelope.event === "turn.completed") {
          setPendingApproval(null);
          break;
        }
      }

      if (assistantText) {
        setMessages((current) => [
          ...current,
          {
            id: nextId("assistant"),
            role: "assistant",
            content: assistantText,
          },
        ]);
      }
      setStreamingText("");
      setStatus("Turn completed");
      void tasks.refresh();
      await onRefreshSessions();
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
      setStatus("Error");
    } finally {
      setIsStreaming(false);
    }
  };

  return (
    <div className="app-shell">
      <header className="top-bar">
        <div className="brand">
          <strong>deep-code</strong>
          <span className="version">v{runtime.version}</span>
        </div>
        <div className="top-meta">
          <span className="workspace" title={runtime.workspace}>
            {runtime.workspace}
          </span>
          {activeSessionId ? (
            <span className="session-badge" title="Active runtime session">
              session: {activeSessionId}
            </span>
          ) : null}
          {backendLabel ? (
            <span className="backend" title={backendLabel}>
              {backendLabel}
            </span>
          ) : (
            <span className="backend">
              {runtime.embedded ? "embedded runtime" : "external runtime"}
            </span>
          )}
          <button type="button" onClick={() => setTasksVisible((v) => !v)}>
            {tasksVisible ? "Hide tasks" : "Show tasks"}
          </button>
          <button type="button" onClick={() => setShowSettings(true)}>
            Settings
          </button>
        </div>
      </header>

      <div className={`main-layout${tasksVisible ? "" : " no-tasks"}`}>
        <SessionList
          sessions={sessions}
          selectedId={activeSessionId}
          onSelect={(id) => void loadSession(id)}
          onRefresh={() => void onRefreshSessions()}
          onNewChat={() => void handleNewChat()}
          onDelete={(id) => void handleDeleteSession(id)}
        />
        <ChatPanel
          messages={messages}
          streamingText={streamingText}
          isStreaming={isStreaming}
          status={status}
          error={error}
          prompt={prompt}
          onPromptChange={setPrompt}
          onSubmit={() => void handleSubmit()}
        />
        {tasksVisible ? (
          <TasksPanel
            auth={auth}
            tab={tasks.tab}
            onTabChange={tasks.setTab}
            jobs={tasks.jobs}
            subagents={tasks.subagents}
            checkpoints={tasks.checkpoints}
            diagnostics={tasks.diagnostics}
            loading={tasks.loading}
            error={tasks.error}
            onRefresh={() => void tasks.refresh()}
            onRestored={(id) => {
              setStatus(`Restored checkpoint ${id}`);
              void tasks.refresh();
            }}
          />
        ) : null}
      </div>

      {pendingApproval ? (
        <ApprovalDialog
          approval={pendingApproval}
          busy={approvalBusy}
          onDecision={(decision) => void handleApprovalDecision(decision)}
        />
      ) : null}

      {showSettings ? (
        <SettingsPage
          doctor={doctor.doctor}
          loading={doctor.loading}
          error={doctor.error}
          onClose={() => setShowSettings(false)}
        />
      ) : null}
    </div>
  );
}
