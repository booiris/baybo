import { Link } from 'react-router-dom';

import { COLUMN_LABEL, type Agent, type Issue, type IssueStatus } from './boardModel';
import { groupByStage, stageProgress } from './stageModel';
import { handleOf } from './teamModel';

const STAGE_TONE: Record<'done' | 'open' | 'waiting', string> = {
  done: 'border-ok/50 bg-ok/10 text-ok',
  open: 'border-black bg-brand/30 text-ink',
  waiting: 'border-black/25 bg-canvas text-ink-soft',
};

const STAGE_NOTE: Record<'done' | 'open' | 'waiting', string> = {
  done: 'finished',
  open: 'in progress',
  waiting: 'starts when the stage above finishes',
};

/**
 * A parent card's steps, grouped into the barriers the server enforces.
 *
 * The stage grouping is the whole point of showing them here: until now the
 * barrier existed — finishing a stage wakes the parent's assignee — and was
 * invisible, so a card could be waiting on a step nobody could see.
 */
export function SubIssues({
  projectId,
  children,
  team,
  disabled,
  onStatus,
}: {
  projectId: string;
  children: Issue[];
  team: Agent[];
  disabled: boolean;
  onStatus: (number: number, status: IssueStatus) => void;
}) {
  if (children.length === 0) return null;
  const stages = groupByStage(children);
  const { done, total } = stageProgress(children);

  return (
    <section className="mt-5">
      <h2 className="flex items-baseline gap-2 font-mono text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft">
        Sub-issues
        <span className="tabular-nums normal-case tracking-normal">
          {done}/{total} done
        </span>
      </h2>

      <div className="mt-2 flex flex-col gap-2">
        {stages.map((stage) => (
          <div key={stage.stage} className="border-2 border-black/20 rounded-md">
            <div
              className={`flex items-baseline gap-2 px-2 py-1 border-b-2 border-black/15 font-mono text-[0.6rem] font-bold uppercase tracking-wider ${
                STAGE_TONE[stage.state]
              }`}
            >
              Stage {stage.stage}
              <span className="normal-case tracking-normal font-normal">
                {STAGE_NOTE[stage.state]}
              </span>
            </div>
            <ul>
              {stage.issues.map((child) => (
                <li
                  key={child.number}
                  className="flex items-center gap-2 px-2 py-1.5 border-b border-black/10 last:border-b-0"
                >
                  <Link
                    to={`/projects/${encodeURIComponent(projectId)}/issues/${child.number}`}
                    className={`min-w-0 flex-1 font-mono text-[0.72rem] hover:underline ${
                      child.cancelled_at_ms != null ? 'line-through opacity-55' : ''
                    }`}
                  >
                    <span className="font-bold">#{child.number}</span> {child.title}
                  </Link>
                  {child.assignee == null ? null : (
                    <span className="shrink-0 font-mono text-[0.58rem] text-ink-soft">
                      @{handleOf(team, child.assignee)}
                    </span>
                  )}
                  <select
                    aria-label={`Status of #${child.number}`}
                    value={child.status}
                    disabled={disabled || child.cancelled_at_ms != null}
                    onChange={(event) => {
                      onStatus(child.number, event.target.value as IssueStatus);
                    }}
                    className="shrink-0 border border-black/30 rounded bg-surface px-1 py-0.5 font-mono text-[0.58rem] disabled:opacity-50"
                  >
                    {(Object.keys(COLUMN_LABEL) as IssueStatus[]).map((status) => (
                      <option key={status} value={status}>
                        {COLUMN_LABEL[status]}
                      </option>
                    ))}
                  </select>
                </li>
              ))}
            </ul>
          </div>
        ))}
      </div>
    </section>
  );
}
