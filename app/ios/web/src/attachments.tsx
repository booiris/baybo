import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useRef,
  useState,
  type CSSProperties,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
  type RefObject,
  type TouchEvent as ReactTouchEvent,
} from "react";
import { useTranslation } from "react-i18next";
import {
  audioSeek,
  audioToggle,
  blobObjectUrl,
  downloadFile,
  onAudioState,
  onFileState,
  playVideo,
  previewFile,
  queryAudioState,
  queryFileState,
  requestVideoPoster,
  shareFile,
  viewImage,
  type AudioStatePayload,
  type FileState,
} from "./bridge";
import { useLongPress } from "./gestures";
import { blobContentDigest, type WireAttachment } from "./types";

/// The attachment cards a message can carry — image, file, audio, video — and
/// the bubble that dispatches to whichever one an attachment turns out to be.
///
/// Lifted out of `Transcript.tsx` when a SECOND surface needed them: a project
/// card's description and comments carry the same attachments as a chat
/// message, and a card page importing the transcript to get an image tile
/// would drag the sync loop, the outbox and the scroll machinery in with it.
///
/// Nothing here knows about rows, ordinals or the transcript's state. What it
/// knows is a `WireAttachment` and the bridge — which is precisely the seam the
/// two surfaces have in common.

/// How far outside the viewport (px, top + bottom) an attachment card begins
/// asking native for anything — a preload band so a card is usually settled by
/// the time it scrolls in, while a back-history page's off-screen ones stay
/// silent. See `useNearViewport`.
const LAZY_ATTACHMENT_ROOT_MARGIN = "400px 0px";


/// The one image type carrying no pixels of its own, and so the one whose size
/// cannot be read off the element that shows it (`measureIntrinsicSize`).
const VECTOR_IMAGE_MIME = "image/svg+xml";

/// How long a vector's pre-paint measurement may hold its image back. The probe
/// decodes a blob the real `<img>` is about to decode anyway, so it settles in
/// the same frame in practice; this only exists because an image that never
/// paints would be a far worse failure than one sized from its loading tile.
const INTRINSIC_PROBE_TIMEOUT_MS = 2000;

/// The natural pixel size of every image this thread has decoded, keyed by blob
/// digest and mirrored to disk with the rows (`PersistedState.imageDims`). A hit
/// means the image rendered here before — so its blob is on the device and its
/// box can be reserved at the exact final size before a single byte crosses the
/// bridge, which is what keeps a re-opened thread from resizing under the reader
/// (see `AttachmentBubble`). Carried on a context rather than props: the value's
/// identity is stable for the transcript's life, so recording a size re-renders
/// nothing and `MessageRow`'s memo survives.
export type ImageDimsStore = {
  get(digest: string): [number, number] | undefined;
  record(digest: string, width: number, height: number): void;
};

export const ImageDimsContext = createContext<ImageDimsStore | null>(null);

/// Rebuild the map from a restored mirror, dropping anything that isn't a usable
/// size — a zero or garbage dimension would poison the reserved box's ratio (CSS
/// divides by it), and the mirror is on-disk JSON, not a trusted type.
export function restoreImageDims(
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

/// Load-once viewport gate for the attachment cards: `false` until the observed
/// element first comes within `LAZY_ATTACHMENT_ROOT_MARGIN` of the viewport,
/// then `true` for good — so scrolling back past a card never re-runs whatever
/// it gates.
///
/// EVERY card in a restored thread mounts at once, and each ungated one costs
/// native work on the app's main thread before the transcript can settle: an
/// image blob crosses as a large base64 string plus an `atob` decode,
/// `queryFileState` / `queryAudioState` are a post out and an
/// `evaluateJavaScript` back apiece, and a downloaded video adds a poster frame
/// (native spins an AVAssetImageGenerator per tile). A long conversation carries
/// dozens of cards; the reader can see two or three. So the gate gets the whole
/// card, not just the image inside it.
///
/// Without IntersectionObserver (not expected on WKWebView; a dev-browser
/// guard) open the gate immediately rather than never loading at all.
function useNearViewport(ref: RefObject<Element | null>): boolean {
  const [near, setNear] = useState(false);
  useEffect(() => {
    if (near) return;
    if (typeof IntersectionObserver === "undefined") {
      setNear(true);
      return;
    }
    const el = ref.current;
    if (!el) return;
    const io = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) {
          setNear(true);
          io.disconnect();
        }
      },
      { rootMargin: LAZY_ATTACHMENT_ROOT_MARGIN },
    );
    io.observe(el);
    return () => io.disconnect();
  }, [near, ref]);
  return near;
}

/// One image attachment in a bubble: lazily downloads the blob via the bridge
/// (cached on device) once its row scrolls near the viewport, wraps it in an
/// object URL, shows a spinner while loading and a tap-to-retry on failure. The
/// lazy gate is load-bearing for history: a back-page can carry dozens of
/// images, and fetching every blob on mount floods the bridge — each image
/// crosses as a large base64 string plus a main-thread `atob` decode
/// (bridge.ts) — which stalls the whole transcript until they all settle (the
/// whole page fails to appear while paging history). `useNearViewport` defers
/// each fetch to when its row actually approaches the screen, so off-screen
/// history images cost nothing until scrolled to. The old in-session previewUrl
/// short-circuit is gone — native previews don't cross the bridge, so a
/// just-sent image renders by fetching its own bytes back over requestBlob
/// (device-cached, so fast).
function AttachmentImage({
  attachment,
  connEpoch,
  onIntrinsicSize,
}: {
  attachment: WireAttachment;
  connEpoch: number;
  /// A vector's measured size, handed up BEFORE its image paints so the bubble
  /// can reserve the box first (`AttachmentBubble`).
  onIntrinsicSize: (width: number, height: number) => void;
}) {
  const { t } = useTranslation();
  const imageDims = useContext(ImageDimsContext);
  const vector = isVectorImage(attachment);
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
  const visible = useNearViewport(holderRef);

  useEffect(() => {
    if (!visible) return;
    let owned: string | null = null;
    let torndown = false;
    // Read through a call, not the flag: the vector branch below checks it a
    // SECOND time after an await, where the compiler — which cannot see the
    // cleanup assign it — has already narrowed the bare flag to false and calls
    // the check dead code.
    const cancelled = () => torndown;
    failedRef.current = false;
    setFailed(false);
    setLoaded(false);
    setUrl(null);
    blobObjectUrl(attachment.blob_id, attachment.mime_type)
      .then(async (u) => {
        if (cancelled()) {
          URL.revokeObjectURL(u);
          return;
        }
        owned = u;
        // A vector is measured and sized BEFORE its image is handed over,
        // never from its own `onLoad`: by then it is inside whatever box the
        // bubble already picked, and WebKit would report that box back as the
        // image's natural size (`measureIntrinsicSize`). The extra decode is a
        // hit on the same blob the `<img>` below is about to decode.
        if (vector) {
          const dims = await measureIntrinsicSize(u);
          // The cleanup below has already revoked `owned`; nothing to undo.
          if (cancelled()) return;
          if (dims !== null) {
            imageDims?.record(blobContentDigest(attachment.blob_id), dims[0], dims[1]);
            onIntrinsicSize(dims[0], dims[1]);
          }
        }
        setUrl(u);
      })
      .catch(() => {
        if (!cancelled()) {
          failedRef.current = true;
          setFailed(true);
        }
      });
    return () => {
      torndown = true;
      if (owned !== null && owned !== "") URL.revokeObjectURL(owned);
    };
    // `imageDims` and `onIntrinsicSize` are both identity-stable by
    // construction (a memoized context value, a `useCallback` with no deps);
    // anything less would refetch the blob on every re-render of the row.
  }, [
    attachment.blob_id,
    attachment.mime_type,
    attempt,
    visible,
    vector,
    imageDims,
    onIntrinsicSize,
  ]);

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
      {url !== null && url !== "" && (
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
              //
              // Raster only. A vector reports back the box it is standing in
              // rather than a size of its own, so it is measured before it gets
              // here (`measureIntrinsicSize`) and recording again from the
              // element would overwrite that truth with the layout.
              if (!vector) {
                const { naturalWidth: w, naturalHeight: h } = e.currentTarget;
                if (w > 0 && h > 0) {
                  imageDims?.record(blobContentDigest(attachment.blob_id), w, h);
                }
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
///
/// A VECTOR is the exception, and takes the same box by a different route: it
/// gets measured before it paints (`measureIntrinsicSize`) and hands its size up
/// here, so the box is reserved with nothing painted under it yet. Both halves
/// of that matter. A vector carries no intrinsic width for this shrink-to-fit
/// bubble to resolve, so without a reserved box an SVG written as a bare
/// `viewBox` lays out at zero width — invisible, and untappable with it. And a
/// stale entry from before it was measured this way (the mirror is on disk and
/// outlives the fix) is corrected on the spot rather than sizing the bubble
/// wrong for the life of the thread.
export function AttachmentBubble({
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
  const [sized, setSized] = useState(() =>
    isImage ? imageDims?.get(blobContentDigest(attachment.blob_id)) : undefined,
  );
  // Identity-stable: `AttachmentImage` takes this as an effect dependency, and a
  // fresh function per render would refetch the blob on every re-render.
  const takeIntrinsicSize = useCallback((width: number, height: number) => {
    setSized((prev) =>
      prev !== undefined && prev[0] === width && prev[1] === height ? prev : [width, height],
    );
  }, []);
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
        <AttachmentImage
          attachment={attachment}
          connEpoch={connEpoch}
          onIntrinsicSize={takeIntrinsicSize}
        />
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
export function isVideoAttachment(attachment: WireAttachment): boolean {
  return attachment.kind === "file" && attachment.mime_type.startsWith("video/");
}

/// An image with no pixel size of its own. Everything about how it is measured
/// differs (`measureIntrinsicSize`), so it is asked once and answered here
/// rather than by a mime comparison at each site. Parameters are stripped: the
/// gateway sends a bare mime today, but `image/svg+xml; charset=utf-8` is a
/// legal spelling of the same type and must not read as a raster blob.
export function isVectorImage(attachment: WireAttachment): boolean {
  return (
    attachment.kind === "image" &&
    attachment.mime_type.split(";")[0].trim().toLowerCase() === VECTOR_IMAGE_MIME
  );
}

/// The size an image takes with NOTHING constraining it, read off a detached
/// `Image` — one that is never inserted into the document, so no layout can
/// colour the answer.
///
/// For a raster blob that is just its pixel count, which is why only vectors pay
/// for this. But WebKit answers `naturalWidth` for an SVG with the size the
/// element is laid out at RIGHT NOW: the same 1200x400 page measures 1200
/// detached, 192 while it decodes inside the 12rem loading tile, and 358 once
/// released into the reading column (all three measured). `AttachmentImage` used
/// to record what its own `onLoad` saw — the tile's number — so the next open of
/// the thread reserved a 192px box for a diagram that had rendered full width,
/// and the image shrank to fit it. An SVG with no `width`/`height` at all is
/// worse off still: it has no intrinsic width for a shrink-to-fit bubble to
/// resolve, and lays out at ZERO until something hands it a definite box.
function measureIntrinsicSize(url: string): Promise<[number, number] | null> {
  return new Promise((resolve) => {
    const probe = new Image();
    const timer = window.setTimeout(() => {
      probe.onload = null;
      probe.onerror = null;
      resolve(null);
    }, INTRINSIC_PROBE_TIMEOUT_MS);
    const settle = (dims: [number, number] | null) => {
      window.clearTimeout(timer);
      resolve(dims);
    };
    probe.onload = () => {
      const { naturalWidth: w, naturalHeight: h } = probe;
      settle(w > 0 && h > 0 ? [w, h] : null);
    };
    probe.onerror = () => settle(null);
    probe.src = url;
  });
}

/// Binary units, and only as much precision as disambiguates: `812 B`,
/// `24 KB`, `2.3 MB`, `140 MB`.
export function formatBytes(bytes: number): string {
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
export function typeLabel(attachment: WireAttachment): string {
  const dot = attachment.filename?.lastIndexOf(".") ?? -1;
  const ext = dot > 0 ? attachment.filename?.slice(dot + 1) : undefined;
  if (ext !== undefined && ext.length > 0 && ext.length <= 4) return ext.toUpperCase();
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
export function splitForMiddleEllipsis(name: string): [string, string] {
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
export function formatTime(totalSeconds: number): string {
  const s = Math.max(0, Math.floor(totalSeconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = String(s % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${sec}` : `${m}:${sec}`;
}

/// One file attachment's on-device lifecycle. Native owns the truth (the blob
/// cache is a directory iOS may purge), so the card asks whenever it comes into
/// view rather than trusting a `ready` it saw before.
///
/// `active` is the card's `useNearViewport` gate, and it covers the query as
/// well as the subscription: this is one bridge round trip PER CARD, and a
/// restored thread mounts every card it holds at once — on a long conversation
/// that is a burst of main-thread work landing squarely in the window the
/// transcript is trying to paint its first frame in. Nothing is lost by waiting:
/// a card can only be downloaded or played by being tapped, which needs it on
/// screen, and the gate opens a preload band ahead of that.
function useFileState(blobId: string, active: boolean): { state: FileState; loaded: number } {
  const [state, setState] = useState<FileState>("idle");
  const [loaded, setLoaded] = useState(0);

  useEffect(() => {
    if (!active) return;
    const unsubscribe = onFileState(blobId, (payload) => {
      setState(payload.state);
      if (payload.state === "loading") setLoaded(payload.loaded ?? 0);
    });
    queryFileState(blobId);
    return unsubscribe;
  }, [blobId, active]);

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
  const rootRef = useRef<HTMLButtonElement | null>(null);
  const { state, loaded } = useFileState(attachment.blob_id, useNearViewport(rootRef));
  const type = typeLabel(attachment);
  // A nameless blob has nothing better to title itself with than its type, so
  // the meta line would only repeat it.
  const name = attachment.filename ?? type;
  const [head, tail] = splitForMiddleEllipsis(name);

  const meta =
    state === "loading"
      ? `${formatBytes(loaded)} / ${formatBytes(attachment.size)}`
      : attachment.filename !== undefined && attachment.filename !== ""
        ? `${type} · ${formatBytes(attachment.size)}`
        : formatBytes(attachment.size);

  const onTap = useCallback(() => {
    if (state === "loading") return;
    if (state === "ready") previewFile(attachment.blob_id, name, attachment.mime_type);
    else downloadFile(attachment.blob_id);
  }, [state, attachment.blob_id, attachment.mime_type, name]);

  const share = useSharePress(
    useCallback(() => {
      if (state !== "ready") return false;
      shareFile(attachment.blob_id, name, attachment.mime_type);
      return true;
    }, [state, attachment.blob_id, attachment.mime_type, name]),
  );

  return (
    <button
      ref={rootRef}
      type="button"
      className={`attachment-file ${state}`}
      onClick={onTap}
      {...share}
    >
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
/// so a 2 Hz position tick re-renders one card; the query resyncs a card that
/// appears mid-playback (session switch, thread reload).
///
/// Gated on the same `useNearViewport` flag as `useFileState`, for the same
/// reason and with the same safety: a track can only be playing because someone
/// tapped its card, and a card off-screen has nothing to show about it — the
/// query runs the moment it scrolls back into the band.
function useAudioState(blobId: string, active: boolean): AudioStatePayload {
  const [audio, setAudio] = useState<AudioStatePayload>({
    blobId,
    state: "stopped",
    position: 0,
    duration: 0,
  });

  useEffect(() => {
    if (!active) return;
    const unsubscribe = onAudioState(blobId, setAudio);
    queryAudioState(blobId);
    return unsubscribe;
  }, [blobId, active]);

  return audio;
}

/// The audio card's seek bar — rendered in EVERY state so the card's height
/// never jumps as playback starts/ends. Until the engine is engaged the bar is
/// inert and empty: no handlers, so a tap on it bubbles to the card (play) and
/// a hold arms the share like anywhere else on the card.
///
/// Engaged, a drag scrubs locally (the fill follows the finger, not the
/// engine) and commits one `audioSeek` on lift; the committed value keeps
/// rendering until the ENGINE's next push lands (native answers a seek with an
/// optimistic state, so that's near-immediate), because falling back to the
/// stale pre-seek `position` would snap the fill backwards for the round trip.
/// Pointer events stop at the bar so a scrub never toggles the card under it;
/// `touch-action: none` (CSS) keeps a horizontal drag from scrolling the
/// thread.
function AudioTrack({
  blobId,
  position,
  duration,
  interactive,
}: {
  blobId: string;
  position: number;
  duration: number;
  interactive: boolean;
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

  if (!interactive) {
    return (
      <div className="audio-track" aria-hidden="true">
        <span className="audio-track-fill" style={{ width: "0%" }} />
      </div>
    );
  }

  const shown = scrub ?? (duration > 0 ? position / duration : 0);

  return (
    <div
      ref={barRef}
      className="audio-track"
      // A still finger resting on the track (a slow scrub) must not arm the
      // card's long-press share — touch events propagate independently of the
      // pointer events captured below.
      onTouchStart={(e) => e.stopPropagation()}
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
  const rootRef = useRef<HTMLButtonElement | null>(null);
  const near = useNearViewport(rootRef);
  const { state, loaded } = useFileState(attachment.blob_id, near);
  const audio = useAudioState(attachment.blob_id, near);
  const type = typeLabel(attachment);
  const name = attachment.filename ?? type;
  const [head, tail] = splitForMiddleEllipsis(name);
  const playing = audio.state === "playing";
  // Once the engine has touched this track the meta line becomes time and the
  // scrubber goes live; `stopped` (never played / ended / usurped) reads like
  // a resting card again.
  const engaged = state === "ready" && audio.state !== "stopped" && audio.duration > 0;

  // The engine's duration is PRECISE (the asset is opened with precise
  // timing) and permanently supersedes the wire's probe — after playback ends
  // the resting meta must not fall back to an estimate the play just
  // disproved. Held in a ref: it only matters on renders something else
  // already triggered.
  const engineDurationMs = useRef(0);
  useEffect(() => {
    if (audio.duration > 0) engineDurationMs.current = audio.duration * 1000;
  }, [audio.duration]);
  const restDurationMs =
    engineDurationMs.current > 0 ? engineDurationMs.current : (attachment.duration_ms ?? null);

  // At rest the track's length rides the WIRE (`duration_ms`, probed at
  // attach time), so it shows before any byte is downloaded — the engine
  // takes over once it has loaded the real thing.
  const meta =
    state === "loading"
      ? `${formatBytes(loaded)} / ${formatBytes(attachment.size)}`
      : engaged
        ? `${formatTime(audio.position)} / ${formatTime(audio.duration)}`
        : [
            attachment.filename !== undefined && attachment.filename !== "" ? type : null,
            restDurationMs != null ? formatTime(restDurationMs / 1000) : null,
            formatBytes(attachment.size),
          ]
            .filter(Boolean)
            .join(" · ");

  const onTap = useCallback(() => {
    if (state === "loading") return;
    if (state === "ready") audioToggle(attachment.blob_id, name, attachment.mime_type);
    else downloadFile(attachment.blob_id);
  }, [state, attachment.blob_id, attachment.mime_type, name]);

  const share = useSharePress(
    useCallback(() => {
      if (state !== "ready") return false;
      shareFile(attachment.blob_id, name, attachment.mime_type);
      return true;
    }, [state, attachment.blob_id, attachment.mime_type, name]),
  );

  return (
    <button
      ref={rootRef}
      type="button"
      className={`attachment-file audio ${state}`}
      onClick={onTap}
      aria-label={playing ? t("chat.audioPause") : t("chat.audioPlay")}
      {...share}
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
        <AudioTrack
          blobId={attachment.blob_id}
          position={audio.position}
          duration={audio.duration}
          interactive={engaged}
        />
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

export function clampVideoRatio(ratio: number): number {
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
  const rootRef = useRef<HTMLButtonElement | null>(null);
  const { state, loaded } = useFileState(attachment.blob_id, useNearViewport(rootRef));
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
      if (owned !== null && owned !== "") {
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

  const share = useSharePress(
    useCallback(() => {
      if (state !== "ready") return false;
      shareFile(attachment.blob_id, name, attachment.mime_type);
      return true;
    }, [state, attachment.blob_id, attachment.mime_type, name]),
  );

  return (
    <button
      ref={rootRef}
      type="button"
      className={`attachment-video ${state}${poster !== null && poster !== "" ? " has-poster" : ""}`}
      style={{ "--video-ar": String(ratio) } as CSSProperties}
      onClick={onTap}
      aria-label={state === "ready" ? t("chat.videoPlay") : t("chat.videoDownload")}
      {...share}
    >
      {poster !== null && poster !== "" && (
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

/// Long-press → share, for a card that ALSO has a tap action: when the press
/// fires, the synthetic click that follows the lift is swallowed in the
/// capture phase, so a share never also downloads/plays/previews. `onShare`
/// returns whether it fired — an undownloaded card shares nothing, and its
/// follow-up click must stay a plain tap. The suppression re-arms on the next
/// touch, so a fired press whose click never materialised (finger dragged
/// away after the fire) can't eat a later genuine tap.
function useSharePress(onShare: () => boolean): {
  onTouchStart: (e: ReactTouchEvent) => void;
  onTouchMove: (e: ReactTouchEvent) => void;
  onTouchEnd: () => void;
  onClickCapture: (e: ReactMouseEvent) => void;
} {
  const suppress = useRef(false);
  const fire = useCallback(() => {
    if (onShare()) suppress.current = true;
  }, [onShare]);
  const press = useLongPress(fire);
  const pressStart = press.onTouchStart;
  const onTouchStart = useCallback(
    (e: ReactTouchEvent) => {
      suppress.current = false;
      pressStart(e);
    },
    [pressStart],
  );
  const onClickCapture = useCallback((e: ReactMouseEvent) => {
    if (suppress.current) {
      suppress.current = false;
      e.preventDefault();
      e.stopPropagation();
    }
  }, []);
  return {
    onTouchStart,
    onTouchMove: press.onTouchMove,
    onTouchEnd: press.onTouchEnd,
    onClickCapture,
  };
}
