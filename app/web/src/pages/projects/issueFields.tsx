import { Avatar } from './Avatar';
import type { PickerOption } from './Picker';
import type { Portrait } from './portrait';
import {
  COLUMNS,
  COLUMN_LABEL,
  PRIORITIES,
  STATUS_PILL,
  type Agent,
  type IssuePriority,
  type IssueStatus,
} from './boardModel';

/// The three properties a card is created with and edited by, spelled once.
///
/// The rail on the issue page and the chip row in the create modal set the
/// same three things, and until this existed they each had their own list of
/// what the choices are and their own idea of what one looks like — the modal
/// had already grown a second `PRIORITY_LABEL`. Two spellings of "High" is one
/// stale rename away from a board where the same card reads two ways.

export const PRIORITY_LABEL: Record<IssuePriority, string> = {
  urgent: 'Urgent',
  high: 'High',
  medium: 'Medium',
  low: 'Low',
  none: 'No priority',
};

/// Priority is read at a glance or not at all, so it carries its own colour
/// rather than sitting in the same ink as everything around it.
export const PRIORITY_TONE: Record<IssuePriority, string> = {
  urgent: 'text-err',
  high: 'text-warn',
  medium: 'text-ink',
  low: 'text-ink-soft',
  none: 'text-ink-soft',
};

/// The status chip minus its horizontal padding — a trigger and a panel row
/// need different amounts, and everything else about them has to match.
export const statusChipShape =
  'rounded-md border py-[1px] font-mono text-[0.58rem] uppercase tracking-wider';

export function StatusPill({ status }: { status: IssueStatus }) {
  return (
    <span className={`${statusChipShape} px-2 ${STATUS_PILL[status]}`}>
      {COLUMN_LABEL[status]}
    </span>
  );
}

/// In board order and wearing the board's own pills, so the panel shows the
/// pipeline the card moves along rather than listing five words.
export const statusOptions: PickerOption[] = COLUMNS.map((status) => ({
  value: status,
  label: COLUMN_LABEL[status],
  node: <StatusPill status={status} />,
}));

export const priorityOptions: PickerOption[] = PRIORITIES.map((priority) => ({
  value: priority,
  label: PRIORITY_LABEL[priority],
  node: <span className={PRIORITY_TONE[priority]}>{PRIORITY_LABEL[priority]}</span>,
}));

/// Faces, not a list of handles: the roster is recognised by its portraits
/// everywhere else on this board, and a picker that spells them out instead is
/// the one place it is not.
export function assigneeOptions(agents: Agent[], portrait: Portrait): PickerOption[] {
  return [
    { value: '', label: 'Unassigned' },
    ...agents.map((agent) => ({
      value: agent.id,
      label: `@${agent.handle} — ${agent.name}`,
      node: (
        <span className="inline-flex min-w-0 items-center gap-1.5">
          <Avatar handle={agent.handle} src={portrait(agent.id)} size="sm" />
          <span className="truncate">@{agent.handle}</span>
        </span>
      ),
    })),
  ];
}
