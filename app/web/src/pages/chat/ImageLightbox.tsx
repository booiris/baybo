import { type PointerEvent as ReactPointerEvent, useCallback, useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { RiAddLine, RiCloseLine, RiDownloadLine, RiSubtractLine } from 'react-icons/ri';

/** `scale` is a multiplier over the FIT size, so 1 always means "fully visible"
 * regardless of the viewport — the floor, since zooming below fit shows nothing
 * the thumbnail didn't already. */
const MIN_SCALE = 1;
const MAX_SCALE = 8;
const BUTTON_STEP = 1.5;
/** Wheel deltas are device-dependent; this exponent turns one notch into a
 * multiplicative step small enough that a trackpad flick doesn't slam the cap. */
const WHEEL_STEP = 0.0022;
/** Fallback double-click target when the image is already larger than its
 * natural size at fit (a small image on a big screen) — 1:1 would zoom OUT. */
const DOUBLE_CLICK_SCALE = 2;
/** Pointer travel below this stays a click (close on backdrop), above it is a
 * drag — so a pan that ends over the backdrop doesn't dismiss the viewer. */
const DRAG_SLOP_PX = 4;

interface View {
  scale: number;
  x: number;
  y: number;
  /** Whether THIS transition should ease. A discrete jump (button, key,
   * double-click) reads better eased; a wheel notch, a pinch, or a drag must
   * track the input 1:1 — easing them lags the pointer and, on a large image,
   * keeps a fresh compositor animation in flight for every event. */
  animate: boolean;
}

const FIT: View = { scale: 1, x: 0, y: 0, animate: true };

function clamp(v: number, lo: number, hi: number): number {
  return Math.min(hi, Math.max(lo, v));
}

const TOOL_BUTTON =
  'h-9 w-9 inline-flex items-center justify-center border-2 border-black rounded-md bg-surface text-ink text-lg shadow-brutal-xs hover:bg-canvas active:translate-x-[1px] active:translate-y-[1px] active:shadow-none disabled:opacity-40 disabled:cursor-not-allowed cursor-pointer';

/** Full-screen viewer for a single attachment image: wheel/pinch zoom about the
 * cursor, drag to pan, double-click to toggle 1:1, download, Esc to close.
 *
 * It takes the object URL the thumbnail already fetched rather than a blob id —
 * re-fetching would re-download the blob and show a spinner over an image the
 * user is looking at. Mirrors the iOS transcript's tap-to-open viewer
 * (`.attachment-open` → native pinch/zoom), which is why the thumbnail alone was
 * never enough: on web there was no way to see the image at any other size. */
export function ImageLightbox({
  url,
  alt,
  onClose,
}: {
  url: string;
  alt: string;
  onClose: () => void;
}) {
  const frameRef = useRef<HTMLDivElement | null>(null);
  /** The image's own row, BELOW the chrome bar rather than under it — every
   * measurement (fit, zoom anchor, pan clamp) is against this box, so no zoom
   * level can slide the picture beneath the toolbar or the filename. */
  const stageRef = useRef<HTMLDivElement | null>(null);
  const imgRef = useRef<HTMLImageElement | null>(null);
  /** Live pointers by id — one pans, two pinch. */
  const pointersRef = useRef(new Map<number, { x: number; y: number }>());
  const travelRef = useRef(0);
  const pinchRef = useRef<number | null>(null);

  const [view, setView] = useState<View>(FIT);
  const [natural, setNatural] = useState<{ w: number; h: number } | null>(null);
  /** Layout width of the fitted <img>. `offsetWidth` is pre-transform, so it
   * stays the fit size at every zoom level — that is what the zoom percentage
   * and the 1:1 target are measured against. */
  const [fitW, setFitW] = useState(0);
  const [dragging, setDragging] = useState(false);

  /** Keep the image from being panned past its own edges: at scale s it
   * overhangs the stage by half the excess on each axis, and that is exactly how
   * far the translation may go. */
  const clampView = useCallback((next: View): View => {
    const stage = stageRef.current;
    const img = imgRef.current;
    if (!stage || !img) return next;
    const overX = Math.max(0, (img.offsetWidth * next.scale - stage.clientWidth) / 2);
    const overY = Math.max(0, (img.offsetHeight * next.scale - stage.clientHeight) / 2);
    return {
      scale: next.scale,
      x: clamp(next.x, -overX, overX),
      y: clamp(next.y, -overY, overY),
      animate: next.animate,
    };
  }, []);

  /** Zoom by `factor`, holding the point under (clientX, clientY) still. With no
   * anchor (keyboard, toolbar) it zooms about the stage's center. */
  const zoomBy = useCallback(
    (factor: number, clientX?: number, clientY?: number, animate = true) => {
      setView((v) => {
        const scale = clamp(v.scale * factor, MIN_SCALE, MAX_SCALE);
        const stage = stageRef.current;
        if (!stage || scale === v.scale) return v;
        const rect = stage.getBoundingClientRect();
        const ax = (clientX ?? rect.left + rect.width / 2) - rect.left - rect.width / 2;
        const ay = (clientY ?? rect.top + rect.height / 2) - rect.top - rect.height / 2;
        const ratio = scale / v.scale;
        return clampView({
          scale,
          x: ax - (ax - v.x) * ratio,
          y: ay - (ay - v.y) * ratio,
          animate,
        });
      });
    },
    [clampView],
  );

  const measure = useCallback(() => {
    const img = imgRef.current;
    if (img) setFitW(img.offsetWidth);
    setView((v) => clampView(v));
  }, [clampView]);

  useEffect(() => {
    window.addEventListener('resize', measure);
    return () => window.removeEventListener('resize', measure);
  }, [measure]);

  useEffect(() => {
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key === 'Escape') {
        onClose();
        return;
      }
      if (e.key === '+' || e.key === '=') zoomBy(BUTTON_STEP);
      else if (e.key === '-' || e.key === '_') zoomBy(1 / BUTTON_STEP);
      else if (e.key === '0') setView(FIT);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [onClose, zoomBy]);

  // The page behind must not scroll while the viewer owns the screen.
  useEffect(() => {
    const prev = document.body.style.overflow;
    document.body.style.overflow = 'hidden';
    return () => {
      document.body.style.overflow = prev;
    };
  }, []);

  // React registers `onWheel` passively, so `preventDefault` there can't stop
  // the page (or the browser's own zoom) from scrolling under the viewer.
  useEffect(() => {
    const frame = frameRef.current;
    if (!frame) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      zoomBy(Math.exp(-e.deltaY * WHEEL_STEP), e.clientX, e.clientY, false);
    };
    frame.addEventListener('wheel', onWheel, { passive: false });
    return () => frame.removeEventListener('wheel', onWheel);
  }, [zoomBy]);

  const toggleZoom = useCallback(
    (clientX: number, clientY: number) => {
      if (view.scale > MIN_SCALE) {
        setView(FIT);
        return;
      }
      const oneToOne = natural && fitW > 0 ? natural.w / fitW : DOUBLE_CLICK_SCALE;
      const target = clamp(
        oneToOne > 1.02 ? oneToOne : DOUBLE_CLICK_SCALE,
        MIN_SCALE,
        MAX_SCALE,
      );
      zoomBy(target / view.scale, clientX, clientY);
    },
    [fitW, natural, view.scale, zoomBy],
  );

  const pointerCenter = () => {
    const pts = [...pointersRef.current.values()];
    const sx = pts.reduce((a, p) => a + p.x, 0) / pts.length;
    const sy = pts.reduce((a, p) => a + p.y, 0) / pts.length;
    return { x: sx, y: sy };
  };

  const pointerSpread = () => {
    const [a, b] = [...pointersRef.current.values()];
    return Math.hypot(a.x - b.x, a.y - b.y);
  };

  const onPointerDown = (e: ReactPointerEvent) => {
    pointersRef.current.set(e.pointerId, { x: e.clientX, y: e.clientY });
    e.currentTarget.setPointerCapture(e.pointerId);
    if (pointersRef.current.size === 1) {
      travelRef.current = 0;
      setDragging(true);
    } else if (pointersRef.current.size === 2) {
      pinchRef.current = pointerSpread();
    }
  };

  const onPointerMove = (e: ReactPointerEvent) => {
    const prev = pointersRef.current.get(e.pointerId);
    if (!prev) return;
    pointersRef.current.set(e.pointerId, { x: e.clientX, y: e.clientY });
    travelRef.current += Math.hypot(e.clientX - prev.x, e.clientY - prev.y);

    if (pointersRef.current.size >= 2) {
      const spread = pointerSpread();
      const start = pinchRef.current;
      pinchRef.current = spread;
      if (start !== null && start > 0) {
        const c = pointerCenter();
        zoomBy(spread / start, c.x, c.y, false);
      }
      return;
    }
    if (view.scale <= MIN_SCALE) return;
    const dx = e.clientX - prev.x;
    const dy = e.clientY - prev.y;
    setView((v) => clampView({ scale: v.scale, x: v.x + dx, y: v.y + dy, animate: false }));
  };

  const endPointer = (e: ReactPointerEvent) => {
    pointersRef.current.delete(e.pointerId);
    if (pointersRef.current.size < 2) pinchRef.current = null;
    if (pointersRef.current.size === 0) setDragging(false);
  };

  const zoomPct = natural && fitW > 0 ? Math.round((fitW * view.scale * 100) / natural.w) : null;
  const zoomed = view.scale > MIN_SCALE;

  return createPortal(
    <div
      ref={frameRef}
      // `flex flex-col` with a shrink-0 bar and a flex-1 stage: the chrome takes
      // its height OUT of the image's box instead of floating over it, so a
      // zoomed-in picture can never end up behind the filename or the buttons.
      className="fixed inset-0 z-[60] flex flex-col overflow-hidden bg-black/85 touch-none select-none"
      role="dialog"
      aria-modal="true"
      aria-label={alt}
      // Every press restarts the travel tally, so a click on the backdrop is
      // never judged by how far a *previous* pan on the image drifted.
      onPointerDown={() => {
        travelRef.current = 0;
      }}
      // A pan that drifts over the backdrop must not dismiss; only a real click does.
      onClick={() => {
        if (travelRef.current <= DRAG_SLOP_PX) onClose();
      }}
    >
      <div
        className="shrink-0 flex items-center justify-between gap-3 p-3 sm:px-4"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="min-w-0 flex items-center gap-2 px-3 py-1.5 border-2 border-black rounded-md bg-surface font-mono text-[0.7rem]">
          <span className="truncate">{alt}</span>
          {natural ? (
            <span className="shrink-0 text-ink-soft tabular-nums">
              {natural.w}×{natural.h}
            </span>
          ) : null}
        </div>

        <div className="shrink-0 flex items-center gap-2">
          <button
            type="button"
            className={TOOL_BUTTON}
            title="Zoom out (-)"
            aria-label="Zoom out"
            disabled={view.scale <= MIN_SCALE}
            onClick={() => zoomBy(1 / BUTTON_STEP)}
          >
            <RiSubtractLine />
          </button>
          <button
            type="button"
            className="h-9 min-w-[4.5rem] px-2 border-2 border-black rounded-md bg-surface font-mono text-[0.75rem] font-bold tabular-nums shadow-brutal-xs hover:bg-canvas active:translate-x-[1px] active:translate-y-[1px] active:shadow-none cursor-pointer"
            title="Reset to fit (0)"
            aria-label="Reset zoom"
            onClick={() => setView(FIT)}
          >
            {zoomPct === null ? 'FIT' : `${zoomPct}%`}
          </button>
          <button
            type="button"
            className={TOOL_BUTTON}
            title="Zoom in (+)"
            aria-label="Zoom in"
            disabled={view.scale >= MAX_SCALE}
            onClick={() => zoomBy(BUTTON_STEP)}
          >
            <RiAddLine />
          </button>
          <a
            href={url}
            download={alt}
            className={TOOL_BUTTON}
            title={`Download ${alt}`}
            aria-label="Download image"
          >
            <RiDownloadLine />
          </a>
          <button
            type="button"
            className={TOOL_BUTTON}
            title="Close (Esc)"
            aria-label="Close"
            onClick={onClose}
          >
            <RiCloseLine />
          </button>
        </div>
      </div>

      {/* `overflow-hidden` is what actually keeps the chrome clear, and reserving
          the row's height is only half the fix. Zoom is a TRANSFORM: it enlarges
          the image visually without touching its layout box, so a zoomed image
          extends past this row — and a non-`none` transform makes the <img> a
          stacking context, which paints in the positioned-descendants phase, i.e.
          ON TOP of the in-flow bar above. Only the clip stops that.

          Vertical padding is symmetric on purpose, beyond just looking even: the
          <img> centers in the CONTENT box while the pan clamp measures the
          PADDING box (`clientHeight`), so unequal top/bottom would let the image
          drift further one way than the other. */}
      <div
        ref={stageRef}
        className="flex-1 min-h-0 overflow-hidden flex items-center justify-center px-4 py-8 sm:px-10 sm:py-14"
      >
        <img
          ref={imgRef}
          src={url}
          alt={alt}
          draggable={false}
          // Deliberately NO `will-change: transform`: it pins a composited layer
          // that Chrome rasterizes at the ZOOMED size, and at the 8× ceiling that
          // layer got large enough to stall the renderer outright. Transform
          // transitions are composited without the hint anyway.
          className="max-h-full max-w-full object-contain"
          style={{
            transform: `translate(${view.x}px, ${view.y}px) scale(${view.scale})`,
            transition: view.animate ? 'transform 120ms ease-out' : 'none',
            cursor: zoomed ? (dragging ? 'grabbing' : 'grab') : 'zoom-in',
          }}
          onLoad={(e) => {
            setNatural({
              w: e.currentTarget.naturalWidth,
              h: e.currentTarget.naturalHeight,
            });
            measure();
          }}
          onClick={(e) => e.stopPropagation()}
          onDoubleClick={(e) => {
            e.stopPropagation();
            toggleZoom(e.clientX, e.clientY);
          }}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={endPointer}
          onPointerCancel={endPointer}
        />
      </div>
    </div>,
    document.body,
  );
}
