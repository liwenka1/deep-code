export interface RuntimeInfo {
  baseUrl: string;
  workspace: string;
  version: string;
  embedded: boolean;
  authRequired: boolean;
  authToken?: string;
}

export interface ActiveSessionResponse {
  session_id: string | null;
  backend_label: string;
}

export interface SessionSummary {
  id: string;
  updated_at_ms: number;
  message_count: number;
  preview: string;
}

export interface MessageRecord {
  role: "system" | "user" | "assistant" | "tool";
  content: string;
  tool_call_id?: string;
}

export interface SessionRecord {
  id: { "0": string } | string;
  workspace: string;
  messages: MessageRecord[];
  preview?: string;
}

export interface SseEnvelope {
  seq: number;
  timestamp: string;
  event: string;
  payload: Record<string, unknown>;
}

export interface ApprovalRequest {
  call_id: string;
  tool_name: string;
  description: string;
  arguments: unknown;
  risk_level?: string;
  requires_sandbox?: boolean;
  read_only?: boolean;
}

export type ChatRole = "user" | "assistant" | "system" | "tool";

export interface ChatMessage {
  id: string;
  role: ChatRole;
  content: string;
  toolName?: string;
  toolStatus?: "success" | "denied" | "error" | "pending";
  meta?: string;
}

export interface PendingApproval {
  callId: string;
  toolName: string;
  description: string;
  arguments: unknown;
  riskLevel?: string;
  requiresSandbox?: boolean;
}

export interface BackgroundJob {
  id: string;
  command: string;
  cwd: string;
  status: string;
  exit_code?: number | null;
  background: boolean;
}

export interface SubAgentRecord {
  agent_id: string;
  name: string;
  role: string;
  status: string;
  assignment: string;
  result?: string | null;
  error?: string | null;
  transcript_handle?: { "0": string } | string | null;
  started_at_ms: number;
  finished_at_ms?: number | null;
}

export interface CheckpointSummary {
  id: string;
}

export interface DiagnosticEntry {
  id: string;
  summary: string;
  rendered: string;
  timestamp: string;
}

export interface DoctorReport {
  version: string;
  config_path: string;
  config_present: boolean;
  workspace: string;
  api_key: { source: string };
  base_url: string;
  default_model: string;
  deepseek: {
    auto_model: boolean;
    reasoning_effort: string;
    cost_currency: string;
    beta_endpoint: boolean;
    models: Array<{
      id: string;
      context_window: number;
      supports_reasoning: boolean;
      supports_tools: boolean;
    }>;
    api_key_hint: string;
  };
  sandbox: {
    available: boolean;
    kind?: string | null;
    detail: string;
  };
  mcp: {
    config_path: string;
    workspace_config_path: string;
    present: boolean;
    servers: Array<{
      name: string;
      enabled: boolean;
      status: string;
      detail: string;
      tool_count: number;
    }>;
    errors: string[];
  };
  skills: {
    total_count: number;
    directories: Array<{ path: string; present: boolean; count: number }>;
    warnings: string[];
  };
  hooks: {
    config_path: string;
    present: boolean;
  };
}

export function sessionIdString(id: SessionRecord["id"]): string {
  if (typeof id === "string") return id;
  return id["0"] ?? String(id);
}

export function handleIdString(
  id: SubAgentRecord["transcript_handle"],
): string | null {
  if (!id) return null;
  if (typeof id === "string") return id;
  return id["0"] ?? null;
}

export type TasksTab = "jobs" | "subagents" | "diagnostics" | "checkpoints";
