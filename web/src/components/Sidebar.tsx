import { NavLink } from 'react-router-dom';
import { RiBookReadLine, RiFileList3Line } from 'react-icons/ri';

const navItem =
  'flex items-center gap-3 px-4 py-3 no-underline text-ink font-semibold rounded-md transition-all duration-200 border-2 border-transparent hover:bg-gray-100';
const navItemActive = 'bg-brand text-white border-[3px] border-black shadow-brutal-sm hover:bg-brand';

export function Sidebar() {
  return (
    <aside className="w-[250px] border-r-[3px] border-black flex flex-col bg-white shrink-0">
      <div className="px-5 py-6">
        <h1 className="text-2xl font-bold -tracking-[0.05em]">AURA</h1>
        <span className="text-sm text-ink-soft">v1.0.4</span>
      </div>

      <nav className="flex-1 px-4 py-2.5 flex flex-col gap-3">
        <NavLink
          to="/logs"
          className={({ isActive }) => `${navItem} ${isActive ? navItemActive : ''}`}
        >
          <RiFileList3Line className="text-xl" />
          Log
        </NavLink>
      </nav>

      <div className="mx-4 mt-5 p-6 border-t border-gray-200">
        <a
          href="#docs"
          className={`${navItem} text-ink-soft hover:bg-transparent hover:text-ink`}
        >
          <RiBookReadLine className="text-xl" />
          Docs
        </a>
      </div>
    </aside>
  );
}
