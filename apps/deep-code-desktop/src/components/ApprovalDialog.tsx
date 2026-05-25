import type { PendingApproval } from "../api/types";

interface ApprovalDialogProps {
  approval: PendingApproval;
  busy: boolean;
  onDecision: (decision: "approved" | "denied") => void;
}

export function ApprovalDialog({
  approval,
  busy,
  onDecision,
}: ApprovalDialogProps) {
  const argsText =
    typeof approval.arguments === "string"
      ? approval.arguments
      : JSON.stringify(approval.arguments, null, 2);

  return (
    <div className="approval-overlay">
      <div className="approval-dialog">
        <h3>Tool approval required</h3>
        <p className="approval-tool">{approval.toolName}</p>
        <p>{approval.description}</p>
        {approval.riskLevel ? (
          <p className="task-meta">Risk: {approval.riskLevel}</p>
        ) : null}
        {approval.requiresSandbox ? (
          <p className="task-meta">Requires sandbox</p>
        ) : null}
        <pre className="approval-args">{argsText}</pre>
        <div className="approval-actions">
          <button
            type="button"
            className="deny"
            disabled={busy}
            onClick={() => onDecision("denied")}
          >
            Deny
          </button>
          <button
            type="button"
            className="approve"
            disabled={busy}
            onClick={() => onDecision("approved")}
          >
            Approve
          </button>
        </div>
      </div>
    </div>
  );
}
