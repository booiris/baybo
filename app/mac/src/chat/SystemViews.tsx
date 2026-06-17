import { useEffect, useState, type ReactNode } from 'react';
import {
  listCron,
  listSkills,
  listTools,
  type CronJobView,
  type ToolManifest,
} from '../api/admin';
import type { AdminClient } from '../api/client';
import { AutomationIcon, PluginIcon } from '../components/icons';

// Read-only system panes reached from the sidebar nav: 插件 (Plugins — the
// registered skills + tool manifests) and 自动化 (Automation — scheduled cron
// jobs). Both fill the main content area next to the persistent sidebar.

function Pane({
  icon,
  title,
  subtitle,
  children,
}: {
  icon: ReactNode;
  title: string;
  subtitle: string;
  children: ReactNode;
}) {
  return (
    <div className="flex flex-1 flex-col overflow-hidden">
      <header
        data-tauri-drag-region="deep"
        className="flex items-center gap-2.5 border-b-[3px] border-border bg-canvas px-6 pb-3 pt-7"
      >
        <span className="text-ink">{icon}</span>
        <h2 className="text-base font-bold uppercase tracking-tight">{title}</h2>
      </header>
      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto max-w-3xl px-8 py-8">
          <p className="mb-5 text-sm text-ink-soft">{subtitle}</p>
          {children}
        </div>
      </div>
    </div>
  );
}

function SectionLabel({ children, count }: { children: ReactNode; count: number }) {
  return (
    <div className="mb-2 mt-6 flex items-center gap-2 first:mt-0">
      <h3 className="text-xs font-bold uppercase tracking-wider text-ink-soft">{children}</h3>
      <span className="rounded-full border-2 border-border bg-rail px-1.5 font-mono text-[11px] font-bold leading-tight">
        {count}
      </span>
    </div>
  );
}

function Empty({ text }: { text: string }) {
  return <p className="font-mono text-xs text-ink-soft">{text}</p>;
}

export function PluginsView({ client }: { client: AdminClient }) {
  const [skills, setSkills] = useState<string[]>([]);
  const [tools, setTools] = useState<ToolManifest[]>([]);

  useEffect(() => {
    listSkills(client).then(setSkills, () => undefined);
    listTools(client).then(setTools, () => undefined);
  }, [client]);

  return (
    <Pane
      icon={<PluginIcon className="h-4 w-4" />}
      title="插件 · Plugins"
      subtitle="Skills and tools registered with this Aura runtime."
    >
      <SectionLabel count={skills.length}>Skills</SectionLabel>
      {skills.length > 0 ? (
        <div className="flex flex-wrap gap-2">
          {skills.map((s) => (
            <span
              key={s}
              className="rounded-brutal border-2 border-border bg-surface px-3 py-1.5 font-mono text-xs shadow-brutal-sm"
            >
              {s}
            </span>
          ))}
        </div>
      ) : (
        <Empty text="No skills registered." />
      )}

      <SectionLabel count={tools.length}>Tools</SectionLabel>
      {tools.length > 0 ? (
        <ul className="space-y-2">
          {tools.map((t) => (
            <li
              key={t.name}
              className="rounded-brutal border-2 border-border bg-surface px-3 py-2 shadow-brutal-sm"
            >
              <p className="font-mono text-sm font-bold">{t.name}</p>
              {t.description && (
                <p className="mt-0.5 text-xs leading-relaxed text-ink-soft">{t.description}</p>
              )}
            </li>
          ))}
        </ul>
      ) : (
        <Empty text="No tools registered." />
      )}
    </Pane>
  );
}

function scheduleLabel(s: CronJobView['schedule']): string {
  return s.kind === 'cron' ? s.expr : new Date(s.time).toLocaleString();
}

const STATUS_TONE: Record<CronJobView['status'], string> = {
  enabled: 'border-ok text-ok',
  disabled: 'border-ink-soft text-ink-soft',
  executed: 'border-info text-info',
};

export function AutomationView({ client }: { client: AdminClient }) {
  const [jobs, setJobs] = useState<CronJobView[]>([]);

  useEffect(() => {
    listCron(client).then(setJobs, () => undefined);
  }, [client]);

  return (
    <Pane
      icon={<AutomationIcon className="h-4 w-4" />}
      title="自动化 · Automation"
      subtitle="Scheduled cron jobs that fire unattended agent turns."
    >
      {jobs.length > 0 ? (
        <ul className="space-y-3">
          {jobs.map((j) => (
            <li
              key={j.id}
              className="rounded-brutal border-[3px] border-border bg-surface p-4 shadow-brutal-sm"
            >
              <div className="mb-2 flex items-center gap-2">
                <span className="rounded-brutal border-2 border-border bg-canvas px-2 py-0.5 font-mono text-xs font-bold">
                  {scheduleLabel(j.schedule)}
                </span>
                <span
                  className={`rounded-full border-2 bg-surface px-2 py-0.5 font-mono text-[11px] font-bold uppercase ${STATUS_TONE[j.status]}`}
                >
                  {j.status}
                </span>
                <span className="font-mono text-[11px] text-ink-soft">{j.timezone}</span>
              </div>
              <p className="text-sm leading-relaxed">{j.prompt}</p>
              {j.next_trigger_at && (
                <p className="mt-2 font-mono text-[11px] text-ink-soft">
                  next · {new Date(j.next_trigger_at).toLocaleString()}
                </p>
              )}
            </li>
          ))}
        </ul>
      ) : (
        <Empty text="No automations scheduled." />
      )}
    </Pane>
  );
}
