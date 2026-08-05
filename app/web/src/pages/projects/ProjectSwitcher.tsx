import { useEffect, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { RiArrowDownSLine } from 'react-icons/ri';

import type { Project } from './boardModel';
import { CreateProjectForm } from './CreateProjectForm';

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
  const [creating, setCreating] = useState(false);
  const root = useRef<HTMLDivElement>(null);

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
                {projects.map((project) => {
                  const isCurrent = project.id === current?.id;
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
                        <span className="ml-auto shrink-0 text-[0.58rem] text-ink-soft truncate max-w-[45%]">
                          {project.workdir}
                        </span>
                      </Link>
                    </li>
                  );
                })}
                {projects.length === 0 ? (
                  <li className="px-3 py-2 font-mono text-[0.7rem] text-ink-soft">
                    No projects yet
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
            </>
          )}
        </div>
      ) : null}
    </div>
  );
}
