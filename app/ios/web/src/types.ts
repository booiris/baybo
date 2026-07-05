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
/// they sit next to `kind: "message"`) and a `Frame::HistoryPage` entry (a bare
/// `wire::Message`, no `kind`). Same shape either way.
export type WireMessage = {
  content: string;
  role?: "user" | "assistant";
  platform_msg_id?: string;
  // The row's persisted ordinal — the catch-up cursor. Present on durable rows
  // (live final messages + replayed history).
  ordinal?: number;
  attachments?: WireAttachment[];
};

/// One in-flight work step in a `Frame::WorkSnapshot` — the wire mirror of the
/// Rust `wire::WireWorkStep` (snake_case fields). `reasoning` / `prose` carry
/// `text`; a `tool` step carries the call's id + display name/label and, once
/// the call finished within the buffered turn, `status` + `summary`.
export type WireWorkStepFrame = {
  kind: "reasoning" | "prose" | "tool";
  text?: string;
  call_id?: string;
  tool?: string;
  label?: string;
  status?: string;
  summary?: string;
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
  | { kind: "turn_state"; active: boolean }
  // `transient: true` marks mid-turn progress narration (folded into the work
  // block); absent/false is a terminal notice (its own centered row).
  | { kind: "notice"; level: string; text: string; transient?: boolean }
  // The in-flight turn's whole work block, replayed on a mid-turn (re)subscribe
  // so a client that reconnected (after backgrounding) recovers the reasoning /
  // tool steps it missed. Idempotent snapshot — REPLACES the open block.
  | { kind: "work_snapshot"; steps: WireWorkStepFrame[] }
  // A COMPLETED turn's collapsed work block, replayed on catch-up right before
  // that turn's reply — recovers the thinking for a turn that finished while we
  // were backgrounded. Rendered as a closed "思考了" block.
  | { kind: "work_replay"; steps: WireWorkStepFrame[] }
  | { kind: "reset"; reason: string }
  | {
      kind: "history_page";
      messages: WireMessage[];
      oldest_ordinal?: number | null;
      newest_ordinal?: number | null;
      has_more: boolean;
    }
  // Server-pushed standalone media a tool produced mid-turn (its own bubble).
  | { kind: "attachment"; user_id?: string; attachments?: WireAttachment[] }
  // Synthesized NATIVE-side (not a wire frame): a chat_fetch_history call
  // failed before reaching the leg, so the paging/reset guards armed for it
  // must unwind (the old Tauri invoke() rejection played this role).
  | { kind: "history_failed"; error: string }
  // Frames we don't render (reasoning, tool progress, ping/pong, …) arrive with
  // other `kind`s and fall through the switch's `default`.
  | { kind: "other" };

export type ChatMsg = {
  id: string;
  role: "user" | "assistant" | "notice";
  content: string;
  attachments?: WireAttachment[];
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
/// back on the next launch as `init.restoredState`. `lastOrdinal` is the
/// newest-edge catch-up cursor; `oldestOrdinal` the scroll-up paging cursor.
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
