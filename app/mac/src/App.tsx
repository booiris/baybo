import { useCallback, useEffect, useState } from 'react';
import { getConnection, type Connection } from './api/connection';
import { setupStatus, startRuntime } from './api/setup';
import { SetupWizard } from './setup/SetupWizard';
import { Workspace } from './chat/Workspace';

// App lifecycle (docs/mac-app.md §5): check setup status -> run the wizard when
// unconfigured, otherwise start the embedded runtime and connect. The real chat
// UI (sidebar + thread + composer) lands in later milestones; for now the
// "ready" screen confirms the loopback gateway answers an authenticated call.
type Phase =
  | { k: 'loading' }
  | { k: 'wizard'; gitAvailable: boolean }
  | { k: 'booting' }
  | { k: 'ready'; conn: Connection; status: unknown }
  | { k: 'error'; message: string };

export function App() {
  const [phase, setPhase] = useState<Phase>({ k: 'loading' });

  const connect = useCallback(async (signal: AbortSignal) => {
    setPhase({ k: 'booting' });
    try {
      const conn = await getConnection(signal);
      const res = await fetch(`${conn.baseUrl}/v1/status`, {
        headers: { Authorization: `Bearer ${conn.adminToken}` },
        signal,
      });
      if (!res.ok) throw new Error(`/v1/status → HTTP ${res.status}`);
      setPhase({ k: 'ready', conn, status: await res.json() });
    } catch (e) {
      if (signal.aborted) return;
      setPhase({ k: 'error', message: e instanceof Error ? e.message : String(e) });
    }
  }, []);

  useEffect(() => {
    const ac = new AbortController();
    (async () => {
      try {
        const s = await setupStatus();
        if (s.configured) {
          await startRuntime();
          await connect(ac.signal);
        } else {
          setPhase({ k: 'wizard', gitAvailable: s.gitAvailable });
        }
      } catch (e) {
        if (ac.signal.aborted) return;
        setPhase({ k: 'error', message: e instanceof Error ? e.message : String(e) });
      }
    })();
    return () => ac.abort();
  }, [connect]);

  if (phase.k === 'wizard') {
    return (
      <SetupWizard
        gitAvailable={phase.gitAvailable}
        onComplete={() => {
          const ac = new AbortController();
          void connect(ac.signal);
        }}
      />
    );
  }

  if (phase.k === 'ready') {
    return <Workspace conn={phase.conn} />;
  }

  return (
    <div
      data-tauri-drag-region
      className="flex min-h-screen items-center justify-center bg-canvas p-8"
    >
      <div className="w-[520px] rounded-brutal border-[3px] border-border bg-surface p-8 shadow-brutal">
        <div className="mb-6 flex items-center gap-3">
          <div className="flex h-10 w-10 items-center justify-center rounded-brutal border-[3px] border-border bg-rail font-mono text-xl font-bold">
            A
          </div>
          <div>
            <h1 className="text-2xl font-bold uppercase tracking-tight">Aura</h1>
          </div>
        </div>

        {phase.k === 'loading' && (
          <p className="font-mono text-sm text-ink-soft">Checking setup…</p>
        )}

        {phase.k === 'booting' && (
          <p className="font-mono text-sm text-ink-soft">Starting the in-process runtime…</p>
        )}

        {phase.k === 'error' && (
          <div className="rounded-brutal border-2 border-err bg-surface p-3 text-sm font-bold text-err shadow-brutal-sm">
            {phase.message}
          </div>
        )}
      </div>
    </div>
  );
}
