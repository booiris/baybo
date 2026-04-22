import { Navigate, Route, Routes } from 'react-router-dom';
import { Sidebar } from './components/Sidebar';
import { TopNav } from './components/TopNav';
import { LoginScreen } from './components/LoginScreen';
import { LogsPage } from './pages/LogsPage';
import { useAuth } from './api/auth';

export default function App() {
  const { token } = useAuth();

  if (!token) return <LoginScreen />;

  return (
    <div className="flex h-screen">
      <Sidebar />
      <main className="flex-1 flex flex-col overflow-hidden bg-canvas">
        <TopNav breadcrumbs={['System Logs']} />
        <Routes>
          <Route path="/" element={<Navigate to="/logs" replace />} />
          <Route path="/logs" element={<LogsPage />} />
          <Route path="*" element={<Navigate to="/logs" replace />} />
        </Routes>
      </main>
    </div>
  );
}
