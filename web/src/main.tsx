import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { HashRouter } from 'react-router-dom';
import App from './App';
import { AdminAuthProvider } from './api/auth';
import './index.css';

const root = document.getElementById('root');
if (!root) {
  throw new Error('#root element not found');
}

createRoot(root).render(
  <StrictMode>
    <AdminAuthProvider>
      <HashRouter>
        <App />
      </HashRouter>
    </AdminAuthProvider>
  </StrictMode>,
);
