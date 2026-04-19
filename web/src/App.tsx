import { Navigate, Route, Routes, useLocation } from 'react-router-dom';
import { Sidebar } from './components/Sidebar';
import { TopNav } from './components/TopNav';
import { LoginScreen } from './components/LoginScreen';
import { JobsPage } from './pages/JobsPage';
import { TracePage } from './pages/TracePage';
import { useAuth } from './api/auth';

function titleFromPath(pathname: string): string {
  if (pathname.startsWith('/trace')) return 'Trace';
  return 'Jobs';
}

export default function App() {
  const { token } = useAuth();
  const { pathname } = useLocation();

  if (!token) return <LoginScreen />;

  return (
    <div className="flex h-screen">
      <Sidebar />
      <main className="flex-1 flex flex-col overflow-y-auto bg-canvas">
        <TopNav current={titleFromPath(pathname)} />
        <Routes>
          <Route path="/" element={<Navigate to="/jobs" replace />} />
          <Route path="/jobs" element={<JobsPage />} />
          <Route path="/trace" element={<TracePage />} />
          <Route path="*" element={<Navigate to="/jobs" replace />} />
        </Routes>
      </main>
    </div>
  );
}
