import { useEffect, useState } from 'react';
import { RiAddLine } from 'react-icons/ri';

import { useAdminClient } from '../../api/auth';
import { fetchModelPool } from './api';
import type { Agent, IssueRun } from './boardModel';
import { previewHandle, queuedAgentIds, workingAgentIds } from './teamModel';
import { Avatar, runNote, type AvatarRun } from './Avatar';
import type { Portrait } from './portrait';

/// Seats on a board. Mirrors `MAX_TEAM_AGENTS`; the server is the judge, so
/// this only decides what the form says before it asks.
const MAX_AGENTS = 16;

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
  const working = workingAgentIds(activeRuns);
  const queued = queuedAgentIds(activeRuns);

  return (
    <div className="flex items-center gap-1">
      {team.map((agent) => (
        <TeamMemberChip
          key={agent.id}
          agent={agent}
          portrait={portrait}
          run={
            working.has(agent.id) ? 'running' : queued.has(agent.id) ? 'queued' : 'idle'
          }
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
          className="w-6 h-6 rounded-full border-2 border-dashed border-ink-soft text-ink-soft flex items-center justify-center hover:border-black hover:text-ink shrink-0"
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
      className={`shrink-0 rounded-full ${
        agent.lead ? 'bg-brand p-[2px]' : ''
      } hover:brightness-95`}
    >
      <Avatar
        handle={agent.handle}
        src={portrait(agent.id)}
        run={run}
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
  const [llm, setLlm] = useState('');
  const [pool, setPool] = useState<{ names: string[]; defaultName: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const client = useAdminClient();
  const handle = previewHandle(name);

  useEffect(() => {
    let canceled = false;
    void fetchModelPool(client).then((outcome) => {
      if (!canceled && outcome.kind === 'ok') setPool(outcome.value);
    });
    return () => {
      canceled = true;
    };
  }, [client]);

  return (
    <div className="fixed inset-0 z-40 bg-black/40 flex items-start justify-center p-6 overflow-y-auto">
      <form
        className="bg-surface border-[3px] border-black rounded-md shadow-brutal w-full max-w-md my-auto p-4 flex flex-col gap-3"
        onSubmit={(event) => {
          event.preventDefault();
          setBusy(true);
          setError(null);
          void onSubmit({
            name,
            role,
            framework,
            ...(llm === '' ? {} : { llm }),
          }).then((failure) => {
            setBusy(false);
            setError(failure);
          });
        }}
      >
        <h2 className="font-mono text-sm font-bold">New agent</h2>
        <label className="flex flex-col gap-1 font-mono text-[0.68rem]">
          Name
          <input
            autoFocus
            value={name}
            onChange={(event) => {
              setName(event.target.value);
            }}
            placeholder="Test Engineer"
            className="border-2 border-black rounded-md px-2 py-1 font-sans text-sm"
          />
          <span className="text-ink-soft">
            {handle === null
              ? 'Its @handle comes from this name, and neither can be changed afterwards — not by you, not by the agent.'
              : `Becomes @${handle} (a number is added if that is taken). Permanent — not changeable by you or by the agent.`}
          </span>
        </label>
        <label className="flex flex-col gap-1 font-mono text-[0.68rem]">
          Role
          <textarea
            value={role}
            onChange={(event) => {
              setRole(event.target.value);
            }}
            rows={3}
            placeholder="Writes and maintains the test suite."
            className="border-2 border-black rounded-md px-2 py-1 font-sans text-sm resize-y"
          />
          <span className="text-ink-soft">One line. It seeds the agent&rsquo;s own soul.</span>
        </label>
        <div className="grid grid-cols-2 gap-3">
          <label className="flex flex-col gap-1 font-mono text-[0.68rem]">
            Framework
            <select
              value={framework}
              onChange={(event) => {
                setFramework(event.target.value as Agent['framework']);
              }}
              className="border-2 border-black rounded-md px-2 py-1 font-mono text-[0.72rem]"
            >
              <option value="baybo">native</option>
              <option value="claude">claude</option>
              <option value="codex">codex</option>
            </select>
          </label>
          <label className="flex flex-col gap-1 font-mono text-[0.68rem]">
            LLM pin · optional
            <select
              value={llm}
              onChange={(event) => {
                setLlm(event.target.value);
              }}
              className="border-2 border-black rounded-md px-2 py-1 font-mono text-[0.72rem]"
            >
              <option value="">default-llm{pool === null ? '' : ` (${pool.defaultName})`}</option>
              {(pool?.names ?? []).map((model) => (
                <option key={model} value={model}>
                  {model}
                </option>
              ))}
            </select>
          </label>
        </div>
        <p className="font-mono text-[0.62rem] text-ink-soft">
          team {teamSize}/{maxAgents} · the lead hires against the same cap, but cannot choose
          either of these two.
        </p>
        {error !== null ? (
          <p className="border-2 border-err text-err rounded-md px-2 py-1 font-mono text-[0.68rem] break-words">
            {error}
          </p>
        ) : null}
        <div className="flex justify-end gap-2">
          <button
            type="button"
            onClick={onCancel}
            className="border-2 border-black rounded-md px-3 py-1 font-mono text-[0.68rem] bg-surface"
          >
            Cancel
          </button>
          <button
            type="submit"
            disabled={busy || name.trim() === '' || role.trim() === ''}
            className="border-2 border-black rounded-md px-3 py-1 font-mono text-[0.68rem] bg-brand text-white disabled:opacity-50"
          >
            {busy ? 'Adding…' : 'Add agent'}
          </button>
        </div>
      </form>
    </div>
  );
}
