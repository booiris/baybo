import { useState } from 'react';
import { Link, Outlet, useNavigate, useSearchParams } from 'react-router-dom';
import { RiSearchLine } from 'react-icons/ri';
import { setOptimizedToolIo, useOptimizedToolIo } from '../lib/toolIo';

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
        <div className="ml-auto flex items-center gap-3">
          <ToolIoToggle />
          <form
            className="relative"
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
        </div>
      </header>
      <main className="flex-1 overflow-auto scroll-area p-6">
        <Outlet />
      </main>
    </div>
  );
}

/** Global switch for how tool input/output renders in the trace views:
 * `clean` strips the JSON envelope and shows real newlines; `raw` keeps
 * the pretty-printed JSON. Defaults to `clean`. */
function ToolIoToggle() {
  const optimized = useOptimizedToolIo();
  return (
    <div
      className="flex items-center gap-1.5"
      title="Tool input/output: 'clean' strips the JSON envelope and renders real newlines; 'raw' shows pretty-printed JSON"
    >
      <span className="hidden md:inline text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft">
        tool i/o
      </span>
      <div className="flex border-2 border-black rounded-md overflow-hidden text-[0.65rem] font-bold uppercase tracking-wider">
        <button
          type="button"
          onClick={() => setOptimizedToolIo(true)}
          className={`px-2 py-1 cursor-pointer ${
            optimized ? 'bg-brand text-white' : 'bg-white text-ink hover:bg-gray-50'
          }`}
        >
          clean
        </button>
        <button
          type="button"
          onClick={() => setOptimizedToolIo(false)}
          className={`px-2 py-1 cursor-pointer border-l-2 border-black ${
            optimized ? 'bg-white text-ink hover:bg-gray-50' : 'bg-brand text-white'
          }`}
        >
          raw
        </button>
      </div>
    </div>
  );
}
