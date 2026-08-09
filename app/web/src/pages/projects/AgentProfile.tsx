import { Link } from 'react-router-dom';
import { RiCloseLine } from 'react-icons/ri';

import { COLUMN_LABEL, type Agent, type Issue, type IssueRun } from './boardModel';
import { workingAgentIds } from './teamModel';

export function AgentProfile({
  agent,
  team,
  issues,
  activeRuns,
  readOnly,
  projectId,
  onClose,
  onRemove,
}: {
  agent: Agent;
  team: Agent[];
  issues: Issue[];
  activeRuns: IssueRun[];
  readOnly: boolean;
  projectId: string;
  onClose: () => void;
  onRemove: (agent: Agent) => void;
}) {
  const working = workingAgentIds(activeRuns).has(agent.id);
  const assigned = issues.filter(
    (issue) => issue.assignee === agent.id && issue.cancelled_at_ms == null,
  );
  const hiredBy = agent.hired_by;
  const hirerStillHere = hiredBy != null && team.some((row) => row.id === hiredBy.id);

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
          <span className="rounded border border-black/30 bg-brand/25 px-1 font-mono text-[0.52rem] font-bold uppercase">
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

      <div className="flex-1 overflow-y-auto p-3 flex flex-col gap-3">
        <div>
          <p className="font-sans text-[0.9rem] font-bold">{agent.name}</p>
          <p className="mt-1 font-sans text-[0.78rem] leading-snug break-words">
            {agent.description}
          </p>
        </div>

        <dl className="font-mono text-[0.62rem] text-ink-soft flex flex-col gap-1">
          <div className="flex gap-2">
            <dt className="shrink-0">Runs on</dt>
            <dd className="text-ink">{agent.framework}</dd>
          </div>
          {agent.llm == null ? null : (
            <div className="flex gap-2">
              <dt className="shrink-0">Model</dt>
              <dd className="text-ink break-all">{agent.llm}</dd>
            </div>
          )}
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

        {readOnly || agent.lead ? null : (
          <button
            type="button"
            onClick={() => {
              onRemove(agent);
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
