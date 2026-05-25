import { type RuntimeAuth, restoreCheckpoint } from "../api/runtime";
import type {
  BackgroundJob,
  CheckpointSummary,
  DiagnosticEntry,
  SubAgentRecord,
  TasksTab,
} from "../api/types";
import { handleIdString } from "../api/types";

interface TasksPanelProps {
  auth: RuntimeAuth;
  tab: TasksTab;
  onTabChange: (tab: TasksTab) => void;
  jobs: BackgroundJob[];
  subagents: SubAgentRecord[];
  checkpoints: CheckpointSummary[];
  diagnostics: DiagnosticEntry[];
  loading: boolean;
  error: string | null;
  onRefresh: () => void;
  onRestored: (id: string) => void;
}

const TABS: Array<{ id: TasksTab; label: string }> = [
  { id: "jobs", label: "Jobs" },
  { id: "subagents", label: "Sub-agents" },
  { id: "diagnostics", label: "Diagnostics" },
  { id: "checkpoints", label: "Checkpoints" },
];

export function TasksPanel({
  auth,
  tab,
  onTabChange,
  jobs,
  subagents,
  checkpoints,
  diagnostics,
  loading,
  error,
  onRefresh,
  onRestored,
}: TasksPanelProps) {
  const handleRestore = async (id: string) => {
    if (!window.confirm(`Restore workspace from checkpoint "${id}"?`)) {
      return;
    }
    await restoreCheckpoint(auth, id);
    onRestored(id);
  };

  return (
    <aside className="tasks-panel">
      <div className="tasks-header">
        <h2>Tasks</h2>
        <button type="button" onClick={onRefresh} title="Refresh">
          ↻
        </button>
      </div>

      <div className="tasks-tabs">
        {TABS.map((item) => (
          <button
            key={item.id}
            type="button"
            className={tab === item.id ? "active" : ""}
            onClick={() => onTabChange(item.id)}
          >
            {item.label}
          </button>
        ))}
      </div>

      {loading ? <p className="muted tasks-status">Refreshing…</p> : null}
      {error ? <p className="error-inline tasks-status">{error}</p> : null}

      <div className="tasks-body">
        {tab === "jobs" ? (
          jobs.length === 0 ? (
            <p className="muted">No shell jobs yet.</p>
          ) : (
            <ul className="task-list">
              {jobs.map((job) => (
                <li key={job.id} className="task-item">
                  <div className="task-title">
                    <code>{job.id}</code>
                    <span className={`status-pill ${job.status}`}>
                      {job.status}
                    </span>
                  </div>
                  <div className="task-command">{job.command}</div>
                  <div className="task-meta">
                    {job.cwd}
                    {job.background ? " · background" : " · foreground"}
                    {job.exit_code != null ? ` · exit ${job.exit_code}` : ""}
                  </div>
                </li>
              ))}
            </ul>
          )
        ) : null}

        {tab === "subagents" ? (
          subagents.length === 0 ? (
            <p className="muted">No sub-agents in this session.</p>
          ) : (
            <ul className="task-list">
              {subagents.map((agent) => (
                <li key={agent.agent_id} className="task-item">
                  <div className="task-title">
                    <strong>{agent.name}</strong>
                    <span className={`status-pill ${agent.status}`}>
                      {agent.status}
                    </span>
                  </div>
                  <div className="task-meta">
                    {agent.role} · {agent.agent_id}
                  </div>
                  <div className="task-command">{agent.assignment}</div>
                  {agent.result ? (
                    <pre className="task-preview">{agent.result}</pre>
                  ) : null}
                  {agent.error ? (
                    <p className="error-inline">{agent.error}</p>
                  ) : null}
                  {handleIdString(agent.transcript_handle) ? (
                    <div className="task-meta">
                      handle: {handleIdString(agent.transcript_handle)}
                    </div>
                  ) : null}
                </li>
              ))}
            </ul>
          )
        ) : null}

        {tab === "diagnostics" ? (
          diagnostics.length === 0 ? (
            <p className="muted">
              Diagnostics appear after file edits with LSP enabled.
            </p>
          ) : (
            <ul className="task-list">
              {diagnostics.map((entry) => (
                <li key={entry.id} className="task-item">
                  <div className="task-title">{entry.summary}</div>
                  <div className="task-meta">{entry.timestamp}</div>
                  <pre className="task-preview">{entry.rendered}</pre>
                </li>
              ))}
            </ul>
          )
        ) : null}

        {tab === "checkpoints" ? (
          checkpoints.length === 0 ? (
            <p className="muted">No checkpoints yet.</p>
          ) : (
            <ul className="task-list">
              {checkpoints.map((checkpoint) => (
                <li key={checkpoint.id} className="task-item checkpoint-item">
                  <code>{checkpoint.id}</code>
                  <button
                    type="button"
                    onClick={() => void handleRestore(checkpoint.id)}
                  >
                    Restore
                  </button>
                </li>
              ))}
            </ul>
          )
        ) : null}
      </div>
    </aside>
  );
}
