import type { WireAttachment } from "../types";

// Hand-written raw-JSON mirrors; issueSentinel.ts pins them to the generated
// OpenAPI schema because no generated binding sits on this FFI path.
export type AgentRef = { id: string; handle: string };

export type Actor =
  | { kind: "user" }
  | { kind: "system" }
  | ({ kind: "agent" } & AgentRef);

export type IssueStatus = "backlog" | "todo" | "in_progress" | "review" | "done";
export type IssuePriority = "urgent" | "high" | "medium" | "low" | "none";
export type RunStatus = "queued" | "held" | "running" | "done" | "failed" | "cancelled";

export type IssueAttachment = {
  blob_id: string;
  mime_type: string;
  size: number;
  filename?: string;
};

export type SubIssueProgress = { done: number; total: number };

export type IssueDetail = {
  number: number;
  project_id: string;
  title: string;
  description: string;
  attachments?: IssueAttachment[];
  status: IssueStatus;
  priority: IssuePriority;
  /// The agent's PROFILE ID, not a handle. Resolving it is the board's job —
  /// the payload carries a handle map for exactly this.
  assignee?: string;
  position: number;
  pinned: boolean;
  branch?: string;
  blocked_reason?: string;
  parent?: number;
  filed_from?: number;
  stage: number;
  sub_issues?: SubIssueProgress;
  unread: number;
  last_run_failed: boolean;
  approval_pending: boolean;
  opened_by_agent: boolean;
  cancelled_at_ms?: number;
  created_at_ms: number;
  updated_at_ms: number;
};

export type IssueRun = {
  number: number;
  attempt: number;
  agent_id: string;
  status: RunStatus;
  trigger: string;
  session_id?: string;
  error?: string;
  created_at_ms: number;
  started_at_ms?: number;
  settled_at_ms?: number;
  cost_micros?: number;
  input_tokens?: number;
  output_tokens?: number;
};

/// Open by design so a future event kind renders generically instead of
/// crashing the whole activity list.
export type IssueEventBody = { kind: string } & Record<string, unknown>;

export type IssueEvent = {
  id: string;
  client_msg_id?: string;
  number: number;
  actor: Actor;
  body: IssueEventBody;
  created_at_ms: number;
  send_state?: "sending" | "failed";
};

/// A child card, as the board knows it. Not from the issue DTO — see
/// `SubIssueProgress`.
export type ChildIssue = {
  number: number;
  title: string;
  status: IssueStatus;
  cancelled_at_ms?: number;
};

export function actorHandle(actor: Actor): string | null {
  return actor.kind === "agent" ? actor.handle : null;
}

export function isAgent(actor: Actor): boolean {
  return actor.kind === "agent";
}

export function toWireAttachment(a: IssueAttachment): WireAttachment {
  const mime = a.mime_type;
  const kind: WireAttachment["kind"] = mime.startsWith("image/")
    ? "image"
    : mime.startsWith("audio/")
      ? "audio"
      : "file";
  return {
    kind,
    blob_id: a.blob_id,
    mime_type: mime,
    size: a.size,
    ...(a.filename !== undefined ? { filename: a.filename } : {}),
  };
}

export function isLiveRun(run: IssueRun): boolean {
  // The server's rule is unsettled, not a status match.
  return run.settled_at_ms === undefined;
}
