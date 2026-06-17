import { useMemo, useState, type ReactNode } from 'react';
import type { SessionSummary } from '../api/admin';
import { AutomationIcon, NewChatIcon, PluginIcon } from '../components/icons';

// The conversation sidebar (Codex layout, Aura tokens): traffic-light spacer,
// primary nav (新对话 / 插件 / 自动化), and a project → conversation tree.
// 设置 lives in the far-left app rail.

export type MainView = 'chat' | 'plugins' | 'automation' | 'settings';

// The backend has no project field on sessions yet, so all conversations live
// under one derived workspace project. Rendered via `.map` so a real
// `project_id` (or a frontend assignment) drops in with zero JSX change.
const WORKSPACE_NAME = 'aura';

interface ProjectGroup {
  id: string;
  name: string;
  sessions: SessionSummary[];
}

/** Locale-light relative time for the conversation rows (刚刚 / N 分 / N 时 /
 *  N 天) — avoids pulling in an i18n/date dependency. */
function relTime(iso: string): string {
  const then = new Date(iso).getTime();
  if (Number.isNaN(then)) return '';
  const secs = Math.max(0, (Date.now() - then) / 1000);
  if (secs < 60) return '刚刚';
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins} 分`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours} 时`;
  const days = Math.floor(hours / 24);
  if (days < 30) return `${days} 天`;
  return `${Math.floor(days / 30)} 月`;
}

function NavItem({
  icon,
  label,
  onClick,
  active,
}: {
  icon: ReactNode;
  label: string;
  onClick: () => void;
  active?: boolean;
}) {
  return (
    <button
      onClick={onClick}
      className={`flex w-full items-center gap-2.5 rounded-brutal border-2 px-2.5 py-1.5 text-sm font-medium transition-all hover:-translate-y-0.5 active:translate-y-0 active:shadow-none ${
        active
          ? 'border-border bg-selected shadow-brutal-sm'
          : 'border-transparent hover:border-border hover:bg-surface hover:shadow-brutal-sm'
      }`}
    >
      <span className="flex h-4 w-4 shrink-0 items-center justify-center text-ink-soft">
        {icon}
      </span>
      <span className="truncate">{label}</span>
    </button>
  );
}

export function Sidebar({
  sessions,
  activeId,
  view,
  onNewChat,
  onSelect,
  onHide,
  onView,
}: {
  sessions: SessionSummary[];
  activeId: string | null;
  view: MainView;
  onNewChat: () => void;
  onSelect: (id: string) => void;
  onHide: (id: string) => void;
  onView: (v: MainView) => void;
}) {
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());

  const projects = useMemo<ProjectGroup[]>(
    () => [{ id: 'default', name: WORKSPACE_NAME, sessions }],
    [sessions],
  );

  const toggle = (id: string) =>
    setCollapsed((c) => {
      const n = new Set(c);
      if (n.has(id)) n.delete(id);
      else n.add(id);
      return n;
    });

  return (
    <aside
      data-tauri-drag-region="deep"
      className="flex w-64 shrink-0 flex-col border-r-[3px] border-border bg-canvas"
    >
      <div data-tauri-drag-region="deep" className="h-7 shrink-0" />

      <nav data-tauri-drag-region="deep" className="flex flex-col gap-0.5 px-2 pb-2">
        <NavItem icon={<NewChatIcon className="h-4 w-4" />} label="新对话" onClick={onNewChat} />
        <NavItem
          icon={<PluginIcon className="h-4 w-4" />}
          label="插件"
          active={view === 'plugins'}
          onClick={() => onView('plugins')}
        />
        <NavItem
          icon={<AutomationIcon className="h-4 w-4" />}
          label="自动化"
          active={view === 'automation'}
          onClick={() => onView('automation')}
        />
      </nav>

      <div className="mx-3 my-1 border-t-2 border-border/20" />

      <div className="no-scrollbar flex-1 overflow-y-auto px-2 pb-2">
        {projects.map((p) => {
          const isOpen = !collapsed.has(p.id);
          return (
            <div key={p.id} className="mb-2">
              <button
                onClick={() => toggle(p.id)}
                className="flex w-full items-center gap-1.5 rounded-brutal px-2 py-1.5 font-mono text-[11px] font-bold uppercase tracking-wider text-ink-soft hover:text-ink"
              >
                <span className={`text-[9px] transition-transform ${isOpen ? 'rotate-90' : ''}`}>
                  ▸
                </span>
                <span className="truncate">{p.name}</span>
                <span className="ml-auto font-mono text-[10px] font-normal text-ink-soft">
                  {p.sessions.length}
                </span>
              </button>
              {isOpen && (
                <div className="mt-0.5 flex flex-col gap-0.5">
                  {p.sessions.map((s) => (
                    <div
                      key={s.session_id}
                      onClick={() => onSelect(s.session_id)}
                      className={`group flex cursor-pointer items-center justify-between gap-2 rounded-brutal border-2 py-1.5 pl-5 pr-2 transition-all hover:-translate-y-0.5 active:translate-y-0 active:shadow-none ${
                        view === 'chat' && s.session_id === activeId
                          ? 'border-border bg-selected shadow-brutal-sm'
                          : 'border-transparent hover:border-border hover:bg-surface hover:shadow-brutal-sm'
                      }`}
                    >
                      <span className="truncate text-sm font-medium">
                        {s.last_user_text || '新对话'}
                      </span>
                      <span className="shrink-0 font-mono text-[10px] text-ink-soft group-hover:hidden">
                        {relTime(s.last_active)}
                      </span>
                      <button
                        onClick={(e) => {
                          e.stopPropagation();
                          onHide(s.session_id);
                        }}
                        className="hidden shrink-0 font-mono text-xs text-ink-soft hover:text-err group-hover:block"
                        title="移除对话"
                      >
                        ✕
                      </button>
                    </div>
                  ))}
                  {p.sessions.length === 0 && (
                    <p className="px-2 py-2 font-mono text-xs text-ink-soft">暂无对话</p>
                  )}
                </div>
              )}
            </div>
          );
        })}
      </div>
    </aside>
  );
}
