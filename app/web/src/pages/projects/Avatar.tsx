/// The one agent face on this board.
///
/// The board card, the issue header, a timeline comment and a step row all
/// draw it, so a teammate looks the same wherever it turns up and its run
/// state reads identically in all four.
///
/// Which picture to draw is decided in `portrait.ts` — an uploaded avatar
/// when there is one, a generated bottts face when there is not. What is
/// decided *here* is only how it is framed. The operator and the board are
/// not agents, get no portrait, and keep a monogram.

/// What this agent's run is doing, or `null` for nothing running. Whether a
/// dot is drawn is `dot`'s to say, not this — see it below.
import { HELD_RUN_NOTE } from './budgetModel';

export type AvatarRun = 'queued' | 'running' | 'held' | null;

/// A face's footprint. Exported because anything that has to line up with one
/// — the team strip's `＋` seat — must take the number from here: written out
/// a second time, the row stops being a row the first time this changes.
export const AVATAR_BOX = {
  sm: 'w-[18px] h-[18px]',
  md: 'w-[22px] h-[22px]',
  lg: 'w-[26px] h-[26px]',
} as const;

const SIZE = {
  sm: `${AVATAR_BOX.sm} text-[0.46rem]`,
  md: `${AVATAR_BOX.md} text-[0.5rem]`,
  lg: `${AVATAR_BOX.lg} text-[0.58rem]`,
} as const;

const INK = '#2a2520';

function initialsOf(handle: string): string {
  if (handle === 'you') return 'ME';
  const parts = handle.replace(/^@/, '').split(/[^a-zA-Z0-9]+/).filter((part) => part !== '');
  if (parts.length === 0) return '?';
  const second = parts.length > 1 ? parts[1][0] : '';
  return (parts[0][0] + second).toUpperCase();
}

/// How a run state reads in a tooltip. Exported because a caller with a
/// richer sentence than `@handle` still has to say the state the same way —
/// two spellings of "working" is how the strip and the card start disagreeing
/// about what the ring means.
export function runNote(run: AvatarRun): string {
  switch (run) {
    case 'running':
      return ' — working';
    case 'queued':
      return ' — queued, waiting for a free slot';
    case 'held':
      return ` — held, ${HELD_RUN_NOTE}`;
    default:
      return '';
  }
}

function titleOf(handle: string, run: AvatarRun): string {
  return `${handle === 'you' ? 'you' : `@${handle}`}${runNote(run)}`;
}

export function Avatar({
  handle,
  src = null,
  run = null,
  size = 'md',
  dot = false,
  title,
}: {
  /// A bare handle (no `@`), or the literal `you` for the operator. Names the
  /// chip; the picture comes from `src`.
  handle: string;
  /// The resolved portrait. Null for whoever has none — the operator, the
  /// board — who fall back to initials.
  src?: string | null;
  run?: AvatarRun;
  size?: keyof typeof SIZE;
  /// Draw the live-state dot even with nothing running — grey for idle.
  ///
  /// Asked for rather than assumed, because most callers have no run data at
  /// all: a comment's author, a picker's options. A grey dot there would be
  /// this component claiming "idle" on their behalf, which they never said.
  /// A run state implies a dot; this is how the team strip gets one without.
  dot?: boolean;
  /// A fuller sentence than `@handle`, from a caller that has one. It goes
  /// here rather than on a wrapper: nested `title`s mean the tooltip depends
  /// on which pixel the cursor is over.
  title?: string;
}) {
  const working = run === 'running';
  return (
    <span
      title={title ?? titleOf(handle, run)}
      style={src === null ? { background: INK, color: '#faf6ec' } : undefined}
      className={`relative inline-flex shrink-0 items-center justify-center rounded-full border border-black font-mono font-bold ${
        SIZE[size]
      } ${run === 'queued' || run === 'held' ? 'opacity-45' : ''}`}
    >
      {src === null ? (
        initialsOf(handle)
      ) : (
        // Clipped by the image rather than by the chip: `overflow-hidden` on
        // the chip would cut off the run ring and the status dot, which both
        // sit outside its box on purpose.
        <img src={src} alt="" className="absolute inset-0 h-full w-full rounded-full object-cover" />
      )}
      {working ? (
        <span
          aria-hidden
          className="absolute -inset-[2px] rounded-full motion-safe:animate-spin"
          style={{
            animationDuration: '1.6s',
            background:
              'conic-gradient(from 0deg, transparent 0 300deg, rgba(242,193,78,0.95) 330deg, transparent 360deg)',
            WebkitMask:
              'radial-gradient(farthest-side, transparent calc(100% - 4px), #000 calc(100% - 3px))',
            mask: 'radial-gradient(farthest-side, transparent calc(100% - 4px), #000 calc(100% - 3px))',
          }}
        />
      ) : null}
      {!dot && run === null ? null : (
        <span
          aria-hidden
          className={`absolute -right-[3px] -bottom-[3px] w-[9px] h-[9px] rounded-full border border-canvas ${
            working ? 'bg-ok' : 'bg-ink-soft'
          }`}
        />
      )}
    </span>
  );
}
