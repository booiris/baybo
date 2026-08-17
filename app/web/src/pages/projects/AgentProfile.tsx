import { useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { RiCloseLine } from 'react-icons/ri';

import { useAdminClient } from '../../api/auth';
import { fetchModelPool, setAgentModel } from './api';
import { COLUMN_LABEL, type Agent, type Issue, type IssueRun } from './boardModel';
import { Picker } from './Picker';
import { agentRunStates, llmOptions, llmSelected, type ModelPool } from './teamModel';

export function AgentProfile({
  agent,
  team,
  issues,
  activeRuns,
  readOnly,
  projectId,
  onClose,
  onRemove,
  onChanged,
}: {
  agent: Agent;
  team: Agent[];
  issues: Issue[];
  activeRuns: IssueRun[];
  readOnly: boolean;
  projectId: string;
  onClose: () => void;
  onRemove: (agent: Agent) => void;
  /// Refetch after an edit the panel made — the roster it was handed is a
  /// snapshot, and a pin it just wrote must not read back stale.
  onChanged: () => void;
}) {
  const working = agentRunStates(activeRuns).get(agent.id) === 'running';
  const assigned = issues.filter(
    (issue) => issue.assignee === agent.id && issue.cancelled_at_ms == null,
  );
  const hiredBy = agent.hired_by;
  const hirerStillHere = hiredBy != null && team.some((row) => row.id === hiredBy.id);
  // The runs this agent has in flight, which is what the status line and
  // the queued count are both about. Drawn from the same frame the board
  // card and the team strip read, so the three cannot disagree.
  const mine = activeRuns.filter((run) => run.agent_id === agent.id);
  const live = mine.find((run) => run.status === 'running') ?? null;
  const queued = mine.filter((run) => run.status !== 'running').length;
  const [confirmRemove, setConfirmRemove] = useState(false);
  const [pinning, setPinning] = useState(false);
  const [pinError, setPinError] = useState<string | null>(null);
  const [pool, setPool] = useState<ModelPool>(null);
  const client = useAdminClient();
  const models = llmOptions(pool);
  const showing = llmSelected(agent.llm, pool);

  useEffect(() => {
    let canceled = false;
    void fetchModelPool(client).then((outcome) => {
      if (!canceled && outcome.kind === 'ok') setPool(outcome.value);
    });
    return () => {
      canceled = true;
    };
  }, [client]);

  // Esc closes, like the other layers on this board.
  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      if (event.key !== 'Escape') return;
      if (confirmRemove) {
        setConfirmRemove(false);
        return;
      }
      onClose();
    }
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('keydown', onKeyDown);
    };
  }, [confirmRemove, onClose]);

  return (
    <aside className="w-[320px] border-l-2 border-black bg-canvas flex flex-col min-h-0">
      <header className="flex items-center gap-2 px-3 py-2 border-b-2 border-black shrink-0">
        <span
          aria-hidden
          className={`w-2.5 h-2.5 rounded-full border border-black shrink-0 ${
            working ? 'bg-ok motion-safe:animate-pulse' : 'bg-ink-soft/40'
          }`}
        />
        <h2 className="font-mono text-[0.72rem] font-bold">@{agent.handle}</h2>
        {agent.lead ? (
          <span className="rounded border-2 border-black bg-brand px-1.5 font-mono text-[0.52rem] font-bold uppercase tracking-wider text-ink">
            lead
          </span>
        ) : null}
        <button
          type="button"
          aria-label="Close the agent profile"
          onClick={onClose}
          className="ml-auto text-ink-soft hover:text-ink"
        >
          <RiCloseLine />
        </button>
      </header>

      <div className="flex-1 overflow-y-auto overscroll-none p-3 flex flex-col gap-3">
        <div>
          <p className="font-sans text-[0.9rem] font-bold">{agent.name}</p>
          <p className="mt-1 font-sans text-[0.78rem] leading-snug break-words">
            {agent.description}
          </p>
        </div>

        {/* What it is doing right now, in words. The header dot alone said
            "green" without saying on what. */}
        <p className="flex items-baseline gap-1.5 font-mono text-[0.66rem]">
          <span
            className={`w-2 h-2 rounded-full border border-black shrink-0 self-center ${
              live !== null
                ? 'bg-ok motion-safe:animate-pulse'
                : queued > 0
                  ? 'bg-brand opacity-40'
                  : 'bg-ink-soft/40'
            }`}
          />
          {live !== null ? (
            <>
              working on{' '}
              <Link
                to={`/projects/${encodeURIComponent(projectId)}/issues/${String(live.number)}`}
                className="font-bold underline"
              >
                #{live.number}
              </Link>{' '}
              · run #{live.attempt}
            </>
          ) : queued > 0 ? (
            <span className="text-ink-soft">
              queued on {queued} card{queued === 1 ? '' : 's'}
            </span>
          ) : (
            <span className="text-ink-soft">idle</span>
          )}
        </p>

        <dl className="font-mono text-[0.62rem] text-ink-soft flex flex-col gap-1">
          <div className="flex gap-2">
            <dt className="shrink-0">Runs on</dt>
            <dd className="text-ink">{agent.framework} (fixed at hire)</dd>
          </div>
          <div className="flex gap-2">
            <dt className="shrink-0">Queued</dt>
            <dd className="text-ink tabular-nums">{queued}</dd>
          </div>
          <div className="flex gap-2">
            <dt className="shrink-0">Joined</dt>
            <dd className="text-ink">
              {new Date(agent.created_at_ms).toLocaleDateString()}
              {hiredBy == null
                ? ' — you added it'
                : ` — hired by @${hiredBy.handle}${hirerStillHere ? '' : ' (since removed)'}`}
            </dd>
          </div>
        </dl>

        <section>
          <h3 className="font-mono text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft">
            Setup
          </h3>
          <label className="block mt-1">
            <span className="font-mono text-[0.58rem] text-ink-soft">llm</span>
            {/* Pool-only: a pin outside it is a teammate that fails every
                time it is woken, so the picker never offers one. */}
            <Picker
              label="llm"
              className="w-full mt-0.5"
              triggerClassName="w-full justify-between text-left border-2 border-black rounded-md bg-surface px-1.5 py-0.5 font-mono text-[0.66rem]"
              panelClassName="left-0"
              value={showing}
              disabled={readOnly || pinning || pool === null}
              options={models}
              onPick={(picked) => {
                setPinning(true);
                void setAgentModel(client, agent.id, picked).then((outcome) => {
                  setPinning(false);
                  setPinError(outcome.kind === 'failed' ? outcome.message : null);
                  if (outcome.kind === 'ok') onChanged();
                });
              }}
            >
              {models.find((one) => one.value === showing)?.label ?? showing}
            </Picker>
          </label>
          {pinError == null ? null : (
            <p className="mt-1 font-mono text-[0.58rem] text-err break-words">{pinError}</p>
          )}
          <p className="mt-1.5 font-mono text-[0.58rem] text-ink-soft">
            Persona{' '}
            <a className="underline" href={`#/agents?agent=${encodeURIComponent(agent.id)}`}>
              SOUL.md · IDENTITY.md
            </a>
          </p>
          <p className="mt-0.5 font-mono text-[0.58rem] text-ink-soft">
            Skills — the shared set, in v1
          </p>
        </section>

        <section>
          <h3 className="font-mono text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft">
            On its plate
            <span className="ml-2 tabular-nums normal-case tracking-normal">
              {assigned.length}
            </span>
          </h3>
          {assigned.length === 0 ? (
            <p className="mt-1 font-mono text-[0.66rem] text-ink-soft">Nothing assigned.</p>
          ) : (
            <ul className="mt-1 flex flex-col gap-1">
              {assigned.map((issue) => (
                <li key={issue.number}>
                  <Link
                    to={`/projects/${encodeURIComponent(projectId)}/issues/${issue.number}`}
                    className="flex items-baseline gap-1.5 border-2 border-black/15 hover:border-black rounded-md px-2 py-1 bg-surface font-mono text-[0.68rem]"
                  >
                    <span className="font-bold">#{issue.number}</span>
                    <span className="min-w-0 flex-1 truncate">{issue.title}</span>
                    <span className="shrink-0 text-[0.56rem] text-ink-soft uppercase">
                      {COLUMN_LABEL[issue.status]}
                    </span>
                  </Link>
                </li>
              ))}
            </ul>
          )}
        </section>

        {readOnly || agent.lead ? null : confirmRemove ? (
          // A tombstone, not a delete: the row stays so every timeline entry
          // that names this agent still resolves, and the handle is reserved
          // for good. None of that is undoable, so it is asked for twice.
          <div className="border-2 border-err rounded-md bg-err/10 p-2">
            <p className="font-mono text-[0.62rem] text-err leading-snug">
              Remove @{agent.handle} from this board? Its past work keeps its name, the handle
              stays reserved, and there is no undo.
            </p>
            <div className="mt-2 flex gap-2">
              <button
                type="button"
                onClick={() => {
                  setConfirmRemove(false);
                  onRemove(agent);
                }}
                className="border-2 border-err bg-err text-white rounded-md px-2 py-0.5 font-mono text-[0.66rem] font-bold"
              >
                Remove
              </button>
              <button
                type="button"
                onClick={() => {
                  setConfirmRemove(false);
                }}
                className="border-2 border-black bg-surface rounded-md px-2 py-0.5 font-mono text-[0.66rem]"
              >
                Keep
              </button>
            </div>
          </div>
        ) : (
          <button
            type="button"
            onClick={() => {
              setConfirmRemove(true);
            }}
            className="self-start border-2 border-err text-err rounded-md px-2 py-0.5 font-mono text-[0.66rem] bg-surface"
          >
            Remove from project
          </button>
        )}
        {agent.lead ? (
          <p className="font-mono text-[0.6rem] text-ink-soft leading-snug">
            The lead coordinates this board and cannot be removed from it.
          </p>
        ) : null}
      </div>
    </aside>
  );
}
