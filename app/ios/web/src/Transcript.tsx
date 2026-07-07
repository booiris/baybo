import { memo, useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import {
  blobObjectUrl,
  fetchHistory,
  log,
  persistState,
  postJumpVisible,
  postMarkRead,
  postSyncRequest,
  retrySend,
  subscribeTranscript,
  type UserSentPayload,
} from "./bridge";
import { MarkdownBody } from "./Markdown";
import { WorkBlockView } from "./WorkBlock";
import {
  uid,
  type ChatMsg,
  type PersistedState,
  type Row,
  type TranscriptRowItem,
  type WireAttachment,
  type WireFrame,
  type WireWorkStepFrame,
  type WorkRow,
  type WorkStep,
} from "./types";

// Map a `Frame::SubscribeState` wire work step onto the transcript's rendered
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

/// Map a REST `ChatWorkStep` (the `work` transcript row's step — snake_case
/// `tool_label` / `tool_status` / `tool_summary`, no `call_id`) onto a rendered
/// WorkStep. A reconstructed step's tool call is already closed, so `status`
/// falls back to "ok" when the persisted result didn't carry one.
function restStepToWork(s: NonNullable<TranscriptRowItem["steps"]>[number]): WorkStep {
  if (s.kind === "tool") {
    return {
      kind: "tool",
      callId: "",
      label: s.tool_label || s.tool || "",
      status: s.tool_status ?? "ok",
      summary: s.tool_summary || undefined,
    };
  }
  return { kind: s.kind, text: s.text ?? "" };
}

/// Translate one full-fidelity transcript row (`ChatTranscriptItem`, carried
/// verbatim by the `sync_page` / `history_page` frames) into a rendered Row,
/// keyed by the server's stable `id` (`m<ordinal>` / `w<ordinal>` / `n<seq>`)
/// — the render key AND redelivery dedup key. `null` for a shape we don't
/// render (an empty/unknown row).
function transcriptItemToRow(item: TranscriptRowItem): Row | null {
  if (item.kind === "work") {
    const steps = (item.steps ?? []).map(restStepToWork);
    return {
      id: item.id,
      role: "work",
      steps,
      active: false,
      elapsedMs:
        item.work_started_at && item.work_ended_at
          ? Math.max(0, Date.parse(item.work_ended_at) - Date.parse(item.work_started_at))
          : undefined,
    };
  }
  if (item.kind === "notice") {
    return { id: item.id, role: "notice", content: item.text ?? "" };
  }
  const role = item.role === "user" ? "user" : "assistant";
  // A user row keeps its send's `platform_msg_id` as the render id (the live
  // echo path's key), so an optimistic bubble reconciles by id; an assistant
  // row uses the stable `m<ordinal>` id.
  const id = role === "user" && item.platform_msg_id ? item.platform_msg_id : item.id;
  return {
    id,
    role,
    content: item.text ?? "",
    attachments: item.attachments,
  };
}

/// Rows per backward-history (scroll-up) page. Matches the gateway's default
/// page size (server-clamped to 1..200), so one fetch loads up to 50 rows.
const HISTORY_PAGE_LIMIT = 50;

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

/// How far outside the viewport (px, top + bottom) an image attachment begins
/// loading its blob — a preload band so an image is usually ready by the time it
/// scrolls in, while a back-history page's off-screen images stay unfetched. See
/// AttachmentImage.
const LAZY_IMAGE_ROOT_MARGIN = "400px 0px";

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

function ordinalFromMessageId(id: string): number | null {
  const match = /^m(\d+)$/.exec(id);
  if (!match) return null;
  const n = Number(match[1]);
  return Number.isSafeInteger(n) ? n : null;
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

/// One image attachment in a bubble: lazily downloads the blob via the bridge
/// (cached on device) once its row scrolls near the viewport, wraps it in an
/// object URL, shows a spinner while loading and a tap-to-retry on failure. The
/// lazy gate is load-bearing for history: a back-page can carry dozens of
/// images, and fetching every blob on mount floods the bridge — each image
/// crosses as a large base64 string plus a main-thread `atob` decode
/// (bridge.ts) — which stalls the whole transcript until they all settle (the
/// whole page fails to appear while paging history). An IntersectionObserver
/// defers each fetch to when its row actually approaches the screen, so
/// off-screen history images cost nothing until scrolled to. The old in-session
/// previewUrl short-circuit is gone — native previews don't cross the bridge, so
/// a just-sent image renders by fetching its own bytes back over requestBlob
/// (device-cached, so fast).
function AttachmentImage({
  attachment,
  connEpoch,
}: {
  attachment: WireAttachment;
  connEpoch: number;
}) {
  const { t } = useTranslation();
  // Load-once gate — flips true when the placeholder nears the viewport and
  // never falls back, so scrolling past a loaded image doesn't refetch it.
  const [visible, setVisible] = useState(false);
  const [url, setUrl] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  // True once the fetched image has actually decoded — the frame reserves a box
  // and holds the spinner until then, so the swap to the natural size happens in
  // one step (no 0-height flash mid-decode).
  const [loaded, setLoaded] = useState(false);
  const [attempt, setAttempt] = useState(0);
  // Mirrors `failed` for the connEpoch retry effect to read without taking
  // `failed` as a dep (which would refetch in a tight loop the instant a fetch
  // fails).
  const failedRef = useRef(false);
  // The reserved placeholder box the observer watches until the row nears the
  // viewport.
  const holderRef = useRef<HTMLDivElement | null>(null);

  // Arm the lazy gate: observe the placeholder and load once it enters the
  // preload band. Disconnects on the first intersection. Without
  // IntersectionObserver (not expected on WKWebView; a dev-browser guard) load
  // eagerly rather than never showing the image.
  useEffect(() => {
    if (visible) return;
    if (typeof IntersectionObserver === "undefined") {
      setVisible(true);
      return;
    }
    const el = holderRef.current;
    if (!el) return;
    const io = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) {
          setVisible(true);
          io.disconnect();
        }
      },
      { rootMargin: LAZY_IMAGE_ROOT_MARGIN },
    );
    io.observe(el);
    return () => io.disconnect();
  }, [visible]);

  useEffect(() => {
    if (!visible) return;
    let owned: string | null = null;
    let cancelled = false;
    failedRef.current = false;
    setFailed(false);
    setLoaded(false);
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
  }, [attachment.blob_id, attachment.mime_type, attempt, visible]);

  // A restored image can race ahead of its leg going live, so an early fetch
  // fails before native has a live session. Retry the moment a (re)connect
  // lands instead of stranding it on tap-to-load. Only bites once visible (an
  // unfetched off-screen image has no failure to retry).
  useEffect(() => {
    if (failedRef.current) setAttempt((a) => a + 1);
  }, [connEpoch]);

  if (!visible) {
    return <div ref={holderRef} className="attachment-placeholder" aria-hidden="true" />;
  }
  if (failed) {
    return (
      <button
        className="attachment-retry"
        onClick={() => setAttempt((a) => a + 1)}
        aria-label={t("chat.tapToLoad")}
      >
        ↻
      </button>
    );
  }
  // Reserved box → spinner while the blob is fetched and the image decodes
  // underneath (invisible until `loaded`), then the box releases to the image's
  // natural size in one step — no 0-height flash between the loading box and the
  // painted image. That release is a small, bounded height change; WKWebView has
  // no scroll anchoring to absorb it if it lands above the fold while reading
  // history, an accepted tradeoff of not knowing image dimensions up front.
  return (
    <div
      className={`attachment-frame${loaded ? " loaded" : ""}`}
      aria-label={loaded ? undefined : t("chat.loadingImage")}
    >
      {!loaded && <span className="attachment-spinner" aria-hidden="true" />}
      {url && (
        <img
          className="attachment-img"
          src={url}
          alt={attachment.filename ?? t("chat.imageAlt")}
          decoding="async"
          onLoad={() => setLoaded(true)}
          onError={() => setFailed(true)}
        />
      )}
    </div>
  );
}

/// One attachment on its OWN bubble — a lazy-loaded image tile or a named file
/// chip, never sharing the text bubble. `children` carries the send-state chrome
/// when this is a user message's last bubble (an image-only send).
function AttachmentBubble({
  attachment,
  connEpoch,
  className,
  children,
}: {
  attachment: WireAttachment;
  connEpoch: number;
  className?: string;
  children?: ReactNode;
}) {
  return (
    <div className={`attachment-bubble${className ? ` ${className}` : ""}`}>
      {attachment.kind === "image" ? (
        <AttachmentImage attachment={attachment} connEpoch={connEpoch} />
      ) : (
        <div className="attachment-file">📎 {attachment.filename ?? attachment.mime_type}</div>
      )}
      {children}
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
  onRetry,
}: {
  m: ChatMsg;
  connEpoch: number;
  onRetry: (m: ChatMsg) => void;
}) {
  const { t } = useTranslation();

  if (m.role === "notice") {
    return <div className="bubble notice">{m.content}</div>;
  }

  const attachments = m.attachments ?? [];

  if (m.role === "assistant") {
    return (
      <div className="msg-group assistant">
        {attachments.map((a, i) => (
          <AttachmentBubble key={`${a.blob_id}-${i}`} attachment={a} connEpoch={connEpoch} />
        ))}
        {m.content && (
          <div className="msg assistant">
            <MarkdownBody text={m.content} />
          </div>
        )}
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
    <div className="msg-group user">
      {attachments.map((a, i) => {
        const carriesSend = !hasText && i === attachments.length - 1;
        return (
          <AttachmentBubble
            key={`${a.blob_id}-${i}`}
            attachment={a}
            connEpoch={connEpoch}
            className={carriesSend && m.sendState ? m.sendState : undefined}
          >
            {carriesSend ? sendChrome : null}
          </AttachmentBubble>
        );
      })}
      {hasText && (
        <div className={`bubble user${sendClass}`}>
          {m.content}
          {sendChrome}
        </div>
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
  initialConnEpoch,
}: {
  restored: PersistedState | null;
  initialConnEpoch: number;
}) {
  const { t } = useTranslation();
  const [messages, setMessages] = useState<Row[]>(() => sanitizeRestoredRows(restored?.messages));
  const [streaming, setStreaming] = useState("");
  const [turnActive, setTurnActive] = useState(false);
  // Latest turn-active value readable synchronously from the sync-apply
  // callbacks (a rebase/baseline REPLACE must not wipe a live streaming reply).
  const turnActiveRef = useRef(false);
  useEffect(() => {
    turnActiveRef.current = turnActive;
  }, [turnActive]);
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
  // restored), so the server's echo or a sync redelivery doesn't render twice.
  const sentIds = useRef<Set<string>>(
    new Set((restored?.messages ?? []).filter((m) => m.role === "user").map((m) => m.id)),
  );
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
  // The sync cursor: the highest coverage watermark this client holds for the
  // session (docs/sync-protocol.md). `null` = no baseline yet — the next sync
  // omits `since_ordinal` and REPLACEs on the newest page. Advanced max-wins
  // from a sync `next_cursor` and from ordinal-stamped live final replies —
  // except while `rebaseDirty`, when only a sync `next_cursor` advances it.
  const lastOrdinal = useRef<number | null>(restored?.lastOrdinal ?? null);
  // True after applying a rebased page, until one non-rebased sync completes:
  // live ordinals render but do not advance the cursor (a row persisted after
  // the page was built but before the turn's final reply would otherwise be
  // leapfrogged forever by the strictly-`>` select). The follow-up sync fires
  // on turn end and on the safety tick.
  const rebaseDirty = useRef(false);
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
  // Highest ordinal already reported to native as read — dedupes the
  // fire-and-forget `mark_read` posts (the cursor advances on every sync and
  // every live reply while the transcript is on screen).
  const lastMarkedRead = useRef(-1);
  // Lowest durable ordinal loaded — the scroll-up paging cursor
  // (`before_ordinal`). `null` = unknown / nothing older to page to.
  const oldestOrdinal = useRef<number | null>(restored?.oldestOrdinal ?? null);
  const [hasMoreOlder, setHasMoreOlder] = useState<boolean>(restored?.hasMoreOlder ?? false);
  const [loadingOlder, setLoadingOlder] = useState(false);
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
    (turn: { active: boolean; started_at?: string }, wireSteps: WireWorkStepFrame[]) => {
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
      const steps = wireSteps.map(wireStepToWork);
      const tail = steps[steps.length - 1];
      const tailProse = tail?.kind === "prose";
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
  }, [hasMoreOlder, requestHistory, appendNotice]);

  // The one forward-recovery pull (docs/sync-protocol.md "The one client
  // algorithm"): session open, reconnect, gap nudge and the safety tick all
  // land here. Posts the current cursor to native, which fetches
  // `GET …/sync?since_ordinal=<cursor>&limit=…` over the active leg and pushes
  // the result back as a local `sync_page` frame. `null` cursor → baseline
  // REPLACE; a rebased response also REPLACEs. `syncInFlight` coalesces a
  // burst of triggers to one pull (cleared by the reply).
  const runSync = useCallback(() => {
    if (syncInFlight.current) return;
    const cursor = lastOrdinal.current;
    const limit = cursor === null ? SYNC_BASELINE_LIMIT : SYNC_MERGE_LIMIT;
    syncInFlight.current = true;
    try {
      postSyncRequest(cursor, limit);
    } catch (e) {
      syncInFlight.current = false;
      log("warn", `sync request failed: ${String(e)}`);
    }
  }, []);

  // Advance the cursor from a completed sync (docs/sync-protocol.md): the
  // coverage watermark `next_cursor` feeds the max even on a rebased page (it
  // is a sync watermark), and `rebased` sets the dirty flag — cleared by any
  // non-rebased sync — so a live ordinal can't leapfrog the rebase window.
  const advanceCursorFromSync = useCallback((nextCursor: number | null, rebased: boolean) => {
    if (nextCursor !== null && (lastOrdinal.current === null || nextCursor > lastOrdinal.current)) {
      lastOrdinal.current = nextCursor;
    }
    rebaseDirty.current = rebased;
  }, []);

  // Advance from an ordinal-stamped live final reply — max-wins, but a
  // rebase-dirty cursor is frozen against live advances until one non-rebased
  // sync completes.
  const advanceCursorFromLive = useCallback((ordinal: number) => {
    if (rebaseDirty.current) return;
    if (lastOrdinal.current === null || ordinal > lastOrdinal.current) lastOrdinal.current = ordinal;
  }, []);

  // The transcript is on screen (native attaches the webview only for the open
  // session), so a cursor advance means the viewer has read up to it — tell
  // native to advance the server read cursor, deduped so it fires only when the
  // cursor actually moved forward.
  const markReadIfAdvanced = useCallback(() => {
    const cursor = lastOrdinal.current;
    if (cursor === null || cursor <= lastMarkedRead.current) return;
    lastMarkedRead.current = cursor;
    postMarkRead(cursor);
  }, []);

  // Apply one `sync_page` frame. REPLACE (rebased, or baseline `since === null`)
  // swaps the durable thread wholesale — keeping the in-flight turn's open work
  // block and any optimistic user rows the page can't carry yet (the
  // REPLACE-overlay rule) — while a difference merge appends the rows above the
  // cursor, reconciling an optimistic send against its persisted row by
  // `platform_msg_id`. Rows arrive ascending; each carries its stable id.
  const applySyncPage = useCallback(
    (frame: Extract<WireFrame, { kind: "sync_page" }>) => {
      syncInFlight.current = false;
      const replace = frame.rebased || frame.since_ordinal === null;
      const pageRows = frame.rows
        .map(transcriptItemToRow)
        .filter((r): r is Row => r !== null);
      // Reseed the redelivery-dedup sets from the page (idempotent Set adds).
      for (const item of frame.rows) {
        if (item.kind === "message") {
          if (typeof item.ordinal === "number") renderedOrdinals.current.add(item.ordinal);
          if (item.platform_msg_id) sentIds.current.add(item.platform_msg_id);
        }
      }
      if (replace) {
        const pageIds = new Set(pageRows.map((r) => r.id));
        setMessages((prev) => {
          // Keep the in-flight turn's open work block and any optimistic user
          // sends still awaiting their durable row (echoed-but-unpersisted, or
          // below a rebase floor) — the page can't carry either.
          const openWork = prev.filter((r) => r.role === "work" && r.active);
          const keptSends = prev.filter(
            (r) => r.role === "user" && r.sendState !== undefined && !pageIds.has(r.id),
          );
          return [...pageRows, ...keptSends, ...openWork];
        });
        oldestOrdinal.current = frame.oldest_ordinal;
        setHasMoreOlder(frame.has_more_older);
        // The rebuilt thread IS the newest page — pre-sync scroll position is
        // meaningless, so snap to the newest edge.
        followRef.current = true;
        if (!turnActiveRef.current) clearStreaming();
      } else {
        setMessages((prev) => {
          const next = [...prev];
          const byId = new Map(next.map((r, i) => [r.id, i] as const));
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
          for (const row of pageRows) {
            const existingIdx = byId.get(row.id);
            if (existingIdx !== undefined) {
              // A redelivery of a row already on screen — reconcile an
              // optimistic send's chrome (drop the spinner), otherwise a no-op.
              const existing = next[existingIdx];
              if (existing.role === "user" && existing.sendState !== undefined) {
                next[existingIdx] = { ...existing, sendState: undefined };
              }
              continue;
            }
            if (row.role === "assistant") closeTrailingWork();
            next.push(row);
            byId.set(row.id, next.length - 1);
          }
          return next;
        });
        if (pageRows.some((r) => r.role === "assistant")) clearStreaming();
      }
      advanceCursorFromSync(frame.next_cursor, frame.rebased);
      markReadIfAdvanced();
    },
    [advanceCursorFromSync, markReadIfAdvanced],
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
          // text and ends the turn's work block. It is also a turn-END signal
          // (record its identity), and — if the cursor is rebase-dirty — the
          // trigger for the follow-up sync that closes the dirty window.
          closeWork();
          clearStreaming();
          recordEndedTurn(activeTurnStart.current);
          activeTurnStart.current = null;
          if (rebaseDirty.current) runSync();
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
        if (frame.active) {
          activeTurnStart.current = frame.started_at ? Date.parse(frame.started_at) : null;
        } else {
          closeWork();
          recordEndedTurn(activeTurnStart.current);
          activeTurnStart.current = null;
          // A turn ending on a rebase-dirty cursor triggers the follow-up sync
          // that closes the dirty window (mirrors the final-Message path).
          if (rebaseDirty.current) runSync();
        }
        break;
      case "subscribe_state":
        // The one atomic state-plane bundle. iOS surfaces only the turn/work
        // halves (no approvals/tasks UI); staleness is judged by turn identity.
        if (frame.turn.active && frame.turn.started_at) {
          activeTurnStart.current = Date.parse(frame.turn.started_at);
        }
        applySubscribeState(frame.turn, frame.work_steps ?? []);
        break;
      case "gap":
        // Server-declared loss on this connection — run the one forward-recovery
        // pull. (`session_id` scoping is native's concern; the webview holds one
        // session, so any gap means "sync me".)
        runSync();
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
      case "sync_page":
        applySyncPage(frame);
        break;
      case "sync_failed":
        // The native chatFetchSync API call failed — unwind the in-flight
        // guard so the next trigger retries; the durable record is intact.
        syncInFlight.current = false;
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
    runSync();
  };

  // Native asked for a sync run (offscreen-buffer-overflow re-attach, or any
  // native-side "go sync" edge). Same one forward-recovery pull.
  const handleSyncRequested = useCallback(() => {
    runSync();
  }, [runSync]);

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
    handleConnEpoch,
    handleBottomInset,
    jumpToLatest,
    handleSyncRequested,
  });
  handlersRef.current = {
    handleFrame,
    handleUserSent,
    markFailed,
    handleConnEpoch,
    handleBottomInset,
    jumpToLatest,
    handleSyncRequested,
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
