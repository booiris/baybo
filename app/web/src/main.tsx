import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { HashRouter } from 'react-router-dom';
import App from './App';
import { AdminAuthProvider } from './api/auth';
import { QueueProvider } from './pages/chat/queueStore';
import { FolderProvider } from './pages/chat/folderStore';
import { registerPwa } from './pwa/registerSW';
import { PwaUpdateBanner } from './pwa/PwaUpdateBanner';
// KaTeX math styling + its self-hosted math fonts. Vite emits the referenced
// font files into `dist/assets` and the gateway serves them off the embedded
// asset table like every other asset — no CDN, so math renders on an air-gapped
// box. Imported before `index.css` so our `.chat-prose .katex-*` overrides win
// the cascade.
import 'katex/dist/katex.min.css';
import './index.css';

const root = document.getElementById('root');
if (!root) {
  throw new Error('#root element not found');
}

// Before render: `beforeinstallprompt` can fire early, and a missed one is
// gone for the session — the browser does not re-offer it.
registerPwa();

createRoot(root).render(
  <StrictMode>
    <AdminAuthProvider>
      <QueueProvider>
        <FolderProvider>
          <HashRouter>
            <App />
          </HashRouter>
          {/* Outside the router so the reload offer survives the login screen. */}
          <PwaUpdateBanner />
        </FolderProvider>
      </QueueProvider>
    </AdminAuthProvider>
  </StrictMode>,
);
