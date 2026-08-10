import { Link } from 'react-router-dom';

import { Avatar } from './Avatar';
import { PickerOverlay } from './PickerOverlay';
import {
  COLUMN_LABEL,
  COLUMN_PILL_LABEL,
  type Agent,
  type Issue,
  type IssueStatus,
} from './boardModel';
import { groupByStage, stageProgress } from './stageModel';
import { handleOf } from './teamModel';

const STAGE_TONE: Record<'done' | 'open' | 'waiting', string> = {
  done: 'bg-ok/10 text-ok',
  open: 'bg-brand/30 text-ink',
  waiting: 'bg-canvas text-ink-soft',
};

const STAGE_NOTE: Record<'done' | 'open' | 'waiting', string> = {
  done: 'finished',
  open: 'in progress',
  waiting: 'starts when the stage above finishes',
};

const STATUS_PILL: Record<IssueStatus, string> = {
  backlog: 'border-black/35 bg-canvas text-ink-soft',
  todo: 'border-black/35 bg-canvas text-ink',
  in_progress: 'border-black bg-brand/30 text-ink font-bold',
  review: 'border-info/45 bg-info/12 text-info',
  done: 'border-ok/50 bg-ok/15 text-ok font-bold',
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
  const people = [
    { value: '', label: 'Unassigned' },
    ...team.map((agent) => ({ value: agent.id, label: `@${agent.handle}` })),
  ];
  const statuses = (Object.keys(COLUMN_LABEL) as IssueStatus[]).map((status) => ({
    value: status,
    label: COLUMN_LABEL[status],
  }));

  return (
    <section className="mt-[18px]">
      <h2
        className="font-mono text-[0.62rem] font-bold uppercase tracking-[0.14em] text-ink-soft"
        title={`${done} of ${total} steps done`}
      >
        Sub-issues
      </h2>

      <div className="mt-2 flex flex-col gap-2.5">
        {stages.map((stage) => (
          <div key={stage.stage} className="border-2 border-black rounded-md bg-surface overflow-hidden">
            <div
              className={`flex items-center gap-2 px-3 py-1.5 border-b-2 border-black font-mono text-[0.62rem] font-bold uppercase tracking-[0.1em] ${
                STAGE_TONE[stage.state]
              }`}
            >
              Stage {stage.stage}
              <span className="tabular-nums normal-case tracking-normal">
                {stageProgress(stage.issues).done}/{stageProgress(stage.issues).total} done
              </span>
              <span className="ml-auto text-[0.56rem] normal-case tracking-normal font-normal text-ink-soft">
                {STAGE_NOTE[stage.state]}
              </span>
            </div>
            <ul>
              {stage.issues.map((child) => {
                const cancelled = child.cancelled_at_ms != null;
                return (
                  <li
                    key={child.number}
                    className="flex items-center gap-2.5 px-3 py-1.5 border-b border-black/15 last:border-b-0"
                  >
                    <span className="shrink-0 font-mono text-[0.6rem] font-bold text-ink-soft">
                      #{child.number}
                    </span>
                    <Link
                      to={`/projects/${encodeURIComponent(projectId)}/issues/${child.number}`}
                      className={`min-w-0 flex-1 truncate font-mono text-[0.7rem] hover:underline ${
                        cancelled ? 'line-through opacity-55' : ''
                      }`}
                    >
                      {child.title}
                    </Link>
                    <PickerOverlay
                      label={`Status of #${child.number}`}
                      value={child.status}
                      disabled={disabled || cancelled}
                      options={statuses}
                      onPick={(picked) => {
                        onStatus(child.number, picked as IssueStatus);
                      }}
                    >
                      <span
                        className={`rounded-full border px-2 font-mono text-[0.54rem] uppercase tracking-wider ${
                          STATUS_PILL[child.status]
                        }`}
                      >
                        {COLUMN_PILL_LABEL[child.status]}
                      </span>
                    </PickerOverlay>
                    <PickerOverlay
                      label={`Assignee of #${child.number}`}
                      value={child.assignee ?? ''}
                      disabled={disabled || cancelled}
                      options={people}
                      onPick={(picked) => {
                        onAssignee(child.number, picked.length > 0 ? picked : null);
                      }}
                    >
                      {child.assignee == null ? (
                        <span
                          title="Unassigned"
                          className="w-[18px] h-[18px] rounded-full border-2 border-dashed border-ink-soft/60"
                        />
                      ) : (
                        <Avatar handle={handleOf(team, child.assignee)} size="sm" />
                      )}
                    </PickerOverlay>
                  </li>
                );
              })}
            </ul>
          </div>
        ))}
      </div>
    </section>
  );
}
