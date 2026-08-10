/// The one agent face on this board.
///
/// The board card, the issue header, a timeline comment and a step row all
/// draw it, so a teammate looks the same wherever it turns up and its run
/// state reads identically in all four. The mockup draws a silhouette and
/// says in a comment that it stands in for an avatar image; we have none,
/// and initials answer *which* teammate — the question every one of those
/// four call sites is actually asking.

export type AvatarRun = 'queued' | 'running' | 'held' | null;

const SIZE = {
  sm: 'w-[18px] h-[18px] text-[0.46rem]',
  md: 'w-[22px] h-[22px] text-[0.5rem]',
  lg: 'w-[26px] h-[26px] text-[0.58rem]',
} as const;

/// Faces are picked by name rather than by roster order, so a teammate keeps
/// its colour across a hire or a removal and the eye can follow it.
const FACES = ['#aecbdd', '#d9bfd4', '#cdd4ab', '#e5c9a0'] as const;

const INK = '#2a2520';

function faceOf(handle: string, lead: boolean): { background: string; color: string } {
  if (handle === 'you') return { background: INK, color: '#faf6ec' };
  if (lead) return { background: '#f2c14e', color: INK };
  let hash = 0;
  for (const ch of handle) hash = (hash * 31 + (ch.codePointAt(0) ?? 0)) % 100_000;
  return { background: FACES[hash % FACES.length], color: INK };
}

function initialsOf(handle: string): string {
  if (handle === 'you') return 'ME';
  const parts = handle.replace(/^@/, '').split(/[^a-zA-Z0-9]+/).filter((part) => part !== '');
  if (parts.length === 0) return '?';
  const second = parts.length > 1 ? parts[1][0] : '';
  return (parts[0][0] + second).toUpperCase();
}

function titleOf(handle: string, run: AvatarRun): string {
  const who = handle === 'you' ? 'you' : `@${handle}`;
  switch (run) {
    case 'running':
      return `${who} — working`;
    case 'queued':
      return `${who} — queued, waiting for a free slot`;
    case 'held':
      return `${who} — held, the project is over its daily budget`;
    default:
      return who;
  }
}

export function Avatar({
  handle,
  run = null,
  lead = false,
  size = 'md',
}: {
  /// A bare handle (no `@`), or the literal `you` for the operator.
  handle: string;
  run?: AvatarRun;
  lead?: boolean;
  size?: keyof typeof SIZE;
}) {
  const face = faceOf(handle, lead);
  const working = run === 'running';
  return (
    <span
      title={titleOf(handle, run)}
      style={face}
      className={`relative inline-flex shrink-0 items-center justify-center rounded-full border-2 border-black font-mono font-bold ${
        SIZE[size]
      } ${run === 'queued' || run === 'held' ? 'opacity-45' : ''}`}
    >
      {initialsOf(handle)}
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
          className={`absolute -right-[3px] -bottom-[3px] w-[9px] h-[9px] rounded-full border-2 border-canvas ${
            working ? 'bg-ok' : 'bg-ink-soft'
          }`}
        />
      )}
    </span>
  );
}
