import { useState } from 'react';
import { RiPushpin2Fill, RiPushpin2Line } from 'react-icons/ri';

import type { Issue } from './boardModel';
import { Avatar, type AvatarRun, runNote } from './Avatar';

/// What a card wears, wherever a card is drawn — the board's column card and
/// the column page's row say these the same way because they say them from
/// here.

/// The glyph's medium wears `text-info` while the picker word "Medium"
/// (issueFields' `PRIORITY_TONE`) wears plain ink — deliberately. A ◆ in ink
/// vanishes into the mono text beside it, and the mark is read at a glance
/// or not at all; the words carry themselves.
export const PRIORITY_MARK: Record<Issue['priority'], { glyph: string; tone: string } | null> = {
  urgent: { glyph: '▲▲', tone: 'text-err' },
  high: { glyph: '▲', tone: 'text-warn' },
  medium: { glyph: '◆', tone: 'text-info' },
  low: { glyph: '▽', tone: 'text-ink-soft' },
  none: null,
};

export function BlockedBadge({ reason }: { reason: string }) {
  return (
    <span
      className="self-start border border-warn/50 bg-warn/10 text-warn rounded px-1.5 font-mono text-[0.56rem] font-bold uppercase"
      title={reason}
    >
      ⚑ Blocked
    </span>
  );
}

/// A failed run leaves the card exactly where it was, wearing nothing — so
/// the board's badge counted failures on a board where no card admitted to
/// one, and finding them meant opening cards one at a time. It wears the
/// Blocked badge's shape in the error tone, because both say the same thing:
/// this card has stopped.
export function FailedBadge() {
  return (
    <span
      className="self-start border border-err/50 bg-err/10 text-err rounded px-1.5 font-mono text-[0.56rem] font-bold uppercase"
      title="This card's newest run failed. Open it to retry."
    >
      ✕ Run failed
    </span>
  );
}

/// The branch chip. Copies rather than navigates: it sits inside a card
/// whose own click opens the issue, so without stopping the event the one
/// affordance the mockup gives it would be unreachable.
export function BranchChip({ branch }: { branch: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      title={`${branch} — click to copy`}
      className="self-start max-w-full truncate border border-black/25 bg-canvas rounded px-1.5 font-mono text-[0.56rem] text-ink-soft cursor-pointer hover:border-black"
      onClick={(event) => {
        event.stopPropagation();
        try {
          // `navigator.clipboard` is typed as always present but is
          // **absent** outside a secure context — which a dashboard served
          // over plain http on a LAN address is. Reading `.writeText` off
          // `undefined` throws synchronously, so the try is the guard; a
          // rejected promise (permission refused) is the other half.
          void navigator.clipboard.writeText(branch).then(
            () => {
              setCopied(true);
              window.setTimeout(() => {
                setCopied(false);
              }, 1200);
            },
            () => {
              // Nothing: the branch name is still in the title attribute,
              // and a "copied" that did not happen is worse than silence.
            },
          );
        } catch {
          // Same reasoning.
        }
      }}
    >
      ⑂ {copied ? 'copied' : branch}
    </button>
  );
}

/// A ring, not a bar. It sits inline beside the assignee on a card whose
/// other rows are already full-width, and a circle reads as "how far
/// through the steps" at a glance where a fifth horizontal bar would just
/// be another line of card furniture.
export function SubIssueRing({ progress }: { progress: { done: number; total: number } }) {
  const { done, total } = progress;
  const fraction = total === 0 ? 0 : done / total;
  const radius = 5;
  const circumference = 2 * Math.PI * radius;
  return (
    <span
      className="inline-flex items-center gap-1 shrink-0"
      title={`${done} of ${total} sub-issues done`}
    >
      <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true" className="shrink-0">
        <circle
          cx="7"
          cy="7"
          r={radius}
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          className="text-black/20"
        />
        <circle
          cx="7"
          cy="7"
          r={radius}
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeDasharray={`${circumference * fraction} ${circumference}`}
          transform="rotate(-90 7 7)"
          className={done === total ? 'text-ok' : 'text-brand'}
        />
      </svg>
      <span className="font-mono text-[0.54rem] text-ink-soft tabular-nums">
        {done}/{total}
      </span>
    </span>
  );
}

/// The pin, which is the same element as the marker: a pinned card wears a
/// filled pushpin, and pressing it is what unpins.
///
/// An unpinned card's pin is invisible until the row is hovered or the
/// button is tabbed to, because a permanent outline glyph on every card is
/// furniture on a surface whose whole language is "nothing here that is not
/// saying something". That does make it a shortcut rather than the only
/// door — the issue's own rail carries a Pinned row that is always there,
/// which is what a touch screen reaches for.
///
/// It has to stop the press twice over. The card's own click opens the
/// issue (the reason `BranchChip` stops one), and the whole card is also the
/// drag handle — dnd-kit's listeners sit on the wrapper, so without stopping
/// `pointerdown` a press that drifted 4px would pick the card up instead.
export function PinButton({
  pinned,
  disabled = false,
  onToggle,
}: {
  pinned: boolean;
  disabled?: boolean;
  onToggle: (pinned: boolean) => void;
}) {
  const Glyph = pinned ? RiPushpin2Fill : RiPushpin2Line;
  return (
    <button
      type="button"
      disabled={disabled}
      aria-pressed={pinned}
      title={pinned ? 'Unpin — stop keeping this at the top' : 'Pin to the top of this column'}
      aria-label={pinned ? 'Unpin this card' : 'Pin this card to the top of its column'}
      onPointerDown={(event) => {
        event.stopPropagation();
      }}
      onClick={(event) => {
        event.preventDefault();
        event.stopPropagation();
        onToggle(!pinned);
      }}
      className={`shrink-0 leading-none transition-opacity disabled:opacity-30 disabled:cursor-not-allowed ${
        pinned
          ? 'text-brand-hover hover:text-ink'
          : 'text-ink-soft opacity-0 hover:text-ink focus-visible:opacity-100 group-hover:opacity-100'
      }`}
    >
      <Glyph aria-hidden className="text-[0.8rem]" />
    </button>
  );
}

/// The assignee as a press: the face and its handle open the profile
/// wherever a card shows them. The card's own click opens the issue, so the
/// button has to claim the event or it could never be the profile's entry
/// point. Without `onOpen` — the drag overlay, an archived surface — the
/// same face draws as a picture instead of a door.
export function AssigneeFace({
  handle,
  src,
  run = null,
  onOpen,
}: {
  handle: string;
  src: string | null;
  run?: AvatarRun;
  onOpen?: () => void;
}) {
  const face = (
    <>
      <Avatar handle={handle} src={src} run={run} size="sm" />
      <span className="min-w-0 truncate font-mono text-[0.58rem] text-ink-soft">@{handle}</span>
    </>
  );
  if (onOpen === undefined) {
    return <span className="flex min-w-0 items-center gap-1.5">{face}</span>;
  }
  return (
    <button
      type="button"
      title={`Open @${handle}'s profile`}
      onClick={(event) => {
        event.stopPropagation();
        onOpen();
      }}
      className="flex items-center gap-1.5 min-w-0 cursor-pointer hover:underline"
    >
      {face}
    </button>
  );
}

/// A card nobody is on says so. Rendering nothing made an untriaged card
/// look the same as one whose footer had simply scrolled off, and the
/// parking lot is exactly where the operator is scanning for work to hand
/// out.
export function UnassignedMark() {
  return (
    <>
      <span className="w-4 h-4 rounded-full border-2 border-dashed border-black/40 shrink-0" />
      <span className="font-mono text-[0.58rem] text-ink-soft italic">unassigned</span>
    </>
  );
}

/// The one word a card says about its run, and the one place it is spelled.
///
/// Both views wrote it inline as `run === 'running' ? 'working' : 'queued'`,
/// which is a two-branch answer to a three-value question: a run the daily
/// ceiling had stopped printed "queued" on a board with every slot free. Held
/// takes the warn tone the issue page's own run chip already gives it, so the
/// card and the card's page say the same thing in the same colour.
///
/// The `why` is in the title rather than the cell because the cell is 0.54rem
/// of uppercase mono — it has room for a state, not for a reason.
const RUN_WORD: Record<Exclude<AvatarRun, null>, { word: string; tone: string }> = {
  running: { word: 'working', tone: 'text-ok' },
  queued: { word: 'queued', tone: 'text-ink-soft' },
  held: { word: 'held', tone: 'text-warn' },
};

export function RunWord({ run, className = '' }: { run: AvatarRun; className?: string }) {
  if (run === null) return null;
  const { word, tone } = RUN_WORD[run];
  return (
    <span
      title={`This card's run${runNote(run)}`}
      className={`shrink-0 font-mono text-[0.54rem] font-bold uppercase ${tone} ${className}`}
    >
      {word}
    </span>
  );
}

/// What the card's number means, spelled out where it is hovered. The
/// badge counts an agent's comments *and* an agent moving the card into
/// Review, so "messages" alone would be a lie on a card that has only been
/// handed back.
export function unreadTitle(unread: number): string {
  return `${unread} new since you opened this card`;
}

/// The board's only count, and the corner the eye lands on. It is the
/// countable half of the rail's dot: every number here is one card away
/// from being cleared, which is the whole reason the rail's own number
/// became a dot.
export function UnreadPill({ unread }: { unread: number }) {
  return (
    <span
      title={unreadTitle(unread)}
      aria-label={unreadTitle(unread)}
      className="shrink-0 min-w-[1rem] h-4 px-1 rounded-full border-2 border-black bg-err text-white text-[0.55rem] font-bold leading-[0.75rem] text-center tabular-nums"
    >
      {unread}
    </span>
  );
}
