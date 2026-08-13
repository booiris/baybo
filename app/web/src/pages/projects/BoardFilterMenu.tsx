import { useCallback, useEffect, useRef, useState } from 'react';
import { RiCloseLine, RiEqualizerLine, RiSearchLine } from 'react-icons/ri';

import { useDismiss } from '../../components/useDismiss';
import {
  assignableAgents,
  HEADER_ACTION,
  HEADER_ACTION_OFF,
  HEADER_ACTION_ON,
  type Agent,
} from './boardModel';
import {
  EMPTY_FILTER,
  isDefault,
  restrictionCount,
  type AssigneeFilter,
  type BoardFilter,
} from './boardFilter';

const ANYONE = '';
const UNASSIGNED = 'unassigned';

/// The search box, on a local draft.
///
/// The filter lives in the URL, so a plain controlled value here is a round
/// trip: keystroke → `setSearchParams` → navigation → re-parse → back in as
/// `value`. An IME composes *inside* the field over many keystrokes, and if
/// that round trip lands one render late React writes the stale value back
/// over the half-composed candidate — the candidate window drops, or a bare
/// pinyin letter is left behind. The draft is what the field shows; the URL is
/// told when the composition ends.
function SearchField({ value, onChange }: { value: string; onChange: (text: string) => void }) {
  const [draft, setDraft] = useState(value);
  const composing = useRef(false);

  // Follow the URL when it moves on its own — Clear, a pasted link, the back
  // button — but never while the IME owns the field.
  useEffect(() => {
    if (!composing.current) setDraft(value);
  }, [value]);

  const publish = (text: string) => {
    setDraft(text);
    if (!composing.current) onChange(text);
  };

  return (
    <label className="flex items-center gap-1.5 border-2 border-black rounded-md bg-canvas px-2 py-1">
      <RiSearchLine aria-hidden className="text-ink-soft text-xs shrink-0" />
      <input
        // The panel opens ready to type: search is the one control here that
        // is reached for mid-thought rather than set and left.
        autoFocus
        type="search"
        value={draft}
        aria-label="Filter cards by title or number"
        placeholder="Title or #number"
        onCompositionStart={() => {
          composing.current = true;
        }}
        onCompositionEnd={(event) => {
          // Chrome fires this before the input event that carries the
          // committed text and Safari after, so publish from both and let the
          // two agree rather than depending on the order.
          composing.current = false;
          publish(event.currentTarget.value);
        }}
        onChange={(event) => {
          publish(event.target.value);
        }}
        className="w-full min-w-0 bg-transparent font-mono text-[0.68rem] outline-none"
      />
    </label>
  );
}

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

/// One button in the header, not a row under it.
///
/// The four controls are ~580px laid out in a line, which is why they had a
/// strip of their own — and that strip cost every board 30px of column height
/// to show four controls that are set once and then looked past. Collapsed,
/// the state cannot hide: the trigger goes gold and carries the number of
/// narrowings, so a board that is holding cards back always says so from the
/// header.
export function BoardFilterMenu({
  filter,
  team,
  onChange,
}: {
  filter: BoardFilter;
  team: Agent[];
  onChange: (next: BoardFilter) => void;
}) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const close = useCallback(() => {
    setOpen(false);
  }, []);
  useDismiss({ open, root, trigger, onDismiss: close });

  const active = restrictionCount(filter);

  return (
    <div className="relative" ref={root}>
      <button
        ref={trigger}
        type="button"
        aria-haspopup="true"
        aria-expanded={open}
        aria-label={active === 0 ? 'Filter the board' : `Filter the board (${active} active)`}
        title="Narrow what the columns show"
        onClick={() => {
          setOpen((showing) => !showing);
        }}
        className={`${HEADER_ACTION} ${
          active > 0 || open ? HEADER_ACTION_ON : HEADER_ACTION_OFF
        }`}
      >
        <RiEqualizerLine aria-hidden />
        Filter
        {active > 0 ? (
          <span className="rounded-full bg-ink text-brand px-1.5 tabular-nums">{active}</span>
        ) : null}
      </button>

      {open ? (
        <div className="absolute right-0 top-[calc(100%+6px)] z-40 w-[250px] bg-surface border-2 border-black rounded-md shadow-brutal p-3 flex flex-col gap-2.5">
          <SearchField
            value={filter.text}
            onChange={(text) => {
              onChange({ ...filter, text });
            }}
          />

          <label className="flex flex-col gap-1 font-mono text-[0.62rem] text-ink-soft">
            Assignee
            <select
              value={assigneeValue(filter.assignee)}
              aria-label="Filter cards by assignee"
              onChange={(event) => {
                onChange({ ...filter, assignee: parseAssignee(event.target.value) });
              }}
              className="border-2 border-black rounded-md bg-canvas px-1.5 py-1 font-mono text-[0.68rem] text-ink"
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

          <label className="flex items-center gap-1.5 font-mono text-[0.68rem] cursor-pointer">
            <input
              type="checkbox"
              checked={filter.blockedOnly}
              onChange={(event) => {
                onChange({ ...filter, blockedOnly: event.target.checked });
              }}
            />
            Blocked only
          </label>

          <label className="flex items-center gap-1.5 font-mono text-[0.68rem] cursor-pointer">
            <input
              type="checkbox"
              checked={filter.failedOnly}
              onChange={(event) => {
                onChange({ ...filter, failedOnly: event.target.checked });
              }}
            />
            Failed run only
          </label>

          <label className="flex items-center gap-1.5 font-mono text-[0.68rem] cursor-pointer">
            <input
              type="checkbox"
              checked={!filter.showCancelled}
              onChange={(event) => {
                onChange({ ...filter, showCancelled: !event.target.checked });
              }}
            />
            Hide cancelled
          </label>

          {isDefault(filter) ? null : (
            <button
              type="button"
              onClick={() => {
                onChange(EMPTY_FILTER);
              }}
              className="self-start inline-flex items-center gap-1 border-2 border-black rounded-md bg-surface px-2 py-0.5 font-mono text-[0.62rem] hover:bg-brand/25"
            >
              <RiCloseLine aria-hidden /> Clear
            </button>
          )}
        </div>
      ) : null}
    </div>
  );
}
