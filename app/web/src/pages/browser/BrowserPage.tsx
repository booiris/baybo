import { useEffect, useRef, useState } from 'react';
import { RiErrorWarningLine, RiTeamLine } from 'react-icons/ri';
import { useAuth } from '../../api/auth';
import type { FrameHeader } from '../../api/browserViewTypes';
import { BrowserViewConnection } from '../../api/browserViewWs';
import { createBitmapDecoder } from './frameDecoder';
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

// Read-only live view of the agent's browser (CDP screencast relayed by the
// gateway). One stream: whichever tab the agent used last.
export function BrowserPage() {
  const { token, baseUrl } = useAuth();
  const [view, setView] = useState<ViewState>(INITIAL_VIEW_STATE);
  const [shown, setShown] = useState<ShownFrame | null>(null);
  const canvasRef = useRef<ScreencastCanvasHandle>(null);

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

  const overlay = describeView(view, shown);
  const target = followedTarget(view);
  const insecure = isInsecureRemote(baseUrl);

  return (
    <div className="p-5 h-full flex flex-col gap-3 overflow-hidden">
      <div className="flex justify-between items-start gap-3 flex-wrap">
        <div>
          <h2 className="text-[1.7rem] font-bold uppercase -tracking-[0.05em] mb-1">Browser</h2>
          <p className="flex items-center gap-1.5 text-ink-soft font-mono text-[0.85rem]">
            <RiTeamLine className="shrink-0" />
            One browser is shared by every session — what you see may be driven by any of them.
          </p>
        </div>
        <span
          className={`shrink-0 border-2 border-black rounded-brutal px-2 py-0.5 text-xs font-bold uppercase tracking-wider ${
            overlay ? 'bg-surface text-ink-soft' : 'bg-ok text-white'
          }`}
        >
          {overlay ? 'Not live' : 'Live · view only'}
        </span>
      </div>

      {insecure ? (
        <div
          role="alert"
          className="flex items-start gap-2 border-2 border-black rounded-brutal bg-warn/15 px-3 py-2 font-mono text-xs"
        >
          <RiErrorWarningLine className="shrink-0 text-warn text-base" />
          <span>
            This dashboard is reached over plain http from another machine, so the browser's
            pixels — including anything typed into its pages — cross the network unencrypted. Use
            an ssh tunnel (<code>ssh -L</code>) or an https proxy.
          </span>
        </div>
      ) : null}

      {view.error && view.error !== 'too_many_viewers' ? (
        <div className="border-2 border-black rounded-brutal bg-surface px-3 py-2 font-mono text-xs text-warn">
          {viewerErrorText(view.error)}
        </div>
      ) : null}

      <div className="flex-1 min-h-0 flex gap-3">
        <div className="flex-1 min-w-0 flex flex-col gap-2">
          <UrlBar target={target} />
          <div className="relative flex-1 min-h-0 border-[3px] border-black rounded-brutal shadow-brutal bg-black/5 overflow-hidden">
            <ScreencastCanvas ref={canvasRef} />
            {overlay ? <StatusOverlay overlay={overlay} /> : null}
          </div>
        </div>
        <aside className="w-64 shrink-0 hidden md:flex flex-col min-h-0">
          <TargetList targets={view.targets} />
        </aside>
      </div>
    </div>
  );
}
