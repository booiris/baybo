import { Fragment } from 'react';
import { RiHomeLine, RiLogoutBoxRLine } from 'react-icons/ri';
import { useAuth } from '../api/auth';

interface TopNavProps {
  breadcrumbs: string[];
}

export function TopNav({ breadcrumbs }: TopNavProps) {
  const { logout } = useAuth();
  return (
    <header className="px-8 py-5 border-b-2 border-black bg-white shrink-0 flex items-center justify-between gap-6">
      <nav className="text-[0.9rem] font-semibold text-ink-soft flex items-center gap-2">
        <RiHomeLine className="text-base" />
        {breadcrumbs.map((label, i) => {
          const isLast = i === breadcrumbs.length - 1;
          return (
            <Fragment key={label}>
              <span>/</span>
              <span className={isLast ? 'text-ink font-bold' : ''}>{label}</span>
            </Fragment>
          );
        })}
      </nav>
      <button
        type="button"
        onClick={logout}
        className="inline-flex items-center gap-1.5 text-[0.85rem] font-bold uppercase tracking-wider text-ink-soft hover:text-ink"
        aria-label="Logout"
      >
        <RiLogoutBoxRLine /> Logout
      </button>
    </header>
  );
}
