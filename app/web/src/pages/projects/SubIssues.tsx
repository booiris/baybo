import { Link, useLocation } from 'react-router-dom';

import { Avatar } from './Avatar';
import { generatedPortrait, type Portrait } from './portrait';
import { Picker, type PickerOption } from './Picker';
import {
  COLUMN_LABEL,
  COLUMN_PILL_LABEL,
  COLUMNS,
  STATUS_PILL,
  issuePath,
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

export function SubIssues({
  projectId,
  children,
  team,
  portrait = generatedPortrait,
  disabled,
  onStatus,
  onAssignee,
}: {
  projectId: string;
  children: Issue[];
  team: Agent[];
  /// Resolved faces from the page that owns the roster; generated ones
  /// otherwise.
  portrait?: Portrait;
  disabled: boolean;
  onStatus: (number: number, status: IssueStatus) => void;
  /// A step's assignee is editable in place, like its status: the mockup's
  /// stage list is where a parent's work is handed out, and sending the
  /// operator to each child's own page to do it defeats the grouping.
  onAssignee: (number: number, assignee: string | null) => void;
}) {
  // Above the early return: a hook cannot sit behind one. It is what a step
  // opened from here comes back to.
  const here = useLocation().pathname;
  if (children.length === 0) return null;
  const stages = groupByStage(children);
  const { done, total } = stageProgress(children);
  const people: PickerOption[] = [
    { value: '', label: 'Unassigned' },
    ...team.map((agent) => ({
      value: agent.id,
      label: `@${agent.handle}`,
      node: (
        <span className="inline-flex items-center gap-1.5">
          <Avatar handle={agent.handle} src={portrait(agent.id)} size="sm" />@{agent.handle}
        </span>
      ),
    })),
  ];
  // In board order, and wearing the board's pills: the panel is where you
  // see what you are moving the step *to*, so it may as well show the
  // pipeline instead of listing five words.
  const statuses: PickerOption[] = COLUMNS.map((status) => ({
    value: status,
    label: COLUMN_LABEL[status],
    node: (
      <span
        className={`rounded-full border px-2 font-mono text-[0.54rem] uppercase tracking-wider ${STATUS_PILL[status]}`}
      >
        {COLUMN_PILL_LABEL[status]}
      </span>
    ),
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
                      to={issuePath(projectId, child.number)}
                      state={{ from: here }}
                      className={`min-w-0 flex-1 truncate font-mono text-[0.7rem] hover:underline ${
                        cancelled ? 'line-through opacity-55' : ''
                      }`}
                    >
                      {child.title}
                    </Link>
                    <Picker
                      label={`Status of #${child.number}`}
                      title="Move this step to another column"
                      value={child.status}
                      disabled={disabled || cancelled}
                      options={statuses}
                      onPick={(picked) => {
                        onStatus(child.number, picked as IssueStatus);
                      }}
                      triggerClassName={`rounded-full border px-2 font-mono text-[0.54rem] uppercase tracking-wider ${
                        STATUS_PILL[child.status]
                      }`}
                    >
                      {COLUMN_PILL_LABEL[child.status]}
                    </Picker>
                    <Picker
                      label={`Assignee of #${child.number}`}
                      title="Hand this step to somebody"
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
                          className="w-[18px] h-[18px] rounded-full border border-dashed border-ink-soft/60"
                        />
                      ) : (
                        <Avatar
                          handle={handleOf(team, child.assignee)}
                          src={portrait(child.assignee)}
                          size="sm"
                        />
                      )}
                    </Picker>
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
