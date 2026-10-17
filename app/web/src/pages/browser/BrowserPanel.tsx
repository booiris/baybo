import { useEffect, useRef, useState, type CSSProperties, type KeyboardEvent, type PointerEvent } from 'react';
import { RiCloseLine, RiErrorWarningLine, RiTeamLine } from 'react-icons/ri';
import { useAuth } from '../../api/auth';
import type { FrameHeader } from '../../api/browserViewTypes';
import { BrowserViewConnection } from '../../api/browserViewWs';
import { createBitmapDecoder } from './frameDecoder';
import { PANEL_MAX_WIDTH_CSS, PANEL_MIN_WIDTH_PX, clampPanelWidth } from './panelPrefs';
import { ScreencastCanvas, type ScreencastCanvasHandle } from './ScreencastCanvas';
import { StatusOverlay } from './StatusOverlay';
import { TargetList } from './TargetList';
import { UrlBar } from './UrlBar';
import {
  INITIAL_VIEW_STATE,
  applyConnStatus,
  applyViewerDown,
  describeView,
  followedTarget,
  isInsecureRemote,
  viewerErrorText,
  type ShownFrame,
  type ViewState,
} from './viewState';

const KEYBOARD_RESIZE_STEP_PX = 32;

// Read-only live view of the agent's browser (CDP screencast relayed by the
// gateway), docked on the right of the chat thread. One stream: whichever tab
// the agent used last. The socket lives exactly as long as the panel is
// mounted, so closing the panel is what tells the gateway to stop streaming.
export function BrowserPanel({
  width,
  onWidthChange,
  onClose,
}: {
  width: number;
  onWidthChange: (width: number) => void;
  onClose: () => void;
}) {
  const { token, baseUrl } = useAuth();
  const [view, setView] = useState<ViewState>(INITIAL_VIEW_STATE);
  const [shown, setShown] = useState<ShownFrame | null>(null);
  const canvasRef = useRef<ScreencastCanvasHandle>(null);
  const asideRef = useRef<HTMLElement>(null);

  useEffect(() => {
    if (token === null) return;
    const decoder = createBitmapDecoder<FrameHeader>((header, bitmap) => {
      canvasRef.current?.draw(bitmap);
      bitmap.close();
      // Same-value updates bail out, so a steady stream does not re-render.
      setShown((prev) =>
        prev?.target_id === header.target_id && prev.browser_gen === header.browser_gen
          ? prev
          : { target_id: header.target_id, browser_gen: header.browser_gen },
      );
    });
    const conn = new BrowserViewConnection({
      baseUrl,
      token,
      onMessage: (msg) => setView((v) => applyViewerDown(v, msg)),
      onFrame: (frame) => decoder.push(frame),
      onStatus: (status) => setView((v) => applyConnStatus(v, status)),
    });
    return () => {
      conn.close();
      decoder.close();
      setView(INITIAL_VIEW_STATE);
      setShown(null);
    };
  }, [token, baseUrl]);

  // The panel is the right edge of its container, so its width is the
  // distance from the pointer to that edge.
  const resizeTo = (next: number) => {
    const container = asideRef.current?.parentElement;
    if (!container) return;
    onWidthChange(clampPanelWidth(next, container.clientWidth));
  };
  const startResize = (e: PointerEvent<HTMLDivElement>) => {
    const aside = asideRef.current;
    if (!aside) return;
    e.preventDefault();
    const right = aside.getBoundingClientRect().right;
    const handle = e.currentTarget;
    handle.setPointerCapture(e.pointerId);
    const move = (ev: globalThis.PointerEvent) => resizeTo(right - ev.clientX);
    const end = () => {
      handle.removeEventListener('pointermove', move);
      handle.removeEventListener('pointerup', end);
      handle.removeEventListener('pointercancel', end);
    };
    handle.addEventListener('pointermove', move);
    handle.addEventListener('pointerup', end);
    handle.addEventListener('pointercancel', end);
  };
  const resizeByKey = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === 'ArrowLeft') resizeTo(width + KEYBOARD_RESIZE_STEP_PX);
    else if (e.key === 'ArrowRight') resizeTo(width - KEYBOARD_RESIZE_STEP_PX);
    else return;
    e.preventDefault();
  };

  const overlay = describeView(view, shown);
  const target = followedTarget(view);
  const insecure = isInsecureRemote(baseUrl);

  return (
    <aside
      ref={asideRef}
      aria-label="Agent browser"
      style={
        {
          '--browser-panel-w': `${width}px`,
          '--browser-panel-min': `${PANEL_MIN_WIDTH_PX}px`,
          '--browser-panel-max': PANEL_MAX_WIDTH_CSS,
        } as CSSProperties
      }
      className="relative flex flex-col min-h-0 bg-canvas border-l-2 border-black max-md:absolute max-md:inset-0 max-md:z-40 max-md:border-l-0 md:shrink-0 md:w-(--browser-panel-w) md:max-w-(--browser-panel-max) md:min-w-(--browser-panel-min)"
    >
      <div
        role="separator"
        aria-orientation="vertical"
        aria-label="Resize browser panel"
        aria-valuenow={width}
        tabIndex={0}
        onPointerDown={startResize}
        onKeyDown={resizeByKey}
        className="max-md:hidden absolute inset-y-0 -left-1.5 w-3 z-10 cursor-col-resize touch-none hover:bg-brand/40 focus-visible:bg-brand/40 focus-visible:outline-none"
      />
      <header className="h-12 shrink-0 px-3 border-b-2 border-black flex items-center justify-between gap-2">
        <h2 className="font-bold text-sm uppercase tracking-wider">Browser</h2>
        <div className="flex items-center gap-2 shrink-0">
          <span
            className={`border-2 border-black rounded-brutal px-1.5 py-0.5 text-[0.65rem] font-bold uppercase tracking-wider ${
              overlay ? 'bg-surface text-ink-soft' : 'bg-ok text-white'
            }`}
          >
            {overlay ? 'Not live' : 'Live · view only'}
          </span>
          <button
            type="button"
            onClick={onClose}
            title="Close browser panel"
            aria-label="Close browser panel"
            className="h-7 w-7 flex items-center justify-center bg-surface text-ink-soft hover:text-ink hover:bg-canvas border-2 border-black rounded-md shadow-brutal-xs active:translate-x-[1px] active:translate-y-[1px] active:shadow-none"
          >
            <RiCloseLine className="text-base" />
          </button>
        </div>
      </header>

      <div className="flex-1 min-h-0 overflow-y-auto p-3 flex flex-col gap-2.5">
        <p className="flex items-start gap-1.5 text-ink-soft font-mono text-[0.7rem] leading-snug">
          <RiTeamLine className="shrink-0 mt-px" />
          One browser is shared by every session — what you see may be driven by any of them.
        </p>

        {insecure ? (
          <div
            role="alert"
            className="flex items-start gap-2 border-2 border-black rounded-brutal bg-warn/15 px-2.5 py-1.5 font-mono text-[0.7rem]"
          >
            <RiErrorWarningLine className="shrink-0 text-warn text-base" />
            <span>
              This dashboard is reached over plain http from another machine, so the browser's
              pixels — including anything typed into its pages — cross the network unencrypted.
              Use an ssh tunnel (<code>ssh -L</code>) or an https proxy.
            </span>
          </div>
        ) : null}

        {view.error && view.error !== 'too_many_viewers' ? (
          <div className="border-2 border-black rounded-brutal bg-surface px-2.5 py-1.5 font-mono text-[0.7rem] text-warn">
            {viewerErrorText(view.error)}
          </div>
        ) : null}

        <UrlBar target={target} />
        {/* A fixed screen-like box rather than the column's full height: a tall
            narrow panel would otherwise letterbox the frame into a strip. */}
        <div className="relative shrink-0 w-full aspect-[16/10] border-[3px] border-black rounded-brutal shadow-brutal-sm bg-black/5 overflow-hidden">
          <ScreencastCanvas ref={canvasRef} />
          {overlay ? <StatusOverlay overlay={overlay} /> : null}
        </div>

        <TargetList targets={view.targets} />
      </div>
    </aside>
  );
}
