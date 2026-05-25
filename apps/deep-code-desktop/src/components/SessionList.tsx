import type { SessionSummary } from "../api/types";

interface SessionListProps {
  sessions: SessionSummary[];
  selectedId: string | null;
  onSelect: (id: string) => void;
  onRefresh: () => void;
  onNewChat: () => void;
  onDelete: (id: string) => void;
}

function formatTime(ms: number): string {
  return new Date(ms).toLocaleString();
}

export function SessionList({
  sessions,
  selectedId,
  onSelect,
  onRefresh,
  onNewChat,
  onDelete,
}: SessionListProps) {
  return (
    <aside className="sidebar">
      <div className="sidebar-header">
        <h2>Sessions</h2>
        <div className="sidebar-actions">
          <button type="button" onClick={onNewChat} title="New chat">
            +
          </button>
          <button type="button" onClick={onRefresh} title="Refresh">
            ↻
          </button>
        </div>
      </div>
      <ul className="session-list">
        {sessions.length === 0 ? (
          <li className="session-empty">No saved sessions yet</li>
        ) : (
          sessions.map((session) => (
            <li key={session.id} className="session-row">
              <button
                type="button"
                className={
                  selectedId === session.id
                    ? "session-item active"
                    : "session-item"
                }
                onClick={() => onSelect(session.id)}
              >
                <span className="session-preview">
                  {session.preview || "Untitled session"}
                </span>
                <span className="session-meta">
                  {session.message_count} msgs ·{" "}
                  {formatTime(session.updated_at_ms)}
                </span>
              </button>
              <button
                type="button"
                className="session-delete"
                title="Delete session"
                onClick={() => onDelete(session.id)}
              >
                ×
              </button>
            </li>
          ))
        )}
      </ul>
    </aside>
  );
}
