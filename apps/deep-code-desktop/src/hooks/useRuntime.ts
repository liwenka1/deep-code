import { invoke } from "@tauri-apps/api/core";
import { useCallback, useEffect, useState } from "react";
import { listSessions, toRuntimeAuth } from "../api/runtime";
import type { RuntimeInfo, SessionSummary } from "../api/types";

export function useRuntime() {
  const [runtime, setRuntime] = useState<RuntimeInfo | null>(null);
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const refreshSessions = useCallback(async (info: RuntimeInfo) => {
    const items = await listSessions(toRuntimeAuth(info));
    setSessions(items);
  }, []);

  const bootstrap = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const info = await invoke<RuntimeInfo>("get_runtime_info");
      if (info.authRequired && !info.authToken) {
        throw new Error(
          "Runtime requires DEEP_CODE_RUNTIME_TOKEN but none is configured.",
        );
      }
      setRuntime(info);
      await refreshSessions(info);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setLoading(false);
    }
  }, [refreshSessions]);

  useEffect(() => {
    void bootstrap();
  }, [bootstrap]);

  return {
    runtime,
    sessions,
    loading,
    error,
    refreshSessions,
    reload: bootstrap,
  };
}
