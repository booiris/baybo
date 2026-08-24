import type { WireAttachment } from "../types";

/// Hand-written mirrors of the gateway's issue DTOs.
///
/// Hand-written for the same reason `types.ts` is: these arrive as raw JSON
/// the ffi passes through untouched, so no generated binding sits on this path.
/// `issueSentinel.ts` pins them to the utoipa schema at compile time — that
/// file, not this one, is what fails when the gateway moves.

export type AgentRef = { id: string; handle: string };

/// Who did the thing an entry records.
///
/// **Internally** tagged by `kind` — an agent arrives as
/// `{ kind: "agent", id, handle }`, flat, not nested under an `agent` key.
/// Getting this wrong costs every `@handle` on the page and nothing else, so
/// it fails silently; the sentinel is what catches it.
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

/// How many of a card's children are done. The card DTO carries the COUNT, not
/// the children — listing them needs the board, which is why the native side
/// puts them on the payload instead (`IssuePayload.children`).
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

/// One timeline entry. `body` is internally tagged by `kind`, and this mirror
/// keeps it OPEN — a kind this build has never heard of must render as a
/// generic line rather than take the card's whole Activity down with it. The
/// gateway adds kinds on its own schedule, and the phone is not the thing that
/// should gate that.
export type IssueEventBody = { kind: string } & Record<string, unknown>;

export type IssueEvent = {
  id: string;
  number: number;
  actor: Actor;
  body: IssueEventBody;
  created_at_ms: number;
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

/// An issue attachment as the attachment cards want it.
///
/// The cards dispatch on `kind`, which the issue DTO does not carry — the
/// transcript's wire frames classify server-side and the issue rows do not.
/// Derived here rather than in a component so there is ONE answer to "is this
/// an image": a second `startsWith("image/")` inside a view is how the two
/// surfaces end up disagreeing about an `image/svg+xml`.
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

/// Whether a run still holds the card's slot. `settled_at_ms` is the question,
/// never a status match — the server picks the row the same way, and a
/// `running` row carrying a settle stamp is a finished run, not a live one.
export function isLiveRun(run: IssueRun): boolean {
  // Absent, never `null`: every optional field here carries
  // `skip_serializing_if = "Option::is_none"`, so `None` is omitted from the
  // JSON entirely — and `issueSentinel.ts` is what holds that true.
  return run.settled_at_ms === undefined;
}
