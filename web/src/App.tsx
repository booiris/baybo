import { useEffect, useState } from 'react';
import { Navigate, Route, Routes, useLocation } from 'react-router-dom';
import { RiLoader4Line } from 'react-icons/ri';
import { Sidebar } from './components/Sidebar';
import { LoginScreen } from './components/LoginScreen';
import { LogsPage } from './pages/LogsPage';
import { TracesPage } from './pages/TracesPage';
import { TraceSessionPage } from './pages/TraceSessionPage';
import { CronPage } from './pages/CronPage';
import { AnalyticsPage } from './pages/AnalyticsPage';
import { LlmPage } from './pages/LlmPage';
import { ChatPage } from './pages/ChatPage';
import { useAuth } from './api/auth';

export default function App() {
  const { token, client, logout } = useAuth();
  const [isValidating, setIsValidating] = useState(true);
  const [isValid, setIsValid] = useState(false);
  const [version, setVersion] = useState<string | undefined>();
  const location = useLocation();

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
        const { data, response } = await client!.GET('/v1/status');
        if (canceled) return;
        if (response.status === 401) {
          logout();
          setIsValid(false);
        } else {
          setVersion(data?.version);
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

  // ChatShell lives outside the admin sidebar — /chat/* renders a
  // distinct shell with its own session-list rail. The admin sidebar
  // remains the entry point and exposes a Chat NavLink that drops
  // into this branch.
  if (location.pathname.startsWith('/chat')) {
    return (
      <Routes>
        <Route path="/chat" element={<ChatPage />} />
        <Route path="/chat/:sessionId" element={<ChatPage />} />
      </Routes>
    );
  }

  return (
    <div className="flex h-screen">
      <Sidebar version={version} />
      <main className="flex-1 flex flex-col overflow-hidden bg-canvas">
        <Routes>
          <Route path="/" element={<Navigate to="/logs" replace />} />
          <Route path="/logs" element={<LogsPage />} />
          <Route path="/traces" element={<TracesPage />} />
          <Route path="/traces/:id" element={<TraceSessionPage />} />
          <Route path="/cron" element={<CronPage />} />
          <Route path="/analytics" element={<AnalyticsPage />} />
          <Route path="/llm" element={<LlmPage />} />
          <Route path="*" element={<Navigate to="/logs" replace />} />
        </Routes>
      </main>
    </div>
  );
}
