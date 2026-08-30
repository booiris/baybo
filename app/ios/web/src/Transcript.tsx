import {
  Fragment,
  memo,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { useTranslation } from "react-i18next";
import {
  copyText,
  fetchHistory,
  log,
  persistState,
  postJumpVisible,
  postMarkRead,
  postOutline,
  postOutlineHere,
  postRunState,
  postSubagents,
  postSyncRequest,
  resendOutline,
  retrySend,
  subscribeTranscript,
  type OutlinePost,
  type UserSentPayload,
} from "./bridge";
import {
  AttachmentBubble,
  ImageDimsContext,
  restoreImageDims,
  type ImageDimsStore,
} from "./attachments";
import { useLongPress } from "./gestures";
import { MarkdownBody, StreamingMarkdownBody } from "./Markdown";
import { advanceFromLive, advanceFromSync, type CursorState } from "./transcript/cursor";
import { compactionStatusText, WorkBlockView } from "./WorkBlock";
import {
  uid,
  type ChatMsg,
  type CompactionPoint,
  type OutlineEntry,
  type PersistedState,
  type Row,
  type TranscriptRowItem,
  type WireApprovalCard,
  type WireFrame,
  type WireWorkStepFrame,
  type WorkRow,
  type WorkStep,
} from "./types";

// Map a `Frame::SubscribeState` wire work step onto the transcript's rendered
// WorkStep. A `tool` step keeps its `call_id` so a later live `ToolCompleted`
// still pairs by id; `status` defaults to "running" until the call finished
// within the buffered turn.
/// The server's RFC3339 step stamp as epoch ms. `undefined` for a gateway that
/// predates `at`, and for anything unparseable — a NaN here would poison every
/// duration derived from it.
function parseStepAt(at: string | null | undefined): number | undefined {
  if (at === null || at === undefined || at === "") return undefined;
  const ms = Date.parse(at);
  return Number.isNaN(ms) ? undefined : ms;
}

export function wireStepToWork(s: WireWorkStepFrame): WorkStep {
  if (s.kind === "tool") {
    return {
      kind: "tool",
      callId: s.call_id ?? "",
      tool: s.tool,
      label: s.label || s.tool || "",
      status: s.status ?? "running",
      summary: s.summary || undefined,
      approval: s.approval || undefined,
      at: parseStepAt(s.at),
    };
  }
  return { kind: s.kind, text: s.text ?? "", at: parseStepAt(s.at) };
}

/// Map a REST `ChatWorkStep` (the `work` transcript row's step — snake_case
/// `tool_label` / `tool_status` / `tool_summary`) onto a rendered WorkStep. A
/// step whose persisted result carried no status stays STATUSLESS ("" — neutral,
/// as app/web renders it): a call that never reported a result is not evidence
/// of success, and the old "ok" default painted every one of them green.
export function restStepToWork(s: NonNullable<TranscriptRowItem["steps"]>[number]): WorkStep {
  if (s.kind === "tool") {
    return {
      kind: "tool",
      // "" only for a row the gateway persisted before it sent `call_id`;
      // `workStepKey` falls back to content-keying for those.
      callId: s.call_id ?? "",
      tool: s.tool,
      label: s.tool_label || s.tool || "",
      status: s.tool_status ?? "",
      summary: s.tool_summary || undefined,
      approval: s.approval || undefined,
      at: parseStepAt(s.at),
    };
  }
  return { kind: s.kind, text: s.text ?? "", at: parseStepAt(s.at) };
}

/// Translate one full-fidelity transcript row (`ChatTranscriptItem`, carried
/// verbatim by the `sync_page` / `history_page` frames) into a rendered Row,
/// keyed by the server's stable `id` (`m<ordinal>` / `w<ordinal>` / `n<seq>`)
/// — the render key AND redelivery dedup key. `null` for a shape we don't
/// render (an empty/unknown row).
/// Cap on the remembered image sizes (see `ImageDimsStore`). An entry is ~60
/// bytes and a thread's images are bounded in practice — this only stops a
/// pathological session from growing the mirror without limit.
const MAX_IMAGE_DIMS = 512;

export function transcriptItemToRow(item: TranscriptRowItem): Row | null {
  if (item.kind === "work") {
    const steps = (item.steps ?? []).map(restStepToWork);
    return {
      id: item.id,
      role: "work",
      steps,
      active: false,
      cancelled: item.cancelled,
      turnComplete: item.turn_complete,
      // Server-anchored turn start — so a reopened/reconciled block's live ticker
      // is `now − true start`, not `now − localOpen` (the latter inflates across
      // app-close / re-entry into an absurd "Worked 7h").
      startedAt: item.work_started_at ? Date.parse(item.work_started_at) : undefined,
      elapsedMs:
        item.work_started_at && item.work_ended_at
          ? Math.max(0, Date.parse(item.work_ended_at) - Date.parse(item.work_started_at))
          : undefined,
    };
  }
  if (item.kind === "notice") {
    // The `/stop` acknowledgement renders as a compact "Stopped" indicator, not
    // the gateway's raw multi-line text (matches the live path).
    if (isStopAckNotice(item.text ?? "")) {
      return { id: item.id, role: "notice", content: "", stopped: true };
    }
    return { id: item.id, role: "notice", content: item.text ?? "", level: item.notice_level };
  }
  const role = item.role === "user" ? "user" : "assistant";
  // The gateway persists `/stop` as a `Command` control event, which
  // reconstructs as a user MESSAGE row (`control_event_item`). Drop it, mirroring
  // the live-echo drop — the button issues `/stop`, it is never a chat bubble.
  if (role === "user" && isStopCommand(item.text ?? "")) return null;
  // A user row keeps its send's `platform_msg_id` as the render id (the live
  // echo path's key), so an optimistic bubble reconciles by id; an assistant
  // row uses the stable `m<ordinal>` id.
  const id = role === "user" && item.platform_msg_id ? item.platform_msg_id : item.id;
  return {
    id,
    role,
    ordinal: item.ordinal ?? undefined,
    content: item.text ?? "",
    attachments: item.attachments,
    createdAt: item.created_at,
  };
}

/// Rows per backward-history (scroll-up) page. Matches the gateway's default
/// page size (server-clamped to 1..200), so one fetch loads up to 50 rows.
const HISTORY_PAGE_LIMIT = 50;

/// How many pages a search-hit jump may spend reaching its ordinal — about 600
/// DISPLAYED rows (an agentic turn persists hundreds of invisible tool rows per
/// visible one, so this reaches far deeper into a transcript than the ordinal
/// arithmetic suggests).
///
/// A cap and not a "load until found": each page is a serial round trip, on the
/// relay leg a Noise tunnel exchange, and the reader is watching the thread grow
/// under them the whole time. Past this the honest answer is to stop where we
/// got to and say so — they are already further back than they started, which is
/// worth something, whereas a minute of silent paging is not.
const JUMP_PAGE_BUDGET = 12;

/// Sync page size, elected per call site (docs/sync-protocol.md): one UI page
/// for a baseline / cold open (`since` absent — a newest-page REPLACE by
/// definition), the server hard cap when merging a difference into an
/// already-rendered thread (a rebase is a REPLACE under a reading user, so
/// incremental merge is preferred all the way to the cap).
const SYNC_BASELINE_LIMIT = 50;
const SYNC_MERGE_LIMIT = 200;

/// Safety-net pull cadence: run the sync loop for the foreground transcript
/// every 3 minutes, skipped when any frame arrived within the interval.
/// Backstops a lost `gap` nudge and suspended-app windows.
const SAFETY_TICK_MS = 180_000;

/// Hard ceiling on the optimistic post-send run-state window (`awaitingReply`).
/// A real turn clears it far sooner — via its first output or its terminal frame
/// — so this only fires when BOTH were missed (a disconnect that hid the turn's
/// output and its close), un-sticking the composer's stop button.
///
/// It is NOT above every pre-first-token latency: a compaction blocks the turn
/// on a full-transcript summarizer call that routinely outlasts this. What keeps
/// the window alive there is the `status` frame — it opens the work block, and
/// `running` reads `workLive` too. Raising the number instead would just make a
/// genuinely lost turn hold the composer hostage for twice as long.
const AWAITING_MAX_MS = 30_000;

/// How close to the top of the chat log (px) triggers a scroll-up fetch of the
/// next older page. A small band so the load fires just before the user hits the
/// very top, hiding the round-trip.
const SCROLL_TOP_THRESHOLD_PX = 64;

/// How close to the bottom of the chat log (px) still counts as "following" the
/// newest edge. Within this band incoming rows / stream deltas keep the log
/// pinned to the bottom; above it (reading history) they leave the viewport
/// alone. Roughly one short bubble, so only genuinely-at-the-edge follows.
const FOLLOW_BOTTOM_THRESHOLD_PX = 96;

/// Cap on the jump-to-latest smooth glide. Browsers finish a smooth scroll well
/// inside this, so hitting the cap means the glide was cancelled (a finger
/// planted mid-flight) — at which point the true scroll position decides the
/// follow/button state again instead of staying pinned by the in-flight flag.
const GLIDE_SETTLE_CAP_MS = 1200;


/// Cap on an outline entry's `text` / `gloss`. A TRANSPORT cap, not a display
/// one — native truncates to its own (shorter) width; this only keeps a pasted
/// wall of text out of every bridge message the sheet's list rides on.
const OUTLINE_TEXT_CAP = 160;

/// Delay before `jumpToMessage`'s one-shot correction pass. The jump drags
/// never-decoded images into the lazy band and they shove the target down as
/// their bytes land — WKWebView has no scroll anchoring to absorb it.
const JUMP_SETTLE_MS = 400;




/// The transcript scrolls the WKWebView's MAIN FRAME (the document), not an
/// inner `overflow:auto` div. A nested overflow scroller inside WKWebView owns
/// an async scroll node that stays asleep until the first touch — a cold-start
/// drag then reads as dead ("tap once to scroll") and an uncaptured drag
/// rubber-bands the whole webview instead of moving history. The main-frame
/// scroller is always live. `.chat-log` is `min-height:100dvh` with no overflow,
/// so every scroll-position op targets `document.scrollingElement`.
function scrollEl(): HTMLElement | null {
  return document.scrollingElement as HTMLElement | null;
}

/// The topmost row still on screen, and how far below the viewport's top edge it
/// sat — what a REPLACE has to put back under a reader parked in history. Every
/// row carries `data-row-id` for exactly this (and for the message index's
/// jump), and a REPLACE reuses a surviving row's key, so the handle is still
/// findable in the rebuilt thread. `null` when no row is on screen to hold onto.
function captureRowAnchor(): { rowId: string; top: number } | null {
  for (const el of document.querySelectorAll("[data-row-id]")) {
    const rect = el.getBoundingClientRect();
    if (rect.bottom <= 0) continue;
    const rowId = el.getAttribute("data-row-id");
    return rowId === null ? null : { rowId, top: rect.top };
  }
  return null;
}

/// Recognise a `/stop` the way the gateway's parser does (leading `/`, first
/// token, tolerant of a `@bot` suffix / trailing args), so the client can drop
/// the command's user echo — the native stop button issues `/stop` as an
/// ordinary send and it must never render as a message bubble. Mirrors
/// app/web's `isStopCommand`.
export function isStopCommand(text: string): boolean {
  const trimmed = text.trim();
  if (!trimmed.startsWith("/")) return false;
  const cmd = trimmed.slice(1).split(/[\s@]/, 1)[0]?.toLowerCase();
  return cmd === "stop";
}

/// A `/stop` acknowledgement notice from the gateway (`build_stop_notice`):
/// `"Stopped.\n- Cancelled the in-progress reply."`, a background-task variant,
/// or the no-op `"Nothing in progress to stop."`. These are text-channel chatter
/// that read oddly as a chat bubble (worst when a thinking-only turn is stopped
/// before any work block exists), so the transcript drops them entirely.
export function isStopAckNotice(text: string): boolean {
  const t = text.trim();
  return t.startsWith("Stopped.") || t === "Nothing in progress to stop.";
}

export function ordinalFromMessageId(id: string): number | null {
  const match = /^m(\d+)$/.exec(id);
  if (!match) return null;
  const n = Number(match[1]);
  return Number.isSafeInteger(n) ? n : null;
}

/// Durable ordinal out of a server row id — a `m<ordinal>` message OR a
/// `w<ordinal>` work block. `null` for a client-minted `uid()` (a live block)
/// or an `n<seq>` notice, neither of which carries an ordinal. Used to place a
/// re-delivered work block into its own turn during a sync-difference merge.
export function rowOrdinal(id: string): number | null {
  const match = /^[mw](\d+)$/.exec(id);
  if (!match) return null;
  const n = Number(match[1]);
  return Number.isSafeInteger(n) ? n : null;
}

/// The durable ordinal a rendered row counts as sync coverage: the one in its
/// `m`/`w` id, or — for a user row, which is keyed by its `platform_msg_id` so
/// an optimistic bubble reconciles — the server ordinal carried beside the id.
/// Without that second half EVERY user message, from this device or another,
/// reads as ordinal-less. `null` for a row that truly has none: an optimistic
/// send, a live work block, an `n<seq>` notice.
function rowCoverageOrdinal(r: Row): number | null {
  if (r.role !== "work" && r.ordinal !== undefined) return r.ordinal;
  return rowOrdinal(r.id);
}

/// Whether the thread already carries the optimistic bubble for `msgId`. Native
/// re-seeds the outbox's unconfirmed sends on EVERY mount edge — it cannot know
/// whether the tree it is seeding came up empty (a resync, a mirror older than
/// the send) or already holds them — so the replay has to be idempotent, and a
/// user row is keyed by its `platform_msg_id` precisely so this can be asked.
export function holdsUserSend(rows: Row[], msgId: string): boolean {
  return rows.some((r) => r.role === "user" && r.id === msgId);
}

/// The `since_ordinal` the next sync may present: the cursor, unless the thread
/// is not a PREFIX of it. A difference answers rows strictly `> since` and the
/// merge appends what it doesn't already hold, so a page that OVERLAPS rendered
/// rows welds them below the thread instead of at their ordinal (and folds a
/// leading work block into the tail's card). Rows do outrun the cursor — it is a
/// coverage watermark, while scroll-up paging and a rebase-dirty freeze render
/// rows that never advance it — so hand those opens the baseline REPLACE, the
/// path a fresh install proves correct. Rows carrying no durable ordinal at all
/// (an optimistic send, a live work block) are not coverage and never trip it.
///
/// This PREVENTS the scramble; it does not repair one. The welding sync itself
/// ends by advancing the cursor to `next_cursor`, and a difference is only
/// returned when its scan did not overrun — so that watermark is the session's
/// newest ordinal, above every rendered row. A mirror scarred by this is
/// therefore persisted already covering itself: the gate stays quiet, the
/// server answers an empty difference, and the order stands. The exit is the
/// per-session resync hatch.
export function syncSince(cursor: number | null, rows: Row[]): number | null {
  if (cursor === null) return null;
  for (const r of rows) {
    const ordinal = rowCoverageOrdinal(r);
    if (ordinal !== null && ordinal > cursor) return null;
  }
  return cursor;
}

/// The rows a rebased page does not reach: everything before the FIRST row whose
/// ordinal the page's window covers. `taken` drops any the rebuild already
/// carries, so an id can never render twice.
///
/// Cut by POSITION, not filtered by ordinal: a notice / `/stop` mark carries no
/// ordinal to compare, and filtering would delete every one of them out of the
/// half that survives. The cut keeps each interleaved row with the neighbours it
/// was written between.
export function rowsAboveFloor(rows: Row[], floor: number, taken: ReadonlySet<string>): Row[] {
  let cut = rows.length;
  for (let i = 0; i < rows.length; i++) {
    const ordinal = rowCoverageOrdinal(rows[i]);
    if (ordinal !== null && ordinal >= floor) {
      cut = i;
      break;
    }
  }
  return rows.slice(0, cut).filter((r) => !taken.has(r.id));
}

/// Ids of the rows that get a `CompactionDivider` rendered *before* them: the
/// first displayed row whose ordinal lands at/after a compaction boundary — the
/// seam where a compaction rewrote the LLM context. The messages above still
/// render (their pre-compaction originals); the model just no longer sees them.
/// Both sides must be loaded: a boundary above every loaded row draws nothing
/// until scroll-up pages the originals in. Notice / live rows (no `m`/`w`
/// ordinal) are skipped so an interleaved notice can't misplace the seam. Maps
/// to the newest crossed boundary's time. Mirrors app/web's `compactionDividerKeys`.
export function compactionDividerIds(
  rows: Row[],
  points: CompactionPoint[],
): Map<string, string> {
  const out = new Map<string, string>();
  if (points.length === 0) return out;
  let prevOrdinal: number | null = null;
  for (const r of rows) {
    const ordinal = rowOrdinal(r.id);
    if (ordinal === null) continue;
    if (prevOrdinal !== null) {
      const lower = prevOrdinal;
      const crossed = points.filter((p) => p.ordinal > lower && p.ordinal <= ordinal);
      if (crossed.length > 0) out.set(r.id, crossed[crossed.length - 1].at);
    }
    prevOrdinal = ordinal;
  }
  return out;
}

/// Fence opener / closer for the gloss flattener, mirroring `mathDelimiters`'s
/// code mask: any indent (a list-nested fence still counts), and a closer must
/// be alone on its line.
const GLOSS_FENCE = /^[ \t]*(`{3,}|~{3,})/;
const GLOSS_FENCE_CLOSE = /^[ \t]*(`{3,}|~{3,})[ \t]*$/;
/// Block leaders stripped off the front of a kept line — blockquote arrows, a
/// heading's hashes, a bullet or an ordered marker. Left in, a flattened reply
/// reads "- one - two". Anchored, and every alternative consumes, so there is
/// nothing for the engine to backtrack over.
const GLOSS_LINE_LEADER = /^[ \t]*(?:>[ \t]*)*(?:#{1,6}[ \t]+|[-*+][ \t]+|\d{1,9}[.)][ \t]+)?/;
const WHITESPACE_RUN = /\s+/g;

function collapseWhitespace(text: string): string {
  return text.replace(WHITESPACE_RUN, " ").trim();
}

/// Markdown flattened onto one line for an outline entry's gloss: fenced code
/// dropped whole, block leaders and emphasis/code marks stripped, a link
/// reduced to its TEXT, whitespace collapsed.
///
/// A LINEAR scanner, not a regex: this runs over whole assistant replies, and a
/// 40KB paste through a backtracking pattern would hang the transcript. Every
/// branch consumes at least one character — an unterminated `](` simply eats
/// the rest, which is the right answer for a malformed tail anyway.
///
/// `_` is deliberately left alone: it carries emphasis far less often than it
/// carries `snake_case`, and stripping it corrupts identifiers in a gloss.
export function flattenGloss(md: string): string {
  const kept: string[] = [];
  let fence: string | null = null;
  for (const line of md.split("\n")) {
    if (fence === null) {
      const open = GLOSS_FENCE.exec(line);
      if (open === null) kept.push(line.replace(GLOSS_LINE_LEADER, ""));
      else fence = open[1];
      continue;
    }
    const close = GLOSS_FENCE_CLOSE.exec(line);
    if (close !== null && close[1].charAt(0) === fence.charAt(0) && close[1].length >= fence.length) {
      fence = null;
    }
  }

  const src = kept.join(" ");
  let out = "";
  let i = 0;
  let openBrackets = 0;
  // Set to the character that ends a span being swallowed whole (a link target
  // or a reference label), so the scan never looks ahead.
  let skipUntil: string | null = null;
  while (i < src.length) {
    const c = src.charAt(i);
    i++;
    if (skipUntil !== null) {
      if (c === skipUntil) skipUntil = null;
      continue;
    }
    if (c === "*" || c === "`") continue;
    // `~` only marks up when doubled. Dropping a lone one turns `~/bin/deploy`
    // into `/bin/deploy` and `~50ms` into `50ms` — a gloss that misinforms.
    if (c === "~" && src.charAt(i) === "~") {
      i++;
      continue;
    }
    if (c === "\\" && i < src.length) {
      out += src.charAt(i);
      i++;
      continue;
    }
    if (c === "!" && src.charAt(i) === "[") continue;
    if (c === "[") {
      openBrackets++;
      continue;
    }
    if (c === "]" && openBrackets > 0) {
      openBrackets--;
      const next = src.charAt(i);
      if (next === "(") {
        skipUntil = ")";
        i++;
      } else if (next === "[") {
        skipUntil = "]";
        i++;
      }
      continue;
    }
    out += c;
  }
  return collapseWhitespace(out);
}

/// `YYYY-MM-DD` in DEVICE-LOCAL time — the one field the native sheet formats
/// (into its day header). Never `toISOString().slice(0, 10)`: that is UTC, so
/// every evening message east of Greenwich would file under tomorrow.
function localDayKey(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  const p = (n: number): string => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

/// Cut to `OUTLINE_TEXT_CAP` by CODE POINT, never by `String.slice`. A slice
/// cuts UTF-16 units, so a cut landing inside a surrogate pair (any emoji or
/// other non-BMP character straddling the cap) emits a lone surrogate — and the
/// native handler is the one bridge message that re-serializes its payload
/// (`JSONSerialization`), which THROWS on an ill-formed string. One such
/// message would blank the entire index for that conversation, on every repost,
/// until the row leaves the loaded window.
function capOutlineText(text: string): string {
  if (text.length <= OUTLINE_TEXT_CAP) return text;
  return Array.from(text).slice(0, OUTLINE_TEXT_CAP).join("");
}

/// The agent's answer to the user row at `from`, or "" when the turn produced
/// none. Work blocks and notices are scanned past — they sit between a prompt
/// and its reply — but the next USER row is the wall: a stopped or still-running
/// turn must gloss as empty rather than borrow the following turn's answer.
function glossAfter(rows: Row[], from: number): string {
  for (let i = from + 1; i < rows.length; i++) {
    const row = rows[i];
    if (row.role === "user") return "";
    if (row.role === "assistant" && row.content.trim() !== "") {
      return capOutlineText(flattenGloss(row.content));
    }
  }
  return "";
}

/// The user's own sends, in thread order, each glossed with the agent's answer
/// — the model behind the native message-index sheet. Derived from the SAME
/// rows the transcript renders, so every entry has a `data-row-id` anchor by
/// construction and the sheet can never offer a row the jump cannot reach.
export function outlineEntries(rows: Row[]): OutlineEntry[] {
  const out: OutlineEntry[] = [];
  for (let i = 0; i < rows.length; i++) {
    const row = rows[i];
    if (row.role !== "user") continue;
    out.push({
      id: row.id,
      text: capOutlineText(collapseWhitespace(row.content)),
      gloss: glossAfter(rows, i),
      // The same call the bubble's own clock uses, so the sheet and the
      // transcript can never disagree about when something was said.
      at: row.createdAt === undefined ? "" : formatTimestampShort(row.createdAt),
      dayKey: row.createdAt === undefined ? "" : localDayKey(row.createdAt),
      attachments: row.attachments?.length ?? 0,
      state: row.sendState,
    });
  }
  return out;
}

/// The tool that mints a child session — `baybo_model::SPAWN_SUBAGENT_TOOL_NAME`.
const SPAWN_SUBAGENT_TOOL = "spawn_subagent";

/// Whether the loaded rows show this conversation spawning a subagent — what
/// lights the header's `Subagents` entry. Read off the rendered rows rather
/// than asked for over the network, so a restored mirror answers it offline.
export function hasSubagentSpawn(rows: Row[]): boolean {
  return rows.some(
    (row) =>
      row.role === "work" &&
      row.steps.some((s) => s.kind === "tool" && s.tool === SPAWN_SUBAGENT_TOOL),
  );
}

/// Keep an unanswered read-only tail expanded even though persisted rows are inactive.
export function unansweredTailWorkIds(rows: Row[]): ReadonlySet<string> {
  const ids = new Set<string>();
  let at = rows.length - 1;
  while (at >= 0 && rows[at].role === "notice") at--;
  while (at >= 0 && rows[at].role === "work") {
    ids.add(rows[at].id);
    at--;
  }
  return ids;
}

/// Identity of a work step for dedup when folding two representations of the
/// same turn's block. Text steps key by kind + text.
///
/// A tool step keys by its call id, which both shapes now carry.
///
/// A row persisted before the gateway sent `call_id` on the REST shape has
/// none, and keying those `tool:` collapsed EVERY reconstructed call in the
/// block to one identity: folding two reconstructed halves of a turn kept the
/// first tool step and silently deleted the rest (a real 88-row turn lost 32 of
/// its steps to it on every restore), while folding a live block with its own
/// reconstruction double-rendered every call.
///
/// So an id-less step keys by what it DID. Distinct calls differ in their label
/// (the command/URL), and near-always in their summary/status too; two calls
/// identical in all three are indistinguishable here and still collapse. This
/// never mixes with the id form (`tool!` vs `tool:`) — an id-less step and an
/// id-carrying one for the SAME call don't dedup, which is why the gateway
/// sends the id rather than this being the answer.
export function workStepKey(s: WorkStep): string {
  if (s.kind !== "tool") return `${s.kind}:${s.text}`;
  if (s.callId) return `tool:${s.callId}`;
  // A NUL separator, so a field containing the separator cannot forge
  // another field's boundary ("a b" + "c" vs "a" + "b c").
  return ["tool!", s.label, s.status, s.summary ?? ""].join("\u0000");
}

/// Anchor for a prose step no tool call follows yet — the tail of an in-flight
/// block, whose successor simply hasn't landed. Mirrors the web chat.
const UNANCHORED_PROSE = "$";

/// Index of the last tool step, or -1. Prose past it is UNANCHORED.
function lastToolIndex(steps: WorkStep[]): number {
  let at = -1;
  steps.forEach((s, i) => {
    if (s.kind === "tool") at = i;
  });
  return at;
}

/// Per-list step identities. Prose keys by the TOOL CALL IT PRECEDES, not by its
/// text alone: two identical paragraphs in one turn ("我看下测试。") share a text
/// key, and `mergeWorkSteps` would then drop the second — invisible while prose
/// stayed hidden inside the collapse, a silently deleted paragraph now that it
/// renders. The successor is the right anchor because a row's Text and its
/// ToolUse blocks are ONE persisted row (the agent loop appends them together),
/// so no page tear can separate them and the live leg, the reconstruction and
/// both halves of `joinWorkHalves` always agree on which call a paragraph
/// precedes. Mirrors the web chat's `workStepKeys`.
export function workStepKeys(steps: WorkStep[]): string[] {
  const keys = steps.map(workStepKey);
  let anchor = UNANCHORED_PROSE;
  for (let i = steps.length - 1; i >= 0; i--) {
    const s = steps[i];
    if (s.kind === "tool") anchor = keys[i];
    else if (s.kind === "prose") keys[i] = `prose:${anchor}:${s.text}`;
  }
  return keys;
}

/// Concatenate two work blocks' steps WITHOUT duplicating shared ones — so
/// folding a torn turn's disjoint halves appends cleanly, while folding two
/// overlapping representations of one turn (live + reconstructed) collapses to
/// a single copy instead of doubling every step.
export function mergeWorkSteps(a: WorkStep[], b: WorkStep[]): WorkStep[] {
  const ka = workStepKeys(a);
  const kb = workStepKeys(b);
  const aLastTool = lastToolIndex(a);
  const bLastTool = lastToolIndex(b);
  // Non-prose identity is a plain set, exactly as before.
  const seenOther = new Set(ka.filter((_, i) => a[i].kind !== "prose"));
  // Prose matches by CONSUMING one of a's copies, so one paragraph can never
  // satisfy two of b's steps — a set would let a's single copy swallow both the
  // anchored and the unanchored occurrence and silently delete a paragraph.
  const freeProse: number[] = [];
  a.forEach((s, i) => {
    if (s.kind === "prose") freeProse.push(i);
  });
  const takeProse = (pred: (i: number) => boolean): boolean => {
    const at = freeProse.findIndex(pred);
    if (at === -1) return false;
    freeProse.splice(at, 1);
    return true;
  };

  const out = [...a];
  b.forEach((s, i) => {
    if (s.kind !== "prose") {
      if (seenOther.has(kb[i])) return;
      seenOther.add(kb[i]);
      out.push(s);
      return;
    }
    const same = (j: number) => (a[j] as { text?: string }).text === s.text;
    // Same paragraph, same anchor: the ordinary case.
    if (takeProse((j) => same(j) && ka[j] === kb[i])) return;
    // One side may hold a paragraph UNANCHORED — folded before its tool call
    // landed — while the other already anchored it. Same paragraph, two keys.
    if (i > bLastTool) {
      // b's tail is unanchored. It is a's paragraph only if b has contributed
      // nothing new yet, i.e. b is still a prefix of a's timeline; once b has
      // moved past a, a repeat of the same text is a LATER paragraph and
      // matching it would delete one.
      if (out.length === a.length && takeProse(same)) return;
    } else if (takeProse((j) => same(j) && j > aLastTool)) {
      return;
    }
    out.push(s);
  });
  return out;
}

/// What a `subscribe_state` bundle says about the reply on screen.
///  • `recovered`  — its TRAILING prose step is the answer streaming right now.
///  • `superseded` — the bundle carries answer text, but not as its tail: the
///    turn moved on, so a reply still on screen is stale and its text already
///    lives in the block as a `prose` step.
///  • `unknown`    — the bundle carries NO answer text at all, so it says
///    nothing about the reply either way. LEAVE IT ALONE. Reachable without a
///    race: `AgentEvent::Message` / `TurnState` clears the channel's in-flight
///    buffer while `active_turn_started_at` keeps reporting the turn active
///    through post-answer finalization, and the buffer also stops recording at
///    `MAX_INFLIGHT_ENTRIES`. Treating that as "stale" deletes a reply the user
///    is reading. Mirrors the web chat's `bundleAnswer`.
export type BundleAnswer =
  | { kind: "recovered"; text: string }
  | { kind: "superseded" }
  | { kind: "unknown" };

export function bundleAnswer(steps: WorkStep[]): BundleAnswer {
  const tail = steps.length > 0 ? steps[steps.length - 1] : undefined;
  if (tail !== undefined && tail.kind === "prose") return { kind: "recovered", text: tail.text };
  return steps.some((s) => s.kind === "prose") ? { kind: "superseded" } : { kind: "unknown" };
}

/// Strip the in-flight ANSWER from a sync page's trailing work block.
///
/// The REST plane folds the live channel's in-flight buffer into the trailing
/// block (`build_history_page` → `reconstruct_transcript`), and an
/// `AgentEvent::AnswerDelta` becomes a `prose` step there — so while a turn is
/// streaming, that block's LAST step is the answer `streamingText` is already
/// painting below it. `applySubscribeState` hoists the same text out of the
/// bundle for exactly this reason; the REST plane needs the same hoist or the
/// paragraph renders twice, once as a speech run and once as the live reply —
/// and, because the collapse no longer hides prose, it stays visible after the
/// turn ends and is persisted into the mirror.
///
/// Safe because a PERSISTED prose step is never a block's last: an intermediate
/// row's Text and its ToolUse are the same row, so reconstruction always emits a
/// tool step after the narration (the same invariant `workStepKeys` anchors on).
/// Only the in-flight tail can be trailing prose. Mirrors the web chat's
/// `dropInFlightAnswerStep`.
export function dropInFlightAnswerStep(rows: Row[]): Row[] {
  const i = lastBeforeNotices(rows);
  const row = i >= 0 ? rows[i] : undefined;
  if (row === undefined || row.role !== "work") return rows;
  const last = row.steps.length > 0 ? row.steps[row.steps.length - 1] : undefined;
  if (last === undefined || last.kind !== "prose") return rows;
  const kept = row.steps.slice(0, -1);
  // A block that held nothing but the in-flight answer was never work at all.
  const next: Row[] = kept.length > 0 ? [{ ...row, steps: kept }] : [];
  return [...rows.slice(0, i), ...next, ...rows.slice(i + 1)];
}

/// Freeze EVERY work row still marked `active` into its "Worked Xs" — walk the
/// whole thread, not just the tail. Called before appending/adopting a fresh
/// live block so the transcript never holds two open "Working" cards at once:
/// there is only ever one in-flight turn, hence one active block.
export function freezeActiveWork(rows: Row[]): Row[] {
  return rows.map((r) =>
    r.role === "work" && r.active
      ? { ...r, active: false, elapsedMs: r.elapsedMs ?? (r.startedAt !== undefined ? Date.now() - r.startedAt : undefined) }
      : r,
  );
}

/// Apply `mutate` to the tail work block, opening one if the turn doesn't have
/// an open block yet (the web chat's ensureWork). The pure core of
/// `withOpenWork`.
///
/// A work frame belongs to the tail work block whenever the tail IS one — even
/// if it was just FROZEN. A restored live block stays `active` and a re-entry's
/// continuation extends it (keeping its real startedAt); but a block can also be
/// frozen MID-STREAM by a `turn_state{inactive}` that raced ahead of a straggler
/// frame — on cancel the gateway emits an unguarded `tool_completed` through the
/// SAME ordered channel the turn-end projector rides, so `[tool_started] →
/// turn_state{inactive} → tool_completed` reaches the client with the block
/// already closed. Folding into the frozen tail rather than forking is the
/// invariant that keeps ONE turn to ONE card: this never appends a work row
/// adjacent to another (the `[work][work]` re-entry split). The straggler even
/// resolves its own still-"running" tool step in place. The block keeps its
/// frozen `active:false`, so a cancelled turn reads "Worked", not a stuck
/// "Working".
///
/// The tail is found by scanning back over a trailing NOTICE run, not by taking
/// `rows[len-1]`: a terminal notice landing once the block is frozen keeps its
/// OWN row (it may be durable — see `severTerminalNoticeIn`), and a plain
/// adjacency check would then fork the turn's continuation into a second card
/// ([work][notice][work]). The block folded back into keeps its frozen
/// `active:false` and its `elapsedMs` — `mutate` only appends steps — so a
/// straggler never re-opens or re-times a settled card.
export function openWorkIn(rows: Row[], mutate: (row: WorkRow) => WorkRow): Row[] {
  const i = lastBeforeNotices(rows);
  const target = i >= 0 ? rows[i] : undefined;
  if (target && target.role === "work") {
    const next = [...rows];
    next[i] = mutate(target);
    return next;
  }
  // Tail is not a work row: this frame opens a NEW block. Freeze EVERY
  // still-`active` block anywhere in the thread first, so a stale open block
  // can't linger as a second live "Working" card beside this one.
  const fresh: WorkRow = { id: uid(), role: "work", steps: [], active: true, startedAt: Date.now() };
  return [...freezeActiveWork(rows), mutate(fresh)];
}

/// A tool-authored mid-turn aside (the wire's `mid_turn` flag — the SERVER
/// declares fold-eligibility, the client never infers it from timing) folds
/// INTO the open work block as a leveled step, so it doesn't sever the block
/// into two cards (the tail must stay a work row for `openWorkIn` to keep
/// extending it). Only an ACTIVE block folds: these asides are live-only
/// (never persisted), so the folded step can't duplicate a durable row. With
/// no active block the aside keeps its own centered `role:"notice"` row. The
/// pure core of `foldMidTurnNotice`.
export function foldMidTurnNoticeIn(rows: Row[], level: string, text: string): Row[] {
  const last = rows[rows.length - 1];
  if (last && last.role === "work" && last.active) {
    return [
      ...rows.slice(0, -1),
      { ...last, steps: [...last.steps, { kind: "notice", level, text, at: Date.now() }] },
    ];
  }
  return [...rows, { id: uid(), role: "notice", content: text }];
}

/// A terminal or durable notice (`mid_turn`-less: the turn-failed / crash /
/// blank-reply notices, `/compact` confirmations) severs: freeze any active
/// block so it can't linger "working" behind the card, and keep the notice its
/// own centered row. Folding it instead would bury the turn's only output
/// inside the collapsing card — these notices beat the projector's
/// `turn_state{inactive}`, so an active-looking block proves nothing.
/// `durableId` is the persisted twin's `n<seq>` row id: the minted row adopts
/// it so the row the next sync redelivers dedups by id instead of rendering
/// the same text twice (a uid-keyed copy would be invisible to that dedup —
/// the exact doubling the stop-ack path avoids by never minting). The pure
/// core of `severTerminalNotice`.
export function severTerminalNoticeIn(rows: Row[], text: string, durableId: string | null): Row[] {
  const frozen = freezeActiveWork(rows);
  if (durableId !== null && durableId !== "") {
    // The durable row may already be on screen (a sync raced the live frame).
    if (rows.some((r) => r.id === durableId)) return frozen;
    return [...frozen, { id: durableId, role: "notice", content: text }];
  }
  return [...frozen, { id: uid(), role: "notice", content: text }];
}

/// Fuse a client work block (`base` — live/restored: freshest streamed steps +
/// active state) with the server's reconstruction of the SAME turn (`recon` —
/// authoritative persisted steps + server-anchored timing). One block, not two:
/// union the steps, anchor `startedAt` to the server's true turn start, and take
/// the server's duration for the frozen label (while still active the live
/// ticker rules, so `elapsedMs` stays unset). Keeps `base`'s id/active so a live
/// block isn't remounted mid-stream. Cancellation is a property of the whole
/// turn, so either side carrying it cancels the card — that is how a `/stop`ped
/// live block gets its label: only the reconstruction knows.
export function reconcileWork(base: WorkRow, recon: WorkRow): WorkRow {
  return {
    ...base,
    steps: mergeWorkSteps(base.steps, recon.steps),
    cancelled: (base.cancelled ?? false) || (recon.cancelled ?? false),
    startedAt: recon.startedAt ?? base.startedAt,
    // Carry the server's authoritative duration even while active (the live
    // ticker ignores it until the block closes) so the frozen "Worked Xs" is the
    // server's number regardless of who closes the block first.
    elapsedMs: recon.elapsedMs ?? base.elapsedMs,
    turnComplete: recon.turnComplete ?? base.turnComplete,
  };
}

/// Join two halves of ONE turn that a PAGE BOUNDARY cut in two.
///
/// A turn longer than a sync page reconstructs as two blocks, because each page
/// is folded on its own: the older page holds the turn's user row, so its half
/// times from the real turn start and is closed by that page's trailing flush;
/// the newer page has no user row, so its half opens at its first intermediate
/// row and is closed by the answer. Neither half's duration is the turn's.
///
/// Span the pair: start at the FIRST half's start (the true turn start), end at
/// the SECOND half's end (`startedAt + elapsedMs`). A restored half has no
/// `startedAt` (stripped), so fall back to a half's own duration — an
/// undercount, but the mirror only holds a split written before this joined at
/// the seam.
///
/// Completeness follows the NEWER half: the pair is whole once the half
/// carrying the turn's end is in, and stays cut off — joinable again — until
/// then, which is what lets a turn spanning three pages fold down in one pass.
/// Cancellation ORs, which amounts to the same rule: the server flags the half
/// the `/stop` closed, and a cut-off head is never flagged.
export function joinWorkHalves(first: WorkRow, second: WorkRow): WorkRow {
  const secondEnd =
    second.startedAt !== undefined && second.elapsedMs !== undefined
      ? second.startedAt + second.elapsedMs
      : undefined;
  const spanned =
    first.startedAt !== undefined && secondEnd !== undefined
      ? Math.max(0, secondEnd - first.startedAt)
      : undefined;
  return {
    ...first,
    steps: mergeWorkSteps(first.steps, second.steps),
    // A half still live keeps the card live; anchored to the turn's true start,
    // so the ticker reads the whole turn rather than restarting at the seam.
    active: first.active || second.active,
    cancelled: (first.cancelled ?? false) || (second.cancelled ?? false),
    startedAt: first.startedAt,
    elapsedMs: spanned ?? first.elapsedMs ?? second.elapsedMs,
    turnComplete: second.turnComplete ?? first.turnComplete,
  };
}

/// Fold two ADJACENT work rows that `sameContinuingTurn` has already cleared as
/// one turn. Which fold depends on what the two rows ARE: two server
/// reconstructions with DIFFERENT `w<ordinal>` ids are sequential halves of one
/// turn (a page boundary cut it), so span them; anything else — a live block
/// beside its own reconstruction, or the same row re-delivered — is two
/// representations of ONE span, so reconcile them.
export function foldWork(prev: WorkRow, next: WorkRow): WorkRow {
  const prevOrd = rowOrdinal(prev.id);
  const nextOrd = rowOrdinal(next.id);
  return prevOrd !== null && nextOrd !== null && prevOrd !== nextOrd
    ? joinWorkHalves(prev, next)
    : reconcileWork(prev, next);
}

/// Whether the block AFTER `prev` may be folded into it — the one question
/// every seam asks before calling `foldWork`. Adjacency alone used to answer it
/// ("a healthy turn has a message row between its block and the next"), which a
/// sync bug that scrambled row order turned into three turns welded under one
/// "Worked 2h 47m" card. The server answers it instead: a reconstructed head
/// qualifies ONLY when it says the page window's edge cut it off
/// (`turnComplete === false`) — a whole block is its own turn and never fuses
/// with the neighbour (a completed turn whose empty final reply left no bubble,
/// abutting the next fire). A live block is keyed by `uid()`, carries no flag,
/// and still fuses: that is the reconcile path, one span in two forms. An
/// `undefined` flag — a mirror written before this existed — DECLINES: an extra
/// card is a cosmetic split, a wrong join swallows a whole turn into another
/// turn's card.
function sameContinuingTurn(prev: WorkRow): boolean {
  if (rowOrdinal(prev.id) === null) return true;
  return prev.turnComplete === false;
}

/// Whether a compaction boundary sits between two work blocks' ordinals — the
/// server already breaks a work block at a watermark, so the two halves are
/// DIFFERENT turns (compaction is a turn boundary) and must not be re-fused: a
/// fused card would swallow the seam the `CompactionDivider` keys off. Both
/// halves must carry a durable `w<ordinal>` id; a live block (no ordinal) never
/// straddles a boundary.
///
/// Kept alongside `sameContinuingTurn`, which subsumes it only for a watermark
/// the server SAW: a split inside one reconstruction window flushes the
/// pre-compaction half `turn_complete: true`, and the turn-complete guard
/// refuses that on its own. A watermark falling in the gap BETWEEN two pages is
/// straddled by no single window, so neither reconstruction splits and the head
/// is an ordinary cut-off (`false`) block — this is the only guard that refuses
/// there.
function crossesCompaction(prev: WorkRow, next: WorkRow, points: CompactionPoint[]): boolean {
  const a = rowOrdinal(prev.id);
  const b = rowOrdinal(next.id);
  if (a === null || b === null) return false;
  return points.some((p) => a < p.ordinal && p.ordinal <= b);
}

/// Collapse every adjacent SAME-TURN work pair in an assembled row list.
/// Idempotent — a healthy list has no such adjacency — so it is safe to run at
/// each seam where rows are joined (a prepended history page, a rebuilt sync
/// page). Two blocks the server calls two turns stay two cards, and two
/// straddling a `compactionPoints` boundary are never fused either — a mid-turn
/// compaction's pre-/post halves are distinct turns with the divider between.
export function foldAdjacentWork(rows: Row[], compactionPoints: CompactionPoint[] = []): Row[] {
  const out: Row[] = [];
  for (const r of rows) {
    const prev = out[out.length - 1];
    if (
      r.role === "work" &&
      prev &&
      prev.role === "work" &&
      sameContinuingTurn(prev) &&
      !crossesCompaction(prev, r, compactionPoints)
    ) {
      out[out.length - 1] = foldWork(prev, r);
      continue;
    }
    out.push(r);
  }
  return out;
}

/// Ids of the work rows in a page whose ordinal is their OWN answer's — read off
/// the page's ordering, never guessed from the row.
///
/// A block is keyed `w<ordinal>` off some row of its turn, and for a tool-free
/// "thinking only" answer there IS no row but the answer itself: the gateway
/// seeds the block from it (`work.ordinal = Some(ordinal)` on the assistant
/// arm), so `w<N>` and `m<N>` are one turn and the block belongs ABOVE the
/// bubble. But a block can also BORROW an ordinal it does not own — one seeded
/// from a progress event, or from the page's last row when nothing of its turn
/// has persisted yet — and that anchor belongs to the turn BEFORE it, so the
/// same equality means the block belongs BELOW the bubble.
///
/// Same ordinal, opposite placement, and nothing on the row distinguishes them.
/// The page does: it emits a block above the answer it owns and below the one it
/// borrowed from. Both items are cut from the same source row, so a window that
/// carries one carries the other.
function blocksAboveTheirAnswer(pageRows: Row[]): Set<string> {
  const answerAt = new Map<number, number>();
  pageRows.forEach((r, i) => {
    const ord = r.role === "assistant" ? rowCoverageOrdinal(r) : null;
    if (ord !== null && !answerAt.has(ord)) answerAt.set(ord, i);
  });
  const owned = new Set<string>();
  pageRows.forEach((r, i) => {
    if (r.role !== "work") return;
    const ord = rowOrdinal(r.id);
    const at = ord === null ? undefined : answerAt.get(ord);
    if (at !== undefined && at > i) owned.add(r.id);
  });
  return owned;
}

/// The thread has TWO tails, and confusing them is a bug with teeth.
///
/// `lastBeforeNotices` is the tail a live frame asks about — "what is the last
/// row that isn't a trailing notice" — because a terminal notice keeps its own
/// row beside the block it interrupts (`severTerminalNoticeIn`) and must not
/// hide it. This is the scan `openWorkIn` runs, and it STOPS at an answer: a
/// settled turn's card is behind its own bubble, and reaching past that bubble
/// would let a later turn's frames rewrite a finished card.
///
/// `tailRunStart` is the tail a durable PLACEMENT asks about — "where does the
/// trailing answer/notice run begin" — because a re-delivered block has to be
/// weighed against the answer of the turn it belongs to. Only ordinal-carrying
/// evidence may cross an answer this way.
function lastBeforeNotices(rows: Row[]): number {
  let j = rows.length - 1;
  while (j >= 0 && rows[j].role === "notice") j--;
  return j;
}

/// Index of the last row that is neither an answer nor a notice — i.e. the row
/// just above the thread's trailing answer/notice run. `-1` when the whole list
/// is that run. A user row ends the scan like any other: it opens the NEXT turn,
/// so nothing below it belongs to what sits above.
function tailRunStart(rows: Row[]): number {
  let j = rows.length - 1;
  while (j >= 0 && (rows[j].role === "assistant" || rows[j].role === "notice")) j--;
  return j;
}

/// Index of the work block sitting directly above that trailing run — the block
/// of the turn the run belongs to. `-1` when the run is not headed by one.
function workBlockAboveTail(rows: Row[]): number {
  const j = tailRunStart(rows);
  return j >= 0 && rows[j].role === "work" ? j : -1;
}

/// Index in `rows` of the work block belonging to the SAME turn as `row`, a
/// durable work row that has ended up BELOW its turn's answer bubble. Accept the
/// block above the trailing answer/notice run only when that run carries the
/// answer this block's ordinal points at, so a genuinely later turn's block (its
/// answer not yet on screen) is never mis-folded. `-1` when there is no such
/// block. Used to re-home a durable `status`/thinking-only block the reopen path
/// can strand below the reply.
///
/// `ownsAnswerOrdinal` is the caller's evidence that this block's ordinal is its
/// OWN answer's rather than one borrowed from the turn above
/// (`blocksAboveTheirAnswer` reads it off the page's ordering) — the tool-free
/// turn, whose block can only ever match its answer by EQUALITY. It defaults
/// off: without that evidence, equality is not proof of anything, and folding on
/// it would weld a later turn into the card above.
export function sameTurnWorkIndex(rows: Row[], row: WorkRow, ownsAnswerOrdinal = false): number {
  const ord = rowOrdinal(row.id);
  if (ord === null) return -1;
  const at = workBlockAboveTail(rows);
  if (at < 0) return -1;
  const sawTurnAnswer = rows.slice(at + 1).some((rj) => {
    if (rj.role !== "assistant") return false;
    const oj = rowOrdinal(rj.id);
    return oj !== null && (oj > ord || (ownsAnswerOrdinal && oj === ord));
  });
  return sawTurnAnswer ? at : -1;
}

/// Merge a DIFFERENCE sync page into the rendered thread: a row already held is
/// reconciled where it stands, one we don't hold is placed AT ITS ORDINAL.
/// Rows arrive ascending; each carries its stable id.
///
/// Placement, not append, because `syncSince`'s prefix gate is evaluated when
/// the request is POSTED and the thread keeps growing while the round trip is in
/// flight. A difference asked for at `since` can therefore land after live
/// frames have rendered rows above it, and a blind tail append then files those
/// page rows below rows they predate — the scramble the gate exists to prevent,
/// re-entered through the back door. Nothing is lost either way; the order is
/// wrong, it persists into the mirror, and `syncSince` stays quiet over it
/// afterwards, so it does not self-heal.
///
/// A row goes immediately after the last row it is NOT older than — which keeps
/// ordinal-less live rows (an optimistic send, the in-flight work block) at the
/// tail where they belong, since a durable row always predates them. Rows with
/// no orderable key at all — an `n<seq>` notice, whose seq is a sequence number
/// and not an ordinal (`rowOrdinal` matches only `m`/`w`) — cannot be placed and
/// still append; `applySyncPage`'s apply-time prefix re-check is what covers
/// them, by refusing a stale page outright.
export function mergeSyncPage(
  prev: Row[],
  pageRows: Row[],
  compactionPoints: CompactionPoint[],
): Row[] {
  const next = [...prev];
  let byId = new Map(next.map((r, i) => [r.id, i] as const));
  const ownsOrdinal = blocksAboveTheirAnswer(pageRows);
  /// Where a page row belongs: just past the last rendered row whose ordinal it
  /// is at or above — but ONLY when a durable row still sits below that point,
  /// which is the proof that this row really did land late. With nothing
  /// ordinal-bearing below, it appends, exactly as it always did.
  ///
  /// That second condition is the whole safety of this function, and it took
  /// three tries to get right. A thread's trailing run is ordinal-less almost
  /// all the time: the in-flight work block is keyed by a `uid()`, a notice by
  /// an `n<seq>` whose seq is not an ordinal, and — the one that keeps catching
  /// people, this file included — EVERY user row this client rendered live,
  /// because the echo carries no ordinal (see the REPLACE-overlay note below).
  /// Ordering a durable row against any of those is guesswork, and guessing
  /// wrong is not cosmetic: file a turn's own answer above its own question and
  /// the reply renders over the card that produced it, `closeTrailingWork` never
  /// runs, and the block spins "Working…" forever while the next turn's steps
  /// weld into it. Refusing to guess costs only the rare late row that has
  /// nothing durable beneath it — which is where a plain append was already the
  /// behaviour, so nothing regresses.
  const placeAt = (row: Row): number => {
    const ord = rowCoverageOrdinal(row);
    if (ord === null) return next.length;
    // A block that carries its OWN answer's ordinal (`blocksAboveTheirAnswer` —
    // the tool-free turn) belongs directly ABOVE that bubble: "just past the
    // last row I am not older than" files a turn's card below its own reply,
    // which is the shape this whole function exists to prevent. Anchor on the
    // bubble itself rather than merely excluding it from the scan — with its
    // twin ordinal skipped, the scan lands on whatever ordinal-less row happens
    // to precede it (a live send, a notice) and files the card above the
    // question that produced it. With the answer not on screen there is nothing
    // to sit above, and the ordinary scan is exactly right.
    if (ownsOrdinal.has(row.id)) {
      const answer = next.findIndex((r) => r.role === "assistant" && rowCoverageOrdinal(r) === ord);
      if (answer >= 0) return answer;
    }
    let at = 0;
    for (let i = 0; i < next.length; i++) {
      const held = rowCoverageOrdinal(next[i]);
      if (held !== null && held <= ord) at = i + 1;
    }
    const anchoredBelow = next.slice(at).some((r) => rowCoverageOrdinal(r) !== null);
    return anchoredBelow ? at : next.length;
  };
  const closeTrailingWork = () => {
    const last = next[next.length - 1];
    if (!last || last.role !== "work" || !last.active) return;
    if (last.steps.length === 0) {
      next.pop();
      return;
    }
    next[next.length - 1] = {
      ...last,
      active: false,
      elapsedMs: last.elapsedMs ?? (last.startedAt !== undefined ? Date.now() - last.startedAt : undefined),
    };
  };
  for (const row of pageRows) {
    const existingIdx = byId.get(row.id);
    if (existingIdx !== undefined) {
      const existing = next[existingIdx];
      // A redelivery of a row already on screen: fold a same-id work
      // block's newer server steps + timing into what's rendered, or
      // reconcile a message row — drop an optimistic send's chrome, and
      // adopt the server's clock over the arrival stamp a live frame /
      // optimistic send left behind, so the time under the bubble is the
      // one a cold open will show. Otherwise a no-op.
      if (existing.role === "work" || row.role === "work") {
        if (existing.role === "work" && row.role === "work") {
          next[existingIdx] = reconcileWork(existing, row);
        }
      } else {
        const createdAt = row.createdAt ?? existing.createdAt;
        // A send of ours is keyed by its `platform_msg_id`, so its ordinal only
        // ever arrives on the durable twin — take it, or the confirmed row stays
        // invisible to `syncSince` forever. A live notice minted under its
        // `durable_id` likewise learns its severity only here.
        const ordinal = row.ordinal ?? existing.ordinal;
        const level = row.level ?? existing.level;
        if (
          existing.sendState !== undefined ||
          createdAt !== existing.createdAt ||
          ordinal !== existing.ordinal ||
          level !== existing.level
        ) {
          next[existingIdx] = { ...existing, sendState: undefined, createdAt, ordinal, level };
        }
      }
      continue;
    }
    // The in-flight turn's reconstructed `w<ordinal>` work block is the
    // SAME turn as the live/restored block at the tail — RECONCILE into
    // it (union steps + adopt server timing) rather than rendering a
    // second card. A turn we don't have yet ends on a non-work tail, so
    // its own work block is still appended. Guarded exactly as
    // `foldAdjacentWork` guards its own seams: only a tail the server calls
    // the same continuing turn, and never across a compaction boundary —
    // the server already broke the block there, so the two halves are
    // different turns and a fused card would swallow the divider's seam.
    const tail = next[next.length - 1];
    if (
      row.role === "work" &&
      tail &&
      tail.role === "work" &&
      sameContinuingTurn(tail) &&
      !crossesCompaction(tail, row, compactionPoints)
    ) {
      next[next.length - 1] = foldWork(tail, row);
      continue;
    }
    // A re-delivered `work` row whose turn ALREADY ended on screen: its
    // block sits ABOVE the turn's answer bubble (+ any trailing
    // notices), so the tail isn't work and id-dedup misses — a live
    // block is keyed by a client `uid()` while the reconstruction keys
    // it `w<ordinal>`, and even two reconstructions disagree
    // (`w<first-tool>` for a full tail vs `w<progress-anchor>` for a
    // difference window). Fold it back into that block instead of
    // pushing a SECOND card below the answer — the observer's `status`
    // narration, made durable, is re-delivered by the inclusive
    // (`after_ordinal >= since`) control-event scan and would otherwise
    // land as a stray "Worked" block under the reply. Bound the
    // back-scan to the SAME turn: reconcile only when the trailing run
    // holds an answer ordinal-above this block, so a genuinely later
    // turn's block (its answer not yet on screen) still appends.
    if (row.role === "work") {
      const at = sameTurnWorkIndex(next, row, ownsOrdinal.has(row.id));
      const target = at >= 0 ? next[at] : undefined;
      if (target && target.role === "work") {
        next[at] = reconcileWork(target, row);
        continue;
      }
    }
    const at = placeAt(row);
    if (at === next.length) {
      // Closing the trailing block is a TAIL act: an answer arriving at the end
      // of the thread ends the turn still running there. An answer placed back
      // among older rows ends nothing — the block below it belongs to a later
      // turn, and freezing it would stop the live card mid-run.
      if (row.role === "assistant") closeTrailingWork();
      next.push(row);
      byId.set(row.id, next.length - 1);
      continue;
    }
    // Every index at or past the splice point shifts, and `byId` is read again
    // on the next iteration to reconcile in place — so rebuild it rather than
    // patch it. Placement is the rare branch (a healthy forward page appends),
    // and the cost is one pass over a thread bounded by the page limit.
    next.splice(at, 0, row);
    byId = new Map(next.map((r, i) => [r.id, i] as const));
  }
  // The tail-append and re-home branches ask the fold guards themselves; the
  // ordinal SPLICE above asks nothing, and `placeAt` always lands a row directly
  // below an ordinal-bearing one — which may be a work block whose turn this row
  // continues. Fold once at the end, as app/web's `applySyncMerge` does. A
  // healthy page has no adjacency and `foldAdjacentWork` is idempotent, so the
  // other two branches pass through unchanged.
  return foldAdjacentWork(next, compactionPoints);
}

/// Apply a REPLACE page (a baseline, or a rebase) over the rendered thread: the
/// server's rows ARE the thread, re-overlaid with the two things no page can
/// carry — the in-flight turn's open work block, and any optimistic user send
/// with no durable row yet. `page` arrives already folded.
///
/// The kept-send predicate is membership in `unconfirmedSends` — the ids this
/// client minted and no page has yet answered for. NOT send chrome, and NOT a
/// missing ordinal; both were tried and both are wrong.
///
/// `sendState` is wrong because the gateway echoes an inbound message to its own
/// sender BEFORE handing it to the router that persists it (`channel/route.rs`,
/// `sub.echo_inbound` ahead of `incoming_tx.send`), and that echo frame carries
/// `ordinal: None` (`channel/adapter.rs`) — so `markSent` clears the spinner
/// while the row is still unpersisted. Gating on chrome dropped exactly the row
/// this rule exists to protect: a first send whose own `connEpoch` sync raced
/// its persistence lost its bubble, the cursor then leapfrogged it on the
/// answer's ordinal (a difference selects strictly `>`, so it could never come
/// back), and the outbox's next mount-edge replay re-appended it BELOW the
/// answer.
///
/// `ordinal === undefined` is wrong for the opposite reason: it is not a
/// property of unconfirmed sends but the STEADY STATE of every user row this
/// client rendered live. The echo never carries an ordinal, so the only stamp is
/// a difference redelivering the durable twin — which the cursor has usually
/// already outrun. Keying on it makes every such row immortal, and the first
/// REPLACE whose window is narrower than the thread tears months of settled
/// questions out of place and welds them below the newest answer.
///
/// A bounded set is the only honest answer, and it is what app/web's
/// `applySyncReplace` has always used (`unconfirmedSendIds`). Ids leave it on
/// the one signal that actually proves durability — a sync page carrying the
/// `platform_msg_id` — which is the same proof native's `reconcileOutboxAfterSync`
/// releases its own outbox entry on, off the very same frame.
///
/// Carry only the SINGLE newest active in-flight block across the rebuild; any
/// earlier still-active block is a stale fork — drop it so it can't re-appear
/// beside the reconstructed thread.
///
/// The page's reconstructed trailing `w<ordinal>` block and the live in-flight
/// block are the SAME turn — while the server says so (`sameContinuingTurn`: an
/// in-flight turn's trailing block is always cut off by the window's edge).
/// Fuse them into ONE block — keep it active, adopt the server id +
/// server-anchored timing, union the steps — instead of rendering both
/// (duplicate/overlapping cards) or dropping either (losing steps or the
/// correct duration). A COMPLETE tail block is a finished turn that merely left
/// no answer bubble; the live block belongs to the next one and keeps its card.
export function applySyncReplace(
  prev: Row[],
  page: Row[],
  unconfirmedSends: ReadonlySet<string>,
): Row[] {
  // A session's rows are never deleted (session data is core data), so an
  // empty page against a thread that holds rows is always a stale read: a
  // baseline the gateway served before this session's first row persisted
  // (echo before persist, plus a new session's actor cold-spawn — the longest
  // such window of any send). Applying it keeps every surviving row but
  // RE-FILES them — the kept sets return behind the page, so an ordinal-less
  // first send lands below the reply that outran it (and a row that lost its
  // kept-set membership is deleted outright). The empty page carries nothing a
  // rebuild could need; the thread it failed to describe is what's on screen.
  if (page.length === 0 && prev.length > 0) return prev;
  const pageIds = new Set(page.map((r) => r.id));
  const openWork = prev.filter((r): r is WorkRow => r.role === "work" && r.active).slice(-1);
  // Rows the page PREDATES. Its newest ordinal is the instant the server
  // snapshotted it, and a durable row above that is one this page cannot be
  // speaking about — a live reply that landed while the request was in flight.
  // Dropping it is permanent, not a redraw: that frame already advanced the
  // cursor to its own ordinal, a difference selects strictly `>`, and the cursor
  // is max-wins — so no later sync can return the row, and on a cold open (the
  // path that runs a baseline) the newest message simply never appears. This is
  // `rowsAboveFloor`'s rule at the other edge: a REPLACE is authoritative
  // between the page's oldest and newest ordinals, and silent outside them.
  const ceiling = page.reduce((hi, r) => Math.max(hi, rowCoverageOrdinal(r) ?? hi), -Infinity);
  const keptLive = prev.filter((r) => {
    const ordinal = rowCoverageOrdinal(r);
    return ordinal !== null && ordinal > ceiling && !pageIds.has(r.id);
  });
  const keptLiveIds = new Set(keptLive.map((r) => r.id));
  // A row can hold BOTH a membership and a stamped above-ceiling ordinal only
  // through a call-site ordering slip (the confirm handler retires before it
  // stamps) — but keeping the two sets exclusive HERE makes a slip render as
  // one bubble instead of two with duplicate keys.
  const keptSends = prev.filter(
    (r) =>
      r.role === "user" &&
      unconfirmedSends.has(r.id) &&
      !pageIds.has(r.id) &&
      !keptLiveIds.has(r.id),
  );
  let rows = page;
  let carried: Row[] = openWork;
  const live = openWork[0];
  if (live !== undefined) {
    const tail = rows[rows.length - 1];
    if (tail && tail.role === "work" && sameContinuingTurn(tail)) {
      rows = [
        ...rows.slice(0, -1),
        {
          ...tail,
          steps: mergeWorkSteps(tail.steps, live.steps),
          active: true,
          startedAt: tail.startedAt ?? live.startedAt,
        },
      ];
      carried = [];
    } else if (
      keptLive.length === 0 &&
      keptSends.length === 0 &&
      rows.slice(tailRunStart(rows) + 1).some((r) => r.role === "assistant")
    ) {
      // The page ends on an ANSWER, not on this turn's open half: as far as the
      // server is concerned the turn we are still holding open is over, and the
      // page already carries its reconstruction ABOVE that bubble. Appending
      // `carried` here is what put a second "Worked" card below the reply — the
      // DIFFERENCE path has closed its trailing block on exactly this signal
      // (`closeTrailingWork`) since it was written; REPLACE never did.
      //
      // Fold our steps back into the page's own block for that turn instead
      // (`mergeWorkSteps` dedups, and the persisted side is the superset, so
      // this is lossless), or — with no block to fold into — freeze what we hold
      // so it cannot spin "Working" forever behind a settled reply.
      //
      // Only when nothing on screen outranks the page: a row the page predates
      // (`keptLive` / `keptSends` — an owed send is how a LATER turn announces
      // itself) proves the block belongs to that later turn, and it keeps its
      // own live card at the tail.
      //
      // And only against POSITIVE evidence of the same turn — adjacency is not
      // it. A turn with no user row (a background delivery) leaves both kept
      // sets empty while its block is genuinely the next turn's, and joining
      // there welds two turns into one card, which no later sync undoes. The
      // evidence is overlap: a page that reconstructs the turn we watched holds
      // every step we streamed (persistence is the superset), so the merge
      // ABSORBS at least one of ours. Nothing absorbed means two turns.
      const at = workBlockAboveTail(rows);
      const above = at >= 0 ? rows[at] : undefined;
      const target = above !== undefined && above.role === "work" ? above : undefined;
      const fused = target !== undefined ? mergeWorkSteps(target.steps, live.steps) : undefined;
      if (target !== undefined && fused !== undefined && fused.length < target.steps.length + live.steps.length) {
        rows = [
          ...rows.slice(0, at),
          {
            ...target,
            steps: fused,
            cancelled: (target.cancelled ?? false) || (live.cancelled ?? false),
          },
          ...rows.slice(at + 1),
        ];
        carried = [];
      } else {
        // No proof, so no join. Keep what we hold — those steps may be their
        // only copy — but not as a live card: a block that spins "Working"
        // behind a settled reply is the symptom, and nothing at the tail can
        // close it once the page has moved past. An empty one was never work.
        carried = live.steps.length === 0 ? [] : freezeActiveWork(carried);
      }
    }
  }
  return [...rows, ...keptLive, ...keptSends, ...carried];
}

/// Restored rows re-enter with live-turn state INTACT: a work block that was
/// live at persist stays live ("working"), because exiting and re-entering
/// mid-turn — or before the agent's final reply — must NOT collapse it to
/// "worked". The buffered continuation frames extend that same block, and only
/// its terminal reply / turn-end closes it. `startedAt` is stripped from a block
/// restored ACTIVE and only from that one (`keepAnchor`): the anchor is an
/// absolute instant, and it is the live ticker / the close path — neither of
/// which a closed block runs — that would turn it into an absurd "Worked 7h".
/// A block that persisted already-closed stays closed, and keeps its anchor, so
/// `segmentWorkSteps` can still time its runs. Empty blocks have nothing to
/// show; unknown future roles are dropped. Also folds back a turn a mirror split
/// in two.
export function clearAwaitingApproval(step: WorkStep): WorkStep {
  return step.kind === "tool" && step.awaitingApproval
    ? { ...step, awaitingApproval: undefined }
    : step;
}

/// Does the mirror hold a work block whose turn can no longer be timed here?
///
/// An ACTIVE block legitimately has no duration — it is still running. A CLOSED
/// one always got its `elapsedMs` from `closeWork` (`now − startedAt`) or from
/// the server via `reconcileWork`, so a closed block with none is a mirror we
/// broke: either it closed after a restore had already stripped its `startedAt`
/// (the anchor is gone, and the local clock knows `now`, not when the turn
/// ended), or a legacy adjacency heal dropped the number outright. The duration
/// is unrecoverable locally — but the gateway still has it
/// (`work_started_at`/`work_ended_at`), and re-timing needs the row re-delivered
/// for `reconcileWork` to fuse. A difference sync never re-delivers a row below
/// the cursor, and `hasMoreOlder: false` can leave no history page to page in
/// either, so the block would read "worked for a moment" forever. A BASELINE
/// sync is the only way back — hence `cursor: null` on such a restore.
///
/// Only the TAIL is asked, and that window is load-bearing rather than a
/// shortcut. The demotion buys a re-timing only for a block the baseline page
/// actually re-delivers, and that page is the newest `SYNC_BASELINE_LIMIT`
/// transcript rows. Older blocks used to be healed by deletion — the REPLACE
/// dropped everything the page didn't carry — but a non-repair REPLACE now KEEPS
/// the rows above the page's floor (`rowsAboveFloor`), so a block up there would
/// survive, match again on the next open, and demote the cursor forever: a
/// baseline REPLACE on every single open, for a block no baseline can reach.
///
/// Within the window it stays self-limiting: the page rebuilds those blocks with
/// the gateway's timing and re-persists healed, so the next open takes the
/// normal difference path. Worst case is one baseline pull per open — of a chat
/// whose newest page holds a turn the gateway cannot time either (a cancelled or
/// crashed turn has no `work_ended_at`), which costs a round trip and, since the
/// rows are no longer in doubt, nothing on screen.
export function hasUntimedWork(rows: Row[] | undefined): boolean {
  return (rows ?? []).slice(-SYNC_BASELINE_LIMIT).some(
    (r) =>
      r.role === "work" &&
      !r.active &&
      r.elapsedMs === undefined &&
      Array.isArray(r.steps) &&
      r.steps.length > 0,
  );
}

/// A restored block's `startedAt`, kept unless it is still ACTIVE.
///
/// The anchor is an absolute epoch instant (the server's `work_started_at`, or
/// `Date.now()` when the block opened), so persisting it is harmless in itself.
/// What is not harmless is letting a block that survives the relaunch STILL
/// RUNNING read it: `LiveElapsed` renders `now − startedAt` (only in the live
/// branch), and the close paths take `elapsedMs ?? Date.now() − startedAt` —
/// both would count every app-closed hour, which is the "absurd Worked 7h" the
/// blanket strip was written against. Neither can fire on a block restored
/// CLOSED: it already carries `elapsedMs`, nothing recomputes it, and no ticker
/// renders. Dropping the anchor there bought nothing and cost the per-run
/// bounds `segmentWorkSteps` needs, so every restored turn's label fell back to
/// a step count with its true duration sitting unused on the row.
function keepAnchor(startedAt: number | undefined, active: boolean): number | undefined {
  return active ? undefined : startedAt;
}

export function sanitizeRestoredRows(rows: Row[] | undefined): Row[] {
  const out: Row[] = [];
  for (let r of rows ?? []) {
    if (r.role === "work") {
      if (!Array.isArray(r.steps) || r.steps.length === 0) continue;
      // A prompt that was still up when we persisted is NOT still up now: it
      // was answered, cancelled, or (after 5 minutes) denied by the gate itself
      // while the app was closed — and none of those signals is redeliverable
      // (`approval_resolved` isn't even broadcast for a timeout, and the
      // clearing `tool_completed` is a live-only frame). The pending set is
      // re-derived from the authoritative `subscribe_state.pending_approvals` on
      // the next subscribe, so drop the badge rather than let it strand as a
      // permanent "waiting for approval" on a step nothing can ever clear.
      r = { ...r, steps: r.steps.map(clearAwaitingApproval) };
      // Heal a mirror split by the (now-fixed) re-entry bug: two work blocks
      // directly adjacent (NO message row between) are ONE turn torn apart,
      // whether or not either half already closed. Fold the whole run into one
      // card, staying "working" if any piece was still live (a turn with no
      // final reply must not read as "worked"). NOT onto a head the server
      // called COMPLETE, though: that adjacency is one `sameContinuingTurn`
      // deliberately left standing (two turns, two cards), and welding it here
      // would undo the fold guard on every cold open. A head with no flag at
      // all is a pre-guard mirror — the only kind this heal was written for.
      // Since the `openWorkIn` fold-into-frozen-tail invariant now prevents
      // minting a fresh adjacency split, this only ever folds a LEGACY on-disk
      // mirror written by a pre-fix build (it re-persists as one row, so it
      // fires once per such session) — kept as defense-in-depth.
      //
      // KEEP the duration. `prev` anchors the turn, so its `elapsedMs` is the
      // best number available: when the tear stranded a fragment beside a
      // SERVER-reconstructed block (`w<ordinal>`, timed from
      // work_started_at/work_ended_at) it is the whole turn's true span, and
      // when two live halves tore apart it undercounts — still far better than
      // dropping it, which reads as "worked for a moment" on a five-minute turn
      // and is UNRECOVERABLE: the fold re-persists as one row, and a cursor
      // already past the block means no sync or history page ever re-delivers
      // it for `reconcileWork` to re-time. `startedAt` goes only if the fold is
      // still ACTIVE — see `keepAnchor`.
      const prev = out[out.length - 1];
      if (prev && prev.role === "work" && prev.turnComplete !== true) {
        const active = prev.active || r.active;
        out[out.length - 1] = {
          ...prev,
          steps: mergeWorkSteps(prev.steps, r.steps),
          active,
          startedAt: keepAnchor(prev.startedAt, active),
          elapsedMs: prev.elapsedMs ?? r.elapsedMs,
        };
      } else {
        // Heal a DIFFERENT persisted split: a durable progress block that a
        // prior build's reopen sync stranded AFTER its turn's answer bubble (so
        // it isn't adjacent to its own block — the adjacency heal above can't
        // reach it). Fold it back into the turn's pre-answer work block by
        // ordinal, so a mirror already corrupted by that bug self-corrects on
        // the next open instead of keeping the stray "Worked" card below the
        // reply forever (the reopen sync is a no-op once the cursor passed it).
        const at = sameTurnWorkIndex(out, r);
        const target = at >= 0 ? out[at] : undefined;
        if (target && target.role === "work") {
          out[at] = { ...target, steps: mergeWorkSteps(target.steps, r.steps), active: target.active || r.active };
        } else {
          out.push({ ...r, startedAt: keepAnchor(r.startedAt, r.active) });
        }
      }
    } else if (r.role === "user" || r.role === "assistant" || r.role === "notice") {
      // A send still "sending" when we persisted can't be in flight after a
      // relaunch (the leg is gone) — drop the stale spinner. A "failed" state is
      // a real outcome and survives, so its retry dot is there on the next open.
      out.push(r.role === "user" && r.sendState === "sending" ? { ...r, sendState: undefined } : r);
    }
  }
  return out;
}

/// How many of a restored mirror's rows the FIRST commit paints. Everything
/// older is held back and folded in on the next frame.
///
/// The mirror is the whole cold-open story (see docs/sync-and-outbox.md) and it
/// only grows: every turn and every scroll-up page a session ever rendered here
/// is in it. Seeding `messages` with all of it means the first paint waits on
/// the markdown parse, the DOM build and the WebKit layout of the ENTIRE
/// conversation, of which the reader can see one screen — which is exactly why a
/// long chat opens to a longer white screen than a short one, on the same
/// device, off the same disk.
///
/// The number is a screenful with room to spare, not a tuned constant: it only
/// has to be enough that the deferred head lands (one frame later, through
/// `prependOlder`, viewport-anchored) before anyone can scroll to where it
/// isn't.
export const FIRST_PAINT_ROWS = 40;

/// Split a sanitized mirror into what the first commit renders (`tail`) and the
/// older rows deferred past it (`head`, oldest-first). Below the threshold the
/// head is empty and nothing about the open changes.
///
/// Splitting anywhere is safe against the work-block fold that `prependOlder`
/// runs at the seam: `sanitizeRestoredRows` has already folded this array, so
/// any adjacent work pair still standing is one it deliberately left apart —
/// a head with `turnComplete === true` and a durable `w<ordinal>` id — and that
/// is precisely what `sameContinuingTurn` refuses. The rejoin is a no-op.
export function splitForFirstPaint(rows: Row[]): { head: Row[]; tail: Row[] } {
  if (rows.length <= FIRST_PAINT_ROWS) return { head: [], tail: rows };
  return { head: rows.slice(0, -FIRST_PAINT_ROWS), tail: rows.slice(-FIRST_PAINT_ROWS) };
}

/// The oldest durable ordinal in `rows` — the backward-paging floor for a thread
/// rendering only part of its mirror. `null` when nothing in it is ordinal-bearing.
export function oldestRowOrdinal(rows: Row[]): number | null {
  for (const r of rows) {
    const ordinal = rowOrdinal(r.id);
    if (ordinal !== null) return ordinal;
  }
  return null;
}



















/// Wall clock for a message's timestamp and the pre-compaction divider: `HH:MM`
/// same-day, prefixed `MM-DD` on an earlier day. Same name and rule as app/web's
/// `formatTimestampShort` so the two clients read the same. Empty for an
/// unparseable timestamp.
export function formatTimestampShort(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  const p = (n: number) => String(n).padStart(2, "0");
  const hm = `${p(d.getHours())}:${p(d.getMinutes())}`;
  const now = new Date();
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  return sameDay ? hm : `${p(d.getMonth() + 1)}-${p(d.getDate())} ${hm}`;
}











const COPY_TOAST_MS = 1300;
/// Below this gap between the bubble's top and the header-covered strip, the
/// pill would render under the native header overlay — flip it below instead.
const TOAST_HEADER_CLEARANCE_PX = 30;



/// Seam marking where a context compaction rewrote the LLM context. The
/// messages above it still render (their pre-compaction originals); the model no
/// longer sees them — it sees a summary in their place. A hairline + label.
function CompactionDivider({ label, at }: { label: string; at?: string }) {
  const time = at != null ? formatTimestampShort(at) : "";
  return (
    <div className="compaction-divider" role="separator">
      <span>{time ? `${label} ${time}` : label}</span>
    </div>
  );
}

/// One finalized transcript row, rendered as a GROUP of stacked bubbles: each
/// image / file attachment is its OWN bubble, separate from the text bubble —
/// never merged into one. User attachments + text stack right-aligned; assistant
/// attachments stack left with the reply prose below. Notices keep their single
/// centered bubble. Memoized so streaming ticks don't re-parse every settled
/// message's markdown.
const MessageRow = memo(function MessageRow({
  m,
  connEpoch,
  flash,
  onRetry,
}: {
  m: ChatMsg;
  connEpoch: number;
  /// Replay nonce for the jump ring (0 = this row is not the jump target). A
  /// nonce, never a boolean: jumping twice to the same row must bloom twice,
  /// and a boolean would Object.is-bail the re-render (same idiom as `copyId`).
  flash: number;
  onRetry: (m: ChatMsg) => void;
}) {
  const { t } = useTranslation();

  // Long-press copy — armed for every row but only wired onto the user text
  // bubble below (hooks must run unconditionally, ahead of the role returns).
  // `copyId` is a nonce (0 = idle): bumping it every copy — and keying the pill
  // on it — forces a fresh mount so the confirm animation REPLAYS even on a
  // repeat copy inside the toast window (a plain boolean would Object.is-bail
  // the re-render and the pill would sit frozen).
  const [copyId, setCopyId] = useState(0);
  const [toastBelow, setToastBelow] = useState(false);
  const bubbleRef = useRef<HTMLDivElement | null>(null);
  const copyTimer = useRef<number | undefined>(undefined);
  useEffect(() => () => clearTimeout(copyTimer.current), []);
  const copy = useCallback(() => {
    if (m.role !== "user" || !m.content) return;
    copyText(m.content);
    // The pill floats above the bubble by default; a bubble near the top of the
    // scroll would push it under the native header overlay, so flip it below.
    const el = bubbleRef.current;
    const log = el?.closest(".chat-log");
    const inset = log ? parseFloat(getComputedStyle(log).paddingTop) || 0 : 0;
    setToastBelow(el !== null && el.getBoundingClientRect().top - inset < TOAST_HEADER_CLEARANCE_PX);
    setCopyId((n) => n + 1);
    clearTimeout(copyTimer.current);
    copyTimer.current = window.setTimeout(() => setCopyId(0), COPY_TOAST_MS);
  }, [m.role, m.content]);
  const longPress = useLongPress(copy);

  if (m.role === "notice") {
    if (m.stopped) {
      // Compact stand-in for the gateway's `/stop` acknowledgement: a hairline
      // rule flanking a small square + "Stopped", centered.
      return (
        <div className="stopped-indicator" role="status" data-row-id={m.id}>
          <span className="stopped-mark" aria-hidden="true" />
          {t("chat.stopped")}
        </div>
      );
    }
    const level = m.level ?? "";
    return (
      <div
        className={level === "" ? "bubble notice" : `bubble notice notice-${level}`}
        data-row-id={m.id}
      >
        {m.content}
      </div>
    );
  }

  const attachments = m.attachments ?? [];
  // The group's `align-items` already sides it: the agent's clock lands at the
  // reply's bottom-left, the user's at the bubble's bottom-right. A row restored
  // from a pre-timestamp mirror simply has none.
  const time = m.createdAt !== undefined ? formatTimestampShort(m.createdAt) : "";
  const timeEl =
    time !== "" ? (
      <time className="msg-time" dateTime={m.createdAt}>
        {time}
      </time>
    ) : null;

  // The bloom is declared before BOTH branches: the message index only ever
  // offers the user's own sends, but a search hit lands on agent prose just as
  // often, and a jump that scrolls without marking its target reads as a jump
  // that did nothing.
  const ring = flash !== 0 ? <span key={flash} className="jump-ring" aria-hidden="true" /> : null;

  if (m.role === "assistant") {
    return (
      <div className="msg-group assistant" data-row-id={m.id}>
        {attachments.map((a, i) => (
          <AttachmentBubble key={`${a.blob_id}-${i}`} attachment={a} connEpoch={connEpoch} />
        ))}
        {m.content && (
          <div className="msg assistant">
            {ring}
            <MarkdownBody text={m.content} />
          </div>
        )}
        {/* An attachment-only reply has no prose div to host the ring, so the
            group carries it — the one case where there is nothing narrower. */}
        {!m.content && ring}
        {timeEl}
      </div>
    );
  }

  // A user send: the send indicator (spinner / retry dot) rides the message's
  // LAST bubble — the text bubble, or the last attachment bubble when the send
  // carries no text.
  const sendClass = m.sendState ? ` ${m.sendState}` : "";
  const sendChrome =
    m.sendState === "sending" ? (
      <span className="send-spinner" aria-hidden="true" />
    ) : m.sendState === "failed" ? (
      <button className="send-failed" onClick={() => onRetry(m)} aria-label={t("chat.retrySend")}>
        <span aria-hidden="true">!</span>
      </button>
    ) : null;
  const hasText = m.content.length > 0;
  return (
    <div className="msg-group user" data-row-id={m.id}>
      {attachments.map((a, i) => {
        const carriesSend = !hasText && i === attachments.length - 1;
        return (
          <AttachmentBubble
            key={`${a.blob_id}-${i}`}
            attachment={a}
            connEpoch={connEpoch}
            className={carriesSend && m.sendState ? m.sendState : undefined}
          >
            {carriesSend ? (
              <>
                {sendChrome}
                {ring}
              </>
            ) : null}
          </AttachmentBubble>
        );
      })}
      {hasText && (
        <div
          ref={bubbleRef}
          className={`bubble user${sendClass}${copyId !== 0 ? " copied" : ""}`}
          onTouchStart={longPress.onTouchStart}
          onTouchMove={longPress.onTouchMove}
          onTouchEnd={longPress.onTouchEnd}
          onTouchCancel={longPress.onTouchEnd}
        >
          {m.content}
          {sendChrome}
          {copyId !== 0 && (
            <span
              key={copyId}
              className={`copy-toast${toastBelow ? " copy-toast-below" : ""}`}
              aria-hidden="true"
            >
              <span className="copy-toast-check">✓</span>
              {t("chat.copied")}
            </span>
          )}
          {ring}
        </div>
      )}
      {timeEl}
    </div>
  );
});

/// The transcript-only chat thread. All chrome (header, composer, connection
/// state) is native SwiftUI; this renders the message log and keeps the
/// hardest-won behaviors: frame handling, reset recovery, history paging, and
/// the scroll/follow model.
export function Transcript({
  restored,
  initialConnEpoch,
  expandUnansweredTail = false,
}: {
  restored: PersistedState | null;
  initialConnEpoch: number;
  expandUnansweredTail?: boolean;
}) {
  const { t } = useTranslation();
  // The mirror, split so the first commit paints only the newest screenfuls
  // (`splitForFirstPaint`), with the backward-paging state that describes what
  // is actually RENDERED while the older half is withheld: the floor is the
  // tail's own oldest ordinal, and there is always more older (the head itself).
  // `drainDeferredHead` hands the mirror's own values back when it folds it in.
  //
  // Built in one `useState` initializer, not inline: a `useRef(expr)` argument
  // is evaluated on EVERY render, and this walks and slices the whole thread.
  const [restoredSplit] = useState(() => {
    const { head, tail } = splitForFirstPaint(sanitizeRestoredRows(restored?.messages));
    return {
      head,
      tail,
      oldestOrdinal: head.length > 0 ? oldestRowOrdinal(tail) : (restored?.oldestOrdinal ?? null),
      hasMoreOlder: head.length > 0 || (restored?.hasMoreOlder ?? false),
    };
  });
  // The deferred older rows, oldest-first. Drained ONCE, on the frame after the
  // first paint, through the same `prependOlder` seam scroll-up paging uses — so
  // the viewport is anchored and the fold/dedup rules at the seam still run.
  // Emptied without being rendered if a REPLACE lands first: those rows describe
  // a thread the server has just rebased away.
  const deferredHead = useRef<Row[]>(restoredSplit.head);
  const [messages, setMessages] = useState<Row[]>(restoredSplit.tail);
  // The rendered thread, readable from `runSync` — which must NOT re-create on
  // every row change: its identity gates the mount and safety-tick sync effects,
  // so a `messages` dependency would fire a pull on every bubble. Seeded from
  // the restored mirror, since the mount sync runs before the effect below.
  const messagesRef = useRef<Row[]>(messages);
  useEffect(() => {
    messagesRef.current = messages;
  }, [messages]);
  const [streaming, setStreaming] = useState("");
  const [turnActive, setTurnActive] = useState(false);
  // Latest turn-active value readable synchronously from the sync-apply
  // callbacks (a rebase/baseline REPLACE must not wipe a live streaming reply).
  const turnActiveRef = useRef(false);
  useEffect(() => {
    turnActiveRef.current = turnActive;
  }, [turnActive]);
  // Optimistic "a send is in flight, awaiting the turn to start" — mirrors the
  // web chat's `awaitingReply`. It bridges the gap between a user send and the
  // server's first `turn_state{active}` so the composer's stop button appears
  // the instant the user sends, and — until interjection ships — typing can't
  // flip it back to a send button mid-turn. Cleared the moment the server speaks
  // about the turn (turn_state / subscribe_state / an assistant reply / a
  // terminal notice) or the send fails, so it can never strand the stop button.
  const [awaitingReply, setAwaitingReply] = useState(false);
  // The full streamed answer so far. State updates are coalesced through one
  // rAF per frame burst — every push crosses the bridge as its own JS task, so
  // without this each delta would re-render (and re-parse markdown) alone.
  const streamText = useRef("");
  /// When the reply currently streaming BEGAN. A prose step marks the boundary
  /// of the stretch of work before it, so it must be stamped with when the
  /// model started speaking — not when the fold ran, which is a whole frame
  /// later, once the next work frame arrived.
  const streamStartedAt = useRef<number | undefined>(undefined);
  const streamRaf = useRef<number | undefined>(undefined);
  // Bumped by native on each successful (re)connect (setConnEpoch). Drives the
  // attachment auto-retry and replaces the old per-dial connGen guard.
  const [connEpoch, setConnEpoch] = useState(initialConnEpoch);
  const connEpochRef = useRef(initialConnEpoch);
  // platform_msg_ids already rendered (our optimistic sends + anything
  // restored), so the server's echo or a sync redelivery doesn't render twice.
  const sentIds = useRef<Set<string>>(
    new Set((restored?.messages ?? []).filter((m) => m.role === "user").map((m) => m.id)),
  );
  // The sends this client minted that no sync page has answered for yet — the
  // REPLACE-overlay's kept set (`applySyncReplace`). Starts EMPTY even when the
  // mirror restores optimistic-looking rows: the outbox is the authority on what
  // is still owed, and native re-seeds it through `userSent` on every mount edge.
  // A restored row absent from that replay is durable, and a REPLACE should drop
  // it in favour of the page's own copy.
  const unconfirmedSends = useRef<Set<string>>(new Set());
  // Durable ordinals already rendered. This catches the network-race where an
  // old leg delivers a final Message just before a sync redelivery carries the
  // same row again.
  const renderedOrdinals = useRef<Set<number>>(
    new Set(
      (restored?.messages ?? [])
        .filter((m) => m.role === "user" || m.role === "assistant")
        .map((m) => ordinalFromMessageId(m.id))
        .filter((n): n is number => n !== null),
    ),
  );
  // Evaluated once, at mount. A mirror holding a block we can no longer time
  // drops its cursor below, so the mount's sync REBUILDS rather than differences
  // — the gateway's copy is the only one that still knows the duration. This
  // does NOT delay the first paint: `messages` seeds from the mirror in its own
  // initializer and the paint path never reads the cursor, so the thread renders
  // on the first commit as always; the rebuilt page swaps in when it lands,
  // reusing every row's DOM (a block keeps its `w<ordinal>` key across the
  // REPLACE, so it re-times in place rather than remounting).
  const [restoredUntimedWork] = useState(() => hasUntimedWork(restored?.messages));
  // The sync cursor + its rebase-dirty flag (see transcript/cursor.ts, which
  // owns the advance rule). `cursor: null` = no baseline yet — the next sync
  // omits `since_ordinal` and REPLACEs on the newest page.
  const cursorRef = useRef<CursorState>({
    cursor: restoredUntimedWork ? null : (restored?.lastOrdinal ?? null),
    rebaseDirty: false,
  });
  // Started-at epoch-ms of turns this client has already seen END — the
  // turn-identity staleness test for a `subscribe_state` bundle's turn/work
  // halves (never cursor-vs-`as_of_ordinal` arithmetic). Bounded FIFO.
  const endedTurnStarts = useRef<number[]>([]);
  // Started-at epoch-ms of the currently-active turn (from a live
  // `turn_state`/`subscribe_state`), so its END can be recorded by identity.
  const activeTurnStart = useRef<number | null>(null);
  // Epoch-ms of the last frame seen for this session — the safety tick skips
  // when the stream proved itself live within the interval.
  const lastFrameAt = useRef(0);
  // Guards one in-flight sync request (the `sync_page`/`sync_failed` reply
  // clears it) so a burst of triggers coalesces to one pull.
  const syncInFlight = useRef(false);
  // Whether the baseline currently in flight is a REPAIR — see `runSync`. Only a
  // repair may throw the loaded history away; the other baselines rebuild the
  // newest page over rows that were never in doubt.
  const baselineIsRepair = useRef(false);
  // Render-visible mirror of that guard. An EMPTY thread with a sync in flight
  // is the one open the mirror cannot serve — a session this device has never
  // rendered (started on web/TUI, a cron fire, a push tap into a new session,
  // the first open after a re-pair) has no cached rows by construction, so its
  // first rows MUST come off the network. Everything else paints from the mirror
  // at mount and never sees this. The guard is a ref precisely so it doesn't
  // re-render; the loading line needs state, so keep the two in lockstep through
  // `setSyncInFlight` and never write the ref directly.
  const [syncing, setSyncing] = useState(false);
  const setSyncInFlight = useCallback((inFlight: boolean) => {
    syncInFlight.current = inFlight;
    setSyncing(inFlight);
  }, []);
  // Highest ordinal already reported to native as read — dedupes the
  // fire-and-forget `mark_read` posts (the cursor advances on every sync and
  // every live reply while the transcript is on screen).
  const lastMarkedRead = useRef(-1);
  // Lowest durable ordinal loaded — the scroll-up paging cursor
  // (`before_ordinal`). `null` = unknown / nothing older to page to.
  const oldestOrdinal = useRef<number | null>(restoredSplit.oldestOrdinal);
  const [hasMoreOlder, setHasMoreOlder] = useState<boolean>(restoredSplit.hasMoreOlder);
  // Mirror of `hasMoreOlder` for the jump loop, which re-evaluates INSIDE the
  // frame handler that just called `setHasMoreOlder` — the state it would close
  // over there is a render behind, and reading it stale is the difference
  // between stopping at the top of the thread and paging past it forever.
  const hasMoreOlderRef = useRef(restoredSplit.hasMoreOlder);
  hasMoreOlderRef.current = hasMoreOlder;
  const [loadingOlder, setLoadingOlder] = useState(false);
  // Compaction boundaries (`{ ordinal, at }[]`), the authoritative set carried
  // on every `sync_page`. Seeds the pre-compaction divider; restored from the
  // mirror so a cold open paints it before the mount sync refreshes it.
  const [compactionPoints, setCompactionPoints] = useState<CompactionPoint[]>(
    () => restored?.compactionPoints ?? [],
  );
  // Latest boundaries for the fold guard on the history-prepend seam (that path
  // carries no frame of its own; `applySyncPage` uses the frame's set directly).
  const compactionPointsRef = useRef<CompactionPoint[]>(compactionPoints);
  useEffect(() => {
    compactionPointsRef.current = compactionPoints;
  }, [compactionPoints]);
  // Tags the in-flight backward-history (scroll-up) request so its pushed
  // `history_page` reply is matched: the epoch captured at request time lets a
  // reply that arrives under a superseded connection epoch be dropped as stale.
  // `null` = no history page in flight.
  const relayHistory = useRef<{ epoch: number } | null>(null);
  // In-flight guard for an older-page load, so a scroll-event burst fires one
  // fetch. `loadingOlder` (state) drives the spinner; this ref is the race-free
  // gate.
  const pagingRef = useRef(false);
  const logRef = useRef<HTMLDivElement>(null);
  // Set just before a scroll-up PREPEND so the layout effect can re-anchor the
  // viewport (prepending above the top would otherwise jump the scroll
  // position).
  const prependAnchor = useRef<{ prevScrollHeight: number; prevScrollTop: number } | null>(null);
  // Set just before a REPLACE swaps the durable thread under a reader who is NOT
  // at the newest edge: the row they were on and where it sat. The layout effect
  // below puts it back under the same pixel, and falls back to the newest edge
  // when the rebuild dropped it.
  const replaceAnchor = useRef<{ rowId: string; top: number } | null>(null);
  // Whether the viewport is pinned to the newest edge (bottom). Maintained by
  // the window scroll listener; new content auto-scrolls only while pinned, so a
  // reader who scrolled up into history isn't yanked back down.
  const followRef = useRef(true);
  // True while a finger is down on the transcript. The programmatic pin-to-
  // newest writes below (stream deltas, ResizeObserver, the keyboard-slide rAF
  // loop) fight the drag on the main-frame scroller — a write landing mid-drag
  // slams scrollTop back to the bottom every frame. Suspend them while touching.
  const userTouchingRef = useRef(false);
  // Document scrollTop captured at touchstart, so touchend can tell a deliberate
  // upward DRAG (must stay put) from a pure HOLD at the bottom during streaming
  // (content grew below with pins suspended → catch up on lift). Without this,
  // any sub-threshold drag sprang back on release.
  const touchStartScrollTop = useRef(0);
  // scrollHeight captured at touchstart, so touchend re-pins ONLY when content
  // actually landed during the touch (a hold at the bottom during streaming) —
  // not on a plain tap. A re-pin scrolls inside the touchend handler, which
  // makes WebKit cancel the tap's synthetic `click`, so an unconditional re-pin
  // eats taps on work blocks / buttons whenever `followRef` is set but the
  // scroll isn't exactly at the bottom.
  const touchStartScrollHeight = useRef(0);
  // Drives the jump-to-latest button — a render concern, unlike followRef
  // (a ref precisely so scrolling doesn't re-render).
  const [showJump, setShowJump] = useState(false);
  // True while the jump-to-latest smooth glide is in flight. The glide fires
  // scroll events that still read as "off the edge"; onScroll holds the
  // follow/button state while this is set so the button doesn't flicker back.
  const glidingRef = useRef(false);
  const glideTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(glideTimer.current), []);
  // Which row the jump ring is blooming around, and the replay nonce that lets
  // it bloom again on a repeat jump to the same row (see MessageRow's `flash`).
  const [flash, setFlash] = useState({ id: "", nonce: 0 });
  // A search hit whose row is not loaded yet: the ordinal to reach and how many
  // more pages may be spent reaching it. A ref, not state — the loop is driven
  // by frames landing, and re-rendering on each step would buy nothing.
  const pendingJump = useRef<{ ordinal: number; pagesLeft: number } | null>(null);
  const jumpSettleTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(jumpSettleTimer.current), []);

  // Sizes of the images this thread has decoded, restored from the mirror and
  // rewritten with it. Held in a ref (not state) and handed out on a context
  // whose identity never changes: recording a size must not re-render the
  // transcript — every row would re-render on every image that lands.
  // Lazily, via useState: `useRef(restoreImageDims(...))` would rebuild the whole
  // map on EVERY render (useRef's argument is a value, not an initializer) —
  // including every rAF-coalesced streaming tick. The Map is mutated in place, so
  // its identity is stable and the setter is never needed.
  const [imageDims] = useState(() => restoreImageDims(restored?.imageDims));
  // The persist effect below only runs when the ROWS change, so a newly recorded
  // image size would never reach disk on its own. Keep the latest payload's
  // closure here and let `record` fire it directly — the bridge debounces, so a
  // burst of decodes still collapses into one write.
  const persistLatest = useRef<() => void>(() => {});
  const imageDimsStore = useMemo<ImageDimsStore>(
    () => ({
      get: (digest) => imageDims.get(digest),
      record: (digest, width, height) => {
        const known = imageDims.get(digest);
        if (known && known[0] === width && known[1] === height) return;
        // Insertion-ordered, so the oldest entry is the first key.
        if (imageDims.size >= MAX_IMAGE_DIMS) {
          const oldest = imageDims.keys().next().value;
          if (oldest !== undefined) imageDims.delete(oldest);
        }
        imageDims.set(digest, [width, height]);
        persistLatest.current();
      },
    }),
    [imageDims],
  );

  // Mirror the thread to native on every change so a webview reload / app
  // relaunch restores it (via init.restoredState). Debounced bridge-side.
  //
  // An empty thread with no cursor writes NOTHING. It has nothing to restore, and
  // the transcript mounts for every compose draft — including the throwaway one
  // the app prewarms at launch — each under a fresh uuid that never becomes a
  // chat-list row. Persisting those minted a mirror file per abandoned draft that
  // no code could ever reach again: a draft has no row, so the user can't delete
  // it, and nothing sweeps the directory (see `TranscriptStore`). Don't create
  // the orphan rather than hunt it later.
  useEffect(() => {
    persistLatest.current = () => {
      if (messages.length === 0 && cursorRef.current.cursor === null) return;
      // The first commit deliberately renders only the mirror's newest rows
      // (`splitForFirstPaint`); writing in that window persists the TRUNCATED
      // thread and loses the rest for good — the file is the only copy of a row
      // the sync cursor has long since passed. The debounce usually hides this
      // (the drain lands a frame later and supersedes the pending write), but
      // `flushPersist` is synchronous and both `pagehide` and native's detach
      // fire it — so a back-out inside that frame is a real path, not a
      // theoretical one.
      if (deferredHead.current.length > 0) return;
      persistState({
        messages,
        lastOrdinal: cursorRef.current.cursor,
        oldestOrdinal: oldestOrdinal.current,
        hasMoreOlder,
        imageDims: Object.fromEntries(imageDims),
        compactionPoints,
      });
    };
    persistLatest.current();
  }, [messages, hasMoreOlder, imageDims, compactionPoints]);

  // Open the thread at its newest edge — a restored thread would otherwise
  // mount showing its OLDEST rows. Pre-paint, so the top never flashes by.
  useLayoutEffect(() => {
    const el = scrollEl();
    if (el) el.scrollTop = el.scrollHeight;
  }, []);

  // While pinned to the newest edge, keep it in view as content lands (rows,
  // stream deltas, the turn indicator) — pre-paint, so a bubble never paints
  // off-screen first. A scroll-up PREPEND and a REPLACE are exempt even while
  // pinned (a short thread's "load earlier" tap): the anchor effects below own
  // those viewport changes — this effect is declared first so an armed anchor is
  // still visible.
  useLayoutEffect(() => {
    const el = scrollEl();
    if (
      el &&
      followRef.current &&
      !prependAnchor.current &&
      !replaceAnchor.current &&
      !userTouchingRef.current
    ) {
      el.scrollTop = el.scrollHeight;
    }
  }, [messages, streaming, turnActive]);

  // `.chat-log` now grows with its content (min-height, no inner scroll), so a
  // ResizeObserver on it fires on the late async growth the one-shot mount pin
  // races — webfont swap (font-display:swap reflow) and bridge-loaded images —
  // as well as the keyboard padding slide. While pinned, hold the newest edge
  // through all of it (this is what keeps the first keyboard raise from snapping
  // up an un-repinned drift). Pin the DOCUMENT, not the observed box.
  useEffect(() => {
    const box = logRef.current;
    if (!box) return;
    const ro = new ResizeObserver(() => {
      const el = scrollEl();
      if (!el) return;
      // A resize that leaves nothing below the fold — an empty/short draft, or
      // the prewarm 0→full-size grow when a reused draft first paints — clears a
      // jump button latched by a transient off-edge scroll during that resize:
      // onScroll is the only recompute of follow/showJump, and a non-scrollable
      // thread emits no further scroll event to correct it. Nothing to scroll ⇒
      // always following, button hidden.
      if (el.scrollHeight - el.clientHeight <= FOLLOW_BOTTOM_THRESHOLD_PX) {
        followRef.current = true;
        setShowJump(false);
        return;
      }
      if (followRef.current && !userTouchingRef.current) el.scrollTop = el.scrollHeight;
    });
    ro.observe(box);
    return () => ro.disconnect();
  }, []);

  // Track finger-down (on the document — the whole page is the scroller now) so
  // the pin-to-newest writes yield to a drag. Passive — never blocks the scroll.
  useEffect(() => {
    const down = () => {
      userTouchingRef.current = true;
      // Any touch at all disarms a pending jump re-seat. Checking
      // `userTouchingRef` at the deadline is not enough: it is already false
      // during momentum scrolling and after any drag that ended inside the
      // window, and the correction would then yank back a reader who had
      // deliberately scrolled away from where the jump landed.
      clearTimeout(jumpSettleTimer.current);
      const el = scrollEl();
      touchStartScrollTop.current = el?.scrollTop ?? 0;
      touchStartScrollHeight.current = el?.scrollHeight ?? 0;
    };
    const up = () => {
      userTouchingRef.current = false;
      const el = scrollEl();
      if (!el) return;
      // Catch up to the newest edge on lift ONLY for a hold at the bottom where
      // content actually GREW while pins were suspended — NOT for a deliberate
      // upward drag (must stay put), and NOT for a plain tap (re-pinning it
      // scrolls inside the touchend handler, so WebKit cancels the tap's
      // synthetic click — taps on work blocks / buttons then need several tries).
      const draggedUp = el.scrollTop < touchStartScrollTop.current - 2;
      const grew = el.scrollHeight - touchStartScrollHeight.current > 1;
      if (followRef.current && !draggedUp && grew) el.scrollTop = el.scrollHeight;
    };
    window.addEventListener("touchstart", down, { passive: true });
    window.addEventListener("touchend", up, { passive: true });
    window.addEventListener("touchcancel", up, { passive: true });
    return () => {
      window.removeEventListener("touchstart", down);
      window.removeEventListener("touchend", up);
      window.removeEventListener("touchcancel", up);
    };
  }, []);

  // After a scroll-up PREPEND, restore the viewport so the content the user was
  // looking at stays put (the log is `flex-direction: column`, so inserting
  // older rows above the top would otherwise shove everything down). Runs
  // pre-paint keyed on `messages`; only acts when a prepend armed the anchor.
  useLayoutEffect(() => {
    const anchor = prependAnchor.current;
    const el = scrollEl();
    if (!anchor || !el) return;
    el.scrollTop = anchor.prevScrollTop + (el.scrollHeight - anchor.prevScrollHeight);
    prependAnchor.current = null;
  }, [messages]);

  // After a REPLACE, put the row the reader was on back under the same pixel.
  // Declared AFTER the prepend anchor so a history page and a REPLACE landing in
  // one batch settle on the MEASURED row rather than on the prepend's arithmetic
  // (which knows nothing about the rows the REPLACE swapped out from under it).
  useLayoutEffect(() => {
    const anchor = replaceAnchor.current;
    const el = scrollEl();
    if (!anchor || !el) return;
    replaceAnchor.current = null;
    const node = document.querySelector(`[data-row-id="${CSS.escape(anchor.rowId)}"]`);
    if (node === null) {
      // The rebuild dropped the row — there is nothing left to hold a position
      // against, which is the baseline repair path's whole premise. Newest edge.
      followRef.current = true;
      el.scrollTop = el.scrollHeight;
      return;
    }
    el.scrollTop += node.getBoundingClientRect().top - anchor.top;
  }, [messages]);

  // ---- work-frame commit coalescing ----------------------------------------
  // reasoning / tool / status / notice frames arrive one wire frame per task
  // (each its own evaluateJavaScript), so their setMessages calls can never
  // batch on their own: a chatty Working phase committed — and recomputed
  // every O(thread) render derivation — at wire rate, not display rate. Row
  // transforms from those handlers queue here and land as ONE reduced
  // setMessages per animation frame, in arrival order. Anything that must
  // observe the applied rows in its own task (a terminal message, a sync
  // page, a send append — each reads or extends the tail the queued ops
  // target) drains the queue first via flushRowOps; React batches the drain
  // with the caller's own update, so the barrier costs no extra commit.
  const rowOps = useRef<Array<(rows: Row[]) => Row[]>>([]);
  const rowOpsRaf = useRef<number | undefined>(undefined);
  const flushRowOps = useCallback(() => {
    if (rowOpsRaf.current !== undefined) {
      cancelAnimationFrame(rowOpsRaf.current);
      rowOpsRaf.current = undefined;
    }
    const ops = rowOps.current;
    if (ops.length === 0) return;
    rowOps.current = [];
    setMessages((rows) => ops.reduce((acc, op) => op(acc), rows));
  }, []);
  const enqueueRowOp = useCallback(
    (op: (rows: Row[]) => Row[]) => {
      rowOps.current.push(op);
      if (rowOpsRaf.current === undefined) {
        rowOpsRaf.current = requestAnimationFrame(() => {
          rowOpsRaf.current = undefined;
          flushRowOps();
        });
      }
    },
    [flushRowOps],
  );
  useEffect(
    () => () => {
      if (rowOpsRaf.current !== undefined) cancelAnimationFrame(rowOpsRaf.current);
    },
    [],
  );

  const foldMidTurnNotice = useCallback(
    (level: string, text: string) => {
      enqueueRowOp((rows) => foldMidTurnNoticeIn(rows, level, text));
    },
    [enqueueRowOp],
  );

  const severTerminalNotice = useCallback(
    (text: string, durableId: string | null) => {
      enqueueRowOp((rows) => severTerminalNoticeIn(rows, text, durableId));
    },
    [enqueueRowOp],
  );

  const appendNotice = useCallback(
    (text: string) => {
      enqueueRowOp((m) => [...m, { id: uid(), role: "notice", content: text }]);
    },
    [enqueueRowOp],
  );

  // A user opened a collapsed work block: stop following the newest edge so the
  // block grows DOWNWARD from its summary. Left following, the pin (ResizeObserver
  // / layout effect) chases the bottom as the steps insert and shoves the summary
  // up. Disengage synchronously so it beats the growth's pin; once the steps have
  // painted, reflect whether the newest edge is now off-screen (jump button).
  // Only on open — collapsing shrinks content and needs no change.
  const handleWorkToggle = useCallback((open: boolean) => {
    if (!open) return;
    followRef.current = false;
    requestAnimationFrame(() => {
      const el = scrollEl();
      if (!el) return;
      setShowJump(el.scrollHeight - el.scrollTop - el.clientHeight > FOLLOW_BOTTOM_THRESHOLD_PX);
    });
  }, []);

  // The server acknowledged our own send (its echo arrived by platform_msg_id) —
  // clear the send-state chrome (spinner / retry dot) on that optimistic bubble
  // and stamp the ordinal the echo brought. The row stays keyed by its
  // `platform_msg_id`, so this stamp is the only thing that ever makes a send of
  // ours count as sync coverage (`rowCoverageOrdinal`).
  const markSent = useCallback((msgId: string, ordinal: number | null) => {
    setMessages((rows) => {
      // Return the ORIGINAL array when nothing changed so React bails out of
      // the re-render — `sendConfirmed` now calls this once per user row of
      // every sync page, and the common case is a row already settled.
      const next = rows.map((r) => {
        if (r.role !== "user" || r.id !== msgId) return r;
        const stamped = ordinal ?? r.ordinal;
        if (r.sendState === undefined && stamped === r.ordinal) return r;
        return { ...r, sendState: undefined, ordinal: stamped };
      });
      return next.every((r, i) => r === rows[i]) ? rows : next;
    });
  }, []);

  // Native's send Task errored — flip the still-sending bubble to the failed
  // (red retry dot) state. Guarded on "sending" so a late failure can't stomp a
  // bubble the echo already delivered.
  const markFailed = useCallback((msgId: string) => {
    // The send never reached the gateway, so no turn will start — leave the
    // optimistic awaiting window so the stop button doesn't strand.
    setAwaitingReply(false);
    setMessages((rows) =>
      rows.map((r) =>
        r.role === "user" && r.id === msgId && r.sendState === "sending" ? { ...r, sendState: "failed" } : r,
      ),
    );
  }, []);

  // Tap the red dot: re-post the payload native-side (same msgId → idempotent)
  // and flip the bubble back to sending so the spinner returns while it retries.
  const retryMessage = useCallback((m: ChatMsg) => {
    retrySend({ msgId: m.id, text: m.content, attachments: m.attachments ?? [] });
    // Re-enter the awaiting window — the resend can start a turn.
    setAwaitingReply(true);
    setMessages((rows) =>
      rows.map((r) => (r.role === "user" && r.id === m.id ? { ...r, sendState: "sending" } : r)),
    );
  }, []);

  // ---- streaming answer (rAF-coalesced) ------------------------------------

  const appendStreaming = useCallback((text: string) => {
    if (streamText.current.length === 0) streamStartedAt.current = Date.now();
    streamText.current += text;
    if (streamRaf.current === undefined) {
      streamRaf.current = requestAnimationFrame(() => {
        streamRaf.current = undefined;
        setStreaming(streamText.current);
      });
    }
  }, []);

  // Set the streaming reply to an exact text in ONE synchronous update — no rAF
  // defer, no clear→append two-step. A WorkSnapshot recovers the answer tail
  // with this, so the reply line grows in place (batched with the block replace)
  // instead of blanking for a frame.
  const setStreamingText = useCallback((text: string) => {
    if (text.length === 0) streamStartedAt.current = undefined;
    else if (streamText.current.length === 0) streamStartedAt.current = Date.now();
    streamText.current = text;
    if (streamRaf.current !== undefined) {
      cancelAnimationFrame(streamRaf.current);
      streamRaf.current = undefined;
    }
    setStreaming(text);
  }, []);

  const clearStreaming = useCallback(() => setStreamingText(""), [setStreamingText]);

  useEffect(
    () => () => {
      if (streamRaf.current !== undefined) cancelAnimationFrame(streamRaf.current);
    },
    [],
  );

  // ---- work block (the turn's thinking / tool process) ---------------------

  const withOpenWork = useCallback(
    (mutate: (row: WorkRow) => WorkRow) => {
      enqueueRowOp((rows) => openWorkIn(rows, mutate));
    },
    [enqueueRowOp],
  );

  const pushWorkStep = useCallback(
    (step: WorkStep) => {
      // Live frames carry no time of their own, so arrival IS the step's time.
      const stamped = step.at === undefined ? { ...step, at: Date.now() } : step;
      withOpenWork((w) => ({ ...w, steps: [...w.steps, stamped] }));
    },
    [withOpenWork],
  );

  // Rewrite the tail work block's tool steps IN PLACE. Unlike `withOpenWork`
  // this never OPENS a block: an approval frame is not a work frame, and
  // `approval_resolved` is broadcast connection-wide (it carries no session), so
  // a session with no block open would otherwise sprout an empty "Working" card
  // every time some other conversation answered a prompt.
  const rewriteToolSteps = useCallback(
    (mutate: (step: WorkStep) => WorkStep) => {
      enqueueRowOp((rows) => {
        const last = rows[rows.length - 1];
        if (!last || last.role !== "work") return rows;
        const steps = last.steps.map((s) => (s.kind === "tool" ? mutate(s) : s));
        return [...rows.slice(0, -1), { ...last, steps }];
      });
    },
    [enqueueRowOp],
  );

  // A prompt opened on `toolCallId`: badge that step as waiting. Keyed by the
  // TOOL call's id (`tool_call_id`), which is what the step carries — the
  // prompt's own `call_id` is a fresh id per prompt and is only stashed so the
  // matching `approval_resolved` can find the step again.
  const markStepAwaitingApproval = useCallback(
    (toolCallId: string, promptId: string) => {
      rewriteToolSteps((s) =>
        s.kind === "tool" && s.callId === toolCallId ? { ...s, awaitingApproval: promptId } : s,
      );
    },
    [rewriteToolSteps],
  );

  // A prompt was answered: clear the waiting badge and label the step with the
  // decision. Matched by PROMPT id, because that is all `approval_resolved`
  // carries. The label is provisional until the call completes and the server's
  // own `approval` lands on it — identical value, but that one is the persisted
  // twin that survives a reload.
  const resolveStepApproval = useCallback(
    (promptId: string, decision: string) => {
      rewriteToolSteps((s) =>
        s.kind === "tool" && s.awaitingApproval === promptId
          ? { ...s, awaitingApproval: undefined, approval: decision }
          : s,
      );
    },
    [rewriteToolSteps],
  );

  // Answer text followed by more work was intermediate: settle it into the
  // block as a prose step so reasoning and answer interleave cleanly (the web
  // chat's flush-and-fold on any non-delta work frame). Purely a
  // reclassification — `segmentWorkSteps` paints the resulting step at the
  // same reading weight, in the same place the streaming reply occupied, so
  // the reader sees nothing move.
  const foldStreamingIntoProse = useCallback(() => {
    const text = streamText.current;
    if (!text) return;
    const at = streamStartedAt.current;
    pushWorkStep({ kind: "prose", text, at });
    // Drain the queued step in THIS task, so the prose step and the emptied
    // reply commit together — left to the rAF, the paragraph blanks for a
    // frame mid-read (the exact bug `setStreamingText` exists to prevent).
    flushRowOps();
    clearStreaming();
  }, [clearStreaming, pushWorkStep, flushRowOps]);

  // Close the tail work block: freeze the elapsed label, or drop the block
  // entirely when the turn produced no steps (a plain direct answer).
  const closeWork = useCallback(() => {
    enqueueRowOp((rows) => {
      const last = rows[rows.length - 1];
      if (!last || last.role !== "work" || !last.active) return rows;
      if (last.steps.length === 0) return rows.slice(0, -1);
      // Prefer the server's authoritative duration (reconciled in) over the
      // wall-clock fallback, which is only correct for a purely live-watched turn.
      const elapsedMs = last.elapsedMs ?? (last.startedAt !== undefined ? Date.now() - last.startedAt : undefined);
      return [...rows.slice(0, -1), { ...last, active: false, elapsedMs }];
    });
  }, [enqueueRowOp]);

  // Remember a turn we've seen END (turn_state{active:false} or its final
  // Message), so a later `subscribe_state` bundle for the SAME turn — matched by
  // started_at — is judged stale by turn identity and its turn/work halves are
  // discarded. Bounded FIFO; the exact size is unimportant (a client rarely
  // holds more than one live turn's identity at a time).
  const recordEndedTurn = useCallback((startedMs: number | null) => {
    if (startedMs === null) return;
    const seen = endedTurnStarts.current;
    if (seen.includes(startedMs)) return;
    seen.push(startedMs);
    if (seen.length > 8) seen.shift();
  }, []);

  // Apply one `subscribe_state` bundle's turn/work halves. The bundle is the
  // whole coalesced turn — a superset of anything shown live — so REPLACE the
  // open block's steps rather than append (appending would double-render the
  // head already on screen before we backgrounded). The trailing prose step is
  // the CURRENT answer tail, which the live view renders as the streaming reply
  // below the block, not as a work step — route it to the stream. Staleness is
  // judged by turn identity (`startedMs` already seen END), never by cursor
  // arithmetic; a stale bundle leaves the transcript untouched.
  const applySubscribeState = useCallback(
    (
      turn: { active: boolean; started_at?: string },
      wireSteps: WireWorkStepFrame[],
      pendingApprovals: WireApprovalCard[],
    ) => {
      // The bundle reads and rewrites the tail block — queued live steps must
      // have landed first or the rebuild silently drops them.
      flushRowOps();
      const startedMs = turn.started_at ? Date.parse(turn.started_at) : null;
      if (startedMs !== null && endedTurnStarts.current.includes(startedMs)) return;
      if (!turn.active) {
        // No turn in flight at snapshot time — close any block we're holding
        // open (e.g. a restored mid-turn block whose turn actually finished).
        setTurnActive(false);
        closeWork();
        return;
      }
      setTurnActive(true);
      // The bundle REPLACES the block's steps, so the awaiting badge has to be
      // re-derived here too — the `approval_requested` frame that set it may
      // predate this connection (the prompt outlived a reconnect), and the
      // rebuilt steps carry no memory of it.
      const awaitingByCall = new Map(
        pendingApprovals
          .filter((c) => c.tool_call_id)
          .map((c) => [c.tool_call_id as string, c.call_id]),
      );
      const steps = wireSteps.map(wireStepToWork).map((s) =>
        s.kind === "tool" && awaitingByCall.has(s.callId)
          ? { ...s, awaitingApproval: awaitingByCall.get(s.callId) }
          : s,
      );
      const answer = bundleAnswer(steps);
      const workSteps = answer.kind === "recovered" ? steps.slice(0, -1) : steps;
      // Drive the live reply to the recovered answer tail (or clear it) in one
      // shot, batched with the block replace below — so the reply grows in place
      // rather than blanking for a frame. `unknown` leaves it ALONE: a bundle
      // with no answer text in it is no evidence that the reply on screen is
      // stale, and clearing there deletes a paragraph mid-read.
      if (answer.kind === "recovered") setStreamingText(answer.text);
      else if (answer.kind === "superseded") setStreamingText("");
      setMessages((rows) => {
        // The tail is read PAST any trailing notice run, exactly as `openWorkIn`
        // reads it: a terminal notice keeps its own row, and asking `rows[len-1]`
        // instead would see the notice, miss both the block to rebuild and the
        // answer the guard below turns on, and open a second card. It stops at an
        // ANSWER, though — reaching past one would hand this bundle a settled
        // turn's card to rewrite, which is a worse bug than the one it fixes.
        const at = lastBeforeNotices(rows);
        const tail = at >= 0 ? rows[at] : undefined;
        const openBlock = tail && tail.role === "work" ? tail : undefined;
        if (workSteps.length === 0) {
          // Answer-only turn: no block, the streamed reply stands alone; drop a
          // stale empty/restored block if it's the tail.
          return openBlock && openBlock.steps.length === 0
            ? [...rows.slice(0, at), ...rows.slice(at + 1)]
            : rows;
        }
        // A stale finalization-window bundle: this turn's answer already landed
        // here (the tail is the committed reply) but the gateway still reports
        // `turn.active` — its `active_turn_started_at` lingers through the job's
        // post-answer finalization — and ships a rolling in-flight work window.
        // Do NOT resurrect the ended turn's work as a second block under the
        // reply (the [work][reply][work] split). A genuine next turn opens its
        // block from the live turn_state / reasoning / tool frames that follow,
        // not from this snapshot.
        if (!openBlock && tail && tail.role === "assistant") return rows;
        // Re-open a block a prior restore froze (relaunch mid-turn) and replace
        // its steps; otherwise open a fresh one after the turn's user message.
        // Anchor `startedAt` to the server turn start (`startedMs`) when the
        // block has none (restore strips it) so the live ticker reads real
        // elapsed, not `now − localReopen`.
        const rebuilt: WorkRow = openBlock
          ? { ...openBlock, steps: workSteps, active: true, startedAt: openBlock.startedAt ?? startedMs ?? Date.now(), elapsedMs: undefined }
          : { id: uid(), role: "work", steps: workSteps, active: true, startedAt: startedMs ?? Date.now() };
        // `rebuilt` is THE in-flight block — freeze any other still-active block
        // above it so re-opening one never leaves two live "Working" cards.
        return openBlock
          ? [...freezeActiveWork(rows.slice(0, at)), rebuilt, ...rows.slice(at + 1)]
          : [...freezeActiveWork(rows), rebuilt];
      });
    },
    [closeWork, setStreamingText, flushRowOps],
  );

  // Fire a backward-history (scroll-up) request through native. The API result
  // is pushed later as a local `history_page` frame; the current epoch tags it
  // against late delivery across a reconnect. One at a time — returns `false`
  // if a request is already in flight (the caller then unwinds its own guards).
  const requestHistory = useCallback((beforeOrdinal: number | null): boolean => {
    if (relayHistory.current) return false;
    relayHistory.current = { epoch: connEpochRef.current };
    try {
      fetchHistory(beforeOrdinal, HISTORY_PAGE_LIMIT);
      return true;
    } catch (e) {
      relayHistory.current = null;
      throw e;
    }
  }, []);

  // Prepend an older page above the current top (scroll-up paging), preserving
  // the viewport via `prependAnchor` (read by the layout effect after the DOM
  // updates). Paged rows are strictly older than the current oldest, so they
  // can't overlap — the id-set filter is just a safety net. Re-seeds `sentIds`
  // so a later live echo of an own message doesn't double-render.
  const prependOlder = useCallback((older: Row[], newOldest: number | null, more: boolean) => {
    flushRowOps();
    const anchorEl = scrollEl();
    if (older.length > 0 && anchorEl) {
      prependAnchor.current = {
        prevScrollHeight: anchorEl.scrollHeight,
        prevScrollTop: anchorEl.scrollTop,
      };
    }
    for (const m of older) {
      if (m.role === "user") sentIds.current.add(m.id);
      const ordinal = ordinalFromMessageId(m.id);
      if (ordinal !== null) renderedOrdinals.current.add(ordinal);
    }
    setMessages((m) => {
      const seen = new Set(m.map((x) => x.id));
      const fresh = older.filter((x) => !seen.has(x.id));
      // Fold at the seam: a turn longer than a page has a work block on BOTH
      // sides of it (each page folds its own half), and they meet here with no
      // message row between — one turn must stay one card. But NOT across a
      // compaction boundary (`compactionPointsRef`): a mid-turn compaction's
      // halves are distinct turns, kept apart so the divider lands between them.
      return foldAdjacentWork([...fresh, ...m], compactionPointsRef.current);
    });
    // Only advance the cursor on a non-empty page; an empty page leaves it put.
    if (newOldest !== null) oldestOrdinal.current = newOldest;
    setHasMoreOlder(more);
    hasMoreOlderRef.current = more;
  }, [flushRowOps]);

  // Fold the mirror's withheld older rows into the thread (see
  // `splitForFirstPaint`), restoring the paging state they were held back from.
  // Idempotent and one-shot: the reservoir is cleared before the prepend, so a
  // drain racing the frame effect below can't double-render it.
  //
  // Goes through `prependOlder` rather than a bare `setMessages` so the head
  // arrives under exactly the rules a scroll-up page does — viewport anchored by
  // `prependAnchor`, `sentIds`/`renderedOrdinals` re-seeded, seam folded (a
  // no-op on an already-sanitized split, see `splitForFirstPaint`).
  const drainDeferredHead = useCallback(() => {
    const head = deferredHead.current;
    if (head.length === 0) return;
    deferredHead.current = [];
    prependOlder(head, restored?.oldestOrdinal ?? null, restored?.hasMoreOlder ?? false);
    // `prependOlder` leaves the floor PUT on a null — its "an empty page changes
    // nothing" rule. Here null is the mirror's real answer ("no durable floor to
    // page from"), and the tail's own oldest — seeded only to describe the half
    // that was rendered — must not outlive the half it described.
    oldestOrdinal.current = restored?.oldestOrdinal ?? null;
  }, [prependOlder, restored]);

  // One frame after the first paint. `useEffect` alone runs before the browser
  // has painted the commit, which would put the whole thread back in the first
  // frame and undo the split; the nested rAF lands after it.
  useEffect(() => {
    if (deferredHead.current.length === 0) return;
    let inner = 0;
    const outer = requestAnimationFrame(() => {
      inner = requestAnimationFrame(drainDeferredHead);
    });
    return () => {
      cancelAnimationFrame(outer);
      cancelAnimationFrame(inner);
    };
  }, [drainDeferredHead]);

  // Recover from a gateway `Frame::Reset` (catch-up gap over the replay cap, or
  // outbound back-pressure). Left unhandled this *loops*: the stale pre-gap
  // cursor goes back out on the next reconnect and overflows again. One
  // native `fetchHistory` rebuilds the thread and reseeds the cursors — no
  // Load the next older page (scroll-up): fire a native fetchHistory whose
  // `history_page` reply prepends in the frame switch (and clears the guards
  // there). `pagingRef` gates re-entry.
  const loadOlder = useCallback(() => {
    if (pagingRef.current || !hasMoreOlder) return;
    // Rows we already hold beat a round trip — and this is the safety net for a
    // reservoir the post-paint frame never drained (rAF is throttled while the
    // webview is hidden), which would otherwise re-fetch what is on disk and
    // fail outright offline.
    if (deferredHead.current.length > 0) {
      drainDeferredHead();
      return;
    }
    const before = oldestOrdinal.current;
    if (before === null) return; // no cursor — can't page older
    pagingRef.current = true;
    setLoadingOlder(true);
    try {
      const fired = requestHistory(before);
      // If a request was already in flight, unwind — the `history_page` handler
      // clears the guards only for the request it actually serves.
      if (!fired) {
        pagingRef.current = false;
        setLoadingOlder(false);
      }
    } catch (e) {
      pagingRef.current = false;
      setLoadingOlder(false);
      log("warn", `history page failed: ${String(e)}`);
      appendNotice(t("chat.recoverFailed", { error: String(e) }));
    }
  }, [hasMoreOlder, requestHistory, appendNotice, drainDeferredHead, t]);

  // The one forward-recovery pull (docs/sync-protocol.md "The one client
  // algorithm"): session open, reconnect, gap nudge and the safety tick all
  // land here. Posts `syncSince` to native, which fetches
  // `GET …/sync?since_ordinal=<since>&limit=…` over the active leg and pushes
  // the result back as a local `sync_page` frame. `null` → baseline REPLACE (no
  // cursor, or a thread the cursor doesn't cover); a rebased response also
  // REPLACEs. `syncInFlight` coalesces a burst of triggers to one pull (cleared
  // by the reply).
  const runSync = useCallback(() => {
    if (syncInFlight.current) return;
    const cursor = cursorRef.current.cursor;
    const since = syncSince(cursor, messagesRef.current);
    // WHY this baseline is going out, which the reply cannot tell us — the frame
    // carries only `since_ordinal: null`, and the two reasons want opposite
    // things from the rebuild. `syncSince` refusing a NON-null cursor means a
    // rendered row outran it: the thread may be out of ORDER (the scramble this
    // gate was written against), so the repair has to drop all of it. A cursor
    // that is simply `null` — a fresh install, or the deliberate
    // `restoredUntimedWork` demotion — says nothing against the rows on screen;
    // we just have no watermark, or want one block re-timed. Read once per pull:
    // `syncInFlight` keeps one request outstanding at a time.
    baselineIsRepair.current = since === null && cursor !== null;
    const limit = since === null ? SYNC_BASELINE_LIMIT : SYNC_MERGE_LIMIT;
    setSyncInFlight(true);
    try {
      postSyncRequest(since, limit);
    } catch (e) {
      setSyncInFlight(false);
      log("warn", `sync request failed: ${String(e)}`);
    }
  }, [setSyncInFlight]);

  const advanceCursorFromSync = useCallback((nextCursor: number | null, rebased: boolean) => {
    cursorRef.current = advanceFromSync(cursorRef.current, nextCursor, rebased);
  }, []);

  const advanceCursorFromLive = useCallback((ordinal: number) => {
    cursorRef.current = advanceFromLive(cursorRef.current, ordinal);
  }, []);

  // The transcript is on screen (native attaches the webview only for the open
  // session), so a cursor advance means the viewer has read up to it — tell
  // native to advance the server read cursor, deduped so it fires only when the
  // cursor actually moved forward.
  const markReadIfAdvanced = useCallback(() => {
    const cursor = cursorRef.current.cursor;
    if (cursor === null || cursor <= lastMarkedRead.current) return;
    lastMarkedRead.current = cursor;
    postMarkRead(cursor);
  }, []);

  // Apply one `sync_page` frame. REPLACE (rebased, or baseline `since === null`)
  // swaps the durable thread wholesale under the REPLACE-overlay rule
  // (`applySyncReplace`), while a difference merge files the rows above the
  // cursor — appending, or placing at its ordinal one that landed late
  // (`mergeSyncPage`) — and reconciles an optimistic send against its persisted
  // row by `platform_msg_id`. Rows arrive ascending; each carries a stable id.
  const applySyncPage = useCallback(
    (frame: Extract<WireFrame, { kind: "sync_page" }>) => {
      // Queued work-frame ops target the pre-page tail; land them before the
      // page merges or REPLACEs so they aren't applied to rebuilt rows.
      flushRowOps();
      setSyncInFlight(false);
      const replace = frame.rebased || frame.since_ordinal === null;
      const pageRows = frame.rows
        .map(transcriptItemToRow)
        .filter((r): r is Row => r !== null);
      // `applySyncReplace` already refuses the row swap for an empty page over
      // a non-empty thread (see its comment) — but the swap is not all a
      // REPLACE does. Left to run, this branch would also null the paging
      // floor, drop a not-yet-drained mirror head, clear the compaction
      // dividers and hide the load-older affordance, all off a page that
      // described nothing. Provably no-ops today (an empty page implies zero
      // durable rows), so this is the same statement at the frame level: a
      // REPLACE carrying no rows against a thread that has some is stale in
      // its entirety, not just row-wise. `messagesRef` can lag a same-batch
      // live append, so this fails OPEN — the applySyncReplace guard still
      // protects the rows.
      if (replace && pageRows.length === 0 && messagesRef.current.length > 0) {
        advanceCursorFromSync(frame.next_cursor, frame.rebased);
        markReadIfAdvanced();
        return;
      }
      // Every sync carries the authoritative boundary set (empty ⇒ never
      // compacted), so a warm re-entry's difference sync refreshes the divider
      // just like a baseline REPLACE does.
      setCompactionPoints(frame.compaction_points ?? []);
      // The prefix invariant `mergeSyncPage` merges under is checked when the
      // request is POSTED (`runSync`), and the thread grows during the round
      // trip — so a difference can land on a thread it is no longer a prefix
      // of. `mergeSyncPage` PLACES every page row carrying an ordinal, so that
      // alone is not a reason to refuse a page. This is the backstop for the
      // rows placement genuinely cannot file: an `n<seq>` notice, whose seq is a
      // sequence number and not an ordinal.
      //
      // BOTH conditions, and the narrowness is the point: keying on the
      // overrun alone discards pages that merge perfectly — one live reply
      // landing mid-round-trip is enough to trip it — and each discard costs a
      // round trip plus a REPLACE.
      //
      // Re-running the sync is the whole response: the cursor is NOT demoted to
      // `null`. `runSync` re-derives `syncSince` from the CURRENT thread, so the
      // ordinary case posts a fresh difference from the cursor the live rows
      // already advanced, and only a genuinely uncovered thread falls through to
      // a baseline. Minting `{cursor: null}` here would force a REPLACE onto a
      // warm thread — dropping any live row the page had not yet caught, which
      // a difference could then never return (it selects strictly `>`). The
      // discarded page's `next_cursor` is not adopted either: it was not applied.
      //
      // Reading `messagesRef` can lag a live `setMessages` in the same batch, so
      // the check fails OPEN — falling through to a merge that places what it
      // can, which is the behaviour without this guard, not a worse one.
      const unplaceable = pageRows.some((r) => rowCoverageOrdinal(r) === null);
      if (
        !replace &&
        unplaceable &&
        frame.since_ordinal !== null &&
        syncSince(frame.since_ordinal, messagesRef.current) === null
      ) {
        log("warn", `stale difference page carries an unplaceable row (since=${frame.since_ordinal}); re-syncing`);
        runSync();
        return;
      }
      // Reseed the redelivery-dedup sets from the page (idempotent Set adds),
      // and release every send the page proves durable — the ONLY thing that
      // ever retires an id from the REPLACE-overlay's kept set, and the same
      // proof native takes off this frame to release its outbox entry
      // (`reconcileOutboxAfterSync`). Both branches: a difference confirms a
      // send just as well as a baseline does.
      for (const item of frame.rows) {
        if (item.kind === "message") {
          if (typeof item.ordinal === "number") renderedOrdinals.current.add(item.ordinal);
          if (item.platform_msg_id) {
            sentIds.current.add(item.platform_msg_id);
            unconfirmedSends.current.delete(item.platform_msg_id);
          }
        }
      }
      if (replace) {
        // A page can hold BOTH halves of a turn it is wide enough to span, or
        // one half beside a turn it cut — fold before anything else reads them,
        // but never across this frame's compaction boundaries.
        const folded = foldAdjacentWork(
          turnActiveRef.current ? dropInFlightAnswerStep(pageRows) : pageRows,
          frame.compaction_points ?? [],
        );
        // Only a REPAIR (`runSync`: a rendered row outran a non-null cursor, so
        // the thread may be out of order) throws the loaded history away. The
        // other two REPLACEs do not put those rows in doubt:
        //
        // - a REBASE says only that the difference outran the server's limit or
        //   its scan bound and here is the newest page instead — NOT that
        //   ordinals were rewritten (docs/sync-protocol.md "Gap > limit →
        //   rebase");
        // - a cursor-less BASELINE is a fresh install (no older rows to keep) or
        //   the deliberate `restoredUntimedWork` demotion, which asks the gateway
        //   to re-time ONE block and says nothing about the rest.
        //
        // Dropping them there is what makes a REPLACE cost a reader the history
        // they scrolled up for plus the round trips that fetched it.
        //
        // `oldestOrdinal` is by construction the oldest durable ordinal actually
        // RENDERED (`prependOlder` sets it from the page it just folded in; the
        // mirror's split seeds it from the half it painted), so a floor strictly
        // below the page's is the same statement as "there are rows above it".
        const repair = frame.since_ordinal === null && baselineIsRepair.current;
        const pageFloor = frame.oldest_ordinal;
        const keepFloor =
          !repair &&
          pageFloor !== null &&
          oldestOrdinal.current !== null &&
          oldestOrdinal.current < pageFloor
            ? pageFloor
            : null;
        // Read the reader's place off the CURRENT DOM, before the swap is
        // scheduled. A reader already at the newest edge is snapped back to it
        // (the pin owns that). One parked up in history is NOT: the rebuilt page
        // reuses every surviving row's key, so the row they were reading is
        // still on screen and the anchor effect puts it back under the same
        // pixel — and only if the rebuild dropped it do they land at the bottom.
        if (!followRef.current) replaceAnchor.current = captureRowAnchor();
        setMessages((prev) => {
          const rebuilt = applySyncReplace(prev, folded, unconfirmedSends.current);
          if (keepFloor === null) return rebuilt;
          const head = rowsAboveFloor(prev, keepFloor, new Set(rebuilt.map((r) => r.id)));
          if (head.length === 0) return rebuilt;
          // The seam is the same one scroll-up paging makes: a turn wider than
          // the page has a work block on both sides of it and must stay one card.
          //
          // With one difference the fold cannot see on its own. A kept head can
          // only END on a work block when the row that CLOSED that turn — its
          // answer bubble, or the notice that severed it — fell at or below the
          // page's floor and so went to the page. The page then re-cut the same
          // turn at its START, and `flush` flags a block cut at its start whole
          // (`turn_complete: true`) exactly as it flags a real turn end, because
          // the accumulator only ever learns about its END. The head's copy
          // inherited that `true` (directly, or through `joinWorkHalves`), so
          // `sameContinuingTurn` refuses and one turn renders as two cards — and
          // it is STICKY: the pair persists into the mirror, and
          // `sanitizeRestoredRows` will not heal a head that says complete.
          // Restate the head half as what it now is, a block whose turn
          // continues below, and let the ordinary guards adjudicate:
          // `crossesCompaction` still refuses across a watermark, and `foldWork`
          // takes `joinWorkHalves` so the card spans the real turn.
          const last = head[head.length - 1];
          const seam =
            last.role === "work" && rowOrdinal(last.id) !== null && rebuilt[0]?.role === "work"
              ? [...head.slice(0, -1), { ...last, turnComplete: false }]
              : head;
          return foldAdjacentWork([...seam, ...rebuilt], frame.compaction_points ?? []);
        });
        if (keepFloor === null) {
          // The page IS the thread now, and it brings its own paging window. Rows
          // still withheld from the first paint describe the thread this REPLACE
          // just rebased away — prepending them a frame later would weld a stale
          // head onto it, under a floor that no longer refers to them.
          deferredHead.current = [];
          oldestOrdinal.current = frame.oldest_ordinal;
          setHasMoreOlder(frame.has_more_older);
          hasMoreOlderRef.current = frame.has_more_older;
        }
        if (!turnActiveRef.current) clearStreaming();
      } else {
        setMessages((prev) => mergeSyncPage(prev, pageRows, frame.compaction_points ?? []));
        if (pageRows.some((r) => r.role === "assistant")) clearStreaming();
      }
      advanceCursorFromSync(frame.next_cursor, frame.rebased);
      markReadIfAdvanced();
    },
    [advanceCursorFromSync, clearStreaming, markReadIfAdvanced, setSyncInFlight, runSync, flushRowOps],
  );

  const handleFrame = (frameJson: string) => {
    let frame: WireFrame;
    try {
      frame = JSON.parse(frameJson) as WireFrame;
    } catch (e) {
      log("warn", `unparseable frame: ${String(e)}`);
      return;
    }
    lastFrameAt.current = Date.now();
    switch (frame.kind) {
      case "message": {
        const ordinal = typeof frame.ordinal === "number" ? frame.ordinal : null;
        // Advance the cursor from the ordinal-stamped final reply (max-wins,
        // frozen while rebase-dirty), then dedup below. A reply while the
        // transcript is on screen is read → advance the server read cursor.
        if (ordinal !== null) {
          advanceCursorFromLive(ordinal);
          markReadIfAdvanced();
        }
        const role = frame.role === "user" ? "user" : "assistant";
        if (role === "user" && frame.platform_msg_id && sentIds.current.has(frame.platform_msg_id)) {
          if (ordinal !== null) renderedOrdinals.current.add(ordinal);
          markSent(frame.platform_msg_id, ordinal); // server confirmed the send — stop the spinner
          return; // our own message / already rendered
        }
        // The native stop BUTTON issues `/stop` as an ordinary chat send; the
        // channel echoes every inbound message to subscribers BEFORE the agent
        // Router intercepts `/stop` out-of-band, so the echo arrives here. Native
        // mints no optimistic bubble for it (it isn't in `sentIds`), and the
        // durable record folds `/stop` into the cancelled work block — never a
        // message row — so left alone the echo renders a stray "/stop" bubble
        // that lingers. Drop it (a typed `/stop` already returned above by id).
        if (role === "user" && isStopCommand(frame.content)) return;
        if (ordinal !== null && renderedOrdinals.current.has(ordinal)) {
          if (role === "user" && frame.platform_msg_id) sentIds.current.add(frame.platform_msg_id);
          return;
        }
        if (role === "user" && frame.platform_msg_id) {
          sentIds.current.add(frame.platform_msg_id);
          // Reaching this append for an ordinal-less user frame means the echo
          // beat `userSent` here — or `userSent` was lost (a retarget burst
          // consumed by the outgoing tree). Either way the row this appends is
          // the send's ONLY rendering, and like the optimistic bubble it is
          // ordinal-less and pre-persist — so it needs the same REPLACE-overlay
          // protection, or the send-time baseline (routinely empty for a first
          // send) deletes it. The page carrying the id retires it, as always.
          // A second paired device's echo enrols here too — knowingly: its row
          // kept (and possibly re-filed below a narrow page) beats deleted,
          // and the set is per-tree, never persisted, so the exposure ends at
          // unmount.
          if (ordinal === null) unconfirmedSends.current.add(frame.platform_msg_id);
        }
        if (role === "assistant") {
          // The terminal message is authoritative: it replaces the streamed
          // text and ends the turn's work block. It is also a turn-END signal
          // (record its identity), and — if the cursor is rebase-dirty — the
          // trigger for the follow-up sync that closes the dirty window.
          closeWork();
          clearStreaming();
          setAwaitingReply(false);
          // The turn is over the moment its answer commits. Waiting for the
          // server's `turn_state{active:false}` to say so leaves the latch — the
          // one signal here with no self-healing cap — stuck true whenever that
          // frame is lost (an offscreen buffer overflow drops it, and no
          // `sync_page` carries turn state to rebuild it), and a stuck latch
          // paints the `work-pending` box under the settled reply forever. The
          // server's own frame follows and is idempotent.
          setTurnActive(false);
          recordEndedTurn(activeTurnStart.current);
          activeTurnStart.current = null;
          if (cursorRef.current.rebaseDirty) runSync();
        }
        if (ordinal !== null) renderedOrdinals.current.add(ordinal);
        // Land queued steps (and the assistant branch's closeWork) first, so
        // the settled row appends BELOW the finished block, in one commit with
        // the cleared streaming reply.
        flushRowOps();
        setMessages((m) => [
          ...m,
          {
            id: frame.platform_msg_id || (ordinal !== null ? `m${ordinal}` : uid()),
            role,
            ordinal: ordinal ?? undefined,
            content: frame.content,
            attachments: frame.attachments,
            // The wire `Message` frame carries no time field, so arrival is the
            // best clock we have; a later reconstruction overwrites it with the
            // server's `created_at`.
            createdAt: new Date().toISOString(),
          },
        ]);
        break;
      }
      case "answer_delta":
        appendStreaming(frame.text);
        break;
      case "reasoning":
        // Thinking chunk: fold any streamed answer back into the block, then
        // merge into the trailing reasoning step so a streamed trace reads as
        // one paragraph.
        foldStreamingIntoProse();
        withOpenWork((w) => {
          const steps = [...w.steps];
          const last = steps[steps.length - 1];
          if (last && last.kind === "reasoning") {
            steps[steps.length - 1] = { ...last, text: last.text + frame.text };
          } else {
            steps.push({ kind: "reasoning", text: frame.text, at: Date.now() });
          }
          return { ...w, steps };
        });
        break;
      case "tool_started":
        foldStreamingIntoProse();
        pushWorkStep({
          kind: "tool",
          callId: frame.call_id,
          tool: frame.tool,
          label: frame.label || frame.tool,
          status: "running",
        });
        break;
      case "tool_completed":
        foldStreamingIntoProse();
        withOpenWork((w) => {
          const steps = [...w.steps];
          for (let i = steps.length - 1; i >= 0; i -= 1) {
            const s = steps[i];
            if (s.kind === "tool" && s.callId === frame.call_id && s.status === "running") {
              steps[i] = {
                ...s,
                status: frame.status,
                summary: frame.summary || undefined,
                // The call is done, so nothing is waiting on the user any more —
                // even when the gate TIMED OUT (no `approval_resolved` is
                // broadcast for that; the completion is the only signal).
                awaitingApproval: undefined,
                approval: frame.approval || s.approval,
              };
              return { ...w, steps };
            }
          }
          // No matching start (e.g. it opened before this page loaded) —
          // record the completion on its own.
          steps.push({
            kind: "tool",
            callId: frame.call_id,
            label: frame.summary || frame.call_id,
            status: frame.status,
            approval: frame.approval || undefined,
          });
          return { ...w, steps };
        });
        break;
      case "approval_requested":
        // A tool call is blocked on the user. The prompt itself is NATIVE (the
        // composer's card); the transcript only badges the step that is waiting,
        // so the user can see WHICH call the card is asking about.
        if (frame.tool_call_id) markStepAwaitingApproval(frame.tool_call_id, frame.call_id);
        break;
      case "approval_resolved":
        // Answered (here, or on another client). The decision lands on the step
        // right away rather than waiting for the call to finish — an approved
        // call keeps running, sometimes for minutes.
        resolveStepApproval(frame.call_id, frame.decision);
        break;
      case "turn_state":
        setTurnActive(frame.active);
        if (frame.active) {
          activeTurnStart.current = frame.started_at ? Date.parse(frame.started_at) : null;
        } else {
          // Turn ended — end the optimistic run-state window (its work block /
          // streaming, if any, close alongside). Kept on the ACTIVE branch so a
          // slow first token doesn't briefly drop the stop button.
          setAwaitingReply(false);
          closeWork();
          recordEndedTurn(activeTurnStart.current);
          activeTurnStart.current = null;
          // A turn ending on a rebase-dirty cursor triggers the follow-up sync
          // that closes the dirty window (mirrors the final-Message path).
          if (cursorRef.current.rebaseDirty) runSync();
        }
        break;
      case "subscribe_state":
        // The one atomic state-plane bundle. iOS renders the turn/work halves
        // here and the approval half natively (the composer card); the
        // transcript takes only each pending prompt's `tool_call_id`, to restore
        // the awaiting badge on the step it blocks. Staleness is judged by turn
        // identity.
        if (frame.turn.active && frame.turn.started_at) {
          activeTurnStart.current = Date.parse(frame.turn.started_at);
        }
        // Do NOT clear the optimistic window here: a `subscribe_state` arrives on
        // every (re)connect, and the send-then-connect path (a first message on a
        // fresh session) delivers `turn.active:false` in the gap AFTER our send
        // but BEFORE the turn starts — clearing here would drop the stop button
        // back to send until the first output (the "stop appears late" bug). A
        // real turn is reflected by applySubscribeState rebuilding the work block
        // / streaming reply; a genuinely idle window self-expires (see the
        // `awaitingReply` timeout) and is cleared by the turn's terminal frame.
        applySubscribeState(frame.turn, frame.work_steps ?? [], frame.pending_approvals ?? []);
        break;
      case "gap":
        // Server-declared loss on this connection — run the one forward-recovery
        // pull. (`session_id` scoping is native's concern; the webview holds one
        // session, so any gap means "sync me".)
        runSync();
        break;
      case "status":
        // A compaction blocks the turn on a full-transcript summarizer call, so
        // without a line here the thread just sits silent for however long that
        // takes. Folding it into the work block (rather than minting a row) also
        // keeps `workLive` true across the pause, which is what stops the
        // `awaitingReply` backstop from flipping the composer back to send
        // mid-turn.
        foldStreamingIntoProse();
        pushWorkStep({ kind: "status", text: compactionStatusText(t, frame.phase) });
        if (frame.phase === "compacted") {
          // The divider is derived from `compaction_points`, which no live frame
          // carries — without this pull it appears on the next cold open.
          runSync();
        }
        break;
      case "notice":
        if (frame.transient) {
          // Mid-turn progress narration belongs to the work block, not the log.
          foldStreamingIntoProse();
          pushWorkStep({ kind: "status", text: frame.text });
        } else if (isStopAckNotice(frame.text)) {
          // A `/stop` acknowledgement. Don't mint a client row for it: the
          // gateway persists this notice, and reconstruction renders it as the
          // compact "Stopped" indicator (`n<seq>` id). Minting a local `uid` row
          // too would double it on the next sync / a relaunch (two ids, same
          // event). Instead end the optimistic window, freeze any open work
          // block, and pull the durable indicator in via one sync.
          setAwaitingReply(false);
          enqueueRowOp((rows) => freezeActiveWork(rows));
          runSync();
        } else if (frame.mid_turn ?? true) {
          // A tool-authored aside — the server's fold-eligibility declaration.
          // Fold into the open block; the turn keeps running. An ABSENT flag
          // means an older gateway that predates it (the new gateway always
          // serializes `mid_turn`): fall back to this legacy fold-if-active
          // heuristic rather than severing, which would freeze the live block
          // on every tool aside and drop the stop affordance mid-turn.
          setAwaitingReply(false);
          foldMidTurnNotice(frame.level, frame.text);
        } else {
          // A terminal notice (a turn failure, a server rejection) means no
          // reply is coming — end the optimistic window so the stop button
          // can't strand, freeze the block, and keep the notice a visible row.
          setAwaitingReply(false);
          severTerminalNotice(frame.text, frame.durable_id ?? null);
        }
        break;
      case "sync_page":
        applySyncPage(frame);
        break;
      case "sync_failed":
        // The native chatFetchSync API call failed — unwind the in-flight
        // guard so the next trigger retries; the durable record is intact.
        setSyncInFlight(false);
        log("warn", `sync fetch failed: ${frame.error}`);
        break;
      case "history_page": {
        // Backward paging (scroll-up) only — the reset-rebuild REPLACE is gone
        // (baseline/rebase now ride `sync_page`). A page with no matching
        // in-flight request (`null`), or one tagged under a superseded
        // connection epoch (a dead leg), is stale — drop it.
        const pending = relayHistory.current;
        relayHistory.current = null;
        if (pending === null || pending.epoch !== connEpochRef.current) break;
        const rows = frame.rows.map(transcriptItemToRow).filter((r): r is Row => r !== null);
        prependOlder(rows, frame.oldest_ordinal ?? null, frame.has_more);
        pagingRef.current = false;
        setLoadingOlder(false);
        break;
      }
      case "history_failed": {
        // Native couldn't enqueue the paging request — unwind the guards the
        // fire site armed.
        const pending = relayHistory.current;
        relayHistory.current = null;
        if (pending === null || pending.epoch !== connEpochRef.current) break;
        pagingRef.current = false;
        setLoadingOlder(false);
        log("warn", `history fetch failed: ${frame.error}`);
        appendNotice(t("chat.recoverFailed", { error: frame.error }));
        break;
      }
      default:
        break; // task_list / ping etc. not surfaced in the transcript
    }
  };

  // Native sent a user message: append the optimistic bubble, seed echo-dedup,
  // snap follow to the newest edge (an own send always returns there).
  //
  // Also the outbox's re-seed path, which runs on every mount edge — so a
  // bubble this thread already holds returns before ANY of that. Not just the
  // append: `awaitingReply` below would raise the composer's stop button on a
  // send that failed days ago and is merely awaiting its red dot. A live send
  // mints a fresh uuid and can never take this exit.
  //
  // Registering the id sits ABOVE that exit on purpose. The re-seed's whole
  // point is that this call is the outbox telling us what it still owes, and
  // that claim is exactly as true when the row is already on screen — a restored
  // mirror rendering the bubble is not proof the gateway ever wrote it. The exit
  // suppresses the duplicate APPEND, not the bookkeeping; leave the add below it
  // and a restored unconfirmed send never enters the kept set, so the next
  // REPLACE deletes the very bubble this path exists to preserve.
  //
  // The ref guard is only the fast path. An echo and this callback can run in
  // one React batch, before `messagesRef` observes the echo's queued append, so
  // the state updater must repeat the identity check against the actual rows it
  // receives.
  const handleUserSent = (payload: UserSentPayload) => {
    // Queued work steps must land in the tail block BEFORE the bubble appends
    // below it — applied after, `openWorkIn` would see the bubble as the tail
    // and mint a second block.
    flushRowOps();
    unconfirmedSends.current.add(payload.msgId);
    if (holdsUserSend(messagesRef.current, payload.msgId)) return;
    sentIds.current.add(payload.msgId);
    followRef.current = true;
    // Optimistically enter the "awaiting reply" window so the composer's stop
    // button appears immediately, before the first `turn_state` lands.
    setAwaitingReply(true);
    setMessages((rows) => {
      if (holdsUserSend(rows, payload.msgId)) return rows;
      return [
        ...rows,
        {
          id: payload.msgId,
          role: "user",
          content: payload.text,
          attachments: payload.attachments.length > 0 ? payload.attachments : undefined,
          sendState: "sending",
          createdAt: new Date().toISOString(),
        },
      ];
    });
  };

  // Native chrome (composer + ridden keyboard) covering the webview's bottom.
  // Arrives once per keyboard/composer settle, at the ANIMATION START (SwiftUI
  // geometry callbacks jump to the target value); the CSS transition on
  // padding-bottom (.inset-animated) then tracks the keyboard's slide. While
  // the padding animates, scrollHeight moves every frame — re-pin through the
  // transition so the newest edge rides the keyboard instead of snapping.
  // Fully imperative: a setState here would re-render the thread per event.
  const insetAnimated = useRef(false);
  const pinDeadline = useRef(0);
  const handleBottomInset = (px: number) => {
    document.documentElement.style.setProperty("--thread-bottom-inset", `${px}px`);
    const box = logRef.current; // carries the .inset-animated padding transition
    const el = scrollEl(); // the actual (document) scroller
    if (!box || !el) return;
    if (!insetAnimated.current) {
      // First (launch) inset: apply without sliding, then arm the transition.
      insetAnimated.current = true;
      if (followRef.current) el.scrollTop = el.scrollHeight;
      requestAnimationFrame(() => box.classList.add("inset-animated"));
      return;
    }
    const already = pinDeadline.current > performance.now();
    pinDeadline.current = performance.now() + 350;
    if (already) return; // a pin loop is running; it picked up the new deadline
    const step = () => {
      if (followRef.current && !userTouchingRef.current) el.scrollTop = el.scrollHeight;
      if (performance.now() < pinDeadline.current) requestAnimationFrame(step);
    };
    requestAnimationFrame(step);
  };

  // Native (re)connected. Any paging request in flight belongs to the old
  // epoch and is abandoned (its late `history_page` is dropped by the epoch
  // tag) — clear the guards so a future fetch isn't blocked / the spinner isn't
  // stuck. Then run the one forward-recovery pull: this is the reconnect edge
  // of the sync loop (the server replays nothing on Subscribe).
  const handleConnEpoch = (epoch: number) => {
    connEpochRef.current = epoch;
    relayHistory.current = null;
    pagingRef.current = false;
    setLoadingOlder(false);
    setConnEpoch(epoch);
    // A sync issued on the leg that just died will never be answered — release
    // its coalescing guard, or the pull below is swallowed by a reply that isn't
    // coming (see `handleSyncRequested`).
    setSyncInFlight(false);
    runSync();
  };

  // Native asked for a sync run (the offscreen-buffer-overflow re-attach, or any
  // other native-side "go sync" edge). Same one forward-recovery pull — but
  // release the in-flight guard FIRST. Native only asks because it dropped what
  // it was carrying, and the `sync_page` it dropped may be the very reply this
  // guard is waiting on: the page rides the ordinary buffered frame path, so an
  // overflow while the user sat on the chat list discards it (`ChatStore
  // .overflowBufferedFrames`) and no `sync_page`/`sync_failed` ever lands. The
  // guard would then stay set for the life of the React tree — which a
  // same-session re-entry does NOT remount — dead-ending every later sync edge
  // (mount, connEpoch, gap, the 3-minute tick) and stranding the thread empty
  // with `.thread-loading` promising a page that will never arrive.
  const handleSyncRequested = useCallback(() => {
    setSyncInFlight(false);
    runSync();
  }, [runSync, setSyncInFlight]);

  // The sheet's "load earlier" row runs the transcript's own backward paging;
  // the prepend grows the outline, which re-posts on its own.
  const handleOutlineLoadOlder = useCallback(() => {
    loadOlder();
  }, [loadOlder]);

  // Which of the user's messages the reader is parked on, answered on demand —
  // a live scan would force a layout on every scroll tick. The topmost row still
  // under the header veil wins, so the sheet opens on what is being read.
  const handleOutlineHereRequested = useCallback(() => {
    const rows = document.querySelectorAll(".msg-group.user[data-row-id]");
    if (rows.length === 0) {
      postOutlineHere(null);
      return;
    }
    // The line is the row's OWN `scroll-margin-top` — the same declaration
    // `jumpToMessage` parks against. Measuring against `.chat-log`'s padding
    // instead puts the line 12px above where a jump lands, so the row the user
    // just jumped to would never be the one reported back as "here".
    const margin = Number.parseFloat(getComputedStyle(rows[0]).scrollMarginTop);
    const line = Number.isNaN(margin) ? 0 : margin;
    let here: string | null = null;
    rows.forEach((el) => {
      if (el.getBoundingClientRect().top <= line + 1) here = el.getAttribute("data-row-id");
    });
    postOutlineHere(here);
  }, []);

  // The one client loop's OPEN edge: run sync on mount (a resident re-entry —
  // hydration-matrix cell E in the retired scheme — that fires no connEpoch
  // edge still hydrates here). Safe to double with the connEpoch edge:
  // `syncInFlight` coalesces, and an empty difference is a no-op.
  useEffect(() => {
    runSync();
  }, [runSync]);

  // Safety-net pull: run sync every 3 minutes for the foreground transcript,
  // skipped when any frame arrived within the interval. Backstops a lost `gap`
  // nudge and suspended-app windows.
  useEffect(() => {
    const id = window.setInterval(() => {
      if (Date.now() - lastFrameAt.current < SAFETY_TICK_MS) return;
      runSync();
    }, SAFETY_TICK_MS);
    return () => window.clearInterval(id);
  }, [runSync]);

  // The document (main frame) is the scroller, so follow/jump/paging state is
  // driven by the window scroll event, not a div's onScroll. Passive; fires on
  // user scrolls and on the programmatic pins alike (idempotent there).
  useEffect(() => {
    const onScroll = () => {
      const el = scrollEl();
      if (!el) return;
      const follow = el.scrollHeight - el.scrollTop - el.clientHeight <= FOLLOW_BOTTOM_THRESHOLD_PX;
      if (glidingRef.current) {
        // Mid-glide positions still read "off the edge" — hold the state
        // jumpToLatest pinned until the glide lands in the follow band.
        if (follow) {
          glidingRef.current = false;
          clearTimeout(glideTimer.current);
        }
      } else {
        followRef.current = follow;
        setShowJump(!follow);
      }
      if (el.scrollTop <= SCROLL_TOP_THRESHOLD_PX) loadOlder();
    };
    window.addEventListener("scroll", onScroll, { passive: true });
    return () => window.removeEventListener("scroll", onScroll);
  }, [loadOlder]);

  // Bridge events call the LATEST handlers through this ref (assigned each
  // render), so the subscription registers once without re-subscribing per
  // render.
  const handlersRef = useRef({
    handleFrame,
    handleUserSent,
    markFailed,
    markSent,
    handleConnEpoch,
    handleBottomInset,
    jumpToLatest,
    jumpToMessage,
    jumpToOrdinal,
    handleOutlineLoadOlder,
    handleOutlineHereRequested,
    handleSyncRequested,
  });
  handlersRef.current = {
    handleFrame,
    handleUserSent,
    markFailed,
    markSent,
    handleConnEpoch,
    handleBottomInset,
    jumpToLatest,
    jumpToMessage,
    jumpToOrdinal,
    handleOutlineLoadOlder,
    handleOutlineHereRequested,
    handleSyncRequested,
  };
  useEffect(
    () =>
      subscribeTranscript({
        frame: (frameJson) => handlersRef.current.handleFrame(frameJson),
        connEpoch: (epoch) => handlersRef.current.handleConnEpoch(epoch),
        userSent: (payload) => handlersRef.current.handleUserSent(payload),
        sendFailed: (msgId) => handlersRef.current.markFailed(msgId),
        // Retire the membership AND stamp the durable row's ordinal — the one
        // mark nothing else can give this bubble: the echo never carries an
        // ordinal, and the proof native acted on is often a point lookup that
        // ships no page. Without the stamp, retiring the id leaves the row with
        // NO keep predicate, and the next REPLACE whose page lacks it deletes
        // the very bubble this call just proved durable. `markSent` files it
        // under the ceiling rule (and clears any lingering send chrome —
        // durability subsumes the echo).
        sendConfirmed: (msgId, ordinal) => {
          unconfirmedSends.current.delete(msgId);
          if (ordinal !== null) handlersRef.current.markSent(msgId, ordinal);
        },
        bottomInset: (px) => handlersRef.current.handleBottomInset(px),
        jumpToLatest: () => handlersRef.current.jumpToLatest(),
        jumpToMessage: (rowId) => handlersRef.current.jumpToMessage(rowId),
        jumpToOrdinal: (ordinal) => handlersRef.current.jumpToOrdinal(ordinal),
        outlineLoadOlder: () => handlersRef.current.handleOutlineLoadOlder(),
        outlineHereRequested: () => handlersRef.current.handleOutlineHereRequested(),
        syncRequested: () => handlersRef.current.handleSyncRequested(),
      }),
    [],
  );

  // Jump-to-latest: glide (not teleport) back to the newest edge, re-arming
  // following and hiding the button up front — onScroll holds both while the
  // glide flag is set. Landing normally settles via onScroll entering the
  // follow band; the cap timer settles a cancelled glide (see
  // GLIDE_SETTLE_CAP_MS).
  function jumpToLatest() {
    const el = scrollEl();
    if (!el) return;
    glidingRef.current = true;
    followRef.current = true;
    setShowJump(false);
    clearTimeout(glideTimer.current);
    glideTimer.current = setTimeout(() => {
      glidingRef.current = false;
      const logEl = scrollEl();
      if (!logEl) return;
      const follow =
        logEl.scrollHeight - logEl.scrollTop - logEl.clientHeight <= FOLLOW_BOTTOM_THRESHOLD_PX;
      followRef.current = follow;
      setShowJump(!follow);
    }, GLIDE_SETTLE_CAP_MS);
    el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
  }

  // A row of the native message-index sheet was tapped: park that user message
  // under the header veil (the clearance is `.msg-group.user`'s
  // `scroll-margin-top`, so nothing is computed here) and bloom the ring.
  // Imperative rather than an effect keyed on the target — the same row can be
  // asked for twice, and the second ask must scroll on the tap, not on a render
  // that identical state never triggers.
  /// A search hit the thread has not paged in yet: keep paging backward until
  /// the ordinal is covered, then jump. `pagesLeft` is the hard cap.
  ///
  /// The window MUST stay contiguous and tail-anchored — there is no
  /// `hasMoreNewer` and every live frame appends to the end, so a window that
  /// stopped short of the newest edge would weld the next reply onto an ancient
  /// row. Paging backward is the only way to reach an old row that keeps that
  /// invariant: it is exactly what the reader's own scroll-up does, just driven.
  function jumpToOrdinal(ordinal: number) {
    pendingJump.current = { ordinal, pagesLeft: JUMP_PAGE_BUDGET };
    advancePendingJump();
  }

  /// One step of the paging loop, re-entered from the `messages` effect below —
  /// NOT a `for` loop: `requestHistory` allows one request at a time and its
  /// reply lands asynchronously in the frame switch, so the only correct shape
  /// is "try, fire one page, be called again when it lands".
  function advancePendingJump() {
    const pending = pendingJump.current;
    if (pending === null) return;

    const target = messagesRef.current.find((r) => rowCoverageOrdinal(r) === pending.ordinal);
    if (target !== undefined) {
      pendingJump.current = null;
      jumpToMessage(target.id);
      return;
    }

    const floor = oldestOrdinal.current;
    // Below the floor: the rows are simply not loaded yet, so page for them.
    // `floor === null` means nothing durable is rendered at all (a cold open
    // whose first page has not landed) — hold, and the effect will re-enter.
    if (floor !== null && pending.ordinal >= floor) {
      // Inside the loaded window and still not found: the ordinal names a row
      // this view does not render at all. Paging can only ever load rows
      // FURTHER BACK, so no number of pages will produce it — stop now rather
      // than dragging the reader through the whole history to fail anyway.
      giveUpPendingJump();
      return;
    }
    if (!hasMoreOlderRef.current) {
      // The top of the thread, with the ordinal never covered.
      if (floor !== null) giveUpPendingJump();
      return;
    }
    if (pending.pagesLeft <= 0) {
      giveUpPendingJump();
      return;
    }
    // Decrement per REQUEST, not per re-entry: this is what bounds the loop
    // when a page comes back empty. `prependOlder` advances the floor only on a
    // non-empty page while `hasMoreOlder` can stay true, so "floor moved" is not
    // a termination condition anyone can rely on — the budget is.
    pending.pagesLeft -= 1;
    loadOlder();
  }

  function giveUpPendingJump() {
    pendingJump.current = null;
    appendNotice(t("chat.jumpNotFound"));
  }

  function jumpToMessage(rowId: string) {
    const node = document.querySelector(`[data-row-id="${CSS.escape(rowId)}"]`);
    if (node === null) {
      // The sheet offered a row this tree no longer holds — its list is a
      // debounce (or a rebase) behind. `resendOutline`, not `postOutline`: the
      // guard would drop this as a duplicate of what we believe we already sent,
      // which is exactly the belief that just proved wrong.
      resendOutline(outlinePostRef.current);
      return;
    }
    // Cancel an in-flight jump-to-latest glide, but never SET glidingRef: its
    // only self-clear is onScroll entering the bottom follow band, which an
    // upward jump never reaches — it would latch follow/showJump for the whole
    // GLIDE_SETTLE_CAP_MS.
    clearTimeout(glideTimer.current);
    glidingRef.current = false;
    // Synchronously, BEFORE any scroll write: the mount pin, the follow layout
    // effect, the ResizeObserver, the touchend catch-up and the keyboard rAF
    // loop all slam scrollTop to the bottom while this is true.
    followRef.current = false;
    setFlash((f) => ({ id: rowId, nonce: f.nonce + 1 }));
    node.scrollIntoView({ block: "start", behavior: "instant" });
    // `showJump` is deliberately not forced on: for a near-bottom target onScroll
    // correctly recomputes it to false, and the native button would appear only
    // to fade straight back out.
    //
    // The jump drags never-decoded images into the lazy band and they shove the
    // target as their bytes land (WKWebView has no scroll anchoring), so re-seat
    // once. The finger always wins. `followRef` is NOT re-asserted — for a
    // near-bottom target `true` is the right answer and onScroll owns it.
    clearTimeout(jumpSettleTimer.current);
    jumpSettleTimer.current = setTimeout(() => {
      if (userTouchingRef.current) return;
      const settled = document.querySelector(`[data-row-id="${CSS.escape(rowId)}"]`);
      settled?.scrollIntoView({ block: "start", behavior: "instant" });
    }, JUMP_SETTLE_MS);
  }

  // The pending-jump loop's re-entry point. Every way a row can arrive — the
  // first paint, a `history_page` prepend, a `sync_page` REPLACE — ends in a
  // `messages` change, so watching that covers them all without the frame
  // switch having to know the loop exists. A no-op whenever nothing is pending,
  // which is almost always.
  //
  // Deliberately NOT keyed on the request landing: a REPLACE can rebuild the
  // thread under the loop, and re-deriving from whatever is now rendered is the
  // only reading that survives that.
  useEffect(() => {
    if (pendingJump.current !== null) advancePendingJump();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [messages]);

  // The sheet is native, but only this tree knows which sends are in it (the
  // optimistic bubble exists here before any echo, `/stop` echoes are filtered
  // here, and the loaded window is this tree's). bridge.ts owns the debounce and
  // the identity guard, so every re-derive just posts.
  const outline = useMemo(() => outlineEntries(messages), [messages]);
  const outlinePost = useMemo<OutlinePost>(
    () => ({ entries: outline, hasMoreOlder, loadingOlder }),
    [outline, hasMoreOlder, loadingOlder],
  );
  // Read by `jumpToMessage`'s self-heal, which fires off a bridge event rather
  // than a render.
  const outlinePostRef = useRef(outlinePost);
  outlinePostRef.current = outlinePost;
  useEffect(() => {
    postOutline(outlinePost);
  }, [outlinePost]);

  // The header's `Subagents` entry, from the rows already on screen — no request,
  // and right offline. LATCHED for the session, which the `key={sessionId}` mount
  // scopes: a REPLACE rebuilds the thread from the newest page and backward paging
  // starts there, so the spawning turn leaves the loaded window routinely — the
  // child it minted does not leave with it.
  const spawnedRef = useRef(false);
  const hasSubagents = useMemo(() => {
    spawnedRef.current ||= hasSubagentSpawn(messages);
    return spawnedRef.current;
  }, [messages]);
  useEffect(() => {
    postSubagents(hasSubagents);
  }, [hasSubagents]);

  // The button itself is native (a liquid-glass circle above the composer) —
  // mirror the visibility over the bridge; taps come back via the
  // `jumpToLatest` transcript event above.
  useEffect(() => {
    postJumpVisible(showJump);
  }, [showJump]);

  // While the turn's work block is live it already signals activity; the bare
  // "Working" pending line only covers the gap before the first frame lands.
  // Read past a trailing notice run, as every other live-frame tail reader does:
  // a terminal notice landing beside a live block would otherwise read as "no
  // block live" and paint the pending box a second time below it. NOT past an
  // answer — `closeWork` freezes `rows[len-1]`, so a block this called live
  // across its own bubble would be one nothing can ever close, and `running`
  // would strand the composer on the stop button for the rest of the session.
  const lastRow = messages[lastBeforeNotices(messages)];
  const workLive = lastRow !== undefined && lastRow.role === "work" && lastRow.active;

  // Mirror the turn's run state to native so the composer's send button flips to
  // a stop affordance while a turn runs. Derived from SELF-CORRECTING signals
  // only — an active work block, a streaming reply, or the optimistic post-send
  // window — deliberately NOT the raw `turnActive` latch. That latch strands true
  // when its closing `turn_state{active:false}` is lost (an offscreen buffer
  // overflow drops it, and a `sync_page` carries no turn state to re-derive it),
  // which would freeze the composer on the stop button and block every send.
  // On mount this posts `false`, resetting a native store that carried a stale
  // run state across a session switch; the flushed/live frames re-raise it.
  const running = awaitingReply || workLive || streaming.length > 0;
  useEffect(() => {
    postRunState(running);
  }, [running]);

  // Hand the optimistic post-send window off to the real run signals the instant
  // the turn produces output (a work block or a streamed reply). Doing it here —
  // NOT in `applySyncPage` — is deliberate: a session-open / reconnect sync is
  // async, so its `sync_page` often lands just AFTER a send and would clear the
  // just-set window mid-flight, dropping the stop button back to send until the
  // first output (the "stop appears late" bug). `workLive`/`streaming` are also
  // what a buffer-overflow recovery sync clears, so once output has started the
  // window is no longer load-bearing and dropping it here can't strand it.
  useEffect(() => {
    if (workLive || streaming.length > 0) setAwaitingReply(false);
  }, [workLive, streaming]);

  // Race-free backstop: the optimistic window self-expires so a missed turn-end
  // (a disconnect that hid both the send's output and the turn's close) can't
  // strand the stop button. Deliberately not tied to any sync/subscribe frame —
  // those race a fresh send — and long enough that a live turn always clears it
  // first via output or its terminal frame.
  useEffect(() => {
    if (!awaitingReply) return;
    const id = window.setTimeout(() => setAwaitingReply(false), AWAITING_MAX_MS);
    return () => window.clearTimeout(id);
  }, [awaitingReply]);

  // Collapse adjacent "Stopped" indicators to one: the live indicator (a
  // client `uid`) and its durable notice row (`n<seq>`, re-delivered by a later
  // sync) are the same event and would otherwise stack two identical marks.
  // Memoized on `messages` so the per-rAF streaming re-renders — which change
  // only the `streaming` string — skip these O(thread) scans.
  const renderRows = useMemo(
    () =>
      messages.filter((m, i) => {
        if (m.role !== "notice" || !m.stopped) return true;
        const prev = messages[i - 1];
        return !(prev && prev.role === "notice" && prev.stopped);
      }),
    [messages],
  );
  const defaultExpandedWorkIds = useMemo(
    () => (expandUnansweredTail ? unansweredTailWorkIds(renderRows) : undefined),
    [expandUnansweredTail, renderRows],
  );
  // Row ids that get a pre-compaction divider rendered before them.
  const dividerBeforeId = useMemo(
    () => compactionDividerIds(renderRows, compactionPoints),
    [renderRows, compactionPoints],
  );

  return (
    <ImageDimsContext.Provider value={imageDimsStore}>
      <div className="chat-log" ref={logRef}>
        {loadingOlder && <div className="older-spinner" aria-hidden="true" />}
        {hasMoreOlder && !loadingOlder && (
          // Affordance for short threads that don't scroll (the onScroll path
          // covers the rest). Tapping pages the next older slice.
          <button className="load-older" onClick={() => loadOlder()}>
            {t("chat.loadOlder")}
          </button>
        )}
        {renderRows.map((m) => {
          const row =
            m.role === "work" ? (
              <WorkBlockView
                key={m.id}
                row={m}
                defaultExpanded={defaultExpandedWorkIds?.has(m.id)}
                onToggle={handleWorkToggle}
              />
            ) : (
              <MessageRow
                key={m.id}
                m={m}
                connEpoch={connEpoch}
                flash={m.id === flash.id ? flash.nonce : 0}
                onRetry={retryMessage}
              />
            );
          const seamAt = dividerBeforeId.get(m.id);
          if (seamAt === undefined) return row;
          return (
            <Fragment key={`${m.id}-seam`}>
              <CompactionDivider label={t("chat.preCompaction")} at={seamAt} />
              {row}
            </Fragment>
          );
        })}
        {streaming && (
          <div className="msg assistant streaming">
            <StreamingMarkdownBody text={streaming} />
          </div>
        )}
        {(awaitingReply || turnActive) && !streaming && !workLive && (
          // Claimed by the SEND, filled when the turn starts. `turnActive` is
          // server-driven — a round trip behind the send — so mounting on it put
          // a 43px row into the log a beat AFTER the bubble had settled, and the
          // follow pin teleported the thread up for it: a second, unprompted
          // lurch. `awaitingReply` is set in the same batch as the optimistic
          // bubble, so the box rides the send's own motion instead. Its lifetime
          // is the stop button's — a send that fails, or never starts a turn,
          // retires both.
          <div className={`work-pending${turnActive ? "" : " reserved"}`}>
            <span className="work-spin">✻</span>
            {t("chat.working")}
          </div>
        )}
        {renderRows.length === 0 && !streaming && !turnActive && syncing && (
          // Nothing cached and nothing live, with the first page still in
          // flight — the only open the mirror can't serve. Blank paper here
          // reads as a broken chat; say we're fetching instead. The 400ms CSS
          // delay keeps it invisible on the two opens that resolve instantly: a
          // restored thread (rows already painted, so this branch never runs)
          // and a compose draft (native answers with a synthesized empty page).
          <div className="thread-loading" aria-live="polite">
            {t("chat.loadingThread")}
          </div>
        )}
      </div>
    </ImageDimsContext.Provider>
  );
}
