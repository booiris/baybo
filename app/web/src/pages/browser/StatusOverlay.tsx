import { RiErrorWarningLine, RiInformationLine } from 'react-icons/ri';
import type { Overlay, OverlayTone } from './viewState';

const TONE_TEXT: Record<OverlayTone, string> = {
  info: 'text-ink',
  warn: 'text-warn',
  err: 'text-err',
};

export function StatusOverlay({ overlay }: { overlay: Overlay }) {
  const Icon = overlay.tone === 'info' ? RiInformationLine : RiErrorWarningLine;
  return (
    <div className="absolute inset-0 flex items-center justify-center bg-canvas/85 p-6" role="status">
      <div className="max-w-md w-full bg-surface border-2 border-black rounded-brutal shadow-brutal-sm p-4 flex flex-col gap-2">
        <div className={`flex items-center gap-2 font-bold uppercase tracking-wider text-sm ${TONE_TEXT[overlay.tone]}`}>
          <Icon className="text-lg shrink-0" />
          <span>{overlay.title}</span>
        </div>
        {overlay.detail !== undefined ? (
          <p className="font-mono text-xs text-ink-soft whitespace-pre-wrap break-words [overflow-wrap:anywhere]">
            {overlay.detail}
          </p>
        ) : null}
        {overlay.progress !== undefined ? (
          <div className="h-2 border-2 border-black rounded-brutal bg-canvas overflow-hidden">
            <div className="h-full bg-brand" style={{ width: `${overlay.progress}%` }} />
          </div>
        ) : null}
      </div>
    </div>
  );
}
