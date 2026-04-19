import { RiLogoutBoxRLine } from 'react-icons/ri';
import { useAuth } from '../api/auth';

export function TopNav({ current }: { current: string }) {
  const { logout } = useAuth();
  return (
    <header className="px-8 py-5 border-b-2 border-black bg-white shrink-0 flex items-center justify-between">
      <div className="text-[0.9rem] font-semibold text-ink-soft">
        <span>Home</span>
        <span className="mx-2">/</span>
        <span className="text-ink font-bold">{current}</span>
      </div>
      <button
        type="button"
        onClick={logout}
        className="inline-flex items-center gap-1.5 text-[0.85rem] font-bold uppercase tracking-wider text-ink-soft hover:text-ink"
      >
        <RiLogoutBoxRLine /> Logout
      </button>
    </header>
  );
}
