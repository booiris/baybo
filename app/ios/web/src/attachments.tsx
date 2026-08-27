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


const LAZY_ATTACHMENT_ROOT_MARGIN = "400px 0px";


/// The one image type carrying no pixels of its own, and so the one whose size
/// cannot be read off the element that shows it (`measureIntrinsicSize`).
const VECTOR_IMAGE_MIME = "image/svg+xml";

const INTRINSIC_PROBE_TIMEOUT_MS = 2000;

export type ImageDimsStore = {
  get(digest: string): [number, number] | undefined;
  record(digest: string, width: number, height: number): void;
};

export const ImageDimsContext = createContext<ImageDimsStore | null>(null);

/// Restored dimensions reserve the same box before media loads. Treat mirror
/// data as untrusted so corrupt values cannot poison layout calculations.
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

function useNearViewport(ref: RefObject<Element | null>): boolean {
  // Restored transcripts mount every row; defer bridge traffic and decode work
  // until a row approaches the viewport. Test/legacy runtimes fall back open.
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
  const [loaded, setLoaded] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const failedRef = useRef(false);
  // The reserved placeholder box the observer watches until the row nears the
  // viewport.
  const holderRef = useRef<HTMLDivElement | null>(null);
  const visible = useNearViewport(holderRef);

  useEffect(() => {
    if (!visible) return;
    let owned: string | null = null;
    let torndown = false;
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
  }, [
    attachment.blob_id,
    attachment.mime_type,
    attempt,
    visible,
    vector,
    imageDims,
    onIntrinsicSize,
  ]);

  useEffect(() => {
    // A failed bridge read may have been caused by the old connection; retry
    // once when native reports a new epoch, but only for a visible row.
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
  return (
    <div
      className={`attachment-frame${loaded ? " loaded" : ""}`}
      aria-label={loaded ? undefined : t("chat.loadingImage")}
    >
      {!loaded && <span className="attachment-spinner" aria-hidden="true" />}
      {url !== null && url !== "" && (
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

export function isVectorImage(attachment: WireAttachment): boolean {
  return (
    attachment.kind === "image" &&
    attachment.mime_type.split(";")[0].trim().toLowerCase() === VECTOR_IMAGE_MIME
  );
}

function measureIntrinsicSize(url: string): Promise<[number, number] | null> {
  // Probe vectors before paint: WebKit can report a constrained or zero
  // naturalWidth after the visible element participates in layout.
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

export function typeLabel(attachment: WireAttachment): string {
  const dot = attachment.filename?.lastIndexOf(".") ?? -1;
  const ext = dot > 0 ? attachment.filename?.slice(dot + 1) : undefined;
  if (ext !== undefined && ext.length > 0 && ext.length <= 4) return ext.toUpperCase();
  const subtype = attachment.mime_type.split("/")[1] ?? attachment.mime_type;
  const bare = subtype.split(";")[0].split("+")[0].split(".").pop() ?? "";
  return (bare || attachment.mime_type).toUpperCase();
}

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

function useFileState(blobId: string, active: boolean): { state: FileState; loaded: number } {
  const [state, setState] = useState<FileState>("idle");
  const [loaded, setLoaded] = useState(0);

  useEffect(() => {
    if (!active) return;
    // Native may purge its downloaded-file cache between visits, so query the
    // current state instead of trusting a prior rendered "ready" value.
    const unsubscribe = onFileState(blobId, (payload) => {
      setState(payload.state);
      if (payload.state === "loading") setLoaded(payload.loaded ?? 0);
    });
    queryFileState(blobId);
    return unsubscribe;
  }, [blobId, active]);

  return { state, loaded };
}

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
    // Keep the local committed position until the engine pushes its next state;
    // otherwise the fill snaps back between pointer-up and the seek response.
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
  const engaged = state === "ready" && audio.state !== "stopped" && audio.duration > 0;

  const engineDurationMs = useRef(0);
  useEffect(() => {
    if (audio.duration > 0) engineDurationMs.current = audio.duration * 1000;
  }, [audio.duration]);
  const restDurationMs =
    engineDurationMs.current > 0 ? engineDurationMs.current : (attachment.duration_ms ?? null);

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
        // Never leave a revoked object URL rendered in a pooled webview.
        URL.revokeObjectURL(owned);
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
    // Suppress the synthetic tap only when the long press actually shared;
    // cancelled/failed long presses must retain the ordinary open action.
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
