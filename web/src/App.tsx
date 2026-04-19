import { Navigate, Route, Routes, useLocation } from 'react-router-dom';
import { Sidebar } from './components/Sidebar';
import { TopNav } from './components/TopNav';
import { LogPage } from './pages/LogPage';
import { TracePage } from './pages/TracePage';

function titleFromPath(pathname: string): string {
  if (pathname.startsWith('/trace')) return 'Trace';
  return 'Log';
}

export default function App() {
  const { pathname } = useLocation();
  return (
    <div className="flex h-screen">
      <Sidebar />
      <main className="flex-1 flex flex-col overflow-y-auto bg-canvas">
        <TopNav current={titleFromPath(pathname)} />
        <Routes>
          <Route path="/" element={<Navigate to="/log" replace />} />
          <Route path="/log" element={<LogPage />} />
          <Route path="/trace" element={<TracePage />} />
          <Route path="*" element={<Navigate to="/log" replace />} />
        </Routes>
      </main>
    </div>
  );
}
