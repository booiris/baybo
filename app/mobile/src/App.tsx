import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useTranslation } from "react-i18next";
import { SUPPORTED_LANGUAGES } from "./i18n";
// PairChallenge / PairedSummary are generated from the src-tauri IPC structs by
// ts-rs (cargo test -p baybo-mobile-app --features ts-export); the bindings are
// regenerated + drift-checked by scripts/check-ts-bindings.sh.
import type { PairAborted } from "./generated/PairAborted";
import type { PairChallenge } from "./generated/PairChallenge";
import type { PairedSummary } from "./generated/PairedSummary";
import {
  attachmentKind,
  imageObjectUrl,
  uploadBytes,
  type ChatTransport,
  type WireAttachment,
} from "./blob";
import {
  directSessionCreate,
  directPushRegister,
  directHistory,
  type ChatSessionDetail,
} from "./direct";

/// Foreground signals (page visibility, window focus, the native `app-resumed`
/// event) can fire 2–3 times on a single iOS resume; coalesce them into one
/// reconnect so we don't open several content-join legs. The Rust shell also
/// guards against a concurrent dial, but debouncing here avoids queueing the work
/// at all.
const FOREGROUND_RECONNECT_DEBOUNCE_MS = 400;

/// Backoff before retrying after an unsolicited content-session drop (the Rust
/// shell's `content-disconnected` event) or a failed dial. Long enough to let a
/// bounced gateway re-arm its relay control leg, short enough that chat recovers
/// on its own without the user backgrounding/foregrounding the app. The pending
/// retry is coalesced (one at a time) so a persistently-down gateway is polled
/// at this cadence, not in a tight loop.
const RECONNECT_BACKOFF_MS = 2000;

/// Rows per transcript-history fetch — both the reset-recovery refetch (newest
/// page) and a scroll-up older page. Matches the gateway's default page size
/// (server-clamped to 1..200), so one fetch recovers/loads up to 50 rows.
const HISTORY_PAGE_LIMIT = 50;

/// How close to the top of the chat log (px) triggers a scroll-up fetch of the
/// next older page. A small band so the load fires just before the user hits the
/// very top, hiding the round-trip.
const SCROLL_TOP_THRESHOLD_PX = 64;

/// Parse a `baybo://pair?h=<relay>&r=<rendezvous-id>&s=<secret>&k=<remote-api-key>`
/// QR payload. Both `r` (public rendezvous id) and `s` (the 256-bit secret, the
/// Noise PSK) are required — there is no typeable fallback, because a short
/// secret would be offline-crackable by a hostile relay. Pairing is relay-only:
/// the app joins the proxy rendezvous (`/pair/join/<rendezvous-id>`) presenting
/// `k` as the relay admission key.
function parseScan(text: string): {
  endpoint?: string;
  rendezvousId: string;
  secret: string;
  remoteApiKey?: string;
} | null {
  try {
    const url = new URL(text);
    if (url.protocol === "baybo:") {
      const h = url.searchParams.get("h") ?? undefined;
      const r = url.searchParams.get("r");
      const s = url.searchParams.get("s");
      const k = url.searchParams.get("k") ?? undefined;
      if (r && s) return { endpoint: h, rendezvousId: r, secret: s, remoteApiKey: k };
    }
  } catch {
    /* not a pairing URL */
  }
  // No manual/short-code fallback: a pairing QR must carry both the rendezvous
  // id and the high-entropy secret.
  return null;
}

/// Light haptic tap fired on a primary button press, so the brutalist "push"
/// has a physical beat to it. Best-effort: haptics are unavailable on desktop /
/// dev, so a failure is swallowed.
async function tapHaptic() {
  try {
    const { impactFeedback } = await import("@tauri-apps/plugin-haptics");
    await impactFeedback("light");
  } catch {
    /* haptics unavailable (e.g. desktop) — non-fatal */
  }
}

/// One durable message row's fields, shared by a live `Frame::Message` (where
/// they sit next to `kind: "message"`) and a `Frame::HistoryPage` entry (a bare
/// `wire::Message`, no `kind`). Same shape either way.
type WireMessage = {
  content: string;
  role?: "user" | "assistant";
  platform_msg_id?: string;
  // The row's persisted ordinal — the catch-up cursor. Present on durable rows
  // (live final messages + replayed history).
  ordinal?: number;
  // Media references on the message (images the user sent or the agent produced);
  // the bytes are fetched lazily over a blob leg.
  attachments?: WireAttachment[];
};

/// A decrypted wire `Frame` as it arrives over the Tauri content channel.
/// MessagePack field names round-trip as snake_case JSON; we only model the few
/// variants the chat view renders and tolerate the rest.
type WireFrame =
  | ({ kind: "message" } & WireMessage)
  | { kind: "answer_delta"; text: string }
  | { kind: "turn_state"; active: boolean }
  | { kind: "notice"; level: string; text: string }
  | { kind: "reset"; reason: string }
  // Server → client backward transcript page, in response to `chat_fetch_history`
  // (the relay leg's REST-less recovery + scroll-up paging). `messages` are bare
  // `wire::Message` rows (ascending), carrying real attachment refs.
  | {
      kind: "history_page";
      messages: WireMessage[];
      oldest_ordinal?: number | null;
      newest_ordinal?: number | null;
      has_more: boolean;
    }
  // Server-pushed standalone media a tool produced mid-turn (its own bubble).
  | { kind: "attachment"; user_id?: string; attachments?: WireAttachment[] }
  // Frames we don't render (reasoning, tool progress, ping/pong, …) arrive with
  // other `kind`s and fall through the switch's `default`.
  | { kind: "other" };

type ChatMsg = {
  id: string;
  role: "user" | "assistant" | "notice";
  content: string;
  // Persisted alongside the text (no preview object URLs — those are in-session
  // only, kept in a ref); on restore the images re-download over a blob leg.
  attachments?: WireAttachment[];
};

// The gateway caps a single blob at 100 MiB (MAX_BLOB_BYTES); reject an oversize
// pick up front rather than read 100 MiB into memory only to be refused.
const MAX_ATTACHMENT_BYTES = 100 * 1024 * 1024;

// One picked image staged in the composer: shown instantly from a local object
// URL while its bytes upload over the blob leg in the background.
type StagedAttachment = {
  localId: string;
  filename: string;
  mime: string;
  size: number;
  previewUrl?: string;
  blobId?: string;
  status: "uploading" | "ready" | "error";
  error?: string;
};

// Pairing default for QR payloads that omit `h=`. The QR must still carry the
// rendezvous id and high-entropy secret.
const DEFAULT_ENDPOINT = "wss://proxy.baybo.space";

// The windowed scanner draws the camera behind the webview and exposes no
// "camera ready" signal, so going transparent up front would flash a black frame
// while the feed warms up. Instead we hold an opaque cover over the page for one
// AVCaptureSession startup, then reveal the camera + reticle together. This is a
// timed approximation — a true ready gate would need a native event from the
// plugin (it has none).
const CAMERA_WARMUP_MS = 300;

// Chat persistence. iOS reclaims a backgrounded WKWebView's content process (and
// can relaunch the app), which reloads the page and wipes React state. So the
// active chat is mirrored to localStorage (disk-backed — survives a reload and a
// cold start); on remount we restore it instantly, then reconnect and replay only
// the gap above `lastOrdinal`, so a background round-trip lands back in the live
// chat instead of the landing screen.
const CHAT_ACTIVE_KEY = "baybo.chat.active";
const CHAT_SESSION_KEY = "baybo.chat.session";
const CHAT_STATE_KEY = "baybo.chat.state";
// Which transport the active chat uses ("relay" | "direct") — persisted so a
// reload/relaunch restores the right one (the relay leg vs the direct WS).
const CHAT_MODE_KEY = "baybo.chat.mode";

// `lastOrdinal` is the WS catch-up cursor (the NEWEST edge): a real ordinal
// replays the gap above it, `0` backfills the whole thread, and `null` means
// "fresh subscribe, no catch-up". `oldestOrdinal` is the scroll-up paging cursor
// (the OLDEST loaded row); `null` = unknown / nothing older to page to.
type ChatState = {
  sessionId: string;
  messages: ChatMsg[];
  lastOrdinal: number | null;
  oldestOrdinal: number | null;
};

function loadChatState(sessionId: string): ChatState | null {
  try {
    const raw = localStorage.getItem(CHAT_STATE_KEY);
    if (!raw) return null;
    const s = JSON.parse(raw) as ChatState;
    // Ignore a thread persisted under a different session id (stale).
    if (s.sessionId !== sessionId || !Array.isArray(s.messages)) return null;
    return s;
  } catch {
    return null;
  }
}

function saveChatState(s: ChatState) {
  try {
    localStorage.setItem(CHAT_STATE_KEY, JSON.stringify(s));
  } catch {
    /* quota exceeded / storage disabled — persistence is best-effort */
  }
}

function clearChatState() {
  try {
    localStorage.removeItem(CHAT_ACTIVE_KEY);
    localStorage.removeItem(CHAT_SESSION_KEY);
    localStorage.removeItem(CHAT_STATE_KEY);
    localStorage.removeItem(CHAT_MODE_KEY);
  } catch {
    /* ignore */
  }
}

/// Rebuild `ChatMsg[]` from a relay `HistoryPage`'s `wire::Message` rows. Mirrors
/// the live `case "message"` mapping — and is RICHER than the direct/REST rebuild:
/// each row carries real `attachments` (blob refs), so images render inline with
/// no placeholder. A `wire::Message` has no `notice`/`work` distinction, so every
/// row is a bubble. Keyed by `platform_msg_id` (the live-path key) when present,
/// else the ordinal, so a row keeps a stable React key across a scroll-up prepend.
function historyMessagesToChatMsgs(messages: WireMessage[]): ChatMsg[] {
  return messages.map((m) => ({
    id: m.platform_msg_id || (typeof m.ordinal === "number" ? `m${m.ordinal}` : crypto.randomUUID()),
    role: m.role === "user" ? "user" : "assistant",
    content: m.content,
    attachments: m.attachments,
  }));
}

/// Rebuild `ChatMsg[]` from a direct REST `ChatSessionDetail.transcript`. `work`
/// items have no mobile concept (the live switch drops reasoning/tool progress);
/// `notice` items render as a notice bubble; `message` items as a bubble. REST
/// history carries only `has_attachments` (no blob refs), so media becomes a
/// `placeholder` rather than a silently-dropped image.
function transcriptToChatMsgs(
  transcript: ChatSessionDetail["transcript"],
  placeholder: string,
): ChatMsg[] {
  const out: ChatMsg[] = [];
  for (const item of transcript) {
    if (item.kind === "work") continue;
    if (item.kind === "notice") {
      out.push({ id: `n${item.ordinal}`, role: "notice", content: item.text });
      continue;
    }
    const role = item.role === "user" ? "user" : "assistant";
    const content = item.has_attachments
      ? item.text
        ? `${item.text}\n${placeholder}`
        : placeholder
      : item.text;
    out.push({ id: `m${item.ordinal}`, role, content });
  }
  return out;
}

/// Normalize a user-typed Baybo address for the direct-login form: trim, drop a
/// trailing slash, default to `https://` when no scheme is given, and validate it
/// parses as an http(s) URL with a host. Returns the normalized URL, or `null`
/// if it isn't a usable address (mirrors the Rust-side check for early feedback).
function normalizeBayboAddress(raw: string): string | null {
  // Detect the scheme on the trimmed-but-NOT-slash-stripped value, so a bare
  // "http://" (no host) is rejected rather than mangled into "https://http:".
  const trimmed = raw.trim();
  if (!trimmed) return null;
  const withScheme = /^[a-z][a-z0-9+.-]*:\/\//i.test(trimmed) ? trimmed : `https://${trimmed}`;
  try {
    const u = new URL(withScheme);
    if ((u.protocol !== "http:" && u.protocol !== "https:") || !u.hostname) return null;
    return withScheme.replace(/\/+$/, ""); // drop a trailing slash only after it validates
  } catch {
    return null;
  }
}

/// One image attachment in a bubble. Uses the in-session local object URL when we
/// have it (our own just-sent pick — instant, no round-trip); otherwise downloads
/// the blob over a blob leg (cached on device), wraps it in an object URL, and
/// shows a spinner while loading and a tap-to-retry on failure.
function AttachmentImage({
  attachment,
  previewUrl,
  transport,
  connEpoch,
}: {
  attachment: WireAttachment;
  previewUrl?: string;
  transport: ChatTransport;
  connEpoch: number;
}) {
  const { t } = useTranslation();
  const [url, setUrl] = useState<string | null>(previewUrl ?? null);
  const [failed, setFailed] = useState(false);
  const [attempt, setAttempt] = useState(0);
  // Mirrors `failed` for the connEpoch retry effect to read without taking `failed`
  // as a dep (which would refetch in a tight loop the instant a fetch fails).
  const failedRef = useRef(false);

  useEffect(() => {
    if (previewUrl) {
      setUrl(previewUrl);
      return;
    }
    let owned: string | null = null;
    let cancelled = false;
    failedRef.current = false;
    setFailed(false);
    setUrl(null);
    imageObjectUrl(attachment.blob_id, attachment.mime_type, transport)
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
    // We own only URLs we downloaded; never revoke the caller's previewUrl here.
    return () => {
      cancelled = true;
      if (owned) URL.revokeObjectURL(owned);
    };
  }, [attachment.blob_id, attachment.mime_type, previewUrl, attempt, transport]);

  // A restored image can race ahead of its leg going live — a direct chat's channel
  // token is only stashed once `chat_connect` completes, so an early fetch fails
  // with "no active direct session". Retry the moment a (re)connect lands instead
  // of stranding it on tap-to-load.
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

/// Render a message's attachments: images inline (via [`AttachmentImage`]), any
/// other kind as a labelled chip. `previews` maps a `blob_id` to an in-session
/// local object URL for images this client just sent.
function AttachmentList({
  attachments,
  previews,
  transport,
  connEpoch,
}: {
  attachments: WireAttachment[];
  previews: Map<string, string>;
  transport: ChatTransport;
  connEpoch: number;
}) {
  return (
    <div className="attachments">
      {attachments.map((a, i) =>
        a.kind === "image" ? (
          <AttachmentImage
            key={`${a.blob_id}-${i}`}
            attachment={a}
            previewUrl={previews.get(a.blob_id)}
            transport={transport}
            connEpoch={connEpoch}
          />
        ) : (
          <div key={`${a.blob_id}-${i}`} className="attachment-file">
            📎 {a.filename ?? a.mime_type}
          </div>
        ),
      )}
    </div>
  );
}

/// The post-pairing chat: opens a Noise content session for `sessionId`, renders
/// the agent's streamed reply, and sends user messages. Survives a background
/// round-trip: the thread is persisted, and the session reconnects + replays the
/// gap whenever the app returns to the foreground.
function ChatView({
  sessionId,
  transport,
  onClose,
}: {
  sessionId: string;
  transport: ChatTransport;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  // The chat commands are transport-agnostic: relay (Noise content leg) vs direct
  // (raw-MessagePack /v1/channel-ws) is selected by passing `leg: transport` to a
  // single backend command, which routes to the right session registry.
  // Parsed once on mount (lazy initializer), then reused for the initial state.
  const [restored] = useState(() => loadChatState(sessionId));
  const [messages, setMessages] = useState<ChatMsg[]>(restored?.messages ?? []);
  const [streaming, setStreaming] = useState("");
  const [turnActive, setTurnActive] = useState(false);
  const [input, setInput] = useState("");
  const [status, setStatus] = useState<string | null>(() => t("chat.connecting"));
  // Bumped on each successful (re)connect. A restored attachment image that failed
  // to load before its leg was live (e.g. a direct chat whose channel token isn't
  // stashed until `chat_connect` completes) re-fetches when this changes.
  const [connEpoch, setConnEpoch] = useState(0);
  // Bumped by `recoverFromReset` to force a fresh dial carrying the corrected
  // cursor (direct: the refetched tail; relay: null). It's a dep of the connect
  // effect, so bumping it re-runs the effect (teardown → re-dial) — distinct from
  // `connEpoch`, which the connect *success* bumps and so must NOT trigger a dial.
  const [reconnectNonce, setReconnectNonce] = useState(0);
  // platform_msg_ids already rendered (our optimistic sends + anything restored),
  // so the server's echo or a catch-up replay doesn't render them twice.
  const sentIds = useRef<Set<string>>(
    new Set((restored?.messages ?? []).filter((m) => m.role === "user").map((m) => m.id)),
  );
  // Highest durable ordinal rendered — the cursor a reconnect catches up from
  // (`null` after a relay reset = re-subscribe fresh with no catch-up).
  const lastOrdinal = useRef<number | null>(restored?.lastOrdinal ?? 0);
  // Lowest durable ordinal loaded — the scroll-up paging cursor (`before_ordinal`).
  // `null` = unknown / nothing older to page to. Set by every history load.
  const oldestOrdinal = useRef<number | null>(restored?.oldestOrdinal ?? null);
  // Whether older rows remain below the top — arms scroll-up. On restore we don't
  // know for sure, so assume there may be more whenever we have a paging cursor; a
  // fetch that returns `has_more: false` (or an empty page) disarms it.
  const [hasMoreOlder, setHasMoreOlder] = useState<boolean>(
    (restored?.oldestOrdinal ?? null) !== null,
  );
  // True while an older page is loading — one at a time, drives the top spinner.
  const [loadingOlder, setLoadingOlder] = useState(false);
  // True while a `Frame::Reset` recovery is in flight, so a burst of Resets
  // (back-pressure) doesn't stack concurrent refetches / reconnects.
  const recovering = useRef(false);
  // Serializes relay history requests over the live leg AND tags the streamed
  // `history_page` reply so its handler knows whether to REPLACE (reset recovery)
  // or PREPEND (scroll-up). `null` = no relay history request in flight.
  const relayHistory = useRef<{ mode: "reset" | "page" } | null>(null);
  // In-flight guard for an older-page load (both transports), so a scroll-event
  // burst fires one fetch. `loadingOlder` (state) drives the spinner; this ref is
  // the race-free gate.
  const pagingRef = useRef(false);
  // The chat-log scroll container — for scroll-to-top detection + anchor restore.
  const logRef = useRef<HTMLDivElement>(null);
  // Set just before a scroll-up PREPEND so the layout effect can re-anchor the
  // viewport (prepending above the top would otherwise jump the scroll position).
  const prependAnchor = useRef<{ prevScrollHeight: number; prevScrollTop: number } | null>(null);
  // Images picked but not yet sent (uploading or ready), shown in the composer.
  const [staged, setStaged] = useState<StagedAttachment[]>([]);
  const stagedRef = useRef<StagedAttachment[]>([]);
  const fileInputRef = useRef<HTMLInputElement>(null);
  // The composer is a textarea (Enter inserts a newline; the Send pill sends), so
  // it grows with its content up to the CSS max-height, then scrolls internally.
  const composerRef = useRef<HTMLTextAreaElement>(null);
  // blob_id -> in-session local object URL for images this client sent, so a sent
  // image renders instantly from the picked file instead of re-downloading it.
  const localPreviews = useRef<Map<string, string>>(new Map());

  useEffect(() => {
    stagedRef.current = staged;
  }, [staged]);

  // Auto-size the composer to its content: reset to one row, then grow to fit
  // (capped by the CSS max-height). Runs on every keystroke and after send clears it.
  useEffect(() => {
    const el = composerRef.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${el.scrollHeight}px`;
  }, [input]);

  // Revoke every local/staged preview URL on unmount (leaving the chat) so object
  // URLs held for instant render or unsent composer picks don't leak.
  useEffect(() => {
    const previews = localPreviews.current;
    return () => {
      const sentPreviewUrls = new Set(previews.values());
      for (const u of sentPreviewUrls) URL.revokeObjectURL(u);
      for (const stagedItem of stagedRef.current) {
        if (stagedItem.previewUrl && !sentPreviewUrls.has(stagedItem.previewUrl)) {
          URL.revokeObjectURL(stagedItem.previewUrl);
        }
      }
    };
  }, []);

  // Mirror the thread to disk on every change so a reload/relaunch restores it.
  useEffect(() => {
    saveChatState({
      sessionId,
      messages,
      lastOrdinal: lastOrdinal.current,
      oldestOrdinal: oldestOrdinal.current,
    });
  }, [sessionId, messages]);

  // After a scroll-up PREPEND, restore the viewport so the content the user was
  // looking at stays put (the log is `flex-direction: column`, so inserting older
  // rows above the top would otherwise shove everything down). Runs pre-paint
  // (layout effect) keyed on `messages`; only acts when a prepend armed the anchor.
  useLayoutEffect(() => {
    const anchor = prependAnchor.current;
    const el = logRef.current;
    if (!anchor || !el) return;
    el.scrollTop = anchor.prevScrollTop + (el.scrollHeight - anchor.prevScrollHeight);
    prependAnchor.current = null;
  }, [messages]);

  // Fire a relay transcript-history request over the live Noise leg. The page
  // can't ride the command's return value (relay has no REST), so it streams back
  // later as a `history_page` frame; `mode` tags it for that handler (REPLACE for
  // a reset, PREPEND for scroll-up). One at a time — returns `false` if a request
  // is already in flight (the caller then unwinds its own guards).
  const requestRelayHistory = useCallback(
    async (mode: "reset" | "page", beforeOrdinal: number | null): Promise<boolean> => {
      if (relayHistory.current) return false;
      relayHistory.current = { mode };
      try {
        await invoke("chat_fetch_history", {
          leg: transport,
          beforeOrdinal,
          limit: HISTORY_PAGE_LIMIT,
        });
        return true;
      } catch (e) {
        relayHistory.current = null;
        throw e;
      }
    },
    [transport],
  );

  // Prepend an older page above the current top (scroll-up paging), preserving the
  // viewport via `prependAnchor` (read by the layout effect after the DOM updates).
  // Paged rows are strictly older than the current oldest, so they can't overlap —
  // the id-set filter is just a safety net. Re-seeds `sentIds` so a later live echo
  // of an own message doesn't double-render.
  const prependOlder = useCallback(
    (older: ChatMsg[], newOldest: number | null, more: boolean) => {
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
    },
    [],
  );

  // Recover from a gateway `Frame::Reset` (catch-up gap over the replay cap, or
  // outbound back-pressure). Left unhandled this *loops*: the stale pre-gap cursor
  // goes back out on the next reconnect and overflows again. Both transports now
  // do a full backfill, by different routes:
  //
  //   * direct — refetch the newest transcript page over REST (`directHistory`),
  //     rebuild the thread, reseed the cursors, and reconnect so the live leg
  //     re-subscribes from the corrected tail (replays only *above* it → nothing →
  //     loop broken).
  //   * relay — no REST, so fire a `chat_fetch_history` over the live leg (which
  //     stays up across a Reset); `case "history_page"` rebuilds the thread and
  //     reseeds the cursors. No reconnect needed — the leg is already live.
  //
  // A re-entrancy guard keeps a burst of Resets (back-pressure) from stacking
  // concurrent refetches.
  const recoverFromReset = useCallback(async () => {
    if (recovering.current) return;
    recovering.current = true;
    try {
      if (transport === "direct") {
        const detail = await directHistory(sessionId);
        const rebuilt = transcriptToChatMsgs(detail.transcript, t("chat.attachmentPlaceholder"));
        // Seed from the real bounds (never the synthetic negatives on control
        // rows); set the refs before `setMessages` so the save effect reads them.
        lastOrdinal.current = detail.newest_ordinal ?? 0;
        oldestOrdinal.current = detail.oldest_ordinal ?? null;
        setHasMoreOlder(detail.has_more);
        setMessages(rebuilt);
        setStreaming("");
        saveChatState({
          sessionId,
          messages: rebuilt,
          lastOrdinal: lastOrdinal.current,
          oldestOrdinal: oldestOrdinal.current,
        });
        setStatus(null);
        setReconnectNonce((n) => n + 1);
      } else {
        // Relay: the rebuild happens in `case "history_page"` when the page lands.
        // If a request is already in flight (`false`), this Reset is coalesced.
        await requestRelayHistory("reset", null);
      }
    } catch (e) {
      setStatus(t("chat.recoverFailed", { error: String(e) }));
    } finally {
      recovering.current = false;
    }
    // `t` omitted from deps for the same reason as `connect` below: a language
    // switch must not re-create this (and through it `connect`) and tear the leg
    // down. The captured language is fine for these one-shot recovery strings.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId, transport, requestRelayHistory]);

  // Load the next older page (scroll-up). Direct pages over REST and prepends
  // inline; relay fires a `chat_fetch_history` whose `history_page` reply prepends
  // in the frame switch (and clears the guards there). `pagingRef` gates re-entry.
  const loadOlder = useCallback(async () => {
    if (pagingRef.current || !hasMoreOlder) return;
    const before = oldestOrdinal.current;
    if (before === null) return; // no cursor — can't page older
    pagingRef.current = true;
    setLoadingOlder(true);
    try {
      if (transport === "direct") {
        const detail = await directHistory(sessionId, before, HISTORY_PAGE_LIMIT);
        prependOlder(
          transcriptToChatMsgs(detail.transcript, t("chat.attachmentPlaceholder")),
          detail.oldest_ordinal ?? null,
          detail.has_more,
        );
        pagingRef.current = false;
        setLoadingOlder(false);
      } else {
        const fired = await requestRelayHistory("page", before);
        // If a request was already in flight, unwind — the `history_page` handler
        // clears the guards only for the request it actually serves.
        if (!fired) {
          pagingRef.current = false;
          setLoadingOlder(false);
        }
      }
    } catch (e) {
      pagingRef.current = false;
      setLoadingOlder(false);
      setStatus(t("chat.recoverFailed", { error: String(e) }));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId, transport, hasMoreOlder, requestRelayHistory, prependOlder]);

  // (Re)open the content session, replaying only the gap above what we've already
  // rendered. Re-run on every foreground: iOS suspends the WS (and may reclaim the
  // webview) in the background, so a resume needs a fresh connection to go live
  // again. Catch-up keys on the ordinal, so reconnecting when nothing changed
  // appends nothing — a cheap no-op.
  const connect = useCallback(() => {
    const channel = new Channel<WireFrame>();
    channel.onmessage = (frame) => {
      switch (frame.kind) {
        case "message": {
          // Advance the cursor first — even for our own echo we dedup below — so a
          // later reconnect doesn't re-replay this row. A `null` cursor (post relay
          // reset) takes the first real ordinal that lands.
          if (
            typeof frame.ordinal === "number" &&
            (lastOrdinal.current === null || frame.ordinal > lastOrdinal.current)
          ) {
            lastOrdinal.current = frame.ordinal;
          }
          const role = frame.role === "user" ? "user" : "assistant";
          if (
            role === "user" &&
            frame.platform_msg_id &&
            sentIds.current.has(frame.platform_msg_id)
          ) {
            return; // our own message / already rendered
          }
          if (role === "user" && frame.platform_msg_id) {
            sentIds.current.add(frame.platform_msg_id);
          }
          setStreaming("");
          setMessages((m) => [
            ...m,
            {
              id: frame.platform_msg_id || crypto.randomUUID(),
              role,
              content: frame.content,
              attachments: frame.attachments,
            },
          ]);
          break;
        }
        case "attachment":
          if (frame.attachments && frame.attachments.length > 0) {
            setStreaming("");
            setMessages((m) => [
              ...m,
              { id: crypto.randomUUID(), role: "assistant", content: "", attachments: frame.attachments },
            ]);
          }
          break;
        case "answer_delta":
          setStreaming((s) => s + frame.text);
          break;
        case "turn_state":
          setTurnActive(frame.active);
          break;
        case "notice":
          setMessages((m) => [
            ...m,
            { id: crypto.randomUUID(), role: "notice", content: frame.text },
          ]);
          break;
        case "reset":
          // The gateway gave up on catch-up (gap over cap) or hit outbound
          // back-pressure. Recover instead of just showing a status — otherwise
          // the stale pre-gap cursor goes back out on the next reconnect and
          // overflows again (the loop). See `recoverFromReset`.
          void recoverFromReset();
          break;
        case "history_page": {
          // The reply to a relay `chat_fetch_history`. `relayHistory.current.mode`
          // (set when we fired it) says whether this is a reset rebuild (REPLACE)
          // or a scroll-up page (PREPEND). Always clear the in-flight tag.
          const pending = relayHistory.current;
          relayHistory.current = null;
          const rows = historyMessagesToChatMsgs(frame.messages);
          if (pending?.mode === "page") {
            prependOlder(rows, frame.oldest_ordinal ?? null, frame.has_more);
            pagingRef.current = false;
            setLoadingOlder(false);
          } else {
            // Reset rebuild: REPLACE the thread with the newest page, reseed both
            // cursors, and clear any paging guards a coincident scroll-up left set.
            for (const m of frame.messages) {
              if (m.role === "user" && m.platform_msg_id) sentIds.current.add(m.platform_msg_id);
            }
            lastOrdinal.current = frame.newest_ordinal ?? 0;
            oldestOrdinal.current = frame.oldest_ordinal ?? null;
            setHasMoreOlder(frame.has_more);
            setStreaming("");
            setMessages(rows);
            saveChatState({
              sessionId,
              messages: rows,
              lastOrdinal: lastOrdinal.current,
              oldestOrdinal: oldestOrdinal.current,
            });
            setStatus(null);
            pagingRef.current = false;
            setLoadingOlder(false);
          }
          break;
        }
        default:
          break; // reasoning / tool progress / etc. not surfaced in mobile chat
      }
    };
    // A reconnect re-establishes the leg, so any relay history request that was
    // in flight on the old leg is abandoned (its `history_page` will never arrive)
    // — clear the guards so a future fetch isn't blocked / the spinner isn't stuck.
    relayHistory.current = null;
    pagingRef.current = false;
    setLoadingOlder(false);
    setStatus(t("chat.connecting"));
    // Returns the dial promise (rejections propagate) so the caller can back off
    // and retry — see `attemptConnect` in the mount effect. `leg: transport` picks
    // the active transport (relay vs direct) backend-side.
    //
    // Cursor (`sinceOrdinal`): the gateway replays rows ABOVE it, so a real ordinal
    // catches up the gap and `0` (the value when localStorage was evicted and we
    // have nothing rendered) backfills the whole thread instead of leaving the chat
    // blank until the next turn. A genuinely empty session replays nothing; an
    // over-cap history yields a `Frame::Reset` (handled by `recoverFromReset`,
    // which backfills via REST (direct) or a `chat_fetch_history` (relay)).
    return invoke("chat_connect", {
      leg: transport,
      sessionId,
      sinceOrdinal: lastOrdinal.current,
      onFrame: channel,
    }).then(() => {
      setStatus(null);
      setConnEpoch((e) => e + 1);
    });
    // `t` is intentionally omitted from deps: re-creating `connect` on a language
    // switch would tear down + re-dial the live session. Status strings set by a
    // later reconnect simply use the language captured here. `recoverFromReset` /
    // `prependOlder` share `connect`'s deps (or are stable), so it's stable across
    // all other renders.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId, transport, recoverFromReset, prependOlder]);

  useEffect(() => {
    let timer: ReturnType<typeof setTimeout> | undefined;
    let backoff: ReturnType<typeof setTimeout> | undefined;

    // Dial, and on failure back off and retry. Every connect path funnels through
    // here so a failed dial (gateway offline / mid-reconnect) self-heals instead
    // of stranding the chat on a "Connect failed" message until the next
    // foreground.
    const attemptConnect = () => {
      connect().catch((e) => {
        setStatus(t("chat.connectFailed", { error: String(e) }));
        scheduleReconnect();
      });
    };
    // Backoff retry, coalesced to one pending attempt so a down gateway is polled
    // at `RECONNECT_BACKOFF_MS`, never in a tight loop.
    function scheduleReconnect() {
      if (backoff) return;
      backoff = setTimeout(() => {
        backoff = undefined;
        attemptConnect();
      }, RECONNECT_BACKOFF_MS);
    }

    attemptConnect(); // first connect is immediate — no foreground burst to coalesce yet

    // iOS suspends the app without ever marking the WKWebView page hidden, so the
    // page's own `visibilitychange` never fires on resume — the chat would keep
    // using a relay leg the OS froze. Reconnect on every foreground signal we can
    // get: page visibility (covers a reloaded webview and dev/browser), window
    // `focus`, and — the reliable one on iOS — the native `app-resumed` event the
    // Rust shell emits from `RunEvent::Resumed`. A single resume fires several of
    // these, so debounce them into one reconnect.
    const scheduleConnect = () => {
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => {
        timer = undefined;
        attemptConnect();
      }, FOREGROUND_RECONNECT_DEBOUNCE_MS);
    };
    const onVisible = () => {
      if (document.visibilityState === "visible") scheduleConnect();
    };
    document.addEventListener("visibilitychange", onVisible);
    window.addEventListener("focus", scheduleConnect);
    let unlisten: (() => void) | undefined;
    let unlistenDropped: (() => void) | undefined;
    let cancelled = false;
    listen("app-resumed", () => scheduleConnect())
      .then((un) => {
        // The effect may have already torn down before listen() resolved.
        if (cancelled) un();
        else unlisten = un;
      })
      .catch(() => {});
    // The live content session ended on its own (socket dropped, liveness lapse,
    // a remote-host restart) — the Rust pump emits this only on an UNsolicited
    // exit (a deliberate reconnect/disconnect aborts the task instead). Reconnect
    // with catch-up so chat recovers without a foreground round-trip.
    listen<string>("content-disconnected", (event) => {
      if (event.payload === sessionId) scheduleReconnect();
    })
      .then((un) => {
        if (cancelled) un();
        else unlistenDropped = un;
      })
      .catch(() => {});
    return () => {
      cancelled = true;
      if (timer) clearTimeout(timer);
      if (backoff) clearTimeout(backoff);
      document.removeEventListener("visibilitychange", onVisible);
      window.removeEventListener("focus", scheduleConnect);
      unlisten?.();
      unlistenDropped?.();
      invoke("chat_disconnect", { leg: transport }).catch(() => {});
    };
    // `reconnectNonce` is a dep so a `Frame::Reset` recovery (which bumps it after
    // fixing the cursor) tears the leg down and re-dials with the corrected cursor.
  }, [connect, reconnectNonce]);

  // Pick one or more images: stage each (instant local preview) and upload its
  // bytes over the blob leg in the background; the staged entry flips to "ready"
  // with its blob_id once the upload lands.
  async function onPickFiles(e: React.ChangeEvent<HTMLInputElement>) {
    const files = Array.from(e.target.files ?? []);
    e.target.value = ""; // let the same file be re-picked (re-fires `change`)
    for (const file of files) {
      const localId = crypto.randomUUID();
      const mime = file.type || "application/octet-stream";
      if (file.size > MAX_ATTACHMENT_BYTES) {
        setStaged((s) => [
          ...s,
          { localId, filename: file.name, mime, size: file.size, status: "error", error: t("chat.tooLarge") },
        ]);
        continue;
      }
      const previewUrl = mime.startsWith("image/") ? URL.createObjectURL(file) : undefined;
      setStaged((s) => [
        ...s,
        { localId, filename: file.name, mime, size: file.size, previewUrl, status: "uploading" },
      ]);
      try {
        const blobId = await uploadBytes(await file.arrayBuffer(), mime, transport);
        if (previewUrl) localPreviews.current.set(blobId, previewUrl);
        setStaged((s) =>
          s.map((a) => (a.localId === localId ? { ...a, blobId, status: "ready" } : a)),
        );
      } catch (err) {
        setStaged((s) =>
          s.map((a) => (a.localId === localId ? { ...a, status: "error", error: String(err) } : a)),
        );
      }
    }
  }

  function removeStaged(localId: string) {
    setStaged((s) => {
      const found = s.find((a) => a.localId === localId);
      // Only revoke a preview that didn't make it into localPreviews (still here =
      // never sent), so we don't pull the URL out from under a rendered bubble.
      if (found?.previewUrl && !found.blobId) URL.revokeObjectURL(found.previewUrl);
      return s.filter((a) => a.localId !== localId);
    });
  }

  async function send() {
    const text = input.trim();
    const ready = staged.filter((a) => a.status === "ready" && a.blobId);
    if (!text && ready.length === 0) return;
    if (staged.some((a) => a.status === "uploading")) {
      setStatus(t("chat.waitingUpload"));
      return;
    }
    const msgId = crypto.randomUUID();
    const attachments: WireAttachment[] = ready.map((a) => ({
      kind: attachmentKind(a.mime),
      blob_id: a.blobId as string,
      mime_type: a.mime,
      size: a.size,
      filename: a.filename,
    }));
    sentIds.current.add(msgId);
    setMessages((m) => [
      ...m,
      { id: msgId, role: "user", content: text, attachments: attachments.length ? attachments : undefined },
    ]);
    setInput("");
    // Sent images keep their preview URL alive via localPreviews (the bubble
    // renders from it); free any staged preview that never made it into a send.
    for (const a of staged) {
      if (a.previewUrl && !a.blobId) URL.revokeObjectURL(a.previewUrl);
    }
    setStaged([]);
    try {
      await invoke("chat_send", { leg: transport, text, msgId, attachments });
    } catch (e) {
      setStatus(t("chat.sendFailed", { error: String(e) }));
    }
  }

  return (
    <main className="screen chat">
      <div className="chat-header">
        <button onClick={onClose}>← {t("chat.back")}</button>
        <h1>{t("chat.title")}</h1>
      </div>
      <div
        className="chat-log"
        ref={logRef}
        onScroll={() => {
          // Auto-load the next older page as the user nears the top; `loadOlder`
          // self-guards (in-flight, no-more, no-cursor), so firing often is safe.
          const el = logRef.current;
          if (el && el.scrollTop <= SCROLL_TOP_THRESHOLD_PX) void loadOlder();
        }}
      >
        {loadingOlder && <div className="bubble assistant muted">…</div>}
        {hasMoreOlder && !loadingOlder && (
          // Affordance for short threads that don't scroll (the onScroll path
          // covers the rest). Tapping pages the next older slice.
          <button className="load-older" onClick={() => void loadOlder()}>
            {t("chat.loadOlder")}
          </button>
        )}
        {messages.map((m) => (
          <div key={m.id} className={`bubble ${m.role}`}>
            {m.attachments && m.attachments.length > 0 && (
              <AttachmentList
                attachments={m.attachments}
                previews={localPreviews.current}
                transport={transport}
                connEpoch={connEpoch}
              />
            )}
            {m.content}
          </div>
        ))}
        {streaming && <div className="bubble assistant streaming">{streaming}</div>}
        {turnActive && !streaming && <div className="bubble assistant muted">…</div>}
      </div>
      {status && <p className="status">{status}</p>}
      {staged.length > 0 && (
        <div className="staged">
          {staged.map((a) => (
            <div key={a.localId} className={`staged-item ${a.status}`}>
              {a.previewUrl ? (
                <img src={a.previewUrl} alt={a.filename} />
              ) : (
                <span className="staged-file">📎</span>
              )}
              {a.status === "uploading" && <span className="staged-badge">…</span>}
              {a.status === "error" && (
                <span className="staged-badge err" title={a.error}>
                  !
                </span>
              )}
              <button className="staged-remove" onClick={() => removeStaged(a.localId)} aria-label={t("chat.remove")}>
                ×
              </button>
            </div>
          ))}
        </div>
      )}
      <div className="row composer">
        <input
          ref={fileInputRef}
          type="file"
          accept="image/*"
          className="hidden"
          onChange={onPickFiles}
        />
        <button className="attach" onClick={() => fileInputRef.current?.click()} aria-label={t("chat.addImage")}>
          ＋
        </button>
        <textarea
          ref={composerRef}
          value={input}
          onChange={(e) => setInput(e.target.value)}
          placeholder={t("chat.placeholder")}
          rows={1}
        />
        <button
          onClick={send}
          disabled={
            staged.some((a) => a.status === "uploading") ||
            (!input.trim() && !staged.some((a) => a.status === "ready" && a.blobId))
          }
        >
          {t("chat.send")}
        </button>
      </div>
    </main>
  );
}

/// The language toggle shown top-right on the landing: a line-art globe + the
/// current language's short code; tapping advances to the next supported
/// language (i18next persists the choice to localStorage).
function LangSwitcher() {
  const { t, i18n } = useTranslation();
  const idx = SUPPORTED_LANGUAGES.findIndex((l) => l.code === i18n.resolvedLanguage);
  const current = SUPPORTED_LANGUAGES[idx >= 0 ? idx : 0];
  const next = SUPPORTED_LANGUAGES[(idx + 1) % SUPPORTED_LANGUAGES.length];
  return (
    <button
      className="lang-switch"
      onClick={() => void i18n.changeLanguage(next.code)}
      // Include the visible label (current.short) in the accessible name so it
      // satisfies WCAG Label-in-Name (voice control), then state the action.
      aria-label={`${t("lang.label")}: ${current.short} → ${next.label}`}
    >
      <svg
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
        strokeLinecap="round"
        strokeLinejoin="round"
        aria-hidden="true"
      >
        <circle cx="12" cy="12" r="9" />
        <path d="M3 12h18" />
        <path d="M12 3c2.6 2.6 2.6 15.4 0 18M12 3c-2.6 2.6-2.6 15.4 0 18" />
      </svg>
      <span>{current.short}</span>
    </button>
  );
}

export default function App() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [challenge, setChallenge] = useState<PairChallenge | null>(null);
  const [paired, setPaired] = useState<PairedSummary | null>(null);
  // On launch, a persisted pairing means we can skip straight to "connected".
  const [rememberedDevice, setRememberedDevice] = useState<string | null>(null);
  // Direct (non-relay) connection: the gateway base URL once connected (null =
  // not in direct mode), the landing's method view, and the login form fields.
  const [directConnected, setDirectConnected] = useState<{ baseUrl: string } | null>(null);
  const [landingView, setLandingView] = useState<"menu" | "direct">("menu");
  const [directUrl, setDirectUrl] = useState("");
  const [directToken, setDirectToken] = useState("");
  // Whether the chat view is open, and the active session id. Both are restored
  // from localStorage so a backgrounded webview reload / app relaunch drops the
  // user straight back into the live chat rather than the landing screen.
  const [chatting, setChatting] = useState(() => localStorage.getItem(CHAT_ACTIVE_KEY) === "1");
  const [sessionId, setSessionId] = useState(() => localStorage.getItem(CHAT_SESSION_KEY) ?? "");
  // Transport of the active/last chat, restored so a reload re-dials the right leg.
  const [chatTransport, setChatTransport] = useState<ChatTransport>(() =>
    localStorage.getItem(CHAT_MODE_KEY) === "direct" ? "direct" : "relay",
  );
  // QR scan flow. "scanning" makes the page transparent so the native camera
  // (drawn behind the webview) shows through; "success" plays a brief
  // confirmation beat on the app background, covering the camera teardown so the
  // jump straight into the handshake doesn't flash. `scanCancelled` suppresses
  // the error toast on a user cancel.
  const [scanPhase, setScanPhase] = useState<"idle" | "scanning" | "success">("idle");
  // Flips true once the camera has had a beat to warm up: drops the warm-up cover
  // and mounts the reticle, revealing the already-transparent live feed.
  const [cameraUp, setCameraUp] = useState(false);
  const scanCancelled = useRef(false);
  // One app binds one gateway, so re-pairing or unpairing is an explicit,
  // confirmed action. `null` = showing the normal connected actions; otherwise
  // the in-progress confirm for replacing or forgetting the current gateway.
  const [pendingAction, setPendingAction] = useState<null | "replace" | "forget">(null);

  useEffect(() => {
    invoke<string | null>("paired_device")
      .then((d) => setRememberedDevice(d))
      .catch(() => {});
    // A persisted direct connection skips straight to the direct "connected" view.
    invoke<{ base_url: string } | null>("direct_status")
      .then((d) => {
        if (d) setDirectConnected({ baseUrl: d.base_url });
      })
      .catch(() => {});
  }, []);

  // Direct-mode push: once connected, best-effort register this app's push
  // binding so a backgrounded direct chat can buzz. Re-tried on every foreground
  // because iOS can deliver the APNs token seconds after launch (after the first
  // register), and a registered token also needs re-asserting after a relaunch.
  // No-op server-side when the gateway has no `[push]` remote host configured.
  useEffect(() => {
    if (!directConnected) return;
    void directPushRegister();
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    listen("app-resumed", () => void directPushRegister())
      .then((un) => {
        if (cancelled) un();
        else unlisten = un;
      })
      .catch(() => {});
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [directConnected]);

  // Stay transparent for the whole scan so the camera (drawn behind the webview)
  // can show through. The warm-up cover below masks the not-yet-ready feed, so
  // this never reveals a black frame — and because the reveal is just dropping
  // that cover (a React element, in sync with paint) rather than toggling this
  // class in an effect (which paints one frame late), there's no white flash.
  useEffect(() => {
    document.documentElement.classList.toggle("scanning", scanPhase === "scanning");
    return () => document.documentElement.classList.remove("scanning");
  }, [scanPhase]);

  // Drop the cover (and mount the reticle) one warm-up beat after scanning
  // starts; the cleanup clears the pending timer and re-arms the cover the moment
  // scanning ends — on a scanned code, a cancel, or unmount.
  useEffect(() => {
    if (scanPhase !== "scanning") {
      setCameraUp(false);
      return;
    }
    const t = setTimeout(() => setCameraUp(true), CAMERA_WARMUP_MS);
    return () => clearTimeout(t);
  }, [scanPhase]);

  async function scan() {
    scanCancelled.current = false;
    try {
      const bs = await import("@tauri-apps/plugin-barcode-scanner");
      let perm: string = await bs.checkPermissions();
      if (perm === "prompt" || perm === "prompt-with-rationale") {
        perm = await bs.requestPermissions();
      }
      if (perm !== "granted") {
        setStatus(t("scan.permissionOff"));
        await bs.openAppSettings().catch(() => {});
        return;
      }
      // Go transparent only once permission is granted, right before the camera
      // opens behind the webview.
      setStatus(null);
      setScanPhase("scanning");
      const res = await bs.scan({ windowed: true, formats: [bs.Format.QRCode] });
      const parsed = parseScan(res.content);
      if (!parsed) {
        setStatus(t("scan.notBaybo"));
        return;
      }
      // Success: buzz, pop a green dot at the reticle centre, then briefly hold
      // and cross-fade out (same background, so no abrupt wipe).
      try {
        const { notificationFeedback } = await import("@tauri-apps/plugin-haptics");
        await notificationFeedback("success");
      } catch {
        // haptics unavailable (e.g. desktop) — non-fatal
      }
      setScanPhase("success");
      await new Promise((resolve) => setTimeout(resolve, 650));
      // Everything the handshake needs rides in on the QR — go straight into it
      // instead of dropping the user back on a form to review and confirm.
      await pairBegin({
        endpoint: parsed.endpoint ?? DEFAULT_ENDPOINT,
        rendezvousId: parsed.rendezvousId,
        secret: parsed.secret,
        remoteApiKey: parsed.remoteApiKey,
      });
    } catch (e) {
      setStatus(scanCancelled.current ? null : t("scan.failed", { error: String(e) }));
    } finally {
      setScanPhase("idle");
    }
  }

  async function cancelScan() {
    scanCancelled.current = true;
    try {
      const { cancel } = await import("@tauri-apps/plugin-barcode-scanner");
      await cancel();
    } catch {
      // already stopped
    }
    setScanPhase("idle");
  }

  // Connect and run the pairing handshake up to the confirmation-code step.
  // Called straight off a successful scan with the QR's endpoint/rendezvous id.
  async function pairBegin(opts: {
    endpoint: string;
    rendezvousId: string;
    secret: string;
    remoteApiKey?: string;
  }) {
    setBusy(true);
    setStatus(t("pair.connecting"));
    // The gateway can cancel pairing (the operator declined, or the link dropped)
    // while we sit on the confirm screen. Listen for that so the screen dismisses
    // itself instead of hanging until the user taps.
    const onAbort = new Channel<PairAborted>();
    onAbort.onmessage = (ev) => {
      setChallenge(null);
      setStatus(t("pair.cancelledReason", { reason: ev.reason }));
    };
    try {
      const c = await invoke<PairChallenge>("pair_begin", {
        endpoint: opts.endpoint,
        rendezvousId: opts.rendezvousId,
        secret: opts.secret,
        remoteApiKey: opts.remoteApiKey,
        onAbort,
      });
      setChallenge(c);
      setStatus(null);
    } catch (e) {
      setStatus(t("pair.failed", { error: String(e) }));
    } finally {
      setBusy(false);
    }
  }

  // Phase 2: send the user's decision; on accept the gateway finalizes once the
  // operator confirms too.
  async function confirmPair(accepted: boolean) {
    if (!challenge) return;
    setBusy(true);
    setStatus(accepted ? t("pair.confirming") : t("pair.cancelling"));
    try {
      if (accepted) {
        const summary = await invoke<PairedSummary>("pair_confirm", {
          deviceId: challenge.deviceId,
          accepted: true,
        });
        setPaired(summary);
        setChallenge(null);
        // A finalized relay pairing supersedes any direct connection (Rust deletes
        // the direct credentials at finalize). `directConnected` gates ahead of
        // `paired`, so clear it or the now-defunct direct screen would keep showing.
        setDirectConnected(null);
        setStatus(null);
      } else {
        // The decline path intentionally returns an error on the Rust side.
        await invoke("pair_confirm", { deviceId: challenge.deviceId, accepted: false }).catch(
          () => {},
        );
        setChallenge(null);
        setStatus(t("pair.cancelled"));
      }
    } catch (e) {
      setStatus(t("pair.failed", { error: String(e) }));
      setChallenge(null);
    } finally {
      setBusy(false);
    }
  }

  // Unpair: clear the keychain record + push key, then drop back to the scan
  // screen fully unpaired.
  async function forgetPairing() {
    setBusy(true);
    try {
      await invoke("forget_pairing");
      setStatus(null);
    } catch (e) {
      setStatus(t("connected.forgetFailed", { error: String(e) }));
    } finally {
      setBusy(false);
      setPendingAction(null);
      setPaired(null);
      setRememberedDevice(null);
      // No gateway → no chat: drop any persisted session so it can't resurrect.
      clearChatState();
      setSessionId("");
      setChatting(false);
    }
  }

  // Open a fresh chat (the previous thread is dropped), persisted with the active
  // flag + transport so a mid-chat reload restores *this* conversation on the
  // right leg. The relay path mints a client UUID; the direct path must use the
  // gateway-assigned session id from `direct_session_create`.
  async function openChat(transport: ChatTransport) {
    let id: string;
    if (transport === "direct") {
      setBusy(true);
      try {
        id = await directSessionCreate();
      } catch (e) {
        setStatus(t("direct.failed", { error: String(e) }));
        return;
      } finally {
        setBusy(false);
      }
    } else {
      id = crypto.randomUUID();
    }
    clearChatState();
    localStorage.setItem(CHAT_SESSION_KEY, id);
    localStorage.setItem(CHAT_ACTIVE_KEY, "1");
    localStorage.setItem(CHAT_MODE_KEY, transport);
    setChatTransport(transport);
    setSessionId(id);
    setChatting(true);
  }

  // Leave the chat (explicit Back): forget the persisted session so the next open
  // starts clean and a later reload won't resurrect a chat the user closed.
  function closeChat() {
    clearChatState();
    setSessionId("");
    setChatting(false);
  }

  // Direct (web-style) login: validate the gateway URL + admin token against
  // `/v1/status` (Rust), persist them, and drop into the direct "connected" view.
  async function directConnect() {
    const token = directToken.trim();
    if (!directUrl.trim() || !token || busy) return;
    // Validate + normalize the address up front (mirrors the Rust check) so a
    // typo gets immediate feedback instead of a confusing network error.
    const baseUrl = normalizeBayboAddress(directUrl);
    if (!baseUrl) {
      setStatus(t("direct.invalidUrl"));
      return;
    }
    setBusy(true);
    setStatus(t("direct.connecting"));
    try {
      const s = await invoke<{ base_url: string }>("direct_login", { baseUrl, token });
      setDirectToken(""); // don't retain the admin token in React state after use
      setStatus(null);
      setLandingView("menu");
      setDirectConnected({ baseUrl: s.base_url });
      // A direct login supersedes any relay pairing (Rust wipes it — see
      // direct::login -> relay::forget_pairing), so drop the relay React state.
      // Otherwise a later disconnect would fall through the render gates to a
      // stale relay "remembered" screen for a pairing that no longer exists.
      setRememberedDevice(null);
      setPaired(null);
      setPendingAction(null);
    } catch (e) {
      const msg = String(e);
      // `invalid_token` is a stable discriminator the Rust side returns for a 401
      // (see direct.rs) — matched here to show the localized message; any other
      // error string is surfaced verbatim.
      setStatus(msg === "invalid_token" ? t("direct.invalidToken") : t("direct.failed", { error: msg }));
    } finally {
      setBusy(false);
    }
  }

  // Direct "disconnect": tear down any live chat, wipe the stored credentials,
  // and return to the landing.
  async function directDisconnect() {
    setBusy(true);
    try {
      await invoke("direct_logout");
    } catch {
      /* best-effort — clear the UI regardless */
    }
    setDirectConnected(null);
    setDirectUrl("");
    setDirectToken("");
    setStatus(null);
    setBusy(false);
    // No connection → no chat: drop any persisted session so it can't resurrect.
    clearChatState();
    setSessionId("");
    setChatting(false);
  }

  // Re-pair: go to the scan screen to bind a different gateway. The current
  // pairing stays in the keychain until the new one finishes (replace-on-
  // success), so backing out of the scan leaves the existing binding intact.
  function startReplace() {
    setPendingAction(null);
    setPaired(null);
    setRememberedDevice(null);
  }

  // The actions on a "connected" screen (shared by the just-paired and the
  // remembered-on-launch views). One app binds one gateway, so the only ways
  // forward are: open the chat, replace this gateway, or forget it.
  function connectedActions() {
    if (pendingAction === "forget") {
      return (
        <div className="confirm-inline">
          <p className="muted">{t("connected.forgetConfirm")}</p>
          <div className="row">
            <button onClick={() => setPendingAction(null)} disabled={busy}>
              {t("pair.cancel")}
            </button>
            <button className="danger" onClick={forgetPairing} disabled={busy}>
              {t("connected.forget")}
            </button>
          </div>
        </div>
      );
    }
    if (pendingAction === "replace") {
      return (
        <div className="confirm-inline">
          <p className="muted">{t("connected.replaceConfirm")}</p>
          <div className="row">
            <button onClick={() => setPendingAction(null)} disabled={busy}>
              {t("pair.cancel")}
            </button>
            <button onClick={startReplace} disabled={busy}>
              {t("connected.replace")}
            </button>
          </div>
        </div>
      );
    }
    return (
      <div className="row">
        <button onClick={() => openChat("relay")}>{t("connected.openChat")}</button>
        <button onClick={() => setPendingAction("replace")}>{t("connected.replace")}</button>
        <button className="danger" onClick={() => setPendingAction("forget")}>
          {t("connected.forget")}
        </button>
      </div>
    );
  }

  if (chatting && sessionId) {
    return <ChatView sessionId={sessionId} transport={chatTransport} onClose={closeChat} />;
  }

  // Direct (non-relay) connection.
  if (directConnected) {
    return (
      <main className="screen screen-center">
        <LangSwitcher />
        <h1>{t("connected.title")}</h1>
        <p className="muted">{t("connected.directReady", { url: directConnected.baseUrl })}</p>
        <div className="row">
          <button onClick={() => openChat("direct")} disabled={busy}>
            {t("connected.openChat")}
          </button>
          <button className="danger" onClick={directDisconnect} disabled={busy}>
            {t("connected.disconnect")}
          </button>
        </div>
        {status && <p className="status">{status}</p>}
      </main>
    );
  }

  if (paired) {
    return (
      <main className="screen screen-center">
        <LangSwitcher />
        <h1>{t("connected.title")}</h1>
        <p className="muted">{t("connected.ready")}</p>
        <dl className="kv">
          <dt>{t("connected.rendezvous")}</dt>
          <dd>{paired.rendezvousId}</dd>
          <dt>{t("connected.relayNode")}</dt>
          <dd>{paired.relayNodeId || "—"}</dd>
        </dl>
        {connectedActions()}
        {status && <p className="status">{status}</p>}
      </main>
    );
  }

  if (challenge) {
    return (
      <main className="screen screen-center">
        <LangSwitcher />
        <h1>{t("pair.confirmTitle")}</h1>
        <p className="muted">{t("pair.confirmHint")}</p>
        <div className="confirm-code">{challenge.confirmCode}</div>
        <div className="row">
          <button onClick={() => confirmPair(false)} disabled={busy}>
            {t("pair.cancel")}
          </button>
          <button onClick={() => confirmPair(true)} disabled={busy}>
            {t("pair.pair")}
          </button>
        </div>
        {status && <p className="status">{status}</p>}
      </main>
    );
  }

  if (rememberedDevice) {
    return (
      <main className="screen screen-center">
        <LangSwitcher />
        <h1>{t("connected.title")}</h1>
        <p className="muted">{t("connected.remembered")}</p>
        {connectedActions()}
        {status && <p className="status">{status}</p>}
      </main>
    );
  }

  return (
    <>
      <main className="screen landing">
        <LangSwitcher />
        <div className="landing-hero">
          <h1 className="wordmark">Baybo</h1>
          <div className="rule" />
          {landingView === "menu" ? (
            <>
              <p className="muted landing-sub">{t("landing.subtitle")}</p>
              <button className="cta" onClick={scan} onPointerDown={tapHaptic} disabled={busy}>
                {t("landing.scan")}
              </button>
              <button
                className="cta cta-secondary"
                onClick={() => {
                  setStatus(null);
                  setLandingView("direct");
                }}
                onPointerDown={tapHaptic}
                disabled={busy}
              >
                {t("landing.direct")}
              </button>
              {status && <p className="status">{status}</p>}
            </>
          ) : (
            <div className="direct-form">
              <p className="muted landing-sub">{t("direct.hint")}</p>
              <label className="field">
                <span className="field-label">{t("direct.urlLabel")}</span>
                <input
                  type="url"
                  inputMode="url"
                  autoCapitalize="off"
                  autoCorrect="off"
                  spellCheck={false}
                  placeholder="https://…"
                  value={directUrl}
                  onChange={(e) => setDirectUrl(e.target.value)}
                />
              </label>
              <div className="field">
                {/* explicit htmlFor (not a wrapping label) so the hint below can
                    be a sibling — keeping it out of the input's accessible name. */}
                <label className="field-label" htmlFor="direct-token">
                  {t("direct.tokenLabel")}
                </label>
                <input
                  id="direct-token"
                  type="password"
                  autoCapitalize="off"
                  autoCorrect="off"
                  spellCheck={false}
                  aria-describedby="direct-token-hint"
                  placeholder={t("direct.tokenPlaceholder")}
                  value={directToken}
                  onChange={(e) => setDirectToken(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") directConnect();
                  }}
                />
                <span className="field-hint" id="direct-token-hint">{t("direct.tokenHint")}</span>
              </div>
              <button
                className="cta"
                onClick={directConnect}
                onPointerDown={tapHaptic}
                disabled={busy || !directUrl.trim() || !directToken.trim()}
              >
                {busy ? t("direct.connecting") : t("direct.connect")}
              </button>
              <button
                className="link-btn"
                onClick={() => {
                  setStatus(null);
                  setLandingView("menu");
                }}
                disabled={busy}
              >
                ← {t("direct.back")}
              </button>
              {status && <p className="status">{status}</p>}
            </div>
          )}
        </div>
        <p className="landing-foot">v{__APP_VERSION__}</p>
      </main>

      {scanPhase === "scanning" && !cameraUp && <div className="scan-warming" />}

      {scanPhase === "scanning" && cameraUp && (
        <div className="scan-overlay">
          <svg
            className="scan-reticle"
            viewBox="0 0 100 100"
            fill="none"
            stroke="#fff"
            strokeWidth="5"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <path d="M8 24 L8 22 Q8 8 22 8 L24 8" />
            <path d="M92 24 L92 22 Q92 8 78 8 L76 8" />
            <path d="M8 76 L8 78 Q8 92 22 92 L24 92" />
            <path d="M92 76 L92 78 Q92 92 78 92 L76 92" />
          </svg>
          <button className="scan-cancel" onClick={cancelScan}>
            {t("scan.cancel")}
          </button>
        </div>
      )}

      {scanPhase === "success" && (
        <div className="scan-panel">
          <span className="scan-dot" />
        </div>
      )}
    </>
  );
}
