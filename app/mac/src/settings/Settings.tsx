import { useCallback, useEffect, useState, type ReactNode } from 'react';
import { admin, listModels } from '../api/admin';
import type { Connection } from '../api/connection';
import { SetupWizard } from '../setup/SetupWizard';

// In-place settings pane (docs/mac-app.md §9): fills the main content area next
// to the persistent app sidebar — NOT a modal, and no longer carries its own
// sub-nav rail (there is a single section). Adding a provider reuses the setup
// wizard inline and needs a relaunch to go live (the embed has hot-reload off).

export function SettingsView({ conn }: { conn: Connection }) {
  const [models, setModels] = useState<string[]>([]);
  const [adding, setAdding] = useState(false);
  const [added, setAdded] = useState(false);

  const refreshModels = useCallback(() => {
    listModels(admin(conn)).then(setModels, () => undefined);
  }, [conn]);

  useEffect(() => {
    refreshModels();
  }, [refreshModels]);

  return (
    <div className="relative flex flex-1 flex-col overflow-hidden">
      <header
        data-tauri-drag-region="deep"
        className="flex items-center gap-3 border-b-[3px] border-border bg-canvas px-6 py-3"
      >
        {adding && (
          <button
            onClick={() => setAdding(false)}
            className="rounded-brutal border-2 border-border bg-surface px-2.5 py-1 text-xs font-bold shadow-brutal-sm hover:-translate-y-0.5 hover:shadow-brutal active:translate-y-0 active:shadow-none"
          >
            ← 返回
          </button>
        )}
        <h2 className="text-base font-bold uppercase tracking-tight">设置 · Settings</h2>
      </header>

      <div className="flex-1 overflow-y-auto">
        {adding ? (
          <SetupWizard
            gitAvailable={true}
            onComplete={() => {
              setAdding(false);
              setAdded(true);
              refreshModels();
            }}
          />
        ) : (
          <div className="mx-auto max-w-2xl px-8 py-10">
            <SettingsSection
              title="Models"
              subtitle="Models discovered from your configured providers."
            >
              {added && (
                <div className="mb-4 rounded-brutal border-2 border-warn bg-surface p-3 text-sm font-bold text-warn shadow-brutal-sm">
                  Provider added — relaunch Aura to use it.
                </div>
              )}
              {models.length > 0 ? (
                <ul className="space-y-2 font-mono text-sm">
                  {models.map((m) => (
                    <li key={m} className="rounded-brutal border-2 border-border bg-canvas px-3 py-2">
                      {m}
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="text-sm text-ink-soft">No models configured.</p>
              )}
              <button
                onClick={() => setAdding(true)}
                className="mt-5 rounded-brutal border-[3px] border-border bg-accent px-4 py-2 text-sm font-bold text-surface shadow-brutal-sm hover:-translate-y-0.5 hover:shadow-brutal active:translate-y-0 active:shadow-none"
              >
                Add provider
              </button>
            </SettingsSection>
          </div>
        )}
      </div>
    </div>
  );
}

function SettingsSection({
  title,
  subtitle,
  children,
}: {
  title: string;
  subtitle?: string;
  children: ReactNode;
}) {
  return (
    <section>
      <h3 className="text-2xl font-bold uppercase tracking-tight">{title}</h3>
      {subtitle && <p className="mb-5 mt-1 text-sm text-ink-soft">{subtitle}</p>}
      {children}
    </section>
  );
}
