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

/// `null` and `'idle'` are not the same answer. `null` is "this caller is not
/// tracking run state" — a comment's author, an execution-log row — and draws
/// no status dot at all. `'idle'` is "tracked, and there is nothing running",
/// which the team strip has to show: a roster where only the busy have a dot
/// cannot be told from one where the dot failed to load.
export type AvatarRun = 'queued' | 'running' | 'held' | 'idle' | null;

const SIZE = {
  sm: 'w-[18px] h-[18px] text-[0.46rem]',
  md: 'w-[22px] h-[22px] text-[0.5rem]',
  lg: 'w-[26px] h-[26px] text-[0.58rem]',
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
      return ' — held, the project is over its daily budget';
    case 'idle':
      return ' — idle';
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
      {run === null ? null : (
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
