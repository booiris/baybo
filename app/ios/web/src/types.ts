/** A media reference carried on a chat message — mirrors the Rust
 * `wire::WireAttachment` (snake_case fields). The bytes never ride the message;
 * only this id does — fetch them with `blobObjectUrl` in bridge.ts. */
export type WireAttachment = {
  kind: "image" | "audio" | "file";
  blob_id: string;
  mime_type: string;
  size: number;
  filename?: string;
};

/** Bucket a mime type into the wire attachment kind — same rule the web chat and
 * gateway use (the kind selects the agent-side content block). */
export function attachmentKind(mime: string): WireAttachment["kind"] {
  if (mime.startsWith("image/")) return "image";
  if (mime.startsWith("audio/")) return "audio";
  return "file";
}

/// One durable message row's fields, shared by a live `Frame::Message` (where
/// they sit next to `kind: "message"`) and the native-synthesized history API
/// page (a bare message row, no `kind`). Same shape either way.
export type WireMessage = {
  content: string;
  role?: "user" | "assistant";
  platform_msg_id?: string;
  // The row's persisted ordinal — the catch-up cursor. Present on durable rows
  // (live final messages + replayed history).
  ordinal?: number;
  attachments?: WireAttachment[];
};

/// One in-flight work step in a `Frame::SubscribeState` bundle — the wire
/// mirror of the Rust `wire::WireWorkStep` (snake_case fields). `reasoning` /
/// `prose` / `status` carry `text` (`status` is the progress observer's
/// transient narration line); a `tool` step carries the call's id + display
/// name/label and, once the call finished within the buffered turn, `status` +
/// `summary`.
export type WireWorkStepFrame = {
  kind: "reasoning" | "prose" | "tool" | "status";
  text?: string;
  call_id?: string;
  tool?: string;
  label?: string;
  status?: string;
  summary?: string;
};

/// One reconstructed step inside a REST `work` transcript row — the gateway's
/// `ChatWorkStep` DTO. Note the REST field names (`tool_label` / `tool_status`
/// / `tool_summary`), unlike the wire `WireWorkStepFrame` above. `status` is
/// the durable shadow of a progress-narration line (a persisted `progress`
/// control event folded back into the block).
export type RestWorkStep = {
  kind: "reasoning" | "prose" | "tool" | "status";
  text?: string;
  tool?: string;
  tool_label?: string;
  tool_status?: string;
  tool_summary?: string;
};

/// One full-fidelity transcript row — the gateway's `ChatTranscriptItem` DTO,
/// carried verbatim (unfiltered) by the native-synthesized `sync_page` and
/// `history_page` frames. `id` is the stable render + redelivery-dedup key
/// (`m<ordinal>` / `w<ordinal>` / `n<seq>`); `ordinal` is absent for notice /
/// control rows (they are not ordinal-addressed).
export type TranscriptRowItem = {
  id: string;
  ordinal?: number | null;
  kind: "message" | "work" | "notice";
  role?: string;
  text?: string;
  platform_msg_id?: string;
  attachments?: WireAttachment[];
  has_attachments?: boolean;
  created_at?: string;
  steps?: RestWorkStep[];
  work_started_at?: string;
  work_ended_at?: string;
  cancelled?: boolean;
  notice_level?: string;
};

/// A decrypted wire `Frame`, arriving as JSON text via `window.baybo.pushFrame`.
/// MessagePack field names round-trip as snake_case JSON; we only model the few
/// variants the transcript renders and tolerate the rest.
export type WireFrame =
  | ({ kind: "message" } & WireMessage)
  | { kind: "answer_delta"; text: string }
  // The model's thinking trace, streamed like answer_delta but folded into the
  // turn's work block instead of the reply body.
  | { kind: "reasoning"; text: string }
  | { kind: "tool_started"; call_id: string; tool: string; label?: string | null }
  | { kind: "tool_completed"; call_id: string; status: string; summary: string }
  // `started_at` is present iff `active` — it is the turn's identity for the
  // SubscribeState staleness test and for re-opening a restored work block.
  | { kind: "turn_state"; active: boolean; started_at?: string }
  // `transient: true` marks mid-turn progress narration (folded into the work
  // block); absent/false is a terminal notice (its own centered row).
  | { kind: "notice"; level: string; text: string; transient?: boolean }
  // The one atomic state-plane bundle, sent right after every Subscribe: turn
  // activity + the in-flight work block streamed so far (plus approvals/tasks,
  // which have no iOS surface — ignored). Staleness of the turn/work halves is
  // judged by turn identity (`turn.started_at`), never by cursor arithmetic.
  | {
      kind: "subscribe_state";
      session_id: string;
      as_of_ordinal?: number;
      turn: { active: boolean; started_at?: string };
      work_steps?: WireWorkStepFrame[];
      pending_approvals?: unknown[];
      tasks?: unknown[];
    }
  // The server dropped frames it owed this connection — run sync. (`session_id`
  // is absent for unattributable loss; native additionally refetches the
  // session list for that case.)
  | { kind: "gap"; session_id?: string }
  // Synthesized NATIVE-side (not a wire frame): one backward history page,
  // rows verbatim `ChatTranscriptItem`s (unfiltered, keyed by row id).
  | {
      kind: "history_page";
      rows: TranscriptRowItem[];
      oldest_ordinal?: number | null;
      newest_ordinal?: number | null;
      has_more: boolean;
    }
  // Server-pushed standalone media a tool produced mid-turn (its own bubble).
  | { kind: "attachment"; user_id?: string; attachments?: WireAttachment[] }
  // Synthesized NATIVE-side (not a wire frame): one sync page — the one
  // forward-recovery pull. `since_ordinal` echoes the request's cursor so a
  // baseline (`null`) REPLACE is distinguishable from a difference merge.
  | {
      kind: "sync_page";
      rows: TranscriptRowItem[];
      since_ordinal: number | null;
      next_cursor: number | null;
      rebased: boolean;
      oldest_ordinal: number | null;
      has_more_older: boolean;
    }
  // Synthesized NATIVE-side (not a wire frame): a chatFetchSync API call
  // failed, so the in-flight sync guard must unwind (the next trigger retries).
  | { kind: "sync_failed"; error: string }
  // Synthesized NATIVE-side (not a wire frame): a chatFetchHistory API call
  // failed, so the paging guards armed for it must unwind.
  | { kind: "history_failed"; error: string }
  // Frames we don't render (task_list, approvals, ping/pong, …) arrive with
  // other `kind`s and fall through the switch's `default`.
  | { kind: "other" };

export type ChatMsg = {
  id: string;
  role: "user" | "assistant" | "notice";
  content: string;
  attachments?: WireAttachment[];
  // Delivery state of an optimistic user send. "sending" (native minted the
  // bubble, awaiting the server echo) drives the spinner; "failed" (native
  // reported the send errored) drives the red retry dot. Cleared (undefined)
  // when the server echoes the message back. On restore a stale "sending" is
  // stripped (the leg is gone), but "failed" persists so the retry dot survives.
  sendState?: "sending" | "failed";
};

/// One entry in a turn's work block — the agent's process (thinking, tool
/// calls, provisional prose the agent superseded, transient progress notices),
/// mirroring the web chat's WorkStep.
export type WorkStep =
  | { kind: "reasoning"; text: string }
  // Answer text that streamed mid-turn but was followed by more work — the
  // agent "went back to thinking", so the text so far was intermediate.
  | { kind: "prose"; text: string }
  | { kind: "status"; text: string }
  | { kind: "tool"; callId: string; label: string; status: string; summary?: string };

/// A turn's collapsible work block, kept as its own transcript row (the web
/// chat's model: the final answer renders BELOW the block, never inside it).
export type WorkRow = {
  id: string;
  role: "work";
  steps: WorkStep[];
  active: boolean;
  /// Epoch ms when the block opened — drives the live elapsed counter.
  startedAt?: number;
  /// Total run, set when the block closes ("Worked Xs").
  elapsedMs?: number;
};

export type Row = ChatMsg | WorkRow;

/// The transcript state mirrored to native over `{type:"persist"}` and handed
/// back on the next launch as `init.restoredState` — a pure `{rows, cursor}`
/// cache. `lastOrdinal` is the sync cursor (`null` = no baseline yet, never a
/// sentinel number); `oldestOrdinal` the scroll-up paging cursor.
export type PersistedState = {
  messages: Row[];
  lastOrdinal: number | null;
  oldestOrdinal: number | null;
  hasMoreOlder: boolean;
};

let uidCounter = 0;

/// Local id mint for React keys. crypto.randomUUID is unavailable under file://
/// (not a secure context); keys only need in-session uniqueness. msgId minting
/// for real sends lives native-side.
export function uid(): string {
  uidCounter += 1;
  return `u${uidCounter.toString(36)}-${Math.floor(Math.random() * 0x100000000).toString(16)}`;
}
