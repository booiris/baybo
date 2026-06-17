import { useEffect, useMemo, useState } from 'react';
import {
  finishSetup,
  listModels,
  listProviders,
  OAUTH_PROVIDERS,
  pickDefaultModel,
  startOauth,
  submitApiKey,
  type ModelInfo,
  type ProviderInfo,
} from '../api/setup';
import { Select } from '../components/Select';

// Bespoke first-run wizard (docs/mac-app.md §5): provider -> credential ->
// model -> finish. API-key providers only for now; OAuth (openai-subscription)
// is a follow-up once the opener plugin is wired.
type Step =
  | { k: 'provider' }
  | { k: 'credential'; provider: ProviderInfo }
  | { k: 'model'; provider: ProviderInfo; entryName: string; baseUrl?: string; models: ModelInfo[] }
  | { k: 'finishing' };

// Curated providers shown first so the picker isn't overwhelming; the rest are
// revealed by the "More providers" toggle.
const FEATURED = ['openai', 'anthropic', 'gemini', 'deepseek', 'minimax', 'openai-subscription'];
const LABELS: Record<string, string> = {
  openai: 'OpenAI',
  anthropic: 'Anthropic',
  gemini: 'Gemini',
  deepseek: 'DeepSeek',
  minimax: 'MiniMax',
  'openai-subscription': 'ChatGPT(subscription)',
};
const providerLabel = (id: string) => LABELS[id] ?? id;

export function SetupWizard({
  gitAvailable,
  onComplete,
}: {
  gitAvailable: boolean;
  onComplete: () => void;
}) {
  const [step, setStep] = useState<Step>({ k: 'provider' });
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  return (
    <div
      data-tauri-drag-region
      className="flex h-screen items-center justify-center overflow-hidden bg-canvas p-8"
    >
      <div className="flex max-h-full w-[560px] flex-col rounded-brutal border-[3px] border-border bg-surface p-8 shadow-brutal">
        <header className="mb-6 flex shrink-0 items-center gap-3">
          <div className="flex h-10 w-10 items-center justify-center rounded-brutal border-[3px] border-border bg-rail font-mono text-xl font-bold">
            A
          </div>
          <div>
            <h1 className="text-2xl font-bold uppercase tracking-tight">Set up Aura</h1>
            <p className="text-sm text-ink-soft">Connect a model to get started</p>
          </div>
        </header>

        {!gitAvailable && (
          <div className="mb-4 shrink-0 rounded-brutal border-2 border-warn bg-surface p-3 text-sm font-bold text-warn shadow-brutal-sm">
            Aura needs the Xcode Command Line Tools (run{' '}
            <code className="font-mono">xcode-select --install</code>) before it can finish setup.
          </div>
        )}

        {error && (
          <div className="mb-4 shrink-0 rounded-brutal border-2 border-err bg-surface p-3 text-sm font-bold text-err shadow-brutal-sm">
            {error}
          </div>
        )}

        <div className="no-scrollbar max-h-[340px] overflow-y-auto">
        {step.k === 'provider' && (
          <ProviderStep
            onPick={(provider) => {
              setError(null);
              setStep({ k: 'credential', provider });
            }}
          />
        )}

        {step.k === 'credential' && (
          <CredentialStep
            provider={step.provider}
            busy={busy}
            onBack={() => setStep({ k: 'provider' })}
            onSubmit={async (apiKey, baseUrl) => {
              setBusy(true);
              setError(null);
              try {
                const { entryName } = await submitApiKey(step.provider.id, apiKey, baseUrl);
                const models = await listModels(step.provider.id);
                setStep({ k: 'model', provider: step.provider, entryName, baseUrl, models });
              } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
              } finally {
                setBusy(false);
              }
            }}
            onOAuth={async () => {
              setBusy(true);
              setError(null);
              try {
                const { entryName } = await startOauth();
                const models = await listModels(step.provider.id).catch(() => []);
                setStep({ k: 'model', provider: step.provider, entryName, models });
              } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
              } finally {
                setBusy(false);
              }
            }}
          />
        )}

        {step.k === 'model' && (
          <ModelStep
            models={step.models}
            busy={busy}
            onBack={() => setStep({ k: 'credential', provider: step.provider })}
            onConfirm={async (model) => {
              setBusy(true);
              setError(null);
              setStep({ k: 'finishing' });
              try {
                await finishSetup({
                  provider: step.provider.id,
                  entryName: step.entryName,
                  model,
                  baseUrl: step.baseUrl,
                });
                onComplete();
              } catch (e) {
                setError(e instanceof Error ? e.message : String(e));
                setStep(step);
              } finally {
                setBusy(false);
              }
            }}
          />
        )}

        {step.k === 'finishing' && (
          <p className="font-mono text-sm text-ink-soft">Writing config and starting Aura…</p>
        )}
        </div>
      </div>
    </div>
  );
}

function ProviderStep({ onPick }: { onPick: (p: ProviderInfo) => void }) {
  const [providers, setProviders] = useState<ProviderInfo[] | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [showAll, setShowAll] = useState(false);

  useEffect(() => {
    listProviders().then(setProviders, (e) => setErr(String(e)));
  }, []);

  if (err) return <p className="text-sm text-err">{err}</p>;
  if (!providers) return <p className="font-mono text-sm text-ink-soft">Loading providers…</p>;

  const byId = new Map(providers.map((p) => [p.id, p]));
  const featured = FEATURED.map((id) => byId.get(id)).filter((p): p is ProviderInfo => Boolean(p));
  const featuredIds = new Set(featured.map((p) => p.id));
  const rest = providers.filter((p) => !featuredIds.has(p.id));
  // Featured first (stable order); "More" appends the rest, never reordering.
  const shown = !showAll && featured.length > 0 ? featured : [...featured, ...rest];
  const canToggle = featured.length > 0 && rest.length > 0;

  return (
    <div>
      <p className="mb-3 text-sm font-bold uppercase tracking-wider text-ink-soft">Provider</p>
      <div className="grid grid-cols-2 gap-3">
        {shown.map((p) => (
          <button
            key={p.id}
            onClick={() => onPick(p)}
            className="rounded-brutal border-[3px] border-border bg-canvas px-4 py-3 text-left font-bold shadow-brutal-sm transition-transform hover:-translate-y-0.5"
          >
            {providerLabel(p.id)}
          </button>
        ))}
      </div>
      {canToggle && (
        <button
          onClick={() => setShowAll((v) => !v)}
          className="mt-3 text-sm font-bold text-ink-soft underline"
        >
          {showAll ? 'Show less' : 'More providers'}
        </button>
      )}
    </div>
  );
}

function CredentialStep({
  provider,
  busy,
  onBack,
  onSubmit,
  onOAuth,
}: {
  provider: ProviderInfo;
  busy: boolean;
  onBack: () => void;
  onSubmit: (apiKey: string, baseUrl?: string) => void;
  onOAuth: () => void;
}) {
  const [apiKey, setApiKey] = useState('');
  const [baseUrl, setBaseUrl] = useState(provider.defaultBaseUrl ?? '');
  const [showAdvanced, setShowAdvanced] = useState(false);

  if (OAUTH_PROVIDERS.has(provider.id)) {
    return (
      <div>
        <p className="mb-3 text-sm text-ink-soft">
          Sign in with your {providerLabel(provider.id)} account — a browser window will open to
          complete the login.
        </p>
        <div className="flex justify-between">
          <button onClick={onBack} className="text-sm font-bold text-ink-soft underline">
            Back
          </button>
          <button
            disabled={busy}
            onClick={onOAuth}
            className="rounded-brutal border-[3px] border-border bg-accent px-5 py-2 font-bold text-surface shadow-brutal-sm disabled:opacity-50"
          >
            {busy ? 'Signing in…' : 'Sign in'}
          </button>
        </div>
      </div>
    );
  }

  return (
    <div>
      <p className="mb-2 text-sm font-bold uppercase tracking-wider text-ink-soft">
        {providerLabel(provider.id)} API key
      </p>
      <input
        type="password"
        autoFocus
        value={apiKey}
        onChange={(e) => setApiKey(e.target.value)}
        placeholder="Paste your API key — encrypted into the vault"
        className="w-full rounded-brutal border-[3px] border-border bg-canvas px-4 py-3 font-mono text-sm focus:outline-none focus:shadow-brutal-sm"
      />
      <button
        type="button"
        onClick={() => setShowAdvanced((v) => !v)}
        className="mt-3 text-sm text-ink-soft underline"
      >
        {showAdvanced ? 'Hide' : 'Advanced'}
      </button>
      {showAdvanced && (
        <input
          type="url"
          value={baseUrl}
          onChange={(e) => setBaseUrl(e.target.value)}
          placeholder="Base URL (optional)"
          className="mt-2 w-full rounded-brutal border-[3px] border-border bg-canvas px-4 py-3 font-mono text-sm focus:outline-none focus:shadow-brutal-sm"
        />
      )}
      <div className="mt-6 flex justify-between">
        <button onClick={onBack} className="text-sm font-bold text-ink-soft underline">
          Back
        </button>
        <button
          disabled={!apiKey.trim() || busy}
          onClick={() => onSubmit(apiKey.trim(), baseUrl.trim() || undefined)}
          className="rounded-brutal border-[3px] border-border bg-accent px-5 py-2 font-bold text-surface shadow-brutal-sm disabled:opacity-50"
        >
          {busy ? 'Checking…' : 'Continue'}
        </button>
      </div>
    </div>
  );
}

function ModelStep({
  models,
  busy,
  onBack,
  onConfirm,
}: {
  models: ModelInfo[];
  busy: boolean;
  onBack: () => void;
  onConfirm: (model: string) => void;
}) {
  const defaultModel = useMemo(() => pickDefaultModel(models), [models]);
  const [selected, setSelected] = useState<string>(defaultModel ?? '');
  const [custom, setCustom] = useState('');

  const model = selected === '__custom__' ? custom.trim() : selected;

  return (
    <div>
      <p className="mb-2 text-sm font-bold uppercase tracking-wider text-ink-soft">Model</p>
      {models.length > 0 ? (
        <Select
          value={selected}
          onChange={setSelected}
          options={[
            ...models.map((m) => ({
              value: m.id,
              label: m.id,
              hint: m.contextWindow ? `${m.contextWindow.toLocaleString()} ctx` : undefined,
            })),
            { value: '__custom__', label: 'Custom model id…' },
          ]}
        />
      ) : (
        <p className="mb-2 text-sm text-ink-soft">
          Couldn’t discover models — enter the model id manually.
        </p>
      )}
      {(models.length === 0 || selected === '__custom__') && (
        <input
          value={custom}
          onChange={(e) => setCustom(e.target.value)}
          placeholder="e.g. claude-sonnet-4-6"
          className="mt-2 w-full rounded-brutal border-[3px] border-border bg-canvas px-4 py-3 font-mono text-sm focus:outline-none"
        />
      )}
      <div className="mt-6 flex justify-between">
        <button onClick={onBack} className="text-sm font-bold text-ink-soft underline">
          Back
        </button>
        <button
          disabled={!model || busy}
          onClick={() => onConfirm(model)}
          className="rounded-brutal border-[3px] border-border bg-accent px-5 py-2 font-bold text-surface shadow-brutal-sm disabled:opacity-50"
        >
          Finish
        </button>
      </div>
    </div>
  );
}
