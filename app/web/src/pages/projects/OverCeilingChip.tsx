import { RiPauseCircleLine } from 'react-icons/ri';

import { HEADER_ACTION, type Project } from './boardModel';
import { boardMeter, boardMeters, heldOnBudget } from './budgetModel';
import { activityFor, burnState, type BoardActivity } from './useBoardActivity';

/// A board that has stopped itself, said in the board header's action group.
///
/// This is where a spent ceiling belongs, and the reason is the membership
/// rule of the rail's mark: everything the rail counts is an **event** —
/// something arrived, broke, or is being asked — and a spent ceiling is none
/// of those. It is a standing condition that stops being true only when the
/// operator changes a number, so rendered as a notification dot it was
/// indistinguishable from a mark that could not be cleared at all, which is
/// exactly how it got reported.
///
/// A **chip in the header group**, and deliberately not a banner across the
/// board. This board is over its ceiling for twenty-three hours of every
/// day: a warn-tinted strip up that long is furniture, unread by the second
/// morning, which is the dot's failure again in a louder register. The
/// header's right-hand group is already where everything that acts on the
/// whole board lives, and `BoardFilterMenu` is the precedent for the shape —
/// a control that tints and carries a figure when the board is holding
/// something back.
///
/// The figure is on screen at rest (`602k / 100k`); the sentence is in the
/// title. That is the trade the bare dot could not make, because a dot has
/// nowhere to put a number.
///
/// The mark is `RiPauseCircleLine`, and the two things it is *not* are the
/// point. Not `⚑` — that is the board's mark for *blocked*, on the card
/// badge, the timeline and the issue rail, and one mark cannot carry two
/// meanings on a board where cards really do get blocked. And not a unicode
/// glyph at all: the card badges are drawn in unicode but this group's
/// icons come from Remix, so a bare character here would be a third
/// vocabulary in one row. "Paused" is also the word the field's own hint
/// uses for a ceiling that has stopped the board.
///
/// **Both** ceilings are named when both are set. `boardMeter` picks one to
/// speak in, which is right for a sentence and wrong here: money and tokens
/// are independent gates, either stops the board, and an operator shown only
/// the tighter of two that are both spent raises one and watches nothing
/// happen. The one that is actually biting is the one at full strength; a
/// ceiling with room left is dimmed beside it.
export function OverCeilingChip({
  project,
  activity,
  held,
  onOpenSettings,
}: {
  project: Project;
  activity: BoardActivity[];
  /// Cards whose run the ceiling is holding, counted off the board's own
  /// live runs — the same source the card's `held` word reads.
  held: number;
  /// Absent on the column page, which has no settings modal of its own. The
  /// chip is still worth drawing there — it is the answer to "why is nothing
  /// moving" — and the board, which does have the control, is the one press
  /// the header's back link already offers.
  onOpenSettings?: () => void;
}) {
  const live = activityFor(activity, project.id);
  const meter = boardMeter(
    { micros: live?.burn_micros ?? 0, tokens: live?.burn_tokens ?? 0 },
    { micros: project.daily_budget_micros, tokens: project.daily_budget_tokens },
  );
  if (burnState(meter.used, meter.ceiling) !== 'over') return null;

  // Absent is not zero: a ceiling of 0 is a board paused on purpose, over
  // from the first second of the day and with nothing enqueued to hold yet.
  const meters = boardMeters(
    { micros: live?.burn_micros ?? 0, tokens: live?.burn_tokens ?? 0 },
    { micros: project.daily_budget_micros, tokens: project.daily_budget_tokens },
  );
  const stopped = held > 0 ? `${heldOnBudget(held)}. ` : '';
  const spent = meters.map((m) => m.text).join(' and ');
  const title = `Over the daily ceiling — ${spent} spent today. ${stopped}Nothing new starts until it resets at 00:00 UTC, or you raise it.`;
  const skin = `${HEADER_ACTION} border-warn/60 bg-warn/12 text-warn tabular-nums`;
  const face = (
    <>
      <RiPauseCircleLine aria-hidden />
      {meters.map((m, index) => (
        <span key={m.text} className={m.over ? undefined : 'opacity-55 font-normal'}>
          {index > 0 ? <span className="opacity-40">{' · '}</span> : null}
          {m.text}
        </span>
      ))}
    </>
  );

  if (onOpenSettings === undefined) {
    return (
      <span className={skin} title={title} role="status">
        {face}
      </span>
    );
  }
  return (
    <button
      type="button"
      className={`${skin} hover:bg-warn/25`}
      title={title}
      onClick={onOpenSettings}
    >
      {face}
    </button>
  );
}
