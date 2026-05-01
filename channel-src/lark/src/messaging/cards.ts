import type { ApprovalDecision, ApprovalRequest } from "@aura/channel-sdk";

export interface ApprovalCardValue {
  aura: "approval";
  call_id: string;
  decision: ApprovalDecision;
}

const APPROVAL_TAG = "approval" as const;

// CardKit v1 schema. v2 streaming cards arrive in Phase 2 — a static
// approval prompt does not benefit from streaming, so v1 is the
// natural choice and stays compatible with tenants that haven't
// enabled CardKit v2.
export function buildApprovalCard(req: ApprovalRequest): object {
  const preview = req.paramsPreview.trim();
  const description = req.description?.trim();
  const lines: string[] = [`**\`${req.tool}\`** wants to run.`];
  if (description) lines.push("", description);
  if (preview) lines.push("", "```", preview, "```");
  return {
    config: { wide_screen_mode: true },
    header: {
      template: "blue",
      title: {
        tag: "plain_text",
        content: "Aura approval requested",
      },
    },
    elements: [
      { tag: "div", text: { tag: "lark_md", content: lines.join("\n") } },
      {
        tag: "action",
        actions: [
          approvalButton(req.callId, "approve", "Approve", "primary"),
          approvalButton(
            req.callId,
            "approve_always",
            "Approve always",
            "default",
          ),
          approvalButton(req.callId, "deny", "Deny", "danger"),
        ],
      },
    ],
  };
}

export function buildResolvedApprovalCard(
  req: ApprovalRequest,
  decision: ApprovalDecision,
): object {
  return {
    config: { wide_screen_mode: true },
    header: {
      template:
        decision === "deny"
          ? "red"
          : decision === "approve_always"
            ? "green"
            : "turquoise",
      title: {
        tag: "plain_text",
        content: `${decisionIcon(decision)} ${decisionLabel(decision)}: ${req.tool}`,
      },
    },
    elements: [
      {
        tag: "div",
        text: {
          tag: "lark_md",
          content: `Approval ${decisionLabel(decision).toLowerCase()}.`,
        },
      },
    ],
  };
}

function approvalButton(
  callId: string,
  decision: ApprovalDecision,
  label: string,
  type: "primary" | "danger" | "default",
): object {
  const value: ApprovalCardValue = { aura: APPROVAL_TAG, call_id: callId, decision };
  return {
    tag: "button",
    text: { tag: "plain_text", content: label },
    type,
    value,
  };
}

export function parseApprovalCardValue(
  value: unknown,
): { callId: string; decision: ApprovalDecision } | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  if (v["aura"] !== APPROVAL_TAG) return null;
  if (typeof v["call_id"] !== "string") return null;
  const decision = v["decision"];
  if (
    decision !== "approve"
    && decision !== "approve_always"
    && decision !== "deny"
  ) {
    return null;
  }
  return { callId: v["call_id"], decision };
}

function decisionLabel(decision: ApprovalDecision): string {
  switch (decision) {
    case "approve":
      return "Approved";
    case "approve_always":
      return "Approved (always)";
    case "deny":
      return "Denied";
  }
}

function decisionIcon(decision: ApprovalDecision): string {
  return decision === "deny" ? "🚫" : "✅";
}
