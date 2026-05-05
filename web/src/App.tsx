import { useEffect, useState } from 'react';
import { Navigate, Route, Routes } from 'react-router-dom';
import { RiLoader4Line } from 'react-icons/ri';
import { Sidebar } from './components/Sidebar';
import { LoginScreen } from './components/LoginScreen';
import { LogsPage } from './pages/LogsPage';
import { TracesPage } from './pages/TracesPage';
import { TraceSessionPage } from './pages/TraceSessionPage';
import { CronPage } from './pages/CronPage';
import { AnalyticsPage } from './pages/AnalyticsPage';
import { useAuth } from './api/auth';

export default function App() {
  const { token, client, logout } = useAuth();
  const [isValidating, setIsValidating] = useState(true);
  const [isValid, setIsValid] = useState(false);

  useEffect(() => {
    if (!token || !client) {
      setIsValid(false);
      setIsValidating(false);
      return;
    }
    
    setIsValidating(true);
    let canceled = false;
    
    async function validate() {
      try {
        const { response } = await client!.GET('/v1/status');
        if (canceled) return;
        if (response.status === 401) {
          logout();
          setIsValid(false);
        } else {
          setIsValid(true);
        }
      } catch {
        if (!canceled) setIsValid(true);
      } finally {
        if (!canceled) setIsValidating(false);
      }
    }
    
    void validate();
    return () => { canceled = true; };
  }, [token, client, logout]);

  if (!token) return <LoginScreen />;

  if (isValidating || !isValid) {
    return (
      <div className="flex items-center justify-center min-h-screen bg-canvas">
        <RiLoader4Line className="text-4xl text-ink-soft animate-spin" />
      </div>
    );
  }

  return (
    <div className="flex h-screen">
      <Sidebar />
      <main className="flex-1 flex flex-col overflow-hidden bg-canvas">
        <Routes>
          <Route path="/" element={<Navigate to="/logs" replace />} />
          <Route path="/logs" element={<LogsPage />} />
          <Route path="/traces" element={<TracesPage />} />
          <Route path="/traces/:id" element={<TraceSessionPage />} />
          <Route path="/cron" element={<CronPage />} />
          <Route path="/analytics" element={<AnalyticsPage />} />
          <Route path="*" element={<Navigate to="/logs" replace />} />
        </Routes>
      </main>
    </div>
  );
}