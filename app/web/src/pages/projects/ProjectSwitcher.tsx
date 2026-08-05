import { useEffect, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { RiArrowDownSLine } from 'react-icons/ri';

import type { Project } from './boardModel';
import { CreateProjectForm } from './CreateProjectForm';
import { attentionFor, useAttention, type ProjectAttention } from './useAttention';

/**
 * The board's top-left pill. There is no project list page, so this is the
 * only way between boards — and the only place a new one is opened.
 */
export function ProjectSwitcher({
  current,
  projects,
  onCreated,
}: {
  current: Project | null;
  projects: Project[];
  onCreated: () => void;
}) {
  const [open, setOpen] = useState(false);
  const waiting = useAttention();
  const [creating, setCreating] = useState(false);
  const [showArchived, setShowArchived] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  // Archived boards are hidden rather than absent: the parent hands over
  // every project, and this decides what the dropdown shows. The current
  // board always appears, so opening an archived one by deep link does not
  // produce a switcher that cannot name where you are.
  const visible = projects.filter(
    (project) => showArchived || project.archived_at_ms == null || project.id === current?.id,
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
        <RiArrowDownSLine />
      </button>

      {open ? (
        <div className="absolute left-0 top-[calc(100%+6px)] z-40 w-[320px] bg-surface border-2 border-black rounded-md shadow-brutal overflow-hidden">
          {creating ? (
            <div className="p-3">
              <CreateProjectForm
                onDone={() => {
                  setCreating(false);
                  setOpen(false);
                  onCreated();
                }}
              />
            </div>
          ) : (
            <>
              <ul className="max-h-[300px] overflow-y-auto">
                {visible.map((project) => {
                  const isCurrent = project.id === current?.id;
                  const stuck = attentionFor(waiting, project.id);
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
                        {/* The rail badge counts boards; this is where the
                            board says what, so a number the rail cannot
                            explain always has somewhere to decompose. */}
                        {stuck === null ? null : (
                          <span
                            title={stuckSummary(stuck)}
                            className="shrink-0 rounded-full border-2 border-black bg-err text-white px-1.5 font-mono text-[0.55rem] font-bold leading-[0.85rem] tabular-nums"
                          >
                            {stuck.approvals + stuck.held + stuck.failed + stuck.unread}
                          </span>
                        )}
                        <span className="ml-auto shrink-0 text-[0.58rem] text-ink-soft truncate max-w-[45%]">
                          {project.workdir}
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
              {projects.some((project) => project.archived_at_ms != null) ? (
                <label className="flex items-center gap-1.5 px-3 py-2 border-t border-black/20 font-mono text-[0.66rem] text-ink-soft cursor-pointer">
                  <input
                    type="checkbox"
                    checked={showArchived}
                    onChange={(event) => {
                      setShowArchived(event.target.checked);
                    }}
                  />
                  Show archived
                </label>
              ) : null}
            </>
          )}
        </div>
      ) : null}
    </div>
  );
}

/** What one board's counts say, for the row's tooltip. */
function stuckSummary(stuck: ProjectAttention): string {
  const parts: string[] = [];
  if (stuck.approvals > 0) parts.push(`${stuck.approvals} waiting on approval`);
  if (stuck.held > 0) parts.push(`${stuck.held} held on budget`);
  if (stuck.failed > 0) parts.push(`${stuck.failed} failed`);
  if (stuck.unread > 0) parts.push(`${stuck.unread} new since you looked`);
  return parts.join(', ');
}
