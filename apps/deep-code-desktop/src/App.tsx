import { AppShell } from "./components/AppShell";
import { useRuntime } from "./hooks/useRuntime";
import "./App.css";

function App() {
  const { runtime, sessions, loading, error, refreshSessions, reload } =
    useRuntime();

  if (loading) {
    return (
      <div className="loading-screen">
        <p>Starting deep-code runtime…</p>
      </div>
    );
  }

  if (error || !runtime) {
    return (
      <div className="loading-screen error">
        <p>Failed to connect to runtime.</p>
        <p className="error-detail">{error}</p>
        <button type="button" onClick={() => void reload()}>
          Retry
        </button>
      </div>
    );
  }

  return (
    <AppShell
      runtime={runtime}
      sessions={sessions}
      onRefreshSessions={async () => {
        await refreshSessions(runtime);
      }}
    />
  );
}

export default App;
