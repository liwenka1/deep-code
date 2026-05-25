import { useCallback, useEffect, useState } from "react";
import {
  fetchDoctor,
  listCheckpoints,
  listJobs,
  listSubagents,
  type RuntimeAuth,
} from "../api/runtime";
import type {
  BackgroundJob,
  CheckpointSummary,
  DiagnosticEntry,
  DoctorReport,
  SubAgentRecord,
  TasksTab,
} from "../api/types";

export function useTasksPanel(auth: RuntimeAuth, enabled: boolean) {
  const [tab, setTab] = useState<TasksTab>("jobs");
  const [jobs, setJobs] = useState<BackgroundJob[]>([]);
  const [subagents, setSubagents] = useState<SubAgentRecord[]>([]);
  const [checkpoints, setCheckpoints] = useState<CheckpointSummary[]>([]);
  const [diagnostics, setDiagnostics] = useState<DiagnosticEntry[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    if (!enabled) {
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const [jobItems, agentItems, checkpointItems] = await Promise.all([
        listJobs(auth),
        listSubagents(auth),
        listCheckpoints(auth),
      ]);
      setJobs(jobItems);
      setSubagents(agentItems);
      setCheckpoints(checkpointItems);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setLoading(false);
    }
  }, [auth, enabled]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    if (!enabled) {
      return;
    }
    const timer = setInterval(() => {
      void refresh();
    }, 3000);
    return () => clearInterval(timer);
  }, [enabled, refresh]);

  const addDiagnostic = useCallback((entry: DiagnosticEntry) => {
    setDiagnostics((current) => [entry, ...current].slice(0, 20));
  }, []);

  return {
    tab,
    setTab,
    jobs,
    subagents,
    checkpoints,
    diagnostics,
    loading,
    error,
    refresh,
    addDiagnostic,
  };
}

export function useDoctor(auth: RuntimeAuth, open: boolean) {
  const [doctor, setDoctor] = useState<DoctorReport | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) {
      return;
    }
    setLoading(true);
    setError(null);
    void fetchDoctor(auth)
      .then(setDoctor)
      .catch((cause: unknown) => {
        setError(cause instanceof Error ? cause.message : String(cause));
      })
      .finally(() => setLoading(false));
  }, [auth, open]);

  return { doctor, loading, error };
}
