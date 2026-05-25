import { readSseStream } from "./sse";
import type {
  ActiveSessionResponse,
  ApprovalRequest,
  BackgroundJob,
  CheckpointSummary,
  DoctorReport,
  RuntimeInfo,
  SessionRecord,
  SessionSummary,
  SseEnvelope,
  SubAgentRecord,
} from "./types";

export type RuntimeAuth = {
  baseUrl: string;
  token?: string;
};

function authHeaders(
  token?: string,
  extra?: Record<string, string>,
): HeadersInit {
  const headers: Record<string, string> = {
    Accept: "application/json",
    ...extra,
  };
  if (token) {
    headers.Authorization = `Bearer ${token}`;
  }
  return headers;
}

async function parseJson<T>(response: Response): Promise<T> {
  if (!response.ok) {
    const text = await response.text();
    throw new Error(text || `HTTP ${response.status}`);
  }
  return response.json() as Promise<T>;
}

export async function fetchDoctor(auth: RuntimeAuth): Promise<DoctorReport> {
  const response = await fetch(`${auth.baseUrl}/v1/doctor`, {
    headers: authHeaders(auth.token),
  });
  return parseJson<DoctorReport>(response);
}

export async function listSessions(
  auth: RuntimeAuth,
): Promise<SessionSummary[]> {
  const response = await fetch(`${auth.baseUrl}/v1/sessions?limit=50`, {
    headers: authHeaders(auth.token),
  });
  return parseJson<SessionSummary[]>(response);
}

export async function getSession(
  auth: RuntimeAuth,
  id: string,
): Promise<SessionRecord> {
  const response = await fetch(
    `${auth.baseUrl}/v1/sessions/${encodeURIComponent(id)}`,
    { headers: authHeaders(auth.token) },
  );
  return parseJson<SessionRecord>(response);
}

export async function resumeSession(
  auth: RuntimeAuth,
  id: string,
): Promise<ActiveSessionResponse> {
  const response = await fetch(
    `${auth.baseUrl}/v1/sessions/${encodeURIComponent(id)}/resume`,
    { method: "POST", headers: authHeaders(auth.token) },
  );
  return parseJson<ActiveSessionResponse>(response);
}

export async function newSession(
  auth: RuntimeAuth,
): Promise<ActiveSessionResponse> {
  const response = await fetch(`${auth.baseUrl}/v1/sessions/new`, {
    method: "POST",
    headers: authHeaders(auth.token),
  });
  return parseJson<ActiveSessionResponse>(response);
}

export async function deleteSession(
  auth: RuntimeAuth,
  id: string,
): Promise<void> {
  const response = await fetch(
    `${auth.baseUrl}/v1/sessions/${encodeURIComponent(id)}`,
    { method: "DELETE", headers: authHeaders(auth.token) },
  );
  if (!response.ok && response.status !== 204) {
    const text = await response.text();
    throw new Error(text || `HTTP ${response.status}`);
  }
}

export async function listJobs(auth: RuntimeAuth): Promise<BackgroundJob[]> {
  const response = await fetch(`${auth.baseUrl}/v1/jobs`, {
    headers: authHeaders(auth.token),
  });
  return parseJson<BackgroundJob[]>(response);
}

export async function listSubagents(
  auth: RuntimeAuth,
): Promise<SubAgentRecord[]> {
  const response = await fetch(`${auth.baseUrl}/v1/subagents`, {
    headers: authHeaders(auth.token),
  });
  return parseJson<SubAgentRecord[]>(response);
}

export async function listCheckpoints(
  auth: RuntimeAuth,
): Promise<CheckpointSummary[]> {
  const response = await fetch(`${auth.baseUrl}/v1/checkpoints`, {
    headers: authHeaders(auth.token),
  });
  return parseJson<CheckpointSummary[]>(response);
}

export async function restoreCheckpoint(
  auth: RuntimeAuth,
  id: string,
): Promise<void> {
  const response = await fetch(
    `${auth.baseUrl}/v1/checkpoints/${encodeURIComponent(id)}/restore`,
    { method: "POST", headers: authHeaders(auth.token) },
  );
  await parseJson(response);
}

export async function submitApproval(
  auth: RuntimeAuth,
  callId: string,
  decision: "approved" | "denied",
): Promise<void> {
  const response = await fetch(`${auth.baseUrl}/v1/approvals`, {
    method: "POST",
    headers: authHeaders(auth.token, {
      "Content-Type": "application/json",
    }),
    body: JSON.stringify({ call_id: callId, decision }),
  });
  await parseJson(response);
}

export async function* streamPrompt(
  auth: RuntimeAuth,
  prompt: string,
): AsyncGenerator<SseEnvelope> {
  const response = await fetch(`${auth.baseUrl}/v1/prompt`, {
    method: "POST",
    headers: authHeaders(auth.token, {
      "Content-Type": "application/json",
      Accept: "text/event-stream",
    }),
    body: JSON.stringify({ prompt }),
  });

  if (!response.ok) {
    const text = await response.text();
    throw new Error(text || `HTTP ${response.status}`);
  }

  yield* readSseStream(response);
}

export function toRuntimeAuth(runtime: RuntimeInfo): RuntimeAuth {
  return {
    baseUrl: runtime.baseUrl,
    token: runtime.authToken,
  };
}

export function extractApprovalRequest(
  envelope: SseEnvelope,
): ApprovalRequest | null {
  if (envelope.event !== "approval.required") {
    return null;
  }
  const request = envelope.payload.request as ApprovalRequest | undefined;
  return request ?? null;
}

export function extractProviderText(envelope: SseEnvelope): string | null {
  if (envelope.event !== "provider") {
    return null;
  }
  const provider = envelope.payload.provider as
    | { type?: string; text?: string }
    | undefined;
  if (!provider || provider.type !== "text_delta" || !provider.text) {
    return null;
  }
  return provider.text;
}

export function extractErrorMessage(envelope: SseEnvelope): string | null {
  if (envelope.event !== "error") {
    return null;
  }
  const message = envelope.payload.message;
  return typeof message === "string" ? message : "Unknown runtime error";
}

export function extractToolResult(envelope: SseEnvelope): {
  callId: string;
  toolName: string;
  status: "success" | "denied" | "error";
  content: string;
} | null {
  if (envelope.event !== "tool.result") {
    return null;
  }
  const result = envelope.payload.result as
    | {
        call_id?: string;
        tool_name?: string;
        status?: string;
        content?: string;
      }
    | undefined;
  if (!result) {
    return null;
  }
  const statusRaw = result.status ?? "success";
  const status =
    statusRaw === "denied" || statusRaw === "error" ? statusRaw : "success";
  return {
    callId: result.call_id ?? `tool-${envelope.seq}`,
    toolName: result.tool_name ?? "tool",
    status,
    content: result.content ?? "",
  };
}

export function extractDiagnostics(envelope: SseEnvelope): {
  summary: string;
  rendered: string;
} | null {
  if (envelope.event !== "diagnostics.updated") {
    return null;
  }
  const summary = envelope.payload.summary;
  const rendered = envelope.payload.rendered;
  if (typeof summary !== "string" || typeof rendered !== "string") {
    return null;
  }
  return { summary, rendered };
}

export function extractCheckpointCreated(envelope: SseEnvelope): string | null {
  if (envelope.event !== "checkpoint.created") {
    return null;
  }
  const id = envelope.payload.id;
  if (typeof id === "string") {
    return id;
  }
  if (id && typeof id === "object" && "0" in id) {
    return String((id as { "0": string })["0"]);
  }
  return null;
}

export type { RuntimeInfo };
