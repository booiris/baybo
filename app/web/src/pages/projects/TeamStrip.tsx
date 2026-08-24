import { useEffect, useState } from 'react';
import { RiAddLine } from 'react-icons/ri';

import { useAdminClient } from '../../api/auth';
import { fetchModelPool } from './api';
import type { Agent, IssueRun } from './boardModel';
import { LlmPinFields } from './LlmPinFields';
import {
  agentRunStates,
  handleProblem,
  UNPINNED,
  type LlmPinValue,
  type ModelPool,
} from './teamModel';
import { Avatar, AVATAR_BOX, runNote, type AvatarRun } from './Avatar';
import { Picker, type PickerOption } from './Picker';
import type { Portrait } from './portrait';

/// Seats on a board. Mirrors `MAX_TEAM_AGENTS`; the server is the judge, so
/// this only decides what the form says before it asks.
const MAX_AGENTS = 16;

/// A form field's label, and its box. The explanation lives *in* the label —
/// "@handle · from the name, and never changes" — rather than in a paragraph
/// under the input, so a field and everything true about it are one thing to
/// read instead of two.
const fieldLabel = 'font-mono text-[0.6rem] font-bold uppercase tracking-[0.12em] text-ink-soft';
const fieldBox =
  'border-2 border-black rounded-md bg-canvas px-2.5 py-[7px] font-mono text-[0.74rem]';

/// A field whose value is chosen rather than typed. Same box as the rest of
/// the form, opened by a press — not a native `<select>`, which draws an
/// operating-system menu in the middle of a hand-drawn modal.
const pickerField = {
  className: 'w-full',
  triggerClassName: `${fieldBox} w-full justify-between text-left`,
  panelClassName: 'left-0',
};

const FRAMEWORKS: PickerOption[] = [
  { value: 'baybo', label: 'native' },
  { value: 'claude', label: 'claude' },
  { value: 'codex', label: 'codex' },
];

export function TeamStrip({
  team,
  activeRuns,
  portrait,
  readOnly,
  hireOpen = false,
  onHireClosed,
  onHire,
  onOpenProfile,
}: {
  team: Agent[];
  activeRuns: IssueRun[];
  /// Resolved faces, from the board's one lookup — the strip must not start a
  /// second, or the same agent gets two pictures on one screen.
  portrait: Portrait;
  readOnly: boolean;
  /// Opened from somewhere else — the settings dialog's team section.
  hireOpen?: boolean;
  onHireClosed?: () => void;
  onHire: (body: { name: string; role: string }) => Promise<string | null>;
  onOpenProfile: (agent: Agent) => void;
}) {
  // Lifted so ⚙ team management can open the same form rather than growing
  // a second one that drifts from it.
  const [ownHiring, setOwnHiring] = useState(false);
  const hiring = ownHiring || hireOpen;
  const setHiring = (value: boolean) => {
    setOwnHiring(value);
    if (!value) onHireClosed?.();
  };
  const runs = agentRunStates(activeRuns);

  return (
    <div className="flex items-center gap-1">
      {team.map((agent) => (
        <TeamMemberChip
          key={agent.id}
          agent={agent}
          portrait={portrait}
          run={runs.get(agent.id) ?? null}
          onOpen={() => {
            onOpenProfile(agent);
          }}
        />
      ))}
      {readOnly ? null : (
        <button
          type="button"
          aria-label="Add an agent to this project"
          title="Add an agent"
          onClick={() => {
            setHiring(true);
          }}
          // A seat's own size. At 24px in a row of 26px faces it sat a pixel
          // proud at both ends of the row.
          className={`${AVATAR_BOX.lg} rounded-full border-2 border-dashed border-ink-soft text-ink-soft flex items-center justify-center hover:border-black hover:text-ink shrink-0`}
        >
          <RiAddLine className="text-xs" />
        </button>
      )}
      {hiring ? (
        <HireAgentForm
          onCancel={() => {
            setHiring(false);
          }}
          teamSize={team.length}
          maxAgents={MAX_AGENTS}
          onSubmit={async (body) => {
            const failure = await onHire(body);
            if (failure === null) setHiring(false);
            return failure;
          }}
        />
      ) : null}
    </div>
  );
}

/// One face on the strip.
///
/// A face and nothing else. The handle rides in the tooltip, because a roster
/// of up to 16 named pills is wider than the header it sits in, and because
/// the avatar is what the operator already recognises everywhere else on the
/// board. The lead wears a gold halo rather than a gold chip — with a real
/// portrait drawn over it, a tinted background is invisible.
function TeamMemberChip({
  agent,
  portrait,
  run,
  onOpen,
}: {
  agent: Agent;
  portrait: Portrait;
  run: AvatarRun;
  onOpen: () => void;
}) {
  return (
    <button
      type="button"
      aria-label={`Open @${agent.handle}'s profile`}
      onClick={onOpen}
      // `flex`, because a `<button>` is inline-block by default and the face
      // inside it is inline — so the button got a line box, and a line box
      // reserves room under the baseline for descenders. The seat came out
      // taller than the 26px face it holds, and the row stopped lining up.
      //
      // The lead's halo is a ring rather than padding for the same reason in
      // the other direction: a ring is drawn outside the box without
      // occupying any, so that seat is 26px like every other.
      className={`flex shrink-0 rounded-full hover:brightness-95 ${
        agent.lead ? 'ring-2 ring-brand' : ''
      }`}
    >
      <Avatar
        handle={agent.handle}
        src={portrait(agent.id)}
        run={run}
        // The strip is the board's live roster, so every seat carries the
        // dot — one that only appeared on the busy could not be told from
        // one that failed to load.
        dot
        size="lg"
        title={`${agent.name} (@${agent.handle})${runNote(run)}\n${agent.description}`}
      />
    </button>
  );
}

function HireAgentForm({
  onCancel,
  onSubmit,
  teamSize,
  maxAgents,
}: {
  onCancel: () => void;
  onSubmit: (body: {
    name: string;
    role: string;
    framework?: Agent['framework'];
    llm?: string;
    model?: string;
    reasoning_effort?: string;
  }) => Promise<string | null>;
  /// How many of the board's seats are taken, and how many there are. The
  /// cap is shared with the lead's own hiring tool, and the only other way
  /// to learn it was a 400 after filling the form in.
  teamSize: number;
  maxAgents: number;
}) {
  const [name, setName] = useState('');
  const [role, setRole] = useState('');
  const [framework, setFramework] = useState<Agent['framework']>('baybo');
  const [pin, setPin] = useState<LlmPinValue>(UNPINNED);
  const [pool, setPool] = useState<ModelPool>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const client = useAdminClient();
  const problem = handleProblem(name);

  useEffect(() => {
    let canceled = false;
    void fetchModelPool(client).then((outcome) => {
      if (!canceled && outcome.kind === 'ok') setPool(outcome.value);
    });
    return () => {
      canceled = true;
    };
  }, [client]);

  // The third way out, alongside ✕ and the backdrop. Not `useDismiss`: that
  // one also closes on a press outside its root, and a modal's outside is the
  // whole screen — the backdrop's own click is what handles that here.
  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      if (event.key === 'Escape') onCancel();
    }
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('keydown', onKeyDown);
    };
  }, [onCancel]);

  return (
    <div
      className="fixed inset-0 z-40 bg-black/40 flex items-start justify-center p-6 overflow-y-auto"
      onClick={onCancel}
    >
      <form
        className="bg-surface border-[3px] border-black rounded-md shadow-brutal w-full max-w-md my-auto"
        onClick={(event) => {
          event.stopPropagation();
        }}
        onSubmit={(event) => {
          event.preventDefault();
          setBusy(true);
          setError(null);
          void onSubmit({
            name,
            role,
            framework,
            // Only the levels actually chosen: an empty one means "inherit",
            // which the wire says by leaving the field out.
            ...(pin.llm === '' ? {} : { llm: pin.llm }),
            ...(pin.model === '' ? {} : { model: pin.model }),
            ...(pin.effort === '' ? {} : { reasoning_effort: pin.effort }),
          }).then((failure) => {
            setBusy(false);
            setError(failure);
          });
        }}
      >
        <div className="flex items-center gap-2 px-4 pt-3 pb-1 font-mono text-[0.66rem] text-ink-soft">
          <span className="font-bold text-ink">New agent</span>
          <button
            type="button"
            className="ml-auto cursor-pointer px-1"
            onClick={onCancel}
            aria-label="Close"
          >
            ✕
          </button>
        </div>

        <div className="px-4 pb-4 flex flex-col gap-2.5">
          {/* One field, not a name and a preview of the handle derived from
              it. The name *is* the handle, so the operator types the thing
              they are about to be stuck with instead of reading it back. */}
          <label className="flex flex-col gap-1">
            <span className={fieldLabel}>
              Name · this is its @handle, and neither ever changes
            </span>
            <input
              autoFocus
              value={name}
              onChange={(event) => {
                setName(event.target.value);
              }}
              placeholder="test-engineer"
              aria-invalid={problem !== null}
              className={`${fieldBox} ${problem === null ? '' : 'border-err'}`}
            />
            {problem === null ? null : (
              <span className="font-mono text-[0.6rem] text-err">{problem}</span>
            )}
          </label>
          <label className="flex flex-col gap-1">
            <span className={fieldLabel}>Role · one line, and it seeds the agent’s own soul</span>
            <textarea
              value={role}
              onChange={(event) => {
                setRole(event.target.value);
              }}
              rows={3}
              placeholder="Writes and maintains the test suite."
              className={`${fieldBox} font-sans resize-y`}
            />
          </label>
          <div className="grid grid-cols-2 gap-3">
            <label className="flex flex-col gap-1">
              <span className={fieldLabel}>Framework</span>
              <Picker
                {...pickerField}
                label="Framework"
                value={framework}
                disabled={busy}
                options={FRAMEWORKS}
                onPick={(next) => {
                  setFramework(next as Agent['framework']);
                }}
              >
                {FRAMEWORKS.find((one) => one.value === framework)?.label ?? framework}
              </Picker>
            </label>
            <LlmPinFields
              value={pin}
              pool={pool}
              disabled={busy}
              fieldLabelClass={fieldLabel}
              pickerProps={pickerField}
              onChange={setPin}
            />
          </div>
          {error !== null ? (
            <p className="border-2 border-err text-err rounded-md px-2 py-1 font-mono text-[0.68rem] break-words">
              {error}
            </p>
          ) : null}
        </div>

        <div className="flex items-center gap-3 px-4 py-3 border-t-2 border-black bg-canvas">
          <p className="font-mono text-[0.58rem] text-ink-soft leading-snug">
            team {teamSize}/{maxAgents} · the lead hires against the same cap, and cannot choose
            either of these two.
          </p>
          <button
            type="submit"
            disabled={busy || name.trim() === '' || role.trim() === '' || problem !== null}
            // Dark on gold. White on this brand fails contrast, which is why
            // the design system says to pair it with ink.
            className="ml-auto shrink-0 border-2 border-black rounded-md px-3 py-1 font-mono text-[0.68rem] font-bold bg-brand text-ink disabled:opacity-50"
          >
            {busy ? 'Creating…' : 'Create agent'}
          </button>
        </div>
      </form>
    </div>
  );
}
