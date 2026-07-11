import {
  createContext,
  memo,
  useCallback,
  useContext,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
  type ReactNode,
  type TouchEvent as ReactTouchEvent,
} from "react";
import { useTranslation } from "react-i18next";
import {
  audioSeek,
  audioToggle,
  blobObjectUrl,
  copyText,
  downloadFile,
  fetchHistory,
  log,
  onAudioState,
  onFileState,
  persistState,
  playVideo,
  postJumpVisible,
  postMarkRead,
  postRunState,
  postSyncRequest,
  previewFile,
  queryAudioState,
  queryFileState,
  requestVideoPoster,
  retrySend,
  viewImage,
  subscribeTranscript,
  type AudioStatePayload,
  type FileState,
  type UserSentPayload,
} from "./bridge";
import { MarkdownBody } from "./Markdown";
import { WorkBlockView } from "./WorkBlock";
import {
  blobContentDigest,
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
    return { id: item.id, role: "notice", content: item.text ?? "" };
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

/// Hard ceiling on the optimistic post-send run-state window (`awaitingReply`).
/// A real turn clears it far sooner — via its first output or its terminal frame
/// — so this only fires when BOTH were missed (a disconnect that hid the turn's
/// output and its close), un-sticking the composer's stop button. Well above any
/// realistic pre-first-token latency, so it never expires under a live turn.
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

/// How far outside the viewport (px, top + bottom) an image attachment begins
/// loading its blob — a preload band so an image is usually ready by the time it
/// scrolls in, while a back-history page's off-screen images stay unfetched. See
/// AttachmentImage.
const LAZY_IMAGE_ROOT_MARGIN = "400px 0px";

/// Cap on the remembered image sizes (see `ImageDimsStore`). An entry is ~60
/// bytes and a thread's images are bounded in practice — this only stops a
/// pathological session from growing the mirror without limit.
const MAX_IMAGE_DIMS = 512;

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

/// Recognise a `/stop` the way the gateway's parser does (leading `/`, first
/// token, tolerant of a `@bot` suffix / trailing args), so the client can drop
/// the command's user echo — the native stop button issues `/stop` as an
/// ordinary send and it must never render as a message bubble. Mirrors
/// app/web's `isStopCommand`.
function isStopCommand(text: string): boolean {
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
function isStopAckNotice(text: string): boolean {
  const t = text.trim();
  return t.startsWith("Stopped.") || t === "Nothing in progress to stop.";
}

function ordinalFromMessageId(id: string): number | null {
  const match = /^m(\d+)$/.exec(id);
  if (!match) return null;
  const n = Number(match[1]);
  return Number.isSafeInteger(n) ? n : null;
}

/// Durable ordinal out of a server row id — a `m<ordinal>` message OR a
/// `w<ordinal>` work block. `null` for a client-minted `uid()` (a live block)
/// or an `n<seq>` notice, neither of which carries an ordinal. Used to place a
/// re-delivered work block into its own turn during a sync-difference merge.
function rowOrdinal(id: string): number | null {
  const match = /^[mw](\d+)$/.exec(id);
  if (!match) return null;
  const n = Number(match[1]);
  return Number.isSafeInteger(n) ? n : null;
}

/// Identity of a work step for dedup when folding two representations of the
/// same turn's block: a tool step is keyed by its call id (stable across the
/// live vs reconstructed shapes); text steps by kind + text.
function workStepKey(s: WorkStep): string {
  return s.kind === "tool" ? `tool:${s.callId}` : `${s.kind}:${s.text}`;
}

/// Concatenate two work blocks' steps WITHOUT duplicating shared ones — so
/// folding a torn turn's disjoint halves appends cleanly, while folding two
/// overlapping representations of one turn (live + reconstructed) collapses to
/// a single copy instead of doubling every step.
function mergeWorkSteps(a: WorkStep[], b: WorkStep[]): WorkStep[] {
  const seen = new Set(a.map(workStepKey));
  const out = [...a];
  for (const s of b) {
    const k = workStepKey(s);
    if (!seen.has(k)) {
      seen.add(k);
      out.push(s);
    }
  }
  return out;
}

/// Freeze EVERY work row still marked `active` into its "Worked Xs" — walk the
/// whole thread, not just the tail. Called before appending/adopting a fresh
/// live block so the transcript never holds two open "Working" cards at once:
/// there is only ever one in-flight turn, hence one active block.
function freezeActiveWork(rows: Row[]): Row[] {
  return rows.map((r) =>
    r.role === "work" && r.active
      ? { ...r, active: false, elapsedMs: r.elapsedMs ?? (r.startedAt !== undefined ? Date.now() - r.startedAt : undefined) }
      : r,
  );
}

/// Fuse a client work block (`base` — live/restored: freshest streamed steps +
/// active state) with the server's reconstruction of the SAME turn (`recon` —
/// authoritative persisted steps + server-anchored timing). One block, not two:
/// union the steps, anchor `startedAt` to the server's true turn start, and take
/// the server's duration for the frozen label (while still active the live
/// ticker rules, so `elapsedMs` stays unset). Keeps `base`'s id/active so a live
/// block isn't remounted mid-stream.
function reconcileWork(base: WorkRow, recon: WorkRow): WorkRow {
  return {
    ...base,
    steps: mergeWorkSteps(base.steps, recon.steps),
    startedAt: recon.startedAt ?? base.startedAt,
    // Carry the server's authoritative duration even while active (the live
    // ticker ignores it until the block closes) so the frozen "Worked Xs" is the
    // server's number regardless of who closes the block first.
    elapsedMs: recon.elapsedMs ?? base.elapsedMs,
  };
}

/// Index in `rows` of the work block belonging to the SAME turn as a work row
/// of durable ordinal `ord` that has ended up ABOVE the turn's answer bubble.
/// Scan back over the trailing answer/notice run; accept the preceding work
/// block only when that run carries an answer ordinal-above `ord`, so a
/// genuinely later turn's block (its answer not yet on screen) is never
/// mis-folded. `-1` when there is no such block. Used to re-home a durable
/// progress `status` block the reopen path can strand below the reply.
function sameTurnWorkIndex(rows: Row[], ord: number): number {
  let j = rows.length - 1;
  let sawTurnAnswer = false;
  while (j >= 0) {
    const rj = rows[j];
    if (rj.role !== "assistant" && rj.role !== "notice") break;
    const oj = rowOrdinal(rj.id);
    if (rj.role === "assistant" && oj !== null && oj > ord) sawTurnAnswer = true;
    j--;
  }
  return sawTurnAnswer && j >= 0 && rows[j].role === "work" ? j : -1;
}

/// Restored rows re-enter with live-turn state INTACT: a work block that was
/// live at persist stays live ("working"), because exiting and re-entering
/// mid-turn — or before the agent's final reply — must NOT collapse it to
/// "worked". The buffered continuation frames extend that same block, and only
/// its terminal reply / turn-end closes it. `startedAt` is STRIPPED here: a
/// persisted client `Date.now()` anchor would make `now − startedAt` count all
/// the time the app was closed (an absurd "Worked 7h"); the next SubscribeState
/// / sync re-anchors it to the server's true turn start. A block that persisted
/// already-closed stays closed. Empty blocks have nothing to show; unknown
/// future roles are dropped. Also folds back a turn a mirror split in two.
function sanitizeRestoredRows(rows: Row[] | undefined): Row[] {
  const out: Row[] = [];
  for (const r of rows ?? []) {
    if (r.role === "work") {
      if (!Array.isArray(r.steps) || r.steps.length === 0) continue;
      // Heal a mirror split by the (now-fixed) re-entry bug: two work blocks
      // directly adjacent (NO message row between) are ONE turn torn apart — a
      // healthy turn always has a message between its block and the next, so
      // adjacency alone marks the tear, whether or not either half already
      // closed. Fold the whole run into one card, staying "working" if any piece
      // was still live (a turn with no final reply must not read as "worked");
      // the split's real duration was lost, so it stays untimed. Since the
      // `withOpenWork` fold-into-frozen-tail invariant now prevents minting a
      // fresh adjacency split, this only ever folds a LEGACY on-disk mirror
      // written by a pre-fix build (it re-persists as one row, so it fires once
      // per such session) — kept as defense-in-depth.
      const prev = out[out.length - 1];
      if (prev && prev.role === "work") {
        out[out.length - 1] = {
          ...prev,
          steps: mergeWorkSteps(prev.steps, r.steps),
          active: prev.active || r.active,
          startedAt: undefined,
          elapsedMs: undefined,
        };
      } else {
        // Heal a DIFFERENT persisted split: a durable progress block that a
        // prior build's reopen sync stranded AFTER its turn's answer bubble (so
        // it isn't adjacent to its own block — the adjacency heal above can't
        // reach it). Fold it back into the turn's pre-answer work block by
        // ordinal, so a mirror already corrupted by that bug self-corrects on
        // the next open instead of keeping the stray "Worked" card below the
        // reply forever (the reopen sync is a no-op once the cursor passed it).
        const ord = rowOrdinal(r.id);
        const at = ord !== null ? sameTurnWorkIndex(out, ord) : -1;
        const target = at >= 0 ? out[at] : undefined;
        if (target && target.role === "work") {
          out[at] = { ...target, steps: mergeWorkSteps(target.steps, r.steps), active: target.active || r.active };
        } else {
          out.push({ ...r, startedAt: undefined });
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

/// The natural pixel size of every image this thread has decoded, keyed by blob
/// digest and mirrored to disk with the rows (`PersistedState.imageDims`). A hit
/// means the image rendered here before — so its blob is on the device and its
/// box can be reserved at the exact final size before a single byte crosses the
/// bridge, which is what keeps a re-opened thread from resizing under the reader
/// (see `AttachmentBubble`). Carried on a context rather than props: the value's
/// identity is stable for the transcript's life, so recording a size re-renders
/// nothing and `MessageRow`'s memo survives.
type ImageDimsStore = {
  get(digest: string): [number, number] | undefined;
  record(digest: string, width: number, height: number): void;
};

const ImageDimsContext = createContext<ImageDimsStore | null>(null);

/// Rebuild the map from a restored mirror, dropping anything that isn't a usable
/// size — a zero or garbage dimension would poison the reserved box's ratio (CSS
/// divides by it), and the mirror is on-disk JSON, not a trusted type.
function restoreImageDims(
  raw: Record<string, [number, number]> | undefined,
): Map<string, [number, number]> {
  const out = new Map<string, [number, number]>();
  for (const [digest, dims] of Object.entries(raw ?? {})) {
    if (!Array.isArray(dims)) continue;
    const [w, h] = dims;
    if (typeof w !== "number" || typeof h !== "number") continue;
    if (!Number.isFinite(w) || !Number.isFinite(h) || w <= 0 || h <= 0) continue;
    out.set(digest, [w, h]);
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
  const imageDims = useContext(ImageDimsContext);
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
        // Tap opens the image full-screen in the native zoomable viewer
        // (`viewImage` → pinch, double-tap-to-restore). A button wraps the img
        // (not an onClick on it) so the wrapper stays put across the load — the
        // img never remounts and its onLoad fires once.
        <button
          type="button"
          className="attachment-open"
          onClick={() =>
            viewImage(attachment.blob_id, attachment.filename ?? "", attachment.mime_type)
          }
          aria-label={t("chat.viewImage")}
        >
          <img
            className="attachment-img"
            src={url}
            alt={attachment.filename ?? t("chat.imageAlt")}
            decoding="async"
            draggable={false}
            onLoad={(e) => {
              // Remember the decoded size: it's what lets the NEXT open of this
              // thread reserve this image's exact box up front instead of
              // flashing a loading tile and then resizing (see
              // `AttachmentBubble`). A zero dimension is never recorded — the
              // reserved box divides by it.
              const { naturalWidth: w, naturalHeight: h } = e.currentTarget;
              if (w > 0 && h > 0) {
                imageDims?.record(blobContentDigest(attachment.blob_id), w, h);
              }
              setLoaded(true);
            }}
            onError={() => setFailed(true)}
          />
        </button>
      )}
    </div>
  );
}

/// One attachment on its OWN bubble — a lazy-loaded image tile or a named file
/// chip, never sharing the text bubble. `children` carries the send-state chrome
/// when this is a user message's last bubble (an image-only send).
///
/// An image whose size this thread already knows (`ImageDimsStore` — it decoded
/// here before, so its blob is on the device) is `sized`: the bubble reserves the
/// image's EXACT final box from the first paint, and the loading tile is dropped
/// (`.attachment-bubble.sized` in styles.css). Nothing under it moves when the
/// bytes land — an already-downloaded image no longer resizes the page, which is
/// what shook a re-opened thread as each 12rem tile released to its real height.
/// The box lives on the BUBBLE and not on the frame inside it: the frame's
/// containing block is this bubble, a shrink-to-fit flex item, so a percentage
/// width there is cyclic and resolves to zero.
///
/// Read once, at mount: a size recorded later belongs to an image that is already
/// painted at its natural size, and re-reading would resize the bubble underneath
/// it. The next open picks the entry up.
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
  const isImage = attachment.kind === "image";
  const isAudio = attachment.kind === "audio";
  const isVideo = isVideoAttachment(attachment);
  const imageDims = useContext(ImageDimsContext);
  const [sized] = useState(() =>
    isImage ? imageDims?.get(blobContentDigest(attachment.blob_id)) : undefined,
  );
  const classes = [
    "attachment-bubble",
    isImage ? "" : isVideo ? "video" : "file",
    sized ? "sized" : "",
    className ?? "",
  ]
    .filter(Boolean)
    .join(" ");
  // Bare numbers, no unit: the reserved box divides one by the other.
  const box = sized
    ? ({ "--img-w": String(sized[0]), "--img-h": String(sized[1]) } as CSSProperties)
    : undefined;
  return (
    <div className={classes} style={box}>
      {isImage ? (
        <AttachmentImage attachment={attachment} connEpoch={connEpoch} />
      ) : isVideo ? (
        <AttachmentVideo attachment={attachment} />
      ) : isAudio ? (
        <AttachmentAudio attachment={attachment} />
      ) : (
        <AttachmentFile attachment={attachment} />
      )}
      {children}
    </div>
  );
}

/// Video has no wire kind of its own — it rides `file` (the gateway buckets
/// only image/audio specially) — so the tile is elected by mime here.
function isVideoAttachment(attachment: WireAttachment): boolean {
  return attachment.kind === "file" && attachment.mime_type.startsWith("video/");
}

/// Binary units, and only as much precision as disambiguates: `812 B`,
/// `24 KB`, `2.3 MB`, `140 MB`.
function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const kb = bytes / 1024;
  if (kb < 1024) return `${Math.round(kb)} KB`;
  const mb = kb / 1024;
  return `${mb < 10 ? mb.toFixed(1) : Math.round(mb)} MB`;
}

/// A short, upper-case type badge. The filename's extension is the most
/// honest source (`.docx` beats the mime's
/// `vnd.openxmlformats-officedocument.wordprocessingml.document`); fall back to
/// the mime subtype with its `+xml` suffix and `vnd.…` vendor path stripped.
function typeLabel(attachment: WireAttachment): string {
  const dot = attachment.filename?.lastIndexOf(".") ?? -1;
  const ext = dot > 0 ? attachment.filename?.slice(dot + 1) : undefined;
  if (ext && ext.length <= 4) return ext.toUpperCase();
  const subtype = attachment.mime_type.split("/")[1] ?? attachment.mime_type;
  const bare = subtype.split(";")[0].split("+")[0].split(".").pop() ?? "";
  return (bare || attachment.mime_type).toUpperCase();
}

/// How much of a long filename's tail always survives. `…-Q3-final.pdf` is what
/// tells a reader this is the final and not the draft; a plain end-ellipsis
/// throws exactly that away.
const FILENAME_TAIL_CHARS = 10;

/// Split a name so CSS can ellipsize the head while the tail stays pinned.
/// Short names take the whole width and get no tail.
function splitForMiddleEllipsis(name: string): [string, string] {
  if (name.length <= FILENAME_TAIL_CHARS * 2) return [name, ""];
  return [name.slice(0, -FILENAME_TAIL_CHARS), name.slice(-FILENAME_TAIL_CHARS)];
}

/// A document with a folded corner — the file already on this device.
const GLYPH_FILE = (
  <>
    <path
      d="M11.6 2.6H5.8a1.7 1.7 0 0 0-1.7 1.7v11.4a1.7 1.7 0 0 0 1.7 1.7h8.4a1.7 1.7 0 0 0 1.7-1.7V6.9L11.6 2.6Z"
      stroke="currentColor"
      strokeWidth="1.2"
      strokeLinejoin="round"
    />
    <path d="M11.5 2.7v4.3h4.3" stroke="currentColor" strokeWidth="1.2" strokeLinejoin="round" />
  </>
);

/// An arrow into a tray — tap to fetch. Also what spins inside the ring while
/// the bytes stream, so the icon never jumps between states.
const GLYPH_DOWNLOAD = (
  <>
    <path
      d="M10 3.4v9.2M6.4 9.2 10 12.8l3.6-3.6"
      stroke="currentColor"
      strokeWidth="1.2"
      strokeLinecap="round"
      strokeLinejoin="round"
    />
    <path
      d="M4.4 14.6v1.2a1.4 1.4 0 0 0 1.4 1.4h8.4a1.4 1.4 0 0 0 1.4-1.4v-1.2"
      stroke="currentColor"
      strokeWidth="1.2"
      strokeLinecap="round"
    />
  </>
);

/// A play triangle, stroked like the rest of the glyph set (nothing filled).
const GLYPH_PLAY = (
  <path
    d="M7.6 5.1v9.8l7.6-4.9z"
    stroke="currentColor"
    strokeWidth="1.2"
    strokeLinejoin="round"
  />
);

/// Two pause bars.
const GLYPH_PAUSE = (
  <path
    d="M7.7 5.6v8.8M12.3 5.6v8.8"
    stroke="currentColor"
    strokeWidth="1.5"
    strokeLinecap="round"
  />
);

/// `m:ss` (`h:mm:ss` past an hour) — playback positions and video durations.
function formatTime(totalSeconds: number): string {
  const s = Math.max(0, Math.floor(totalSeconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = String(s % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${sec}` : `${m}:${sec}`;
}

/// One file attachment's on-device lifecycle. Native owns the truth (the blob
/// cache is a directory iOS may purge), so the card asks on every mount rather
/// than trusting a `ready` it saw before.
function useFileState(blobId: string): { state: FileState; loaded: number } {
  const [state, setState] = useState<FileState>("idle");
  const [loaded, setLoaded] = useState(0);

  useEffect(() => {
    const unsubscribe = onFileState(blobId, (payload) => {
      setState(payload.state);
      if (payload.state === "loading") setLoaded(payload.loaded ?? 0);
    });
    queryFileState(blobId);
    return unsubscribe;
  }, [blobId]);

  return { state, loaded };
}

/// A non-image attachment: a stroked glyph (never the 📎 emoji — it arrives
/// coloured and glossy, the one thing this monochrome system has no room for),
/// the filename middle-clipped on one line, and the type + size beneath. The
/// wire has carried `size` all along; nothing showed it.
///
/// Tapping an undownloaded file fetches it — the glyph becomes an indeterminate
/// ring and the size turns into a `1.2 MB / 2.3 MB` counter, which is where the
/// real progress lives. Tapping it once it's on disk opens the preview.
function AttachmentFile({ attachment }: { attachment: WireAttachment }) {
  const { state, loaded } = useFileState(attachment.blob_id);
  const type = typeLabel(attachment);
  // A nameless blob has nothing better to title itself with than its type, so
  // the meta line would only repeat it.
  const name = attachment.filename ?? type;
  const [head, tail] = splitForMiddleEllipsis(name);

  const meta =
    state === "loading"
      ? `${formatBytes(loaded)} / ${formatBytes(attachment.size)}`
      : attachment.filename
        ? `${type} · ${formatBytes(attachment.size)}`
        : formatBytes(attachment.size);

  const onTap = useCallback(() => {
    if (state === "loading") return;
    if (state === "ready") previewFile(attachment.blob_id, name, attachment.mime_type);
    else downloadFile(attachment.blob_id);
  }, [state, attachment.blob_id, attachment.mime_type, name]);

  return (
    <button type="button" className={`attachment-file ${state}`} onClick={onTap}>
      <span className="file-glyph-slot">
        <svg className="file-glyph" viewBox="0 0 20 20" fill="none" aria-hidden="true">
          {state === "ready" ? GLYPH_FILE : GLYPH_DOWNLOAD}
        </svg>
        {state === "loading" && <span className="file-spinner" aria-hidden="true" />}
      </span>
      <span className="file-text">
        <span className="file-name">
          <span className="file-name-head">{head}</span>
          {tail && <span className="file-name-tail">{tail}</span>}
        </span>
        <span className="file-meta">{meta}</span>
      </span>
    </button>
  );
}

/// Mirror of one blob's slice of the native audio engine (there is ONE player
/// app-wide — see `AudioPlayerCenter`). Subscribed by blob id like `fileState`,
/// so a 2 Hz position tick re-renders one card; the mount-time query resyncs a
/// card that appears mid-playback (session switch, thread reload).
function useAudioState(blobId: string): AudioStatePayload {
  const [audio, setAudio] = useState<AudioStatePayload>({
    blobId,
    state: "stopped",
    position: 0,
    duration: 0,
  });

  useEffect(() => {
    const unsubscribe = onAudioState(blobId, setAudio);
    queryAudioState(blobId);
    return unsubscribe;
  }, [blobId]);

  return audio;
}

/// The seek bar under a playing/paused track. A drag scrubs locally (the fill
/// follows the finger, not the engine) and commits one `audioSeek` on lift; the
/// committed value keeps rendering until the ENGINE's next push lands (native
/// answers a seek with an optimistic state, so that's near-immediate), because
/// falling back to the stale pre-seek `position` would snap the fill backwards
/// for the round trip. Pointer events stop at the bar so a scrub never toggles
/// the card under it; `touch-action: none` (CSS) keeps a horizontal drag from
/// scrolling the thread.
function AudioTrack({
  blobId,
  position,
  duration,
}: {
  blobId: string;
  position: number;
  duration: number;
}) {
  const barRef = useRef<HTMLDivElement | null>(null);
  const [scrub, setScrub] = useState<number | null>(null);
  const committed = useRef(false);

  useEffect(() => {
    if (committed.current) {
      committed.current = false;
      setScrub(null);
    }
  }, [position, duration]);

  const fracAt = (clientX: number): number => {
    const rect = barRef.current?.getBoundingClientRect();
    if (!rect || rect.width <= 0) return 0;
    return Math.min(1, Math.max(0, (clientX - rect.left) / rect.width));
  };

  const shown = scrub ?? (duration > 0 ? position / duration : 0);

  return (
    <div
      ref={barRef}
      className="audio-track"
      onPointerDown={(e) => {
        e.stopPropagation();
        committed.current = false;
        e.currentTarget.setPointerCapture(e.pointerId);
        setScrub(fracAt(e.clientX));
      }}
      onPointerMove={(e) => {
        if (scrub !== null && !committed.current) setScrub(fracAt(e.clientX));
      }}
      onPointerUp={(e) => {
        e.stopPropagation();
        if (scrub === null || committed.current) return;
        audioSeek(blobId, scrub * duration);
        committed.current = true;
      }}
      onPointerCancel={() => {
        committed.current = false;
        setScrub(null);
      }}
      onClick={(e) => e.stopPropagation()}
    >
      <span className="audio-track-fill" style={{ width: `${shown * 100}%` }} />
    </div>
  );
}

/// An audio attachment: the file card's layout with the glyph slot promoted to
/// a play/pause control once the bytes are on disk. The ENGINE is native
/// (AVPlayer on the device-cached blob, `audioToggle` over the bridge): bytes
/// never cross as base64, the ringer switch can't silence it, and playback
/// survives backing out of the chat — the card is only a mirror. Until
/// downloaded it behaves exactly like a file card (tap fetches, ring + byte
/// counter), so a history page of audio costs nothing until asked for.
function AttachmentAudio({ attachment }: { attachment: WireAttachment }) {
  const { t } = useTranslation();
  const { state, loaded } = useFileState(attachment.blob_id);
  const audio = useAudioState(attachment.blob_id);
  const type = typeLabel(attachment);
  const name = attachment.filename ?? type;
  const [head, tail] = splitForMiddleEllipsis(name);
  const playing = audio.state === "playing";
  // Once the engine has touched this track the meta line becomes time and the
  // scrubber appears; `stopped` (never played / ended / usurped) reads like a
  // fresh file card again.
  const engaged = state === "ready" && audio.state !== "stopped" && audio.duration > 0;

  // At rest the track's length rides the WIRE (`duration_ms`, probed at
  // attach time), so it shows before any byte is downloaded — the engine only
  // takes over the meta line once the track is engaged.
  const meta =
    state === "loading"
      ? `${formatBytes(loaded)} / ${formatBytes(attachment.size)}`
      : engaged
        ? `${formatTime(audio.position)} / ${formatTime(audio.duration)}`
        : [
            attachment.filename ? type : null,
            attachment.duration_ms != null ? formatTime(attachment.duration_ms / 1000) : null,
            formatBytes(attachment.size),
          ]
            .filter(Boolean)
            .join(" · ");

  const onTap = useCallback(() => {
    if (state === "loading") return;
    if (state === "ready") audioToggle(attachment.blob_id, name, attachment.mime_type);
    else downloadFile(attachment.blob_id);
  }, [state, attachment.blob_id, attachment.mime_type, name]);

  return (
    <button
      type="button"
      className={`attachment-file audio ${state}`}
      onClick={onTap}
      aria-label={playing ? t("chat.audioPause") : t("chat.audioPlay")}
    >
      <span className="file-glyph-slot">
        <svg className="file-glyph" viewBox="0 0 20 20" fill="none" aria-hidden="true">
          {state !== "ready" ? GLYPH_DOWNLOAD : playing ? GLYPH_PAUSE : GLYPH_PLAY}
        </svg>
        {state === "loading" && <span className="file-spinner" aria-hidden="true" />}
      </span>
      <span className="file-text">
        <span className="file-name">
          <span className="file-name-head">{head}</span>
          {tail && <span className="file-name-tail">{tail}</span>}
        </span>
        {engaged && (
          <AudioTrack
            blobId={attachment.blob_id}
            position={audio.position}
            duration={audio.duration}
          />
        )}
        <span className="file-meta">{meta}</span>
      </span>
    </button>
  );
}

/// Tile ratios stay within a band — an ultra-wide strip or a 9:16 portrait
/// column would blow the reading column open; the cover-fit poster absorbs the
/// difference as a crop.
const VIDEO_RATIO_DEFAULT = 16 / 9;
const VIDEO_RATIO_MIN = 3 / 4;

function clampVideoRatio(ratio: number): number {
  return Math.min(VIDEO_RATIO_DEFAULT, Math.max(VIDEO_RATIO_MIN, ratio));
}

/// Determinate download ring, centered on the video tile. `r` in a 36-unit
/// viewBox; the CSS rotates the start to 12 o'clock.
const VIDEO_RING_RADIUS = 14;
const VIDEO_RING_CIRCUMFERENCE = 2 * Math.PI * VIDEO_RING_RADIUS;

function VideoProgressRing({ fraction }: { fraction: number }) {
  const clamped = Math.min(1, Math.max(0, fraction));
  return (
    <span className="video-disc" aria-hidden="true">
      <svg className="video-ring" viewBox="0 0 36 36">
        <circle className="video-ring-rail" cx="18" cy="18" r={VIDEO_RING_RADIUS} />
        <circle
          className="video-ring-fill"
          cx="18"
          cy="18"
          r={VIDEO_RING_RADIUS}
          strokeDasharray={VIDEO_RING_CIRCUMFERENCE}
          strokeDashoffset={VIDEO_RING_CIRCUMFERENCE * (1 - clamped)}
        />
      </svg>
    </span>
  );
}

/// A video attachment: a fixed-width tile in the image idiom, not a file chip.
/// Undownloaded it's a blank surface with a centered download disc and the size
/// in the corner chip; while fetching, the disc becomes a DETERMINATE ring (the
/// attachment declares its total) and the chip counts bytes; once on disk,
/// native supplies a poster frame + duration (`requestVideoPoster`,
/// AVAssetImageGenerator over the bridge) and the disc becomes a play glyph —
/// tap hands the file to the native full-screen player. The poster's natural
/// size is recorded in `ImageDimsStore`, so a re-opened thread draws this tile
/// at the right ratio from the first paint.
function AttachmentVideo({ attachment }: { attachment: WireAttachment }) {
  const { t } = useTranslation();
  const { state, loaded } = useFileState(attachment.blob_id);
  const imageDims = useContext(ImageDimsContext);
  const digest = blobContentDigest(attachment.blob_id);
  const [dims, setDims] = useState(() => imageDims?.get(digest));
  const [poster, setPoster] = useState<string | null>(null);
  // Seeded from the WIRE (probed at attach time) so the length shows before a
  // byte is downloaded; the poster reply overwrites it with the local probe.
  const [durationMs, setDurationMs] = useState<number | null>(attachment.duration_ms ?? null);
  const name = attachment.filename ?? typeLabel(attachment);

  useEffect(() => {
    if (state !== "ready") return;
    let owned: string | null = null;
    let cancelled = false;
    requestVideoPoster(attachment.blob_id, name, attachment.mime_type)
      .then((p) => {
        if (cancelled) {
          URL.revokeObjectURL(p.url);
          return;
        }
        owned = p.url;
        setPoster(p.url);
        setDurationMs(p.durationMs);
        if (p.width > 0 && p.height > 0) {
          imageDims?.record(digest, p.width, p.height);
          setDims([p.width, p.height]);
        }
      })
      .catch(() => {
        // No poster is cosmetic — the blank tile still downloads and plays.
      });
    return () => {
      cancelled = true;
      if (owned) {
        URL.revokeObjectURL(owned);
        // The render must never reference the revoked URL: a `ready → failed`
        // flip (blob purged, download error) re-runs this effect, and keeping
        // the stale poster would paint a broken <img>. On unmount these are
        // no-ops. The duration falls back to the wire's value, not null — the
        // track's length is still true.
        setPoster(null);
        setDurationMs(attachment.duration_ms ?? null);
      }
    };
    // name/digest derive from the attachment; `state` flipping to ready is the
    // real clock here.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state, attachment.blob_id, attachment.mime_type]);

  const ratio = clampVideoRatio(dims ? dims[0] / dims[1] : VIDEO_RATIO_DEFAULT);
  const fraction = attachment.size > 0 ? loaded / attachment.size : 0;
  // Pre-download the chip pairs length with cost (`1:23 · 24 MB` — the size is
  // what a tap commits to); once the bytes are local only the length matters.
  const chip =
    state === "loading"
      ? `${formatBytes(loaded)} / ${formatBytes(attachment.size)}`
      : durationMs === null
        ? formatBytes(attachment.size)
        : state === "ready"
          ? formatTime(durationMs / 1000)
          : `${formatTime(durationMs / 1000)} · ${formatBytes(attachment.size)}`;

  const onTap = useCallback(() => {
    if (state === "loading") return;
    if (state === "ready") playVideo(attachment.blob_id, name, attachment.mime_type);
    else downloadFile(attachment.blob_id);
  }, [state, attachment.blob_id, attachment.mime_type, name]);

  return (
    <button
      type="button"
      className={`attachment-video ${state}${poster ? " has-poster" : ""}`}
      style={{ "--video-ar": String(ratio) } as CSSProperties}
      onClick={onTap}
      aria-label={state === "ready" ? t("chat.videoPlay") : t("chat.videoDownload")}
    >
      {poster && (
        <img className="video-poster" src={poster} alt="" draggable={false} aria-hidden="true" />
      )}
      <span className="video-overlay" aria-hidden="true">
        {state === "loading" ? (
          <VideoProgressRing fraction={fraction} />
        ) : (
          <span className="video-disc">
            <svg className="video-disc-glyph" viewBox="0 0 20 20" fill="none">
              {state === "ready" ? GLYPH_PLAY : GLYPH_DOWNLOAD}
            </svg>
          </span>
        )}
      </span>
      <span className="video-chip">{chip}</span>
    </button>
  );
}

/// Long-press-to-copy on a user bubble: hold ~450ms without dragging (a drag is
/// a scroll, which cancels). Native owns the clipboard write + confirming haptic
/// (`copyText`); the web side plays the squish + "copied" pill for
/// `COPY_TOAST_MS` before it fades.
const LONG_PRESS_MS = 450;
const LONG_PRESS_MOVE_CANCEL_PX = 10;
const COPY_TOAST_MS = 1300;
/// Below this gap between the bubble's top and the header-covered strip, the
/// pill would render under the native header overlay — flip it below instead.
const TOAST_HEADER_CLEARANCE_PX = 30;

/// Fire `onLongPress` after a still ~`LONG_PRESS_MS` press; any drag past
/// `LONG_PRESS_MOVE_CANCEL_PX` (a scroll) or a lift first cancels it. Touch-only
/// — the pointer here is always a finger on the transcript webview.
function useLongPress(onLongPress: () => void): {
  onTouchStart: (e: ReactTouchEvent) => void;
  onTouchMove: (e: ReactTouchEvent) => void;
  onTouchEnd: () => void;
} {
  const timer = useRef<number | undefined>(undefined);
  const origin = useRef<{ x: number; y: number } | null>(null);
  // The document-level second-finger watch, live only while a press is armed.
  const docWatch = useRef<((e: TouchEvent) => void) | null>(null);

  const cancel = useCallback(() => {
    clearTimeout(timer.current);
    timer.current = undefined;
    origin.current = null;
    if (docWatch.current) {
      document.removeEventListener("touchstart", docWatch.current, true);
      docWatch.current = null;
    }
  }, []);

  // Clear an armed press (timer + document watch) on unmount.
  useEffect(() => cancel, [cancel]);

  const onTouchStart = useCallback(
    (e: ReactTouchEvent) => {
      cancel();
      if (e.touches.length !== 1) return;
      const t = e.touches[0];
      origin.current = { x: t.clientX, y: t.clientY };
      timer.current = window.setTimeout(() => {
        cancel();
        onLongPress();
      }, LONG_PRESS_MS);
      // A second finger landing anywhere — even off the bubble — is a pinch or
      // scroll, not a copy. onTouchStart only re-fires for touches ON the bubble,
      // so watch the whole document while armed; `cancel` removes the listener.
      const watch = (ev: TouchEvent) => {
        if (ev.touches.length > 1) cancel();
      };
      docWatch.current = watch;
      document.addEventListener("touchstart", watch, { passive: true, capture: true });
    },
    [cancel, onLongPress],
  );

  const onTouchMove = useCallback(
    (e: ReactTouchEvent) => {
      if (origin.current === null || timer.current === undefined) return;
      if (e.touches.length !== 1) {
        cancel();
        return;
      }
      const t = e.touches[0];
      if (
        Math.abs(t.clientX - origin.current.x) > LONG_PRESS_MOVE_CANCEL_PX ||
        Math.abs(t.clientY - origin.current.y) > LONG_PRESS_MOVE_CANCEL_PX
      ) {
        cancel();
      }
    },
    [cancel],
  );

  return { onTouchStart, onTouchMove, onTouchEnd: cancel };
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
        <div className="stopped-indicator" role="status">
          <span className="stopped-mark" aria-hidden="true" />
          {t("chat.stopped")}
        </div>
      );
    }
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
  useEffect(() => {
    persistLatest.current = () =>
      persistState({
        messages,
        lastOrdinal: lastOrdinal.current,
        oldestOrdinal: oldestOrdinal.current,
        hasMoreOlder,
        imageDims: Object.fromEntries(imageDims),
      });
    persistLatest.current();
  }, [messages, hasMoreOlder, imageDims]);

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

  // A terminal notice that lands mid-turn folds INTO the open work block as a
  // leveled step, so it doesn't sever the block into two cards (the tail must
  // stay a work row for `withOpenWork` to keep extending it). Only an active
  // block folds: those mid-turn notices are live-only (never persisted), so the
  // folded step can't duplicate a durable row. A notice with no active block —
  // between turns, or a persisted `/stop`/`/compact` outcome anchored after the
  // turn — keeps its own centered `role:"notice"` row (its durable shape).
  const foldTerminalNotice = useCallback((level: string, text: string) => {
    setMessages((rows) => {
      const last = rows[rows.length - 1];
      if (last && last.role === "work" && last.active) {
        return [...rows.slice(0, -1), { ...last, steps: [...last.steps, { kind: "notice", level, text }] }];
      }
      return [...rows, { id: uid(), role: "notice", content: text }];
    });
  }, []);

  const appendNotice = useCallback((text: string) => {
    setMessages((m) => [...m, { id: uid(), role: "notice", content: text }]);
  }, []);

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
      // A work frame belongs to the tail work block whenever the tail IS one —
      // even if it was just FROZEN. A restored live block stays `active` and a
      // re-entry's continuation extends it (keeping its real startedAt); but a
      // block can also be frozen MID-STREAM by a `turn_state{inactive}` that
      // raced ahead of a straggler frame — on cancel the gateway emits an
      // unguarded `tool_completed` through the SAME ordered channel the turn-end
      // projector rides, so `[tool_started] → turn_state{inactive} → tool_completed`
      // reaches the client with the block already closed. Folding into the frozen
      // tail rather than forking is the invariant that keeps ONE turn to ONE card:
      // withOpenWork never appends a work row adjacent to another (the
      // `[work][work]` re-entry split). The straggler even resolves its own
      // still-"running" tool step in place. The block keeps its frozen
      // `active:false`, so a cancelled turn reads "Worked", not a stuck "Working".
      if (last && last.role === "work") {
        return [...rows.slice(0, -1), mutate(last)];
      }
      // Tail is not a work row: this frame opens a NEW block. Freeze EVERY
      // still-`active` block anywhere in the thread first, so a stale open block
      // can't linger as a second live "Working" card beside this one.
      const fresh: WorkRow = { id: uid(), role: "work", steps: [], active: true, startedAt: Date.now() };
      return [...freezeActiveWork(rows), mutate(fresh)];
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
      // Prefer the server's authoritative duration (reconciled in) over the
      // wall-clock fallback, which is only correct for a purely live-watched turn.
      const elapsedMs = last.elapsedMs ?? (last.startedAt !== undefined ? Date.now() - last.startedAt : undefined);
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
        // A stale finalization-window bundle: this turn's answer already landed
        // here (the tail is the committed reply) but the gateway still reports
        // `turn.active` — its `active_turn_started_at` lingers through the job's
        // post-answer finalization — and ships a rolling in-flight work window.
        // Do NOT resurrect the ended turn's work as a second block under the
        // reply (the [work][reply][work] split). A genuine next turn opens its
        // block from the live turn_state / reasoning / tool frames that follow,
        // not from this snapshot.
        if (!openBlock && last && last.role === "assistant") return rows;
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
          ? [...freezeActiveWork(rows.slice(0, -1)), rebuilt]
          : [...freezeActiveWork(rows), rebuilt];
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
          // Carry only the SINGLE newest active in-flight block across the
          // rebuild; any earlier still-active block is a stale fork — drop it so
          // it can't re-appear beside the reconstructed thread.
          const openWork = prev.filter((r): r is WorkRow => r.role === "work" && r.active).slice(-1);
          const keptSends = prev.filter(
            (r) => r.role === "user" && r.sendState !== undefined && !pageIds.has(r.id),
          );
          // The page's reconstructed trailing `w<ordinal>` block and the live
          // in-flight block are the SAME turn. Fuse them into ONE block — keep it
          // active, adopt the server id + server-anchored timing, union the steps
          // — instead of rendering both (duplicate/overlapping cards) or dropping
          // either (losing steps or the correct duration).
          let rows = pageRows;
          let carried = openWork;
          if (openWork.length > 0) {
            const tail = rows[rows.length - 1];
            if (tail && tail.role === "work") {
              rows = [
                ...rows.slice(0, -1),
                {
                  ...tail,
                  steps: mergeWorkSteps(tail.steps, openWork[0].steps),
                  active: true,
                  startedAt: tail.startedAt ?? openWork[0].startedAt,
                },
              ];
              carried = [];
            }
          }
          return [...rows, ...keptSends, ...carried];
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
              elapsedMs: last.elapsedMs ?? (last.startedAt !== undefined ? Date.now() - last.startedAt : undefined),
            };
          };
          for (const row of pageRows) {
            const existingIdx = byId.get(row.id);
            if (existingIdx !== undefined) {
              const existing = next[existingIdx];
              // A redelivery of a row already on screen: reconcile an optimistic
              // send's chrome (drop the spinner), or fold a same-id work block's
              // newer server steps + timing into what's rendered (else a no-op).
              if (existing.role === "user" && existing.sendState !== undefined) {
                next[existingIdx] = { ...existing, sendState: undefined };
              } else if (existing.role === "work" && row.role === "work") {
                next[existingIdx] = reconcileWork(existing, row);
              }
              continue;
            }
            // The in-flight turn's reconstructed `w<ordinal>` work block is the
            // SAME turn as the live/restored block at the tail — RECONCILE into
            // it (union steps + adopt server timing) rather than rendering a
            // second card. A turn we don't have yet ends on a non-work tail, so
            // its own work block is still appended.
            const tail = next[next.length - 1];
            if (row.role === "work" && tail && tail.role === "work") {
              next[next.length - 1] = reconcileWork(tail, row);
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
              const ord = rowOrdinal(row.id);
              const at = ord !== null ? sameTurnWorkIndex(next, ord) : -1;
              const target = at >= 0 ? next[at] : undefined;
              if (target && target.role === "work") {
                next[at] = reconcileWork(target, row);
                continue;
              }
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
        }
        if (role === "assistant") {
          // The terminal message is authoritative: it replaces the streamed
          // text and ends the turn's work block. It is also a turn-END signal
          // (record its identity), and — if the cursor is rebase-dirty — the
          // trigger for the follow-up sync that closes the dirty window.
          closeWork();
          clearStreaming();
          setAwaitingReply(false);
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
          // Turn ended — end the optimistic run-state window (its work block /
          // streaming, if any, close alongside). Kept on the ACTIVE branch so a
          // slow first token doesn't briefly drop the stop button.
          setAwaitingReply(false);
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
        // Do NOT clear the optimistic window here: a `subscribe_state` arrives on
        // every (re)connect, and the send-then-connect path (a first message on a
        // fresh session) delivers `turn.active:false` in the gap AFTER our send
        // but BEFORE the turn starts — clearing here would drop the stop button
        // back to send until the first output (the "stop appears late" bug). A
        // real turn is reflected by applySubscribeState rebuilding the work block
        // / streaming reply; a genuinely idle window self-expires (see the
        // `awaitingReply` timeout) and is cleared by the turn's terminal frame.
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
        } else if (isStopAckNotice(frame.text)) {
          // A `/stop` acknowledgement. Don't mint a client row for it: the
          // gateway persists this notice, and reconstruction renders it as the
          // compact "Stopped" indicator (`n<seq>` id). Minting a local `uid` row
          // too would double it on the next sync / a relaunch (two ids, same
          // event). Instead end the optimistic window, freeze any open work
          // block, and pull the durable indicator in via one sync.
          setAwaitingReply(false);
          setMessages((rows) => freezeActiveWork(rows));
          runSync();
        } else {
          // A terminal notice (a server rejection, a degraded-mode banner) means
          // no turn is starting — end the optimistic window so the stop button
          // can't strand.
          setAwaitingReply(false);
          foldTerminalNotice(frame.level, frame.text);
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
    // Optimistically enter the "awaiting reply" window so the composer's stop
    // button appears immediately, before the first `turn_state` lands.
    setAwaitingReply(true);
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
  const renderRows = messages.filter((m, i) => {
    if (m.role !== "notice" || !m.stopped) return true;
    const prev = messages[i - 1];
    return !(prev && prev.role === "notice" && prev.stopped);
  });

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
        {renderRows.map((m) =>
          m.role === "work" ? (
            <WorkBlockView key={m.id} row={m} onToggle={handleWorkToggle} />
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
    </ImageDimsContext.Provider>
  );
}
