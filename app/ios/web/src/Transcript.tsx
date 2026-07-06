import { memo, useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  blobObjectUrl,
  fetchHistory,
  log,
  persistState,
  postJumpVisible,
  postOrdinal,
  retrySend,
  subscribeTranscript,
  type UserSentPayload,
} from "./bridge";
import { MarkdownBody } from "./MarkdownBody";
import { WorkBlockView } from "./WorkBlock";
import {
  uid,
  type CatchUpItem,
  type ChatMsg,
  type PersistedState,
  type Row,
  type WireAttachment,
  type WireFrame,
  type WireMessage,
  type WireWorkStepFrame,
  type WorkRow,
  type WorkStep,
} from "./types";

// Map a `Frame::WorkSnapshot` wire step onto the transcript's rendered
// WorkStep. A `tool` step keeps its `call_id` so a later live `ToolCompleted`
// still pairs by id; `status` defaults to "running" until the call finished
// within the buffered turn.
function wireStepToWork(s: WireWorkStepFrame): WorkStep {
  if (s.kind === "tool") {
    return {
      kind: "tool",
      callId: s.call_id ?? "",
      label: s.label || s.tool || "",
      status: s.status ?? "running",
      summary: s.summary || undefined,
    };
  }
  return { kind: s.kind, text: s.text ?? "" };
}

/// Rows per transcript-history fetch — both the reset-recovery refetch (newest
/// page) and a scroll-up older page. Matches the gateway's default page size
/// (server-clamped to 1..200), so one fetch recovers/loads up to 50 rows.
const HISTORY_PAGE_LIMIT = 50;

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

/// Rebuild `ChatMsg[]` from a native history API page's message rows. Mirrors
/// the live `case "message"` mapping; each row carries real `attachments` (blob
/// refs), so images render inline. Keyed by `platform_msg_id` (the live-path key)
/// when present, else the ordinal, so a row keeps a stable React key across a
/// scroll-up prepend.
function historyMessagesToChatMsgs(messages: WireMessage[]): ChatMsg[] {
  return messages.map((m) => ({
    id: m.platform_msg_id || (typeof m.ordinal === "number" ? `m${m.ordinal}` : uid()),
    role: m.role === "user" ? "user" : "assistant",
    content: m.content,
    attachments: m.attachments,
  }));
}

function ordinalFromMessageId(id: string): number | null {
  const match = /^m(\d+)$/.exec(id);
  if (!match) return null;
  const n = Number(match[1]);
  return Number.isSafeInteger(n) ? n : null;
}

function stableWorkId(ordinal: number): string {
  return `w${ordinal}`;
}

function stableMessageId(ordinal: number): string {
  return `m${ordinal}`;
}

function isStableWorkId(id: string): boolean {
  return /^w\d+$/.test(id);
}

/// Restored rows re-enter with live-turn state INTACT: a work block that was
/// live at persist stays live ("working"), because exiting and re-entering
/// mid-turn — or before the agent's final reply — must NOT collapse it to
/// "worked". The buffered continuation frames extend that same block (keeping
/// its real `startedAt`), and only its terminal reply / turn-end closes it with
/// a real "Worked Xs". A block that persisted already-closed stays closed.
/// Empty blocks have nothing to show; unknown future roles are dropped. Also
/// folds back together any turn a pre-fix mirror split into two work cards.
function sanitizeRestoredRows(rows: Row[] | undefined): Row[] {
  const out: Row[] = [];
  for (const r of rows ?? []) {
    if (r.role === "work") {
      if (!Array.isArray(r.steps) || r.steps.length === 0) continue;
      // Heal a mirror already split by the old re-entry bug: a work block that
      // never closed cleanly (no elapsedMs) directly followed by another work
      // block is ONE turn torn in two — a healthy turn always has a message
      // between its block and the next. Fold the pieces into one card, staying
      // "working" if either half was still live (a turn with no final reply must
      // not read as "worked"); the split's real duration was lost, so it stays
      // untimed.
      const prev = out[out.length - 1];
      if (prev && prev.role === "work" && prev.elapsedMs === undefined) {
        out[out.length - 1] = {
          ...prev,
          steps: [...prev.steps, ...r.steps],
          active: prev.active || r.active,
          elapsedMs: undefined,
        };
      } else {
        out.push({ ...r });
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

/// One image attachment in a bubble: downloads the blob via the bridge (cached
/// on device), wraps it in an object URL, shows a spinner while loading and a
/// tap-to-retry on failure. The old in-session previewUrl short-circuit is
/// gone — native previews don't cross the bridge, so a just-sent image renders
/// by fetching its own bytes back over requestBlob (device-cached, so fast).
function AttachmentImage({
  attachment,
  connEpoch,
}: {
  attachment: WireAttachment;
  connEpoch: number;
}) {
  const { t } = useTranslation();
  const [url, setUrl] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  const [attempt, setAttempt] = useState(0);
  // Mirrors `failed` for the connEpoch retry effect to read without taking
  // `failed` as a dep (which would refetch in a tight loop the instant a fetch
  // fails).
  const failedRef = useRef(false);

  useEffect(() => {
    let owned: string | null = null;
    let cancelled = false;
    failedRef.current = false;
    setFailed(false);
    setUrl(null);
    blobObjectUrl(attachment.blob_id, attachment.mime_type)
      .then((u) => {
        if (cancelled) {
          URL.revokeObjectURL(u);
          return;
        }
        owned = u;
        setUrl(u);
      })
      .catch(() => {
        if (!cancelled) {
          failedRef.current = true;
          setFailed(true);
        }
      });
    return () => {
      cancelled = true;
      if (owned) URL.revokeObjectURL(owned);
    };
  }, [attachment.blob_id, attachment.mime_type, attempt]);

  // A restored image can race ahead of its leg going live, so an early fetch
  // fails before native has a live session. Retry the moment a (re)connect
  // lands instead of stranding it on tap-to-load.
  useEffect(() => {
    if (failedRef.current) setAttempt((a) => a + 1);
  }, [connEpoch]);

  if (failed) {
    return (
      <button className="attachment-retry" onClick={() => setAttempt((a) => a + 1)}>
        ↻ {t("chat.tapToLoad")}
      </button>
    );
  }
  if (!url) return <div className="attachment-loading">{t("chat.loadingImage")}</div>;
  return <img className="attachment-img" src={url} alt={attachment.filename ?? t("chat.imageAlt")} />;
}

function AttachmentList({
  attachments,
  connEpoch,
}: {
  attachments: WireAttachment[];
  connEpoch: number;
}) {
  return (
    <div className="attachments">
      {attachments.map((a, i) =>
        a.kind === "image" ? (
          <AttachmentImage key={`${a.blob_id}-${i}`} attachment={a} connEpoch={connEpoch} />
        ) : (
          <div key={`${a.blob_id}-${i}`} className="attachment-file">
            📎 {a.filename ?? a.mime_type}
          </div>
        ),
      )}
    </div>
  );
}

/// One finalized transcript row. User messages and notices keep their bubbles;
/// assistant replies render bubble-less at full thread width as markdown (the
/// web chat's reading-band layout). Memoized so streaming ticks don't re-parse
/// every settled message's markdown.
const MessageRow = memo(function MessageRow({
  m,
  connEpoch,
  onRetry,
}: {
  m: ChatMsg;
  connEpoch: number;
  onRetry: (m: ChatMsg) => void;
}) {
  const { t } = useTranslation();
  if (m.role === "assistant") {
    return (
      <div className="msg assistant">
        {m.attachments && m.attachments.length > 0 && (
          <AttachmentList attachments={m.attachments} connEpoch={connEpoch} />
        )}
        {m.content && <MarkdownBody text={m.content} />}
      </div>
    );
  }
  return (
    <div className={`bubble ${m.role}${m.sendState ? ` ${m.sendState}` : ""}`}>
      {m.attachments && m.attachments.length > 0 && (
        <AttachmentList attachments={m.attachments} connEpoch={connEpoch} />
      )}
      {m.content}
      {m.sendState === "sending" && <span className="send-spinner" aria-hidden="true" />}
      {m.sendState === "failed" && (
        <button className="send-failed" onClick={() => onRetry(m)} aria-label={t("chat.retrySend")}>
          <span aria-hidden="true">!</span>
        </button>
      )}
    </div>
  );
});

/// The transcript-only chat thread. All chrome (header, composer, connection
/// state) is native SwiftUI; this renders the message log and keeps the
/// hardest-won behaviors: frame handling, reset recovery, history paging, and
/// the scroll/follow model.
export function Transcript({
  restored,
  listed,
  initialConnEpoch,
}: {
  restored: PersistedState | null;
  listed: boolean;
  initialConnEpoch: number;
}) {
  const { t } = useTranslation();
  const [messages, setMessages] = useState<Row[]>(() => sanitizeRestoredRows(restored?.messages));
  const [streaming, setStreaming] = useState("");
  const [turnActive, setTurnActive] = useState(false);
  // The full streamed answer so far. State updates are coalesced through one
  // rAF per frame burst — every push crosses the bridge as its own JS task, so
  // without this each delta would re-render (and re-parse markdown) alone.
  const streamText = useRef("");
  const streamRaf = useRef<number | undefined>(undefined);
  // Bumped by native on each successful (re)connect (setConnEpoch). Drives the
  // attachment auto-retry and replaces the old per-dial connGen guard.
  const [connEpoch, setConnEpoch] = useState(initialConnEpoch);
  const connEpochRef = useRef(initialConnEpoch);
  // platform_msg_ids already rendered (our optimistic sends + anything
  // restored), so the server's echo or a catch-up replay doesn't render twice.
  const sentIds = useRef<Set<string>>(
    new Set((restored?.messages ?? []).filter((m) => m.role === "user").map((m) => m.id)),
  );
  // Durable ordinals already rendered. This catches the network-race where an
  // old leg delivers a final Message just before a reconnect's Subscribe replay
  // sends the same row again.
  const renderedOrdinals = useRef<Set<number>>(
    new Set(
      (restored?.messages ?? [])
        .filter((m) => m.role === "user" || m.role === "assistant")
        .map((m) => ordinalFromMessageId(m.id))
        .filter((n): n is number => n !== null),
    ),
  );
  // Highest durable ordinal rendered — the newest-edge cursor. Native uses it
  // as sinceOrdinal on reconnect, so every advance is posted over the bridge.
  const lastOrdinal = useRef<number | null>(restored?.lastOrdinal ?? 0);
  // Lowest durable ordinal loaded — the scroll-up paging cursor
  // (`before_ordinal`). `null` = unknown / nothing older to page to.
  const oldestOrdinal = useRef<number | null>(restored?.oldestOrdinal ?? null);
  const [hasMoreOlder, setHasMoreOlder] = useState<boolean>(restored?.hasMoreOlder ?? false);
  const [loadingOlder, setLoadingOlder] = useState(false);
  // True while a `Frame::Reset` recovery is in flight, so a burst of Resets
  // (back-pressure) doesn't stack concurrent refetches.
  const recovering = useRef(false);
  // Serializes native history API requests AND tags the pushed `history_page`
  // reply so its handler knows whether to REPLACE (reset recovery) or PREPEND
  // (scroll-up). The epoch captured at request time lets a reply that arrives
  // under a different connection epoch be dropped as stale. `null` = no history
  // request in flight.
  const relayHistory = useRef<{ mode: "reset" | "page"; epoch: number } | null>(null);
  // A reset recovery that arrived while a request was already in flight, so
  // it's queued to run when that request's `history_page` lands (rather than
  // being dropped — which would leave the stale cursor and re-arm the reset
  // loop).
  const pendingReset = useRef(false);
  // In-flight guard for an older-page load, so a scroll-event burst fires one
  // fetch. `loadingOlder` (state) drives the spinner; this ref is the race-free
  // gate.
  const pagingRef = useRef(false);
  const logRef = useRef<HTMLDivElement>(null);
  // Set just before a scroll-up PREPEND so the layout effect can re-anchor the
  // viewport (prepending above the top would otherwise jump the scroll
  // position).
  const prependAnchor = useRef<{ prevScrollHeight: number; prevScrollTop: number } | null>(null);
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
  // Drives the jump-to-latest button — a render concern, unlike followRef
  // (a ref precisely so scrolling doesn't re-render).
  const [showJump, setShowJump] = useState(false);
  // True while the jump-to-latest smooth glide is in flight. The glide fires
  // scroll events that still read as "off the edge"; onScroll holds the
  // follow/button state while this is set so the button doesn't flicker back.
  const glidingRef = useRef(false);
  const glideTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(glideTimer.current), []);

  // Mirror the thread to native on every change so a webview reload / app
  // relaunch restores it (via init.restoredState). Debounced bridge-side.
  useEffect(() => {
    persistState({
      messages,
      lastOrdinal: lastOrdinal.current,
      oldestOrdinal: oldestOrdinal.current,
      hasMoreOlder,
    });
  }, [messages, hasMoreOlder]);

  // Open the thread at its newest edge — a restored thread would otherwise
  // mount showing its OLDEST rows. Pre-paint, so the top never flashes by.
  useLayoutEffect(() => {
    const el = scrollEl();
    if (el) el.scrollTop = el.scrollHeight;
  }, []);

  // While pinned to the newest edge, keep it in view as content lands (rows,
  // stream deltas, the turn indicator) — pre-paint, so a bubble never paints
  // off-screen first. A scroll-up PREPEND is exempt even while pinned (a short
  // thread's "load earlier" tap): the anchor effect below owns that viewport
  // change — this effect is declared first so the armed anchor is still
  // visible.
  useLayoutEffect(() => {
    const el = scrollEl();
    if (el && followRef.current && !prependAnchor.current && !userTouchingRef.current) {
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
      if (el && followRef.current && !userTouchingRef.current) el.scrollTop = el.scrollHeight;
    });
    ro.observe(box);
    return () => ro.disconnect();
  }, []);

  // Track finger-down (on the document — the whole page is the scroller now) so
  // the pin-to-newest writes yield to a drag. Passive — never blocks the scroll.
  useEffect(() => {
    const down = () => {
      userTouchingRef.current = true;
      touchStartScrollTop.current = scrollEl()?.scrollTop ?? 0;
    };
    const up = () => {
      userTouchingRef.current = false;
      const el = scrollEl();
      if (!el) return;
      // Catch up to the newest edge on lift ONLY for a hold at the bottom
      // (content grew while pins were suspended) — NOT for a deliberate upward
      // drag, which must stay where the finger left it (the old unconditional
      // re-pin sprang every sub-threshold drag back to the bottom).
      const draggedUp = el.scrollTop < touchStartScrollTop.current - 2;
      if (followRef.current && !draggedUp) el.scrollTop = el.scrollHeight;
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

  const appendNotice = useCallback((text: string) => {
    setMessages((m) => [...m, { id: uid(), role: "notice", content: text }]);
  }, []);

  // The server acknowledged our own send (its echo arrived by platform_msg_id) —
  // clear the send-state chrome (spinner / retry dot) on that optimistic bubble.
  const markSent = useCallback((msgId: string) => {
    setMessages((rows) =>
      rows.map((r) => (r.role === "user" && r.id === msgId && r.sendState ? { ...r, sendState: undefined } : r)),
    );
  }, []);

  // Native's send Task errored — flip the still-sending bubble to the failed
  // (red retry dot) state. Guarded on "sending" so a late failure can't stomp a
  // bubble the echo already delivered.
  const markFailed = useCallback((msgId: string) => {
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
    setMessages((rows) =>
      rows.map((r) => (r.role === "user" && r.id === m.id ? { ...r, sendState: "sending" } : r)),
    );
  }, []);

  // ---- streaming answer (rAF-coalesced) ------------------------------------

  const appendStreaming = useCallback((text: string) => {
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

  // Apply `mutate` to the tail work block, opening one if the turn doesn't
  // have an open block yet (the web chat's ensureWork).
  const withOpenWork = useCallback((mutate: (row: WorkRow) => WorkRow) => {
    setMessages((rows) => {
      const last = rows[rows.length - 1];
      // A restored live block stays `active`, so a re-entry's buffered
      // continuation extends THIS block (keeping its real startedAt) instead of
      // opening a second one.
      if (last && last.role === "work" && last.active) {
        return [...rows.slice(0, -1), mutate(last)];
      }
      const fresh: WorkRow = { id: uid(), role: "work", steps: [], active: true, startedAt: Date.now() };
      return [...rows, mutate(fresh)];
    });
  }, []);

  const pushWorkStep = useCallback(
    (step: WorkStep) => {
      withOpenWork((w) => ({ ...w, steps: [...w.steps, step] }));
    },
    [withOpenWork],
  );

  // Answer text followed by more work was intermediate: settle it into the
  // block as a prose step so reasoning and answer interleave cleanly (the web
  // chat's flush-and-fold on any non-delta work frame).
  const foldStreamingIntoProse = useCallback(() => {
    const text = streamText.current;
    if (!text) return;
    clearStreaming();
    pushWorkStep({ kind: "prose", text });
  }, [clearStreaming, pushWorkStep]);

  // Close the tail work block: freeze the elapsed label, or drop the block
  // entirely when the turn produced no steps (a plain direct answer).
  const closeWork = useCallback(() => {
    setMessages((rows) => {
      const last = rows[rows.length - 1];
      if (!last || last.role !== "work" || !last.active) return rows;
      if (last.steps.length === 0) return rows.slice(0, -1);
      const elapsedMs = last.startedAt !== undefined ? Date.now() - last.startedAt : undefined;
      return [...rows.slice(0, -1), { ...last, active: false, elapsedMs }];
    });
  }, []);

  // Recover the in-flight turn's work block on a mid-turn (re)subscribe (an iOS
  // relay leg resuming after backgrounding). The snapshot is the whole coalesced
  // turn — a superset of anything shown live — so REPLACE the open block's steps
  // rather than append (appending would double-render the head already on screen
  // before we backgrounded). The trailing prose step is the CURRENT answer tail,
  // which the live view renders as the streaming reply below the block, not as a
  // work step — route it to the stream so the recovered shape matches live and
  // the terminal Message replaces it cleanly.
  const applyWorkSnapshot = useCallback(
    (wireSteps: WireWorkStepFrame[]) => {
      const steps = wireSteps.map(wireStepToWork);
      if (steps.length === 0) return;
      const tail = steps[steps.length - 1];
      const tailProse = tail.kind === "prose";
      const workSteps = tailProse ? steps.slice(0, -1) : steps;
      // Drive the live reply to the recovered answer tail (or clear it) in one
      // shot, batched with the block replace below — so the reply grows in place
      // rather than blanking for a frame.
      setStreamingText(tailProse ? tail.text : "");
      setMessages((rows) => {
        const last = rows[rows.length - 1];
        const openBlock = last && last.role === "work" ? last : undefined;
        if (workSteps.length === 0) {
          // Answer-only turn: no block, the streamed reply stands alone; drop a
          // stale empty/restored block if it's the tail.
          return openBlock && openBlock.steps.length === 0 ? rows.slice(0, -1) : rows;
        }
        // Re-open a block a prior restore froze (relaunch mid-turn) and replace
        // its steps; otherwise open a fresh one after the turn's user message.
        const rebuilt: WorkRow = openBlock
          ? { ...openBlock, steps: workSteps, active: true, startedAt: openBlock.startedAt ?? Date.now(), elapsedMs: undefined }
          : { id: uid(), role: "work", steps: workSteps, active: true, startedAt: Date.now() };
        return openBlock ? [...rows.slice(0, -1), rebuilt] : [...rows, rebuilt];
      });
    },
    [setStreamingText],
  );

  // Fire a transcript-history request through native. The API result is pushed
  // later as a local `history_page` frame; `mode` tags it for that handler
  // (REPLACE for a reset, PREPEND for scroll-up), and the current epoch tags it
  // against late delivery across a reconnect. One at a time — returns `false`
  // if a request is already in flight (the caller then unwinds its own guards).
  const requestHistory = useCallback((mode: "reset" | "page", beforeOrdinal: number | null): boolean => {
    if (relayHistory.current) return false;
    relayHistory.current = { mode, epoch: connEpochRef.current };
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
  const prependOlder = useCallback((older: ChatMsg[], newOldest: number | null, more: boolean) => {
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
      return [...fresh, ...m];
    });
    // Only advance the cursor on a non-empty page; an empty page leaves it put.
    if (newOldest !== null) oldestOrdinal.current = newOldest;
    setHasMoreOlder(more);
  }, []);

  // Recover from a gateway `Frame::Reset` (catch-up gap over the replay cap, or
  // outbound back-pressure). Left unhandled this *loops*: the stale pre-gap
  // cursor goes back out on the next reconnect and overflows again. One
  // native `fetchHistory` rebuilds the thread and reseeds the cursors — no
  // reconnect needed. If a request is already in flight (`false`) — e.g. a
  // scroll-up page — the reset can't ride it (that response is a
  // PREPEND), so QUEUE it to run when that request completes; dropping it would
  // leave the stale cursor in place and re-arm the very reset loop we're
  // breaking. A re-entrancy guard keeps a burst of Resets from stacking
  // concurrent refetches. `t` is deliberately not a dep: a language switch must
  // not re-create the frame pipeline; the captured language is fine for these
  // one-shot recovery strings.
  const recoverFromReset = useCallback(() => {
    if (recovering.current) return;
    recovering.current = true;
    try {
      const fired = requestHistory("reset", null);
      if (!fired) pendingReset.current = true;
    } catch (e) {
      log("warn", `history recover failed: ${String(e)}`);
      appendNotice(t("chat.recoverFailed", { error: String(e) }));
    } finally {
      recovering.current = false;
    }
  }, [requestHistory, appendNotice]);

  // Load the next older page (scroll-up): fire a native fetchHistory whose
  // `history_page` reply prepends in the frame switch (and clears the guards
  // there). `pagingRef` gates re-entry.
  const loadOlder = useCallback(() => {
    if (pagingRef.current || !hasMoreOlder) return;
    const before = oldestOrdinal.current;
    if (before === null) return; // no cursor — can't page older
    pagingRef.current = true;
    setLoadingOlder(true);
    try {
      const fired = requestHistory("page", before);
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
  }, [hasMoreOlder, requestHistory, appendNotice]);

  // The single owner of the initial-backfill decision (hydration matrix:
  // app/ios/CLAUDE.md). A LISTED session rendering zero rows on a live leg
  // pulls its newest history page — nothing else fills that void: a mirror-less
  // open subscribes with `since_ordinal` None, which the gateway replays
  // nothing for (route.rs). Ground truth is RENDERED ROWS, not mirror
  // existence — a mirror can exist with zero rows (an earlier failed backfill's
  // empty persist), and only this side knows what rendered. Callers are the
  // clock edges where "listed + empty + connected" can newly hold; add a new
  // edge as another CALL, never as a second decision site. Safe to re-call:
  // `requestHistory` dedups in-flight, a landed page makes `messages`
  // non-empty, and a draft (listed=false) never fires.
  const ensureBackfilled = () => {
    if (!listed || messages.length > 0) return;
    if (connEpochRef.current === 0) return; // leg not live yet — the connect edge re-calls
    try {
      requestHistory("reset", null);
    } catch (e) {
      log("warn", `initial history load failed: ${String(e)}`);
    }
  };

  // Clock edge: resident re-entry (matrix cell E). The store stays cached and
  // CONNECTED across back-out → re-enter, so no `connEpoch` edge will come —
  // and the list visit in between pruned this session's mirror if it fell
  // outside the most-recently-active set (opening doesn't bump `lastActive`).
  // On a fresh store this no-ops (epoch still 0); the connect edge takes over.
  useEffect(() => {
    ensureBackfilled();
  }, []);

  const setNewestOrdinal = (value: number | null) => {
    lastOrdinal.current = value;
    postOrdinal(value);
  };

  const applyCatchUp = (items: CatchUpItem[], newestOrdinal: number | null | undefined, truncated: boolean) => {
    if (truncated) {
      recoverFromReset();
      return;
    }
    const hasAssistantMessage = items.some((item) => item.kind === "message" && item.role === "assistant");
    if (hasAssistantMessage) clearStreaming();

    setMessages((rows) => {
      const next = [...rows];
      const hasRowId = (id: string) => next.some((row) => row.id === id);
      const findMessageByOrdinal = (ordinal: number) =>
        next.findIndex((row) => {
          if (row.role !== "user" && row.role !== "assistant") return false;
          return ordinalFromMessageId(row.id) === ordinal;
        });
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
          elapsedMs: last.startedAt !== undefined ? Date.now() - last.startedAt : last.elapsedMs,
        };
      };

      for (const item of items) {
        const ordinal = item.ordinal;
        if (!Number.isSafeInteger(ordinal)) continue;
        if (item.kind === "work") {
          const steps = item.steps.map(wireStepToWork);
          if (steps.length === 0) continue;
          const id = stableWorkId(ordinal);
          if (hasRowId(id)) continue;
          const block: WorkRow = { id, role: "work", steps, active: false };
          const msgIndex = findMessageByOrdinal(ordinal);
          if (msgIndex >= 0) {
            const prev = next[msgIndex - 1];
            if (prev && prev.role === "work" && !isStableWorkId(prev.id)) {
              next[msgIndex - 1] = block;
            } else {
              next.splice(msgIndex, 0, block);
            }
            continue;
          }
          const last = next[next.length - 1];
          if (last && last.role === "work" && !isStableWorkId(last.id)) {
            next[next.length - 1] = block;
          } else {
            next.push(block);
          }
          continue;
        }

        const role = item.role === "user" ? "user" : "assistant";
        const platformMsgId = item.platform_msg_id || "";
        const id = platformMsgId || stableMessageId(ordinal);
        const alreadySent = role === "user" && platformMsgId !== "" && sentIds.current.has(platformMsgId);
        const alreadyRendered =
          hasRowId(id) || renderedOrdinals.current.has(ordinal) || findMessageByOrdinal(ordinal) >= 0;
        if (alreadySent || alreadyRendered) {
          renderedOrdinals.current.add(ordinal);
          if (role === "user" && platformMsgId) {
            sentIds.current.add(platformMsgId);
            // A send confirmed by reconnect merge instead of a live echo — clear
            // its send-state chrome too (backgrounded right after sending).
            const idx = next.findIndex((row) => row.role === "user" && row.id === platformMsgId);
            const existing = idx >= 0 ? next[idx] : undefined;
            if (existing && existing.role === "user" && existing.sendState) {
              next[idx] = { ...existing, sendState: undefined };
            }
          }
          continue;
        }
        if (role === "assistant") closeTrailingWork();
        if (role === "user" && platformMsgId) sentIds.current.add(platformMsgId);
        renderedOrdinals.current.add(ordinal);
        next.push({
          id,
          role,
          content: item.content,
          attachments: item.attachments,
        });
      }
      return next;
    });

    if (typeof newestOrdinal === "number" && (lastOrdinal.current === null || newestOrdinal > lastOrdinal.current)) {
      setNewestOrdinal(newestOrdinal);
    }
  };

  const handleFrame = (frameJson: string) => {
    let frame: WireFrame;
    try {
      frame = JSON.parse(frameJson) as WireFrame;
    } catch (e) {
      log("warn", `unparseable frame: ${String(e)}`);
      return;
    }
    switch (frame.kind) {
      case "message": {
        const ordinal = typeof frame.ordinal === "number" ? frame.ordinal : null;
        // Advance the cursor first — even for our own echo we dedup below — so
        // a later reconnect doesn't re-replay this row. A `null` cursor (post
        // reset) takes the first real ordinal that lands.
        if (ordinal !== null && (lastOrdinal.current === null || ordinal > lastOrdinal.current)) {
          setNewestOrdinal(ordinal);
        }
        const role = frame.role === "user" ? "user" : "assistant";
        if (role === "user" && frame.platform_msg_id && sentIds.current.has(frame.platform_msg_id)) {
          if (ordinal !== null) renderedOrdinals.current.add(ordinal);
          markSent(frame.platform_msg_id); // server confirmed the send — stop the spinner
          return; // our own message / already rendered
        }
        if (ordinal !== null && renderedOrdinals.current.has(ordinal)) {
          if (role === "user" && frame.platform_msg_id) sentIds.current.add(frame.platform_msg_id);
          return;
        }
        if (role === "user" && frame.platform_msg_id) {
          sentIds.current.add(frame.platform_msg_id);
        }
        if (role === "assistant") {
          // The terminal message is authoritative: it replaces the streamed
          // text and ends the turn's work block.
          closeWork();
          clearStreaming();
        }
        if (ordinal !== null) renderedOrdinals.current.add(ordinal);
        setMessages((m) => [
          ...m,
          {
            id: frame.platform_msg_id || (ordinal !== null ? `m${ordinal}` : uid()),
            role,
            content: frame.content,
            attachments: frame.attachments,
          },
        ]);
        break;
      }
      case "attachment":
        if (frame.attachments && frame.attachments.length > 0) {
          const attachments = frame.attachments;
          foldStreamingIntoProse();
          setMessages((m) => [...m, { id: uid(), role: "assistant", content: "", attachments }]);
        }
        break;
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
            steps.push({ kind: "reasoning", text: frame.text });
          }
          return { ...w, steps };
        });
        break;
      case "tool_started":
        foldStreamingIntoProse();
        pushWorkStep({
          kind: "tool",
          callId: frame.call_id,
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
              steps[i] = { ...s, status: frame.status, summary: frame.summary || undefined };
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
          });
          return { ...w, steps };
        });
        break;
      case "turn_state":
        setTurnActive(frame.active);
        if (!frame.active) closeWork();
        break;
      case "work_snapshot":
        applyWorkSnapshot(frame.steps);
        break;
      case "notice":
        if (frame.transient) {
          // Mid-turn progress narration belongs to the work block, not the log.
          foldStreamingIntoProse();
          pushWorkStep({ kind: "status", text: frame.text });
        } else {
          appendNotice(frame.text);
        }
        break;
      case "reset":
        // Native only forwards frames from its live leg, so unlike the old
        // per-dial channel there's no per-frame generation to check here;
        // `recoverFromReset` still self-guards a Reset burst.
        recoverFromReset();
        break;
      case "catch_up":
        applyCatchUp(frame.items, frame.newest_ordinal ?? null, frame.truncated);
        break;
      case "history_page": {
        // `relayHistory.current` (set when we fired the request) says whether
        // this is a reset rebuild (REPLACE) or a scroll-up page (PREPEND). A
        // page with no matching in-flight request (`null`) is stale/duplicate,
        // and one whose request was tagged under a superseded connection epoch
        // belongs to a dead leg — drop both; neither may fall through to the
        // REPLACE path and wipe the thread.
        const pending = relayHistory.current;
        relayHistory.current = null;
        if (pending === null || pending.epoch !== connEpochRef.current) break;
        const rows = historyMessagesToChatMsgs(frame.messages);
        if (pending.mode === "page") {
          prependOlder(rows, frame.oldest_ordinal ?? null, frame.has_more);
        } else {
          // Reset rebuild: REPLACE the thread with the newest page, reseed both
          // cursors.
          for (const m of frame.messages) {
            if (m.role === "user" && m.platform_msg_id) sentIds.current.add(m.platform_msg_id);
          }
          renderedOrdinals.current = new Set(
            rows
              .map((m) => ordinalFromMessageId(m.id))
              .filter((n): n is number => n !== null),
          );
          setNewestOrdinal(frame.newest_ordinal ?? 0);
          oldestOrdinal.current = frame.oldest_ordinal ?? null;
          setHasMoreOlder(frame.has_more);
          clearStreaming();
          // The rebuilt thread IS the newest page — the pre-reset scroll
          // position is meaningless, so snap to the newest edge.
          followRef.current = true;
          setMessages(rows);
        }
        // This request is done; clear any paging guards a coincident scroll-up
        // left set.
        pagingRef.current = false;
        setLoadingOlder(false);
        // A reset queued behind this request (it couldn't ride a page response)
        // now runs — unless this WAS the reset, in which case the queue is
        // moot.
        if (pendingReset.current) {
          pendingReset.current = false;
          if (pending.mode === "page") recoverFromReset();
        }
        break;
      }
      case "history_failed": {
        // Native couldn't enqueue the request — unwind the guards the fire
        // sites armed, exactly like the old invoke() rejection paths did.
        const pending = relayHistory.current;
        relayHistory.current = null;
        if (pending === null || pending.epoch !== connEpochRef.current) break;
        pagingRef.current = false;
        setLoadingOlder(false);
        // Deliberately no retry: on a dead leg the reconnect's Subscribe
        // re-fires Reset if the cursor is still stale (the same self-heal the
        // connEpoch bump relies on); re-firing here would loop fail→retry.
        pendingReset.current = false;
        log("warn", `history fetch failed: ${frame.error}`);
        appendNotice(t("chat.recoverFailed", { error: frame.error }));
        break;
      }
      default:
        break; // task_list / approvals / ping etc. not surfaced in the transcript
    }
  };

  // Native sent a user message: append the optimistic bubble, seed echo-dedup,
  // snap follow to the newest edge (an own send always returns there).
  const handleUserSent = (payload: UserSentPayload) => {
    sentIds.current.add(payload.msgId);
    followRef.current = true;
    setMessages((m) => [
      ...m,
      {
        id: payload.msgId,
        role: "user",
        content: payload.text,
        attachments: payload.attachments.length > 0 ? payload.attachments : undefined,
        sendState: "sending",
      },
    ]);
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

  // Native (re)connected. Any history request in flight belongs to the old
  // epoch and is abandoned (its late `history_page` is dropped by the epoch tag)
  // — clear the guards so a future fetch isn't blocked / the spinner isn't
  // stuck / a queued reset isn't stranded. The reconnect's own subscribe
  // re-triggers a Reset if the cursor is still stale, so a dropped queued reset
  // self-heals.
  const handleConnEpoch = (epoch: number) => {
    connEpochRef.current = epoch;
    relayHistory.current = null;
    pendingReset.current = false;
    pagingRef.current = false;
    setLoadingOlder(false);
    setConnEpoch(epoch);
    // Clock edge: the leg (re)connected (matrix cell D — first connect of a
    // fresh store — plus the re-fire after an epoch bump abandoned an earlier
    // attempt, relayHistory cleared just above). Fired after the ref update so
    // the request's epoch tag matches when its `history_page` lands.
    ensureBackfilled();
  };

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
    handleConnEpoch,
    handleBottomInset,
    jumpToLatest,
  });
  handlersRef.current = {
    handleFrame,
    handleUserSent,
    markFailed,
    handleConnEpoch,
    handleBottomInset,
    jumpToLatest,
  };
  useEffect(
    () =>
      subscribeTranscript({
        frame: (frameJson) => handlersRef.current.handleFrame(frameJson),
        connEpoch: (epoch) => handlersRef.current.handleConnEpoch(epoch),
        userSent: (payload) => handlersRef.current.handleUserSent(payload),
        sendFailed: (msgId) => handlersRef.current.markFailed(msgId),
        bottomInset: (px) => handlersRef.current.handleBottomInset(px),
        jumpToLatest: () => handlersRef.current.jumpToLatest(),
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

  // The button itself is native (a liquid-glass circle above the composer) —
  // mirror the visibility over the bridge; taps come back via the
  // `jumpToLatest` transcript event above.
  useEffect(() => {
    postJumpVisible(showJump);
  }, [showJump]);

  // While the turn's work block is live it already signals activity; the bare
  // "Working" pending line only covers the gap before the first frame lands.
  const lastRow = messages[messages.length - 1];
  const workLive = lastRow !== undefined && lastRow.role === "work" && lastRow.active;

  return (
    <>
      <div className="chat-log" ref={logRef}>
        {loadingOlder && <div className="older-spinner" aria-hidden="true" />}
        {hasMoreOlder && !loadingOlder && (
          // Affordance for short threads that don't scroll (the onScroll path
          // covers the rest). Tapping pages the next older slice.
          <button className="load-older" onClick={() => loadOlder()}>
            {t("chat.loadOlder")}
          </button>
        )}
        {messages.map((m) =>
          m.role === "work" ? (
            <WorkBlockView key={m.id} row={m} />
          ) : (
            <MessageRow key={m.id} m={m} connEpoch={connEpoch} onRetry={retryMessage} />
          ),
        )}
        {streaming && (
          <div className="msg assistant streaming">
            <MarkdownBody text={streaming} />
          </div>
        )}
        {turnActive && !streaming && !workLive && (
          <div className="work-pending">
            <span className="work-spin">✻</span>
            {t("chat.working")}
          </div>
        )}
      </div>
    </>
  );
}
