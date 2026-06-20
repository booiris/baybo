import { useMemo, useState } from 'react';
import {
  RiArrowDownSLine,
  RiArrowRightSLine,
  RiCheckboxBlankLine,
  RiCheckboxCircleFill,
  RiLoader4Line,
} from 'react-icons/ri';

import type { TaskView } from '@aura/chat-core';

/** The wire's `status` is an open `string`; narrow it to the four
 *  states the agent emits so the marker/colour lookup is exhaustive.
 *  Anything unexpected falls back to `pending`. */
type TaskStatus = 'pending' | 'in_progress' | 'completed';

const KNOWN_STATUSES: readonly TaskStatus[] = [
  'pending',
  'in_progress',
  'completed',
];

function asStatus(raw: string): TaskStatus {
  return (KNOWN_STATUSES as readonly string[]).includes(raw)
    ? (raw as TaskStatus)
    : 'pending';
}

interface TaskChecklistProps {
  tasks: TaskView[];
}

/** Live planning checklist for the active session. Driven by the
 *  idempotent `Frame::TaskList` snapshot — the parent passes the
 *  session's current `tasks`, replaced wholesale on each frame. Renders
 *  nothing when the list is empty (no active plan). Presentational only. */
export function TaskChecklist({ tasks }: TaskChecklistProps) {
  const [collapsed, setCollapsed] = useState(false);

  const completed = useMemo(
    () => tasks.filter((t) => asStatus(t.status) === 'completed').length,
    [tasks],
  );
  // Map task id → 1-based position so a `depends_on` hint can read
  // "after #2" without leaking the opaque id to the user.
  const positions = useMemo(() => {
    const map = new Map<string, number>();
    tasks.forEach((t, i) => map.set(t.id, i + 1));
    return map;
  }, [tasks]);

  if (tasks.length === 0) return null;

  return (
    <section className="mb-4 border-2 border-black rounded-md bg-white shadow-brutal-sm">
      <button
        type="button"
        onClick={() => setCollapsed((c) => !c)}
        className="w-full flex items-center gap-2 px-3 py-2 border-b-2 border-black bg-canvas rounded-t-[4px] hover:bg-gray-100 cursor-pointer"
        aria-expanded={!collapsed}
      >
        {collapsed ? (
          <RiArrowRightSLine className="text-base shrink-0 text-ink" />
        ) : (
          <RiArrowDownSLine className="text-base shrink-0 text-ink" />
        )}
        <span className="font-bold uppercase tracking-wider text-[0.8rem] text-ink">
          Task List
        </span>
        <span className="font-mono text-[0.7rem] text-ink-soft">
          · {completed}/{tasks.length} done
        </span>
      </button>

      {collapsed ? null : (
        <ul className="flex flex-col">
          {tasks.map((task) => (
            <TaskRow key={task.id} task={task} positions={positions} />
          ))}
        </ul>
      )}
    </section>
  );
}

function TaskRow({
  task,
  positions,
}: {
  task: TaskView;
  positions: Map<string, number>;
}) {
  const status = asStatus(task.status);
  const deps = (task.depends_on ?? [])
    .map((id) => positions.get(id))
    .filter((n): n is number => n !== undefined);

  return (
    <li className="flex items-start gap-2 px-3 py-1.5 border-t-2 border-black/10 first:border-t-0 font-mono text-[0.8rem] leading-snug">
      <span className="shrink-0 mt-0.5 text-base leading-none">
        <StatusMarker status={status} />
      </span>
      <span className="flex-1 min-w-0 break-words">
        <span className={contentClass(status)}>{task.subject}</span>
        {deps.length > 0 ? (
          <span className="ml-2 text-[0.65rem] text-ink-soft uppercase tracking-wider">
            after {deps.map((n) => `#${n}`).join(', ')}
          </span>
        ) : null}
      </span>
    </li>
  );
}

function StatusMarker({ status }: { status: TaskStatus }) {
  switch (status) {
    case 'in_progress':
      return (
        <RiLoader4Line
          className="text-brand animate-spin"
          aria-label="in progress"
        />
      );
    case 'completed':
      return (
        <RiCheckboxCircleFill className="text-ok" aria-label="completed" />
      );
    case 'pending':
    default:
      return (
        <RiCheckboxBlankLine className="text-ink-soft" aria-label="pending" />
      );
  }
}

function contentClass(status: TaskStatus): string {
  switch (status) {
    case 'in_progress':
      return 'text-ink font-bold';
    case 'completed':
      return 'text-ink-soft line-through';
    case 'pending':
    default:
      return 'text-ink';
  }
}
