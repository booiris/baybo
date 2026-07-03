import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  fetchHistory,
  imageObjectUrl,
  log,
  persistState,
  postOrdinal,
  subscribeTranscript,
  type UserSentPayload,
} from "./bridge";
import { uid, type ChatMsg, type PersistedState, type WireAttachment, type WireFrame, type WireMessage } from "./types";

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

/// Rebuild `ChatMsg[]` from a `HistoryPage`'s `wire::Message` rows. Mirrors the
/// live `case "message"` mapping; each row carries real `attachments` (blob
/// refs), so images render inline. Keyed by `platform_msg_id` (the live-path
/// key) when present, else the ordinal, so a row keeps a stable React key
/// across a scroll-up prepend.
function historyMessagesToChatMsgs(messages: WireMessage[]): ChatMsg[] {
  return messages.map((m) => ({
    id: m.platform_msg_id || (typeof m.ordinal === "number" ? `m${m.ordinal}` : uid()),
    role: m.role === "user" ? "user" : "assistant",
    content: m.content,
    attachments: m.attachments,
  }));
}

/// One image attachment in a bubble: downloads the blob via the bridge (cached
/// on device), wraps it in an object URL, shows a spinner while loading and a
/// tap-to-retry on failure. The old in-session previewUrl short-circuit is
/// gone — native previews don't cross the bridge, so a just-sent image renders
/// by fetching its own bytes back over requestImage (device-cached, so fast).
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
    imageObjectUrl(attachment.blob_id, attachment.mime_type)
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
  const [messages, setMessages] = useState<ChatMsg[]>(restored?.messages ?? []);
  const [streaming, setStreaming] = useState("");
  const [turnActive, setTurnActive] = useState(false);
  // Bumped by native on each successful (re)connect (setConnEpoch). Drives the
  // attachment auto-retry and replaces the old per-dial connGen guard.
  const [connEpoch, setConnEpoch] = useState(initialConnEpoch);
  const connEpochRef = useRef(initialConnEpoch);
  // platform_msg_ids already rendered (our optimistic sends + anything
  // restored), so the server's echo or a catch-up replay doesn't render twice.
  const sentIds = useRef<Set<string>>(
    new Set((restored?.messages ?? []).filter((m) => m.role === "user").map((m) => m.id)),
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
  // Serializes history requests AND tags the streamed `history_page` reply so
  // its handler knows whether to REPLACE (reset recovery) or PREPEND
  // (scroll-up). The epoch captured at request time lets a reply that arrives
  // under a different connection epoch be dropped as stale (the old connGen
  // guard). `null` = no history request in flight.
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
  // the log's onScroll; new content auto-scrolls only while pinned, so a reader
  // who scrolled up into history isn't yanked back down.
  const followRef = useRef(true);
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
    const el = logRef.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, []);

  // While pinned to the newest edge, keep it in view as content lands (rows,
  // stream deltas, the turn indicator) — pre-paint, so a bubble never paints
  // off-screen first. A scroll-up PREPEND is exempt even while pinned (a short
  // thread's "load earlier" tap): the anchor effect below owns that viewport
  // change — this effect is declared first so the armed anchor is still
  // visible.
  useLayoutEffect(() => {
    const el = logRef.current;
    if (el && followRef.current && !prependAnchor.current) {
      el.scrollTop = el.scrollHeight;
    }
  }, [messages, streaming, turnActive]);

  // The log's box shrinks/grows when SwiftUI resizes the webview (keyboard,
  // rotation); while pinned, hold the newest edge through those size changes
  // instead of letting the resize cover it.
  useEffect(() => {
    const el = logRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => {
      if (followRef.current) el.scrollTop = el.scrollHeight;
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // After a scroll-up PREPEND, restore the viewport so the content the user was
  // looking at stays put (the log is `flex-direction: column`, so inserting
  // older rows above the top would otherwise shove everything down). Runs
  // pre-paint keyed on `messages`; only acts when a prepend armed the anchor.
  useLayoutEffect(() => {
    const anchor = prependAnchor.current;
    const el = logRef.current;
    if (!anchor || !el) return;
    el.scrollTop = anchor.prevScrollTop + (el.scrollHeight - anchor.prevScrollHeight);
    prependAnchor.current = null;
  }, [messages]);

  const appendNotice = useCallback((text: string) => {
    setMessages((m) => [...m, { id: uid(), role: "notice", content: text }]);
  }, []);

  // Fire a transcript-history request through native. The page streams back
  // later as a `history_page` frame; `mode` tags it for that handler (REPLACE
  // for a reset, PREPEND for scroll-up), and the current epoch tags it against
  // late delivery across a reconnect. One at a time — returns `false` if a
  // request is already in flight (the caller then unwinds its own guards).
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
    if (older.length > 0 && logRef.current) {
      prependAnchor.current = {
        prevScrollHeight: logRef.current.scrollHeight,
        prevScrollTop: logRef.current.scrollTop,
      };
    }
    for (const m of older) {
      if (m.role === "user") sentIds.current.add(m.id);
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
  // `fetchHistory` over the live leg rebuilds the thread and reseeds the
  // cursors — no reconnect needed. If a request is already in flight (`false`)
  // — e.g. a scroll-up page — the reset can't ride it (that response is a
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

  // Load the next older page (scroll-up): fire a fetchHistory whose
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

  const setNewestOrdinal = (value: number | null) => {
    lastOrdinal.current = value;
    postOrdinal(value);
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
        // Advance the cursor first — even for our own echo we dedup below — so
        // a later reconnect doesn't re-replay this row. A `null` cursor (post
        // reset) takes the first real ordinal that lands.
        if (
          typeof frame.ordinal === "number" &&
          (lastOrdinal.current === null || frame.ordinal > lastOrdinal.current)
        ) {
          setNewestOrdinal(frame.ordinal);
        }
        const role = frame.role === "user" ? "user" : "assistant";
        if (role === "user" && frame.platform_msg_id && sentIds.current.has(frame.platform_msg_id)) {
          return; // our own message / already rendered
        }
        if (role === "user" && frame.platform_msg_id) {
          sentIds.current.add(frame.platform_msg_id);
        }
        setStreaming("");
        setMessages((m) => [
          ...m,
          {
            id: frame.platform_msg_id || uid(),
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
          setStreaming("");
          setMessages((m) => [...m, { id: uid(), role: "assistant", content: "", attachments }]);
        }
        break;
      case "answer_delta":
        setStreaming((s) => s + frame.text);
        break;
      case "turn_state":
        setTurnActive(frame.active);
        break;
      case "notice":
        appendNotice(frame.text);
        break;
      case "reset":
        // Native only forwards frames from its live leg, so unlike the old
        // per-dial channel there's no per-frame generation to check here;
        // `recoverFromReset` still self-guards a Reset burst.
        recoverFromReset();
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
          setNewestOrdinal(frame.newest_ordinal ?? 0);
          oldestOrdinal.current = frame.oldest_ordinal ?? null;
          setHasMoreOlder(frame.has_more);
          setStreaming("");
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
        break; // reasoning / tool progress / etc. not surfaced in the transcript
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
      },
    ]);
  };

  // Native (re)connected. Any history request in flight rode the old leg and is
  // abandoned (its late `history_page` is dropped by the epoch tag) — clear the
  // guards so a future fetch isn't blocked / the spinner isn't stuck / a queued
  // reset isn't stranded. The reconnect's own subscribe re-triggers a Reset if
  // the cursor is still stale, so a dropped queued reset self-heals.
  const handleConnEpoch = (epoch: number) => {
    connEpochRef.current = epoch;
    relayHistory.current = null;
    pendingReset.current = false;
    pagingRef.current = false;
    setLoadingOlder(false);
    setConnEpoch(epoch);
  };

  // Bridge events call the LATEST handlers through this ref (assigned each
  // render), so the subscription registers once without re-subscribing per
  // render.
  const handlersRef = useRef({ handleFrame, handleUserSent, handleConnEpoch });
  handlersRef.current = { handleFrame, handleUserSent, handleConnEpoch };
  useEffect(
    () =>
      subscribeTranscript({
        frame: (frameJson) => handlersRef.current.handleFrame(frameJson),
        connEpoch: (epoch) => handlersRef.current.handleConnEpoch(epoch),
        userSent: (payload) => handlersRef.current.handleUserSent(payload),
      }),
    [],
  );

  // Jump-to-latest: glide (not teleport) back to the newest edge, re-arming
  // following and hiding the button up front — onScroll holds both while the
  // glide flag is set. Landing normally settles via onScroll entering the
  // follow band; the cap timer settles a cancelled glide (see
  // GLIDE_SETTLE_CAP_MS).
  function jumpToLatest() {
    const el = logRef.current;
    if (!el) return;
    glidingRef.current = true;
    followRef.current = true;
    setShowJump(false);
    clearTimeout(glideTimer.current);
    glideTimer.current = setTimeout(() => {
      glidingRef.current = false;
      const logEl = logRef.current;
      if (!logEl) return;
      const follow =
        logEl.scrollHeight - logEl.scrollTop - logEl.clientHeight <= FOLLOW_BOTTOM_THRESHOLD_PX;
      followRef.current = follow;
      setShowJump(!follow);
    }, GLIDE_SETTLE_CAP_MS);
    el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
  }

  return (
    <>
      <div
        className="chat-log"
        ref={logRef}
        onScroll={() => {
          // Track whether the user sits at the newest edge (drives auto-follow
          // + the jump-to-latest button), and auto-load the next older page as
          // they near the top; `loadOlder` self-guards (in-flight, no-more,
          // no-cursor), so firing often is safe.
          const el = logRef.current;
          if (!el) return;
          const follow =
            el.scrollHeight - el.scrollTop - el.clientHeight <= FOLLOW_BOTTOM_THRESHOLD_PX;
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
        }}
      >
        {loadingOlder && <div className="bubble assistant muted">…</div>}
        {hasMoreOlder && !loadingOlder && (
          // Affordance for short threads that don't scroll (the onScroll path
          // covers the rest). Tapping pages the next older slice.
          <button className="load-older" onClick={() => loadOlder()}>
            {t("chat.loadOlder")}
          </button>
        )}
        {messages.map((m) => (
          <div key={m.id} className={`bubble ${m.role}`}>
            {m.attachments && m.attachments.length > 0 && (
              <AttachmentList attachments={m.attachments} connEpoch={connEpoch} />
            )}
            {m.content}
          </div>
        ))}
        {streaming && <div className="bubble assistant streaming">{streaming}</div>}
        {turnActive && !streaming && <div className="bubble assistant muted">…</div>}
      </div>
      {showJump && (
        <button
          type="button"
          className="jump-latest"
          // Cancelling pointerdown (and the mousedown WebKit still synthesizes)
          // keeps the tap from stealing focus, so the native composer keeps its
          // keyboard up while the log glides; click itself still fires.
          onPointerDown={(e) => e.preventDefault()}
          onMouseDown={(e) => e.preventDefault()}
          onClick={jumpToLatest}
          aria-label={t("chat.jumpToLatest")}
        >
          {/* Line-art down arrow. */}
          <svg
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.8"
            strokeLinecap="round"
            strokeLinejoin="round"
            aria-hidden="true"
          >
            <path d="M12 5v14" />
            <path d="M19 12l-7 7-7-7" />
          </svg>
        </button>
      )}
    </>
  );
}
