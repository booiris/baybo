import { useCallback, useEffect, useState } from 'react';
import {
  RiCpuLine,
  RiRefreshLine,
  RiLoader4Line,
  RiStarFill,
  RiStarLine,
  RiEdit2Line,
  RiPlayCircleLine,
  RiCheckLine,
  RiCloseLine,
  RiMoneyDollarCircleLine,
  RiKey2Line,
  RiEyeLine,
  RiEyeOffLine,
} from 'react-icons/ri';
import { Button } from '../components/Button';
import { SelectBox } from '../components/SelectBox';
import { useAdminClient, useAuth } from '../api/auth';
import type { components } from '../api/schema';

type LlmModelEntry = components['schemas']['LlmModelEntry'];
type LlmModelsResponse = components['schemas']['LlmModelsResponse'];
type LlmModelTestResult = components['schemas']['LlmModelTestResult'];
type UpdateLlmModelRequest = components['schemas']['UpdateLlmModelRequest'];
type LlmPricingOverrideDto = components['schemas']['LlmPricingOverrideDto'];

function microToUsdInput(micros: number | null | undefined): string {
  if (micros == null) return '';
  return (micros / 1_000_000).toString();
}

type PricingParse =
  | { kind: 'empty' }
  | { kind: 'value'; micros: number }
  | { kind: 'invalid' };

// Parse a USD-per-1M-tokens decimal into micro-USD. Empty string is
// "cleared" and any non-empty unparseable / negative value is `invalid`
// so the form can reject the submit instead of silently wiping the
// override. Lossy across the float boundary by design — matches how
// operators type pricing in `baybo.json`.
function parseUsdInput(raw: string): PricingParse {
  const trimmed = raw.trim();
  if (trimmed === '') return { kind: 'empty' };
  const parsed = Number(trimmed);
  if (!Number.isFinite(parsed) || parsed < 0) return { kind: 'invalid' };
  return { kind: 'value', micros: Math.round(parsed * 1_000_000) };
}

export function LlmPage() {
  const client = useAdminClient();
  const { logout } = useAuth();

  const [data, setData] = useState<LlmModelsResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);

  const [editing, setEditing] = useState<LlmModelEntry | null>(null);
  const [testing, setTesting] = useState<Record<string, boolean>>({});
  const [testResults, setTestResults] = useState<Record<string, LlmModelTestResult>>({});

  // -------- fetch ---------
  useEffect(() => {
    let canceled = false;
    async function load() {
      setLoading(true);
      setError(null);
      try {
        const modelsRes = await client.GET('/v1/llm/models');
        if (canceled) return;
        if (modelsRes.response.status === 401) {
          logout();
          return;
        }
        if (modelsRes.error || !modelsRes.response.ok) {
          setError(modelsRes.error?.error || `HTTP Error ${modelsRes.response.status}`);
          return;
        }
        setData(modelsRes.data ?? null);
      } catch (e) {
        if (canceled) return;
        setError(e instanceof Error ? `Network error: ${e.message}` : 'Network error contacting gateway');
      } finally {
        if (!canceled) setLoading(false);
      }
    }
    void load();
    return () => { canceled = true; };
  }, [client, logout, refreshKey]);

  // -------- actions ---------
  const handleSetDefault = useCallback(
    async (name: string) => {
      const { response, error: apiError } = await client.PUT('/v1/llm/default', {
        body: { name },
      });
      if (response.status === 401) { logout(); return; }
      if (apiError || !response.ok) {
        setError(apiError?.error || `HTTP Error ${response.status}`);
        return;
      }
      setRefreshKey((k) => k + 1);
    },
    [client, logout],
  );

  const handleTest = useCallback(
    async (name: string) => {
      setTesting((m) => ({ ...m, [name]: true }));
      try {
        const { response, data: result, error: apiError } = await client.POST(
          '/v1/llm/models/{name}/test',
          { params: { path: { name } } },
        );
        if (response.status === 401) { logout(); return; }
        if (apiError || !response.ok) {
          setTestResults((m) => ({
            ...m,
            [name]: {
              ok: false,
              error: apiError?.error || `HTTP Error ${response.status}`,
              provider: '',
              model: '',
            },
          }));
          return;
        }
        if (result) setTestResults((m) => ({ ...m, [name]: result }));
      } finally {
        setTesting((m) => ({ ...m, [name]: false }));
      }
    },
    [client, logout],
  );

  const handleSave = useCallback(
    async (name: string, body: UpdateLlmModelRequest): Promise<string | null> => {
      const { response, error: apiError } = await client.PUT('/v1/llm/models/{name}', {
        params: { path: { name } },
        body,
      });
      if (response.status === 401) { logout(); return 'session expired'; }
      if (apiError || !response.ok) {
        return apiError?.error || `HTTP Error ${response.status}`;
      }
      setEditing(null);
      setRefreshKey((k) => k + 1);
      return null;
    },
    [client, logout],
  );

  // -------- render ---------
  if (loading && !data) {
    return (
      <div className="p-8 flex flex-col items-center justify-center h-full">
        <RiLoader4Line className="animate-spin text-4xl text-ink-soft mb-3" />
        <p className="text-ink-soft font-bold uppercase tracking-wider">Loading LLMs…</p>
      </div>
    );
  }

  return (
    <div className="p-5 h-full flex flex-col overflow-y-auto bg-canvas">
      <div className="mb-6 flex justify-between items-start gap-3 flex-wrap">
        <div>
          <h2 className="text-[1.7rem] font-bold uppercase -tracking-[0.05em] mb-1">LLM</h2>
          {data && (
            <p className="text-ink-soft font-mono text-[0.85rem]">
              Default: <span className="font-bold text-ink">{data.default_name}</span>
              {' · '}
              {data.items.length} configured
            </p>
          )}
        </div>
        <Button
          onClick={() => setRefreshKey((k) => k + 1)}
          disabled={loading}
          className="!py-2 !px-4 !text-[0.9rem] h-10 w-[120px] justify-center gap-1.5"
        >
          <RiRefreshLine className="text-lg shrink-0" /> Refresh
        </Button>
      </div>

      {error && (
        <div className="mb-6 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
          {error}
        </div>
      )}

      {!data || data.items.length === 0 ? (
        <div className="p-8 flex flex-col items-center justify-center h-full">
          <RiCpuLine className="text-6xl text-ink-soft mb-4" />
          <p className="text-ink-soft font-bold uppercase">No LLM entries configured</p>
          <p className="text-ink-soft text-[0.85rem] mt-2">
            Add an entry to <code>baybo.json</code> or run <code>baybo llm add</code>.
          </p>
        </div>
      ) : (
        <div className="space-y-4 mb-6">
          {data.items.map((entry) => (
            <LlmCard
              key={entry.name}
              entry={entry}
              testing={Boolean(testing[entry.name])}
              testResult={testResults[entry.name]}
              onTest={() => handleTest(entry.name)}
              onSetDefault={() => handleSetDefault(entry.name)}
              onEdit={() => setEditing(entry)}
            />
          ))}
        </div>
      )}

      {editing && (
        <EditLlmModal
          entry={editing}
          onClose={() => setEditing(null)}
          onSave={(body) => handleSave(editing.name, body)}
        />
      )}
    </div>
  );
}

// ── LlmCard ───────────────────────────────────────────────────────────

function LlmCard({
  entry,
  testing,
  testResult,
  onTest,
  onSetDefault,
  onEdit,
}: {
  entry: LlmModelEntry;
  testing: boolean;
  testResult?: LlmModelTestResult;
  onTest: () => void;
  onSetDefault: () => void;
  onEdit: () => void;
}) {
  return (
    <div className="bg-white border-[3px] border-black rounded-md shadow-brutal p-5">
      <div className="flex items-start justify-between gap-4 mb-4 flex-wrap">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-3 mb-1 flex-wrap">
            <h3 className="font-bold text-[1.15rem] tracking-tight break-all">{entry.name}</h3>
            {entry.is_default ? (
              <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded-md text-[0.7rem] font-bold uppercase border-2 border-black bg-brand text-ink">
                <RiStarFill /> Default
              </span>
            ) : (
              <button
                type="button"
                onClick={onSetDefault}
                className="inline-flex items-center gap-1 px-2 py-0.5 rounded-md text-[0.7rem] font-bold uppercase border-2 border-black bg-white text-ink hover:bg-gray-50 cursor-pointer"
              >
                <RiStarLine /> Make Default
              </button>
            )}
          </div>
          <div className="font-mono text-[0.85rem] text-ink-soft break-all">
            <span className="font-bold text-ink">{entry.provider}</span>
            {' · '}
            {entry.model}
          </div>
        </div>
        <div className="flex items-center gap-2 shrink-0">
          <Button
            onClick={onTest}
            disabled={testing}
            className="!py-1.5 !px-3 !text-[0.85rem] h-9 gap-1.5"
          >
            {testing ? (
              <RiLoader4Line className="animate-spin text-base shrink-0" />
            ) : (
              <RiPlayCircleLine className="text-base shrink-0" />
            )}
            Test
          </Button>
          <Button
            onClick={onEdit}
            className="!py-1.5 !px-3 !text-[0.85rem] h-9 gap-1.5"
          >
            <RiEdit2Line className="text-base shrink-0" /> Edit
          </Button>
        </div>
      </div>

      {testResult && (
        <div
          className={`mb-4 border-[2px] rounded-md px-3 py-2 font-mono text-[0.85rem] ${
            testResult.ok
              ? 'border-ok text-ok bg-ok/5'
              : 'border-err text-err bg-err/5'
          }`}
        >
          {testResult.ok ? (
            <span className="inline-flex items-center gap-1.5">
              <RiCheckLine /> OK · {testResult.latency_ms}ms
              {testResult.input_tokens != null && (
                <span className="text-ink-soft">
                  · {testResult.input_tokens} in / {testResult.output_tokens} out
                </span>
              )}
            </span>
          ) : (
            <span className="inline-flex items-start gap-1.5">
              <RiCloseLine className="shrink-0 mt-0.5" />
              <span className="break-all">{testResult.error}</span>
            </span>
          )}
        </div>
      )}

      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-3 mb-4">
        <Field label="Context window" override={entry.context_window_override != null}>
          {entry.effective_context_window.toLocaleString()}
        </Field>
        <Field label="Vision" override={entry.supports_vision_override != null}>
          {entry.effective_supports_vision ? 'yes' : 'no'}
        </Field>
        <Field label="Base URL">{entry.base_url ?? <span className="text-ink-soft">— provider default</span>}</Field>
        <Field label="API key">
          <span className="inline-flex items-center gap-1.5">
            <RiKey2Line className={entry.api_key_configured ? 'text-ok' : 'text-err'} />
            {entry.api_key_configured ? 'configured' : 'missing'}
            {entry.api_key_env && (
              <span className="text-ink-soft text-[0.75rem]">(env: {entry.api_key_env})</span>
            )}
          </span>
        </Field>
        {entry.reasoning_effort && (
          <Field label="Reasoning effort">{entry.reasoning_effort}</Field>
        )}
        <Field label="Pricing /1M tokens" override={entry.pricing_override != null}>
          <div className="flex flex-col gap-0.5">
            <span className="inline-flex items-center gap-1.5">
              <RiMoneyDollarCircleLine className="text-ink-soft shrink-0" />
              <span className="text-ink-soft text-[0.75rem]">in</span>
              {formatPricePerMillion(entry.effective_pricing.input_per_1m_tokens)}
              <span className="text-ink-soft text-[0.75rem]">· out</span>
              {formatPricePerMillion(entry.effective_pricing.output_per_1m_tokens)}
            </span>
            {(entry.effective_pricing.cached_input_per_1m_tokens != null ||
              entry.effective_pricing.cache_write_per_1m_tokens != null) && (
              <span className="inline-flex items-center gap-1.5 pl-[1.4rem] text-[0.8rem] text-ink-soft">
                <span>cache</span>
                <span>
                  read{' '}
                  {entry.effective_pricing.cached_input_per_1m_tokens != null
                    ? formatPricePerMillion(entry.effective_pricing.cached_input_per_1m_tokens)
                    : '—'}
                </span>
                <span>·</span>
                <span>
                  write{' '}
                  {entry.effective_pricing.cache_write_per_1m_tokens != null
                    ? formatPricePerMillion(entry.effective_pricing.cache_write_per_1m_tokens)
                    : '—'}
                </span>
              </span>
            )}
          </div>
        </Field>
      </div>
    </div>
  );
}

function Field({
  label,
  override,
  children,
}: {
  label: string;
  override?: boolean;
  children: React.ReactNode;
}) {
  return (
    <div>
      <div className="flex items-center gap-1.5 font-bold uppercase tracking-wider text-[0.7rem] text-ink-soft mb-1">
        {label}
        {override && (
          <span
            title="Operator override active"
            className="px-1 py-px rounded border border-black text-[0.55rem] bg-yellow-50"
          >
            OVR
          </span>
        )}
      </div>
      <div className="font-mono text-[0.9rem] break-all">{children}</div>
    </div>
  );
}

function formatPricePerMillion(microUsd: number): string {
  const usd = microUsd / 1_000_000;
  if (usd === 0) return '—';
  return `$${usd.toFixed(usd >= 1 ? 2 : 4)}`;
}

// ── EditLlmModal ──────────────────────────────────────────────────────

interface EditableState {
  provider: string;
  model: string;
  baseUrl: string;
  apiKeyEnv: string;
  apiKey: string;
  reasoningEffort: string;
  // null = clear override, '' = keep default (no override)
  contextWindow: string;
  supportsVision: 'default' | 'true' | 'false';
  pricingInput: string;
  pricingOutput: string;
  pricingCached: string;
  pricingCacheWrite: string;
}

function entryToState(entry: LlmModelEntry): EditableState {
  return {
    provider: entry.provider,
    model: entry.model,
    baseUrl: entry.base_url ?? '',
    apiKeyEnv: entry.api_key_env ?? '',
    apiKey: '',
    reasoningEffort: entry.reasoning_effort ?? '',
    contextWindow:
      entry.context_window_override == null ? '' : String(entry.context_window_override),
    supportsVision:
      entry.supports_vision_override == null
        ? 'default'
        : entry.supports_vision_override
        ? 'true'
        : 'false',
    pricingInput: microToUsdInput(entry.pricing_override?.input_per_1m_tokens),
    pricingOutput: microToUsdInput(entry.pricing_override?.output_per_1m_tokens),
    pricingCached: microToUsdInput(entry.pricing_override?.cached_input_per_1m_tokens),
    pricingCacheWrite: microToUsdInput(entry.pricing_override?.cache_write_per_1m_tokens),
  };
}

function EditLlmModal({
  entry,
  onClose,
  onSave,
}: {
  entry: LlmModelEntry;
  onClose: () => void;
  onSave: (body: UpdateLlmModelRequest) => Promise<string | null>;
}) {
  const [form, setForm] = useState<EditableState>(() => entryToState(entry));
  const [showKey, setShowKey] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    setSubmitting(true);
    setError(null);

    // Build the diff body. For string fields we always send (it's
    // cheap and the backend re-validates). For Option<Option<T>>
    // toggles we send the explicit null when the operator cleared
    // the override, the value when set, and omit when unchanged.
    const body: UpdateLlmModelRequest = {
      provider: form.provider !== entry.provider ? form.provider : undefined,
      model: form.model !== entry.model ? form.model : undefined,
      base_url: form.baseUrl !== (entry.base_url ?? '') ? (form.baseUrl || null) : undefined,
      api_key_env:
        form.apiKeyEnv !== (entry.api_key_env ?? '') ? (form.apiKeyEnv || null) : undefined,
      reasoning_effort:
        form.reasoningEffort !== (entry.reasoning_effort ?? '')
          ? form.reasoningEffort || null
          : undefined,
      api_key: form.apiKey ? form.apiKey : undefined,
    };

    // supports_vision: 3-way toggle
    const currentVision: 'default' | 'true' | 'false' =
      entry.supports_vision_override == null
        ? 'default'
        : entry.supports_vision_override
        ? 'true'
        : 'false';
    if (form.supportsVision !== currentVision) {
      body.supports_vision =
        form.supportsVision === 'default' ? null : form.supportsVision === 'true';
    }

    // context_window
    const currentCtx =
      entry.context_window_override == null ? '' : String(entry.context_window_override);
    if (form.contextWindow !== currentCtx) {
      if (form.contextWindow === '') {
        body.context_window = null;
      } else {
        const parsed = Number.parseInt(form.contextWindow, 10);
        if (!Number.isFinite(parsed) || parsed <= 0) {
          setError('context_window must be a positive integer (or blank to clear)');
          setSubmitting(false);
          return;
        }
        body.context_window = parsed;
      }
    }

    // pricing — collapse into a single Option<LlmPricingOverride>.
    // Reject the form if any non-empty field doesn't parse, so the
    // operator never silently overwrites a valid override with NaN.
    const pricingFields: Array<[string, string]> = [
      ['Input / 1M', form.pricingInput],
      ['Output / 1M', form.pricingOutput],
      ['Cached read / 1M', form.pricingCached],
      ['Cache write / 1M', form.pricingCacheWrite],
    ];
    const parsed = pricingFields.map(([label, raw]) => [label, parseUsdInput(raw)] as const);
    const invalidLabels = parsed.filter(([, p]) => p.kind === 'invalid').map(([l]) => l);
    if (invalidLabels.length) {
      setError(
        `Invalid pricing (must be a non-negative number, or blank to clear): ${invalidLabels.join(', ')}`,
      );
      setSubmitting(false);
      return;
    }
    const microsOrNull = (p: PricingParse) => (p.kind === 'value' ? p.micros : null);
    const wantInput = microsOrNull(parsed[0][1]);
    const wantOutput = microsOrNull(parsed[1][1]);
    const wantCached = microsOrNull(parsed[2][1]);
    const wantCacheWrite = microsOrNull(parsed[3][1]);
    const cur = entry.pricing_override;
    const anyPricingSet =
      wantInput != null || wantOutput != null || wantCached != null || wantCacheWrite != null;
    const anyPricingChanged =
      wantInput !== (cur?.input_per_1m_tokens ?? null) ||
      wantOutput !== (cur?.output_per_1m_tokens ?? null) ||
      wantCached !== (cur?.cached_input_per_1m_tokens ?? null) ||
      wantCacheWrite !== (cur?.cache_write_per_1m_tokens ?? null);
    if (anyPricingChanged) {
      const next: LlmPricingOverrideDto | null = anyPricingSet
        ? {
            input_per_1m_tokens: wantInput ?? undefined,
            output_per_1m_tokens: wantOutput ?? undefined,
            cached_input_per_1m_tokens: wantCached ?? undefined,
            cache_write_per_1m_tokens: wantCacheWrite ?? undefined,
          }
        : null;
      body.pricing = next;
    }

    const err = await onSave(body);
    setSubmitting(false);
    if (err) setError(err);
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-6"
      role="dialog"
      aria-modal="true"
      onClick={onClose}
    >
      <form
        className="max-w-2xl w-full bg-white border-[3px] border-black rounded-md shadow-brutal overflow-hidden max-h-full flex flex-col"
        onClick={(e) => e.stopPropagation()}
        onSubmit={handleSubmit}
      >
        <header className="shrink-0 flex items-center justify-between gap-6 px-6 py-4 border-b-2 border-black">
          <h3 className="font-bold uppercase tracking-wider min-w-0 break-all">Edit LLM · {entry.name}</h3>
          <button
            type="button"
            onClick={onClose}
            className="shrink-0 text-[0.85rem] font-bold uppercase tracking-wider text-ink-soft hover:text-ink cursor-pointer"
          >
            Close
          </button>
        </header>

        <div className="px-6 py-4 space-y-5 overflow-y-auto min-h-0">
          <p className="text-[0.85rem] text-ink-soft bg-yellow-50 border-2 border-black px-3 py-2 rounded">
            Edits persist to <code>baybo.json</code> immediately. Most fields take effect only after a
            gateway restart — the active runtime keeps its current configuration until then.
          </p>

          <div className="grid grid-cols-2 gap-4">
            <LabeledInput
              label="Provider"
              value={form.provider}
              onChange={(v) => setForm({ ...form, provider: v })}
            />
            <LabeledInput
              label="Model id"
              value={form.model}
              onChange={(v) => setForm({ ...form, model: v })}
            />
            <LabeledInput
              label="Base URL"
              placeholder="(provider default)"
              value={form.baseUrl}
              onChange={(v) => setForm({ ...form, baseUrl: v })}
            />
            <LabeledInput
              label="API key env"
              placeholder="OPENAI_API_KEY"
              value={form.apiKeyEnv}
              onChange={(v) => setForm({ ...form, apiKeyEnv: v })}
            />
          </div>

          <div>
            <div className="flex items-center justify-between mb-1">
              <label className="block text-[0.7rem] font-bold uppercase text-ink-soft">
                API key (vault) — leave blank to keep
              </label>
              <button
                type="button"
                onClick={() => setShowKey((s) => !s)}
                className="text-[0.75rem] text-ink-soft hover:text-ink cursor-pointer inline-flex items-center gap-1"
              >
                {showKey ? <><RiEyeOffLine /> hide</> : <><RiEyeLine /> show</>}
              </button>
            </div>
            <input
              type={showKey ? 'text' : 'password'}
              autoComplete="off"
              value={form.apiKey}
              onChange={(e) => setForm({ ...form, apiKey: e.target.value })}
              placeholder="sk-…"
              className="w-full border-2 border-black rounded-md px-3 py-2 font-mono text-[0.9rem] focus:outline-none focus:shadow-brutal-xs"
            />
          </div>

          <div className="grid grid-cols-2 gap-4">
            <div>
              <label className="block text-[0.7rem] font-bold uppercase text-ink-soft mb-1">
                Context window override
              </label>
              <input
                type="number"
                min={1}
                placeholder="(use factory default)"
                value={form.contextWindow}
                onChange={(e) => setForm({ ...form, contextWindow: e.target.value })}
                className="w-full border-2 border-black rounded-md px-3 py-2 font-mono text-[0.9rem] focus:outline-none"
              />
            </div>
            <div>
              <label className="block text-[0.7rem] font-bold uppercase text-ink-soft mb-1">
                Supports vision
              </label>
              <SelectBox
                value={form.supportsVision}
                onChange={(e) =>
                  setForm({ ...form, supportsVision: e.target.value as 'default' | 'true' | 'false' })
                }
                className="w-full h-10 px-3"
              >
                <option value="default">Factory default</option>
                <option value="true">Force ON</option>
                <option value="false">Force OFF</option>
              </SelectBox>
            </div>
          </div>

          <div>
            <div className="font-bold uppercase tracking-wider text-[0.75rem] mb-2 flex items-center gap-2">
              <RiMoneyDollarCircleLine /> Pricing override (USD per 1M tokens)
            </div>
            <p className="text-[0.75rem] text-ink-soft mb-2">
              Leave a field blank to keep the factory / OpenRouter default. Clearing every field
              removes the override entirely.
            </p>
            <div className="grid grid-cols-2 gap-3">
              <LabeledInput
                label="Input / 1M"
                value={form.pricingInput}
                onChange={(v) => setForm({ ...form, pricingInput: v })}
                placeholder="2.50"
              />
              <LabeledInput
                label="Output / 1M"
                value={form.pricingOutput}
                onChange={(v) => setForm({ ...form, pricingOutput: v })}
                placeholder="10.00"
              />
              <LabeledInput
                label="Cached read / 1M"
                value={form.pricingCached}
                onChange={(v) => setForm({ ...form, pricingCached: v })}
                placeholder="(unset)"
              />
              <LabeledInput
                label="Cache write / 1M"
                value={form.pricingCacheWrite}
                onChange={(v) => setForm({ ...form, pricingCacheWrite: v })}
                placeholder="(unset)"
              />
            </div>
          </div>

          {entry.reasoning_effort != null && (
            <LabeledInput
              label="Reasoning effort"
              value={form.reasoningEffort}
              onChange={(v) => setForm({ ...form, reasoningEffort: v })}
              placeholder="none / minimal / low / medium / high / xhigh"
            />
          )}

          {error && (
            <div className="bg-white border-2 border-err text-err rounded-md px-3 py-2 font-mono text-[0.85rem] break-words">
              {error}
            </div>
          )}
        </div>

        <footer className="shrink-0 flex justify-end gap-2 px-6 py-3 border-t-2 border-black bg-canvas">
          <Button
            type="button"
            onClick={onClose}
            disabled={submitting}
            className="!py-1.5 !px-3 !text-[0.85rem] h-9"
          >
            Cancel
          </Button>
          <Button
            type="submit"
            variant="primary"
            disabled={submitting}
            className="!py-1.5 !px-3 !text-[0.85rem] h-9 gap-1.5"
          >
            {submitting && <RiLoader4Line className="animate-spin text-base shrink-0" />}
            Save changes
          </Button>
        </footer>
      </form>
    </div>
  );
}

function LabeledInput({
  label,
  value,
  onChange,
  placeholder,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
}) {
  return (
    <div>
      <label className="block text-[0.7rem] font-bold uppercase text-ink-soft mb-1">{label}</label>
      <input
        type="text"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        className="w-full border-2 border-black rounded-md px-3 py-2 font-mono text-[0.9rem] focus:outline-none"
      />
    </div>
  );
}
