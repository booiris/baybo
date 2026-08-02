/** A media reference carried on a chat message — mirrors the Rust
 * `wire::WireAttachment` (snake_case fields). The bytes never ride the message;
 * only this id does — fetch them with `blobObjectUrl` in bridge.ts. */
export type WireAttachment = {
  kind: "image" | "audio" | "file";
  blob_id: string;
  mime_type: string;
  size: number;
  filename?: string;
  /** Playback length in ms for `audio`, probed server-side at attach time —
   * what lets the card show a track's length before any byte is downloaded. */
  duration_ms?: number;
};

/** The content-addressed half of a `blob_id` (`sha256:<hex>.<read-token>`) — the
 * web mirror of the core's `blob_content_digest`. Every upload of the same bytes
 * mints a FRESH read token, so two ids can name one blob; the digest, not the
 * id, is a blob's identity. The `sha256:` prefix rides along — this is only ever
 * a local map key, never sent back over the wire. */
export function blobContentDigest(blobId: string): string {
  return blobId.split(".")[0] ?? blobId;
}

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
  /// The decision the call's approval prompt returned, once it completed within
  /// the buffered turn — the durable twin of the live `tool_completed.approval`.
  approval?: string;
  /// RFC3339 instant the channel's in-flight buffer recorded this step, so a
  /// client joining mid-turn can time the stretches between the model's
  /// remarks. Absent from a gateway that predates it.
  at?: string;
};

/// One reconstructed step inside a REST `work` transcript row — the gateway's
/// `ChatWorkStep` DTO. Note the REST field names (`tool_label` / `tool_status`
/// / `tool_summary`), unlike the wire `WireWorkStepFrame` above. `status` is
/// the durable shadow of a progress-narration line (a persisted `progress`
/// control event folded back into the block).
export type RestWorkStep = {
  kind: "reasoning" | "prose" | "tool" | "status";
  text?: string;
  /// The call's id — the same one the live `tool_started` / `tool_completed`
  /// frames carry, so a reconstructed step has a real identity (see
  /// `workStepKey`). Absent on a row persisted before the gateway sent it.
  call_id?: string;
  tool?: string;
  tool_label?: string;
  tool_status?: string;
  tool_summary?: string;
  /// `"approve"` / `"approve_always"` / `"deny"` — persisted on the tool result
  /// (`ToolResultMeta::approval`), so a reload still labels the step the user
  /// judged. Absent when the call never prompted.
  approval?: string;
  /// The source row's `created_at` (RFC3339). Times each stretch of work
  /// between the model's remarks; absent on rows a gateway predating it
  /// reconstructed.
  at?: string;
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
  /// For a `work` row: `true` when a real turn boundary (the final answer, the
  /// next user turn, a `/stop`, a compaction watermark) closed the block INSIDE
  /// this reconstruction window; `false` when the window's trailing edge cut it
  /// off and the turn continues into the adjacent (older) page. Absent on
  /// `message` / `notice` rows, which never fold.
  turn_complete?: boolean;
  notice_level?: string;
};

/// One context-compaction boundary (`ChatSessionDetail.compaction_points`,
/// carried on the `sync_page` frame): the summary-head `ordinal` and the
/// compaction time `at`. A `CompactionDivider` renders before the first
/// displayed row at/after `ordinal`.
export type CompactionPoint = {
  ordinal: number;
  at: string;
};

/// One pending approval prompt inside a `subscribe_state` bundle — the wire
/// mirror of the Rust `wire::ApprovalCard`. The transcript uses only
/// `tool_call_id` (which work step is waiting); the rest drives the native card.
export type WireApprovalCard = {
  call_id: string;
  tool_call_id?: string;
  tool: string;
  params_preview: string;
  description?: string;
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
  | {
      kind: "tool_completed";
      call_id: string;
      status: string;
      summary: string;
      // The decision the call's approval prompt returned; absent when it never
      // prompted. Persisted server-side, so a reload re-labels the same step.
      approval?: string;
    }
  // A tool call is blocked on this user's decision. `call_id` is the PROMPT's id
  // (what a resolve answers with); `tool_call_id` is the blocked TOOL call — the
  // work step to badge. The card itself is NATIVE (see ChatStore.pendingApprovals);
  // the transcript only marks the step as waiting.
  | {
      kind: "approval_requested";
      call_id: string;
      tool_call_id?: string;
      tool: string;
    }
  // Someone answered (this device, or another client). Broadcast to every
  // session's sink, so it is matched by prompt id, never by session.
  | { kind: "approval_resolved"; call_id: string; decision: string }
  // `started_at` is present iff `active` — it is the turn's identity for the
  // SubscribeState staleness test and for re-opening a restored work block.
  | { kind: "turn_state"; active: boolean; started_at?: string }
  // `transient: true` marks mid-turn progress narration (folded into the work
  // block as a status step). `mid_turn: true` marks a tool-authored aside
  // emitted while the turn keeps running (`SessionNotifier`) — the only class
  // the client may fold as a leveled step. Absent/false for both: a terminal
  // or durable notice (turn failures, `/stop` acks, `/compact` confirmations),
  // rendered as its own centered row.
  // `durable_id` is the persisted control event's `n<seq>` row id when the
  // notice was recorded durably before the emit — the live-minted row is keyed
  // by it so the sync-redelivered twin dedups instead of doubling.
  | {
      kind: "notice";
      level: string;
      text: string;
      transient?: boolean;
      mid_turn?: boolean | null;
      durable_id?: string | null;
    }
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
      // The authoritative pending set on (re)subscribe. The native card renders
      // them; the transcript reads only `tool_call_id`, to restore the awaiting
      // badge on a work step whose prompt outlived a reconnect.
      pending_approvals?: WireApprovalCard[];
      tasks?: unknown[];
    }
  // The server dropped frames it owed this connection — run sync. (`session_id`
  // is absent for unattributable loss; native additionally refetches the
  // session list for that case.)
  | { kind: "gap"; session_id?: string }
  // A turn-phase marker the server emits around work the user is waiting on.
  // Today the only phases are compaction start/end: with the compaction now
  // blocking the turn on a full-transcript summarizer call, a turn can sit
  // silent for 30s+ before its first token, and without this the transcript
  // shows nothing to explain the pause.
  | { kind: "status"; session_id?: string; phase: string }
  // Deck frames — no deck UI here; the native deck shell consumes them. They
  // fall through the router's `default` like any unrendered kind; modeled only
  // so wireSentinel.ts pins their shape against the generated contract.
  | { kind: "deck_card_data"; card_id: string; seq: number; payload: string }
  | { kind: "deck_changed" }
  // Synthesized NATIVE-side (not a wire frame): one backward history page,
  // rows verbatim `ChatTranscriptItem`s (unfiltered, keyed by row id).
  | {
      kind: "history_page";
      rows: TranscriptRowItem[];
      oldest_ordinal?: number | null;
      newest_ordinal?: number | null;
      has_more: boolean;
    }
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
      // Compaction boundaries (one `{ ordinal, at }` per compaction), carried on
      // every sync so the divider survives a warm re-entry's difference sync.
      // Absent on a never-compacted session (serde skips an empty vec).
      compaction_points?: CompactionPoint[];
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
  // The row's persisted ordinal, carried BESIDE the id rather than inside it: a
  // user row is keyed by its `platform_msg_id` (the echo-dedup key), so
  // `rowOrdinal` can never recover an ordinal from such an id. `syncSince` reads
  // it — without it the guard is blind to exactly the rows a rebase-dirty freeze
  // renders above the cursor. Absent on an optimistic send, on a notice (not
  // ordinal-addressed), and in a mirror written before this existed.
  ordinal?: number;
  // Severity of a reconstructed notice row (`"info"` / `"warn"` / `"error"`), so
  // a reloaded warning still reads as one instead of neutral grey. Absent on a
  // live-minted notice and on message rows.
  level?: string;
  attachments?: WireAttachment[];
  // When the row was said, as an ISO timestamp — the clock under the bubble.
  // Reconstructed rows carry the server's `created_at`; a live `Frame::Message`
  // has no time field on the wire, so those (and an optimistic send) are stamped
  // on arrival. Absent on a mirror written before this existed, and on notices —
  // those render as centered marks with no clock.
  createdAt?: string;
  // Delivery state of an optimistic user send. "sending" (native minted the
  // bubble, awaiting the server echo) drives the spinner; "failed" (native
  // reported the send errored) drives the red retry dot. Cleared (undefined)
  // when the server echoes the message back. On restore a stale "sending" is
  // stripped (the leg is gone), but "failed" persists so the retry dot survives.
  sendState?: "sending" | "failed";
  // A `role:"notice"` row that stands in for the gateway's `/stop`
  // acknowledgement: rendered as a compact "Stopped" indicator instead of the
  // raw multi-line notice text (which reads oddly as a bubble). `content` is
  // unused for these.
  stopped?: boolean;
};

/// One entry in a turn's work block — the agent's process (thinking, tool
/// calls, the words it said along the way, transient progress notices),
/// mirroring the web chat's WorkStep.
/// Epoch ms for when a step happened — the source row's `created_at`, the
/// instant the live buffer recorded it, or a local stamp when a live frame
/// minted the step (the frames carry no time of their own). Drives each work
/// run's own "处理了 Xs" label; absent on rows a gateway predating
/// `ChatWorkStep.at` reconstructed, which degrades to a step count.
type Timed = { at?: number };

export type WorkStep =
  | ({ kind: "reasoning"; text: string } & Timed)
  // Answer text that streamed mid-turn but was followed by more work — the
  // agent "went back to thinking", so the text so far was intermediate. Held
  // as a step to keep the turn's order, but never hidden by the "Worked Xs"
  // collapse: `segmentWorkSteps` lifts it out at answer typography.
  | ({ kind: "prose"; text: string } & Timed)
  | ({ kind: "status"; text: string } & Timed)
  // An out-of-band agent notice (skill warning, degraded-mode banner) that
  // landed WHILE the turn's work block was open — folded in as a step (styled by
  // `level`) instead of severing the block into two cards. A notice with no open
  // block still renders as its own centered `role:"notice"` row.
  | ({ kind: "notice"; level: string; text: string } & Timed)
  | {
      kind: "tool";
      callId: string;
      label: string;
      status: string;
      summary?: string;
      /// Prompt id, set while this call is blocked on the user's decision — the
      /// step renders "waiting for approval" and clears when the matching
      /// `approval_resolved` (or the call's completion) lands.
      awaitingApproval?: string;
      /// `"approve"` / `"approve_always"` / `"deny"` once the user judged it.
      /// Survives reload (persisted on the tool result).
      approval?: string;
    } & Timed;

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
  /// The turn this block belongs to ended in a `/stop` — the closed card reads
  /// "Cancelled" instead of a plain completion. Server-declared; a live block
  /// learns it when its reconstruction reconciles in.
  cancelled?: boolean;
  /// The server's `turn_complete` for a reconstructed block. `false` marks a
  /// head the page window's edge cut off — the ONLY thing another block may be
  /// JOINED onto (`sameContinuingTurn`). Absent on a live block (it has no
  /// server row yet) and on a mirror written before this existed.
  turnComplete?: boolean;
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
  /// Natural `[width, height]` of every image this thread has decoded, keyed by
  /// blob DIGEST (never the id — its read token rotates). Lets a re-open reserve
  /// each image's exact box before its bytes arrive, so the transcript doesn't
  /// resize under the reader. Absent on a mirror written before this existed.
  imageDims?: Record<string, [number, number]>;
  /// Compaction boundaries last seen for this session, so the pre-compaction
  /// divider paints on the mirror's cold open without waiting for the mount
  /// sync (which then refreshes it). Absent on a mirror written before this
  /// existed, or on a never-compacted session.
  compactionPoints?: CompactionPoint[];
};

/// One row of the message index — the header sheet listing the user's own sends
/// so a long thread can be navigated without scrolling. Derived from the SAME
/// `Row[]` the transcript renders, which is the whole point: every entry has a
/// DOM anchor by construction, so the sheet can never offer a row the jump
/// cannot reach.
export type OutlineEntry = {
  /// `Row.id` — the jump handle, matched against `data-row-id`. A
  /// `platform_msg_id` for anything this device sent, else the server row id.
  /// Never an ordinal: `ordinalFromMessageId` returns null for exactly the rows
  /// this phone sent.
  id: string;
  /// The prompt, whitespace-collapsed and capped at `OUTLINE_TEXT_CAP`. Empty
  /// for an attachment-only send.
  text: string;
  /// The agent's answer to this prompt, markdown flattened to one line and
  /// capped. Empty when the turn is still running, was stopped, or produced no
  /// prose. For an attachment-only send this is the only identifying content.
  gloss: string;
  /// Pre-formatted by `formatTimestampShort` — the same call that stamps the
  /// bubble's own clock, so the list and the transcript can never disagree and
  /// native writes no date parsing. Empty when the row carries no `createdAt`.
  at: string;
  /// `YYYY-MM-DD` in device-local time. The one field native formats (into the
  /// day-header label). Empty when the row carries no `createdAt`.
  dayKey: string;
  /// How many attachments rode with the send — an attachment-only row is
  /// labelled by this count.
  attachments: number;
  /// Only set while the send is unconfirmed.
  state?: "sending" | "failed";
};

let uidCounter = 0;

/// Local id mint for React keys. crypto.randomUUID is unavailable under file://
/// (not a secure context); keys only need in-session uniqueness. msgId minting
/// for real sends lives native-side.
export function uid(): string {
  uidCounter += 1;
  return `u${uidCounter.toString(36)}-${Math.floor(Math.random() * 0x100000000).toString(16)}`;
}
