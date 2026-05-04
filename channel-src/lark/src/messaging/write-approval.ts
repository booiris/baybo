/** In-chat approval card for destructive MCP write operations.
 *
 * The pattern mirrors `feishu_ask_user`: the MCP tool calls the
 * platform's `approveWrite` hook, the platform sends an interactive
 * card and registers a waiter, the user clicks Approve / Deny, and
 * the card-action handler resolves the waiter. The tool only
 * proceeds on `approved`.
 *
 * Why per-call (not per-session) approval: the OAuth grant gives the
 * agent the same scope as the user has — including write. Without a
 * per-call gate, a paired user has no way to refuse a specific
 * destructive change ("yes, agent, you can read my calendar — no,
 * not delete that meeting"). The card is the explicit "are you sure
 * about this specific operation" checkpoint. */

const APPROVAL_TAG = "mcp_write" as const;

export interface WriteApprovalCardOpts {
  callId: string;
  toolName: string;
  /** One-line plain-English description of what's about to happen.
   * Built by the tool — the schema-aware preview ("Delete event
   * 'Q3 review'", "Create calendar event ..."). */
  summary: string;
  /** Optional richer block for multi-line previews (e.g. listing the
   * 5 records that will be created). Rendered as a fenced code block
   * below the summary; truncate at the call site if long. */
  detail?: string;
}

export function buildWriteApprovalCard(opts: WriteApprovalCardOpts): object {
  const elements: object[] = [
    {
      tag: "div",
      text: {
        tag: "lark_md",
        content: `**\`${opts.toolName}\`** wants to make a change.\n\n${opts.summary}`,
      },
    },
  ];
  if (opts.detail) {
    elements.push({
      tag: "div",
      text: {
        tag: "lark_md",
        content: "```\n" + opts.detail.slice(0, 1500) + "\n```",
      },
    });
  }
  elements.push({
    tag: "action",
    actions: [
      approvalButton(opts.callId, "approve", "Approve", "primary"),
      approvalButton(opts.callId, "deny", "Deny", "danger"),
    ],
  });
  return {
    config: { wide_screen_mode: true },
    header: {
      template: "orange",
      title: { tag: "plain_text", content: "⚠️ Approve write operation" },
    },
    elements,
  };
}

export function buildResolvedWriteApprovalCard(
  opts: WriteApprovalCardOpts,
  decision: "approve" | "deny",
): object {
  return {
    config: { wide_screen_mode: true },
    header: {
      template: decision === "approve" ? "green" : "red",
      title: {
        tag: "plain_text",
        content:
          decision === "approve"
            ? `✅ Approved: ${opts.toolName}`
            : `❌ Denied: ${opts.toolName}`,
      },
    },
    elements: [
      {
        tag: "div",
        text: {
          tag: "lark_md",
          content:
            decision === "approve"
              ? `${opts.summary}\n\nProceeding with the operation.`
              : `${opts.summary}\n\nThe agent will not proceed.`,
        },
      },
    ],
  };
}

export type WriteApprovalDecision = "approve" | "deny";

export interface WriteApprovalActionValue {
  aura: typeof APPROVAL_TAG;
  call_id: string;
  decision: WriteApprovalDecision;
}

function approvalButton(
  callId: string,
  decision: WriteApprovalDecision,
  label: string,
  type: "primary" | "danger",
): object {
  const value: WriteApprovalActionValue = {
    aura: APPROVAL_TAG,
    call_id: callId,
    decision,
  };
  return {
    tag: "button",
    text: { tag: "plain_text", content: label },
    type,
    value,
  };
}

/** Discriminator + parser for MCP-write card actions. Used by the
 * platform's card-action router to route only `mcp_write`-tagged
 * clicks to the write-approval waiters (not to the bash-approval
 * pipeline, which uses its own `aura: "approval"` tag). */
export function parseWriteApprovalCardValue(
  value: unknown,
): { callId: string; decision: WriteApprovalDecision } | null {
  if (typeof value !== "object" || value === null) return null;
  const v = value as Record<string, unknown>;
  if (v["aura"] !== APPROVAL_TAG) return null;
  if (typeof v["call_id"] !== "string") return null;
  const decision = v["decision"];
  if (decision !== "approve" && decision !== "deny") return null;
  return { callId: v["call_id"], decision };
}
