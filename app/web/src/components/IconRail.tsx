import { useSyncExternalStore } from 'react';
import { NavLink } from 'react-router-dom';
import {
  RiAlarmLine,
  RiBarChartBoxLine,
  RiChat3Line,
  RiCpuLine,
  RiFileList3Line,
  RiGitMergeLine,
  RiInstallLine,
  RiLogoutBoxRLine,
  RiStackLine,
} from 'react-icons/ri';
import type { IconType } from 'react-icons';
import { useAuth } from '../api/auth';
import { installPrompt } from '../pwa/registerSW';

// Global app rail (replaces the old text sidebar): a solid amber, icon-only
// vertical bar mounted on every route. Chat is the primary destination (the
// "A" mark doubles as its entry, app/mac-style); the admin surfaces sit below;
// logout is pinned to the bottom. Labels surface as hover tooltips.

const railBtn =
  'flex h-8 w-8 items-center justify-center rounded-brutal border-2 text-ink transition-[transform,box-shadow,background-color] duration-100 cursor-pointer';
const railIdle =
  'border-transparent hover:bg-surface hover:border-black hover:shadow-brutal-xs';
const railActive =
  'bg-surface border-black shadow-brutal-sm active:translate-x-[1px] active:translate-y-[1px] active:shadow-none';

const DESTINATIONS: { to: string; label: string; Icon: IconType }[] = [
  { to: '/logs', label: 'Log', Icon: RiFileList3Line },
  { to: '/traces', label: 'Trace', Icon: RiGitMergeLine },
  { to: '/cron', label: 'Cron', Icon: RiAlarmLine },
  { to: '/jobs', label: 'Jobs', Icon: RiStackLine },
  { to: '/analytics', label: 'Analytics', Icon: RiBarChartBoxLine },
  { to: '/llm', label: 'LLM', Icon: RiCpuLine },
];

export function IconRail({ version }: { version?: string }) {
  const { logout } = useAuth();
  // Chromium hands the install prompt to whoever wants it; the rail is where a
  // user already looks for app-level actions. Absent everywhere else (Safari
  // installs via Share → Add to Home Screen), and gone once installed.
  const { canInstall } = useSyncExternalStore(
    installPrompt.subscribe,
    installPrompt.getSnapshot,
  );
  return (
    <aside className="w-12 shrink-0 bg-brand border-r-2 border-black flex flex-col items-center gap-3 pt-3 pb-3">
      <NavLink
        to="/chat"
        title={version ? `Chat · Baybo v${version}` : 'Chat'}
        className={({ isActive }) =>
          `flex h-8 w-8 items-center justify-center rounded-brutal border-2 border-black font-bold text-base text-ink transition-[transform,box-shadow] duration-100 ${
            isActive
              ? 'bg-surface shadow-brutal-sm'
              : 'bg-surface/70 hover:bg-surface hover:shadow-brutal-xs'
          }`
        }
      >
        <RiChat3Line className="text-lg" />
      </NavLink>

      <div className="w-6 border-t-2 border-black/25" />

      <nav className="flex flex-col items-center gap-3">
        {DESTINATIONS.map(({ to, label, Icon }) => (
          <NavLink
            key={to}
            to={to}
            title={label}
            className={({ isActive }) => `${railBtn} ${isActive ? railActive : railIdle}`}
          >
            <Icon className="text-lg" />
          </NavLink>
        ))}
      </nav>

      <div className="mt-auto flex flex-col items-center gap-3">
        {canInstall && (
          <button
            type="button"
            onClick={() => void installPrompt.prompt()}
            title="Install app"
            className={`${railBtn} ${railIdle}`}
          >
            <RiInstallLine className="text-lg" />
          </button>
        )}
        <button type="button" onClick={logout} title="Logout" className={`${railBtn} ${railIdle}`}>
          <RiLogoutBoxRLine className="text-lg" />
        </button>
      </div>
    </aside>
  );
}
