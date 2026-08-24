import { useEffect, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { RiArrowDownSLine } from 'react-icons/ri';

import type { Project } from './boardModel';
import { CreateProjectForm } from './CreateProjectForm';
import {
  attentionElsewhere,
  attentionFor,
  useAttention,
  type ProjectAttention,
} from './useAttention';
import { activityFor, burnState, useBoardActivity, type BurnState } from './useBoardActivity';
import { boardMeter } from './budgetModel';

const BURN_NOTE: Record<BurnState, string> = {
  ok: "Spent today / this board's daily ceiling",
  near: 'Close to the daily ceiling — new runs are held once it is reached',
  over: 'Over the daily ceiling — new runs are held, and start again when it resets at 00:00 UTC or you raise it',
};

export function ProjectSwitcher({
  current,
  projects,
  onCreated,
  refreshKey = 0,
}: {
  current: Project | null;
  projects: Project[];
  onCreated: () => void;
  /// Bumped by the board when anything changes, so an open dropdown's
  /// numbers follow the board instead of going stale behind it.
  refreshKey?: number;
}) {
  const [open, setOpen] = useState(false);
  const waiting = useAttention();
  const [creating, setCreating] = useState(false);
  const [showArchived, setShowArchived] = useState(false);
  // Fetched only while the dropdown is open, then refreshed on the board's
  // own change signal — decision ⑦ asked for live, not for a timer.
  const activity = useBoardActivity(open, refreshKey);
  const root = useRef<HTMLDivElement>(null);
  const archivedCount = projects.filter((project) => project.archived_at_ms != null).length;
  // The rail's dot says something is lit; only this dropdown says which
  // board. Without a mark of its own the trigger reads as inert chrome, and
  // an operator on a quiet board has nothing on screen telling them the
  // answer is one click away — so the dot they can see stays lit through
  // everything they do, which is exactly how "it won't clear" is produced.
  const elsewhere = attentionElsewhere(waiting, current?.id ?? null);
  // Archived boards sort to the tail rather than staying in the server's
  // recency order: they are shown on request, and a recently-touched
  // archived board landing mid-list reads as live work.
  const visible = projects
    .filter(
      (project) => showArchived || project.archived_at_ms == null || project.id === current?.id,
    )
    .sort(
      (a, b) => Number(a.archived_at_ms != null) - Number(b.archived_at_ms != null),
    );

  useEffect(() => {
    if (!open) return;
    function onPointerDown(event: PointerEvent) {
      if (root.current !== null && !root.current.contains(event.target as Node)) {
        setOpen(false);
        setCreating(false);
      }
    }
    function onKeyDown(event: KeyboardEvent) {
      if (event.key === 'Escape') {
        setOpen(false);
        setCreating(false);
      }
    }
    document.addEventListener('pointerdown', onPointerDown);
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('pointerdown', onPointerDown);
      document.removeEventListener('keydown', onKeyDown);
    };
  }, [open]);

  return (
    <div className="relative" ref={root}>
      <button
        type="button"
        className="inline-flex items-center gap-1.5 px-3 py-1 bg-surface border-2 border-black rounded-md shadow-brutal-xs font-mono text-[0.78rem] font-bold cursor-pointer active:translate-x-[2px] active:translate-y-[2px] active:shadow-none"
        onClick={() => {
          setOpen((value) => !value);
        }}
      >
        {current?.name ?? 'Projects'}
        {elsewhere ? (
          <span
            role="status"
            aria-label="Another board is waiting on you"
            title="Another board is waiting on you"
            className="w-2 h-2 shrink-0 rounded-full border border-black bg-err"
          />
        ) : null}
        <RiArrowDownSLine />
      </button>

      {open ? (
        <div className="absolute left-0 top-[calc(100%+6px)] z-40 w-[320px] bg-surface border-2 border-black rounded-md shadow-brutal overflow-hidden">
          {creating ? null : (
            <>
              <ul className="max-h-[300px] overflow-y-auto">
                {visible.map((project) => {
                  const isCurrent = project.id === current?.id;
                  const stuck = attentionFor(waiting, project.id);
                  const live = activityFor(activity, project.id);
                  const meter = boardMeter(
                    { micros: live?.burn_micros ?? 0, tokens: live?.burn_tokens ?? 0 },
                    {
                      micros: project.daily_budget_micros,
                      tokens: project.daily_budget_tokens,
                    },
                  );
                  const burn = burnState(meter.used, meter.ceiling);
                  return (
                    <li key={project.id}>
                      <Link
                        to={`/projects/${encodeURIComponent(project.id)}`}
                        onClick={() => {
                          setOpen(false);
                        }}
                        className={`flex items-baseline gap-2 px-3 py-2 border-b border-black/20 font-mono text-[0.72rem] ${
                          isCurrent ? 'bg-selected font-bold' : 'hover:bg-canvas'
                        }`}
                      >
                        <span className="truncate">{project.name}</span>
                        {project.archived_at_ms == null ? null : (
                          <span className="shrink-0 rounded border border-warn/50 bg-warn/10 text-warn px-1 font-mono text-[0.52rem] font-bold uppercase">
                            archived
                          </span>
                        )}
                        {stuck === null ? null : (
                          <span
                            title={stuckSummary(stuck)}
                            className="shrink-0 rounded-full border-2 border-black bg-err text-white px-1.5 font-mono text-[0.55rem] font-bold leading-[0.85rem] tabular-nums"
                          >
                            {stuck.approvals + stuck.failed + stuck.unread}
                          </span>
                        )}
                        <span className="ml-auto shrink-0 flex items-baseline gap-2 text-[0.58rem] tabular-nums">
                          {live !== null && live.working > 0 ? (
                            <span className="text-ok font-bold" title="Runs executing right now">
                              ● {live.working} working
                            </span>
                          ) : (
                            <span className="text-ink-soft">idle</span>
                          )}
                          <span
                            className={burn === 'ok' ? 'text-ink-soft' : 'text-warn font-bold'}
                            title={BURN_NOTE[burn]}
                          >
                            {meter.text}
                          </span>
                        </span>
                      </Link>
                    </li>
                  );
                })}
                {visible.length === 0 ? (
                  <li className="px-3 py-2 font-mono text-[0.7rem] text-ink-soft">
                    {projects.length === 0 ? 'No projects yet' : 'No live projects'}
                  </li>
                ) : null}
              </ul>
              <button
                type="button"
                className="w-full text-left px-3 py-2.5 font-mono text-[0.72rem] font-bold cursor-pointer hover:bg-canvas"
                onClick={() => {
                  setCreating(true);
                }}
              >
                ＋ New project…
              </button>
              {archivedCount > 0 ? (
                <label className="flex items-center gap-1.5 px-3 py-2 border-t border-black/20 font-mono text-[0.66rem] text-ink-soft cursor-pointer">
                  <input
                    type="checkbox"
                    checked={showArchived}
                    onChange={(event) => {
                      setShowArchived(event.target.checked);
                    }}
                  />
                  Show archived ({archivedCount})
                </label>
              ) : null}
            </>
          )}
        </div>
      ) : null}

      {creating ? (
        // A modal, not a form squeezed into a 320px popover: five fields
        // and a workdir path do not fit a dropdown, and the one field
        // people get wrong is the one with the longest explanation.
        <div
          className="fixed inset-0 z-50 bg-black/40 flex items-start justify-center p-8 overflow-y-auto"
          onClick={() => {
            setCreating(false);
          }}
        >
          <div
            className="w-full max-w-lg my-auto bg-surface border-[3px] border-black rounded-md shadow-brutal"
            onClick={(event) => {
              event.stopPropagation();
            }}
          >
            <div className="flex items-center gap-2 px-4 py-2 border-b-2 border-black">
              <h2 className="font-mono text-[0.78rem] font-bold">New project</h2>
              <button
                type="button"
                aria-label="Close"
                className="ml-auto cursor-pointer px-1"
                onClick={() => {
                  setCreating(false);
                }}
              >
                ✕
              </button>
            </div>
            <div className="p-4">
              <CreateProjectForm
                onDone={() => {
                  setCreating(false);
                  setOpen(false);
                  onCreated();
                }}
              />
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}

function stuckSummary(stuck: ProjectAttention): string {
  const parts: string[] = [];
  if (stuck.approvals > 0) parts.push(`${stuck.approvals} waiting on approval`);
  if (stuck.failed > 0) parts.push(`${stuck.failed} failed`);
  if (stuck.unread > 0) parts.push(`${stuck.unread} new since you looked`);
  return parts.join(', ');
}
