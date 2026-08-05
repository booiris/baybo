import { RiCloseLine, RiSearchLine } from 'react-icons/ri';

import { assignableAgents, type Agent } from './boardModel';
import { EMPTY_FILTER, isDefault, type AssigneeFilter, type BoardFilter } from './boardFilter';

/** The `assignee` select's sentinel values, kept out of the handle space. */
const ANYONE = '';
const UNASSIGNED = 'unassigned';

function assigneeValue(filter: AssigneeFilter): string {
  switch (filter.kind) {
    case 'anyone':
      return ANYONE;
    case 'unassigned':
      return UNASSIGNED;
    case 'handle':
      return `@${filter.handle}`;
  }
}

function parseAssignee(value: string): AssigneeFilter {
  if (value === UNASSIGNED) return { kind: 'unassigned' };
  if (value.startsWith('@')) return { kind: 'handle', handle: value.slice(1) };
  return { kind: 'anyone' };
}

/**
 * A second, thin header row that narrows the board.
 *
 * Its own row because the bar above it is already carrying the switcher,
 * the team strip and the Activity toggle, and a filter that has to fight
 * for width is a filter nobody uses.
 */
export function BoardFilterBar({
  filter,
  team,
  onChange,
}: {
  filter: BoardFilter;
  team: Agent[];
  onChange: (next: BoardFilter) => void;
}) {
  return (
    <div className="shrink-0 flex flex-wrap items-center gap-2 px-4 py-1.5 border-b-2 border-black/20 bg-canvas">
      <label className="flex items-center gap-1.5 border-2 border-black rounded-md bg-surface px-2 py-0.5">
        <RiSearchLine aria-hidden className="text-ink-soft text-xs shrink-0" />
        <input
          type="search"
          value={filter.text}
          aria-label="Filter cards by title or number"
          placeholder="Title or #number"
          onChange={(event) => {
            // Not debounced: this drives a memo over an in-memory array,
            // not a request.
            onChange({ ...filter, text: event.target.value });
          }}
          className="w-44 bg-transparent font-mono text-[0.68rem] outline-none"
        />
      </label>

      <label className="flex items-center gap-1.5 font-mono text-[0.62rem] text-ink-soft">
        Assignee
        <select
          value={assigneeValue(filter.assignee)}
          aria-label="Filter cards by assignee"
          onChange={(event) => {
            onChange({ ...filter, assignee: parseAssignee(event.target.value) });
          }}
          className="border-2 border-black rounded-md bg-surface px-1.5 py-0.5 font-mono text-[0.68rem]"
        >
          <option value={ANYONE}>Anyone</option>
          <option value={UNASSIGNED}>Unassigned</option>
          {assignableAgents(team).map((agent) => (
            <option key={agent.id} value={`@${agent.handle}`}>
              @{agent.handle}
            </option>
          ))}
        </select>
      </label>

      <button
        type="button"
        aria-pressed={filter.blockedOnly}
        onClick={() => {
          onChange({ ...filter, blockedOnly: !filter.blockedOnly });
        }}
        className={`border-2 rounded-md px-2 py-0.5 font-mono text-[0.62rem] font-bold uppercase tracking-wider ${
          filter.blockedOnly ? 'border-warn bg-warn/20 text-warn' : 'border-black bg-surface'
        }`}
      >
        Blocked only
      </button>

      <label className="flex items-center gap-1.5 font-mono text-[0.68rem] text-ink-soft cursor-pointer">
        <input
          type="checkbox"
          checked={filter.showCancelled}
          onChange={(event) => {
            onChange({ ...filter, showCancelled: event.target.checked });
          }}
        />
        Show cancelled
      </label>

      {isDefault(filter) ? null : (
        <button
          type="button"
          onClick={() => {
            onChange(EMPTY_FILTER);
          }}
          className="ml-auto inline-flex items-center gap-1 border-2 border-black rounded-md bg-surface px-2 py-0.5 font-mono text-[0.62rem]"
        >
          <RiCloseLine aria-hidden /> Clear
        </button>
      )}
    </div>
  );
}
