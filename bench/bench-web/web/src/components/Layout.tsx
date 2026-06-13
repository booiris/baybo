import { useState } from 'react';
import { Link, Outlet, useNavigate, useSearchParams } from 'react-router-dom';
import { RiSearchLine } from 'react-icons/ri';

export function Layout() {
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const [q, setQ] = useState(params.get('q') ?? '');

  return (
    <div className="h-full flex flex-col">
      <header className="border-b-[3px] border-black bg-white flex items-center gap-4 px-5 py-3 shrink-0">
        <Link to="/" className="font-bold text-lg tracking-tight">
          AURA<span className="text-brand">·BENCH</span>
        </Link>
        <form
          className="ml-auto relative"
          onSubmit={(e) => {
            e.preventDefault();
            const term = q.trim();
            if (term) navigate(`/search?q=${encodeURIComponent(term)}`);
          }}
        >
          <RiSearchLine className="absolute left-2.5 top-1/2 -translate-y-1/2 text-ink-soft" />
          <input
            value={q}
            onChange={(e) => setQ(e.target.value)}
            placeholder="search tasks…"
            className="border-2 border-black rounded-md pl-8 pr-3 py-1.5 font-mono text-sm w-72 bg-canvas focus:outline-none focus:shadow-brutal-xs"
          />
        </form>
      </header>
      <main className="flex-1 overflow-auto scroll-area p-6">
        <Outlet />
      </main>
    </div>
  );
}
