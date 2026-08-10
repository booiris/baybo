import { Link } from 'react-router-dom';

import { COLUMN_LABEL, type Agent, type Issue, type IssueStatus } from './boardModel';
import { groupByStage, stageProgress } from './stageModel';

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

export function SubIssues({
  projectId,
  children,
  team,
  disabled,
  onStatus,
  onAssignee,
}: {
  projectId: string;
  children: Issue[];
  team: Agent[];
  disabled: boolean;
  onStatus: (number: number, status: IssueStatus) => void;
  /// A step's assignee is editable in place, like its status: the mockup's
  /// stage list is where a parent's work is handed out, and sending the
  /// operator to each child's own page to do it defeats the grouping.
  onAssignee: (number: number, assignee: string | null) => void;
}) {
  if (children.length === 0) return null;
  const stages = groupByStage(children);
  const { done, total } = stageProgress(children);

  return (
    <section className="mt-5">
      <h2
        className="font-mono text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft"
        title={`${done} of ${total} steps done`}
      >
        Sub-issues
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
              <span className="tabular-nums normal-case tracking-normal">
                {stageProgress(stage.issues).done}/{stageProgress(stage.issues).total} done
              </span>
              <span className="ml-auto normal-case tracking-normal font-normal">
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
                  <select
                    aria-label={`Assignee of #${child.number}`}
                    value={child.assignee ?? ''}
                    disabled={disabled || child.cancelled_at_ms != null}
                    onChange={(event) => {
                      const picked = event.target.value;
                      onAssignee(child.number, picked.length > 0 ? picked : null);
                    }}
                    className="shrink-0 max-w-[7.5rem] border border-black/30 rounded bg-surface px-1 py-0.5 font-mono text-[0.58rem] disabled:opacity-50"
                  >
                    <option value="">Unassigned</option>
                    {team.map((agent) => (
                      <option key={agent.id} value={agent.id}>
                        @{agent.handle}
                      </option>
                    ))}
                  </select>
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
