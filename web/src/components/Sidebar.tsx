import { NavLink } from 'react-router-dom';
import { RiFileList3Line } from 'react-icons/ri';

const navItem =
  'flex items-center gap-2.5 px-3 py-2.5 no-underline text-ink text-[1rem] font-bold uppercase tracking-wider rounded-md transition-[transform,box-shadow] duration-100 border-[3px] border-transparent cursor-pointer hover:bg-gray-100';
const navItemActive =
  'bg-brand text-white !border-black shadow-brutal-sm hover:!bg-brand active:translate-x-[2px] active:translate-y-[2px] active:shadow-none';

export function Sidebar() {
  return (
    <aside className="w-[180px] border-r-2 border-black flex flex-col bg-white shrink-0">
      <div className="px-4 py-5">
        <h1 className="text-4xl font-bold uppercase -tracking-[0.05em]">AURA</h1>
        <span className="text-sm text-ink-soft font-mono">v1.0.4</span>
      </div>

      <nav className="flex-1 px-3 py-2 flex flex-col gap-2">
        <NavLink
          to="/logs"
          className={({ isActive }) => `${navItem} ${isActive ? navItemActive : ''}`}
        >
          <RiFileList3Line className="text-xl" />
          Log
        </NavLink>
      </nav>


    </aside>
  );
}
