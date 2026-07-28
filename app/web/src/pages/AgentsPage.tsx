import { useCallback, useEffect, useRef, useState } from 'react';
import {
  RiAddLine,
  RiDeleteBinLine,
  RiImageAddLine,
  RiLoader4Line,
  RiLockLine,
  RiRefreshLine,
} from 'react-icons/ri';

import { Button } from '../components/Button';
import { SelectBox } from '../components/SelectBox';
import { useAdminClient, useAuth } from '../api/auth';
import { useMockMode, MOCK_AGENT_PROFILES } from '../api/mock';
import type { components } from '../api/schema';
// 256² webp squeezed from assets/baybo.png — the builtin profile's default
// face when no avatar blob is set (avatar_blob_id stays NULL in the DB).
import bayboAvatar from '../assets/baybo-avatar.webp';

type AgentProfile = components['schemas']['AgentProfileDto'];
type AgentFramework = components['schemas']['AgentFrameworkDto'];
type SkillInfo = { name: string; description: string };

const FRAMEWORKS: { value: AgentFramework; label: string }[] = [
  { value: 'baybo', label: 'Baybo' },
  { value: 'claude', label: 'Claude Code' },
  { value: 'codex', label: 'Codex' },
];

const BAYBO_ONLY_HINT = 'baybo framework only — ignored by external frameworks';

// Mirrors `baybo_model::MAX_AGENT_PROFILE_NAME_CHARS` (the server cap).
const MAX_AGENT_NAME_CHARS = 64;

const fieldLabel = 'block text-[0.7rem] font-bold uppercase text-ink-soft mb-1';
const textInput =
  'w-full px-3 py-2 bg-white border border-black rounded-md font-mono text-sm outline-none focus:shadow-brutal-xs disabled:opacity-60 disabled:bg-canvas disabled:cursor-not-allowed';

// Deterministic per-agent portrait tint so avatar-less agents still read
// as distinct characters; the builtin always sits on brand gold.
const TILE_TINTS = ['bg-brand/60', 'bg-selected/60', 'bg-info/25', 'bg-ok/25', 'bg-warning/30'];

function tileTint(agent: Pick<AgentProfile, 'id' | 'builtin'>): string {
  if (agent.builtin) return TILE_TINTS[0];
  let h = 0;
  for (const c of agent.id) h = (h + c.charCodeAt(0)) % TILE_TINTS.length;
  return TILE_TINTS[h];
}

export function AgentsPage() {
  const client = useAdminClient();
  const { token, baseUrl, logout } = useAuth();
  const isMock = useMockMode();

  const [agents, setAgents] = useState<AgentProfile[]>([]);
  const [registeredSkills, setRegisteredSkills] = useState<SkillInfo[]>([]);
  const [llmNames, setLlmNames] = useState<string[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);
  // Sidebar selection: an agent id, or 'new' for the create form.
  const [selected, setSelected] = useState<string | 'new' | null>(null);
  const [pendingDelete, setPendingDelete] = useState<AgentProfile | null>(null);
  const [mutationError, setMutationError] = useState<string | null>(null);
  const [mutating, setMutating] = useState(false);

  useEffect(() => {
    let canceled = false;
    async function fetchData() {
      if (isMock) {
        setAgents(MOCK_AGENT_PROFILES);
        setRegisteredSkills([
          { name: 'brainstorm', description: 'Generate and expand ideas quickly.' },
          { name: 'commit-helper', description: 'Draft conventional-commit messages from a diff.' },
          { name: 'weekly-report', description: "Summarize the week's work into a report." },
        ]);
        setLlmNames(['primary', 'fast']);
        setLoading(false);
        setError(null);
        return;
      }
      setLoading(true);
      setError(null);
      try {
        const [agentsRes, skillsRes, llmRes] = await Promise.all([
          client.GET('/v1/agents'),
          client.GET('/v1/skills'),
          client.GET('/v1/llm/models'),
        ]);
        if (canceled) return;
        for (const r of [agentsRes, skillsRes, llmRes]) {
          if (r.response.status === 401) {
            logout();
            return;
          }
        }
        if (agentsRes.error || !agentsRes.response.ok) {
          setError(agentsRes.error?.error || `HTTP Error ${agentsRes.response.status}`);
          return;
        }
        setAgents(agentsRes.data?.items ?? []);
        setRegisteredSkills(
          (skillsRes.data?.items ?? []).slice().sort((a, b) => a.name.localeCompare(b.name)),
        );
        setLlmNames((llmRes.data?.items ?? []).map((m) => m.name));
        // A dead picker source must not masquerade as "nothing registered".
        const failed: string[] = [];
        if (skillsRes.error || !skillsRes.response.ok) failed.push('skills');
        if (llmRes.error || !llmRes.response.ok) failed.push('models');
        if (failed.length > 0) {
          setError(`Failed to load ${failed.join(', ')} for the editor — retry Refresh.`);
        }
      } catch (e) {
        if (canceled) return;
        setError(e instanceof Error ? `Network error: ${e.message}` : 'Network error contacting gateway');
      } finally {
        if (!canceled) setLoading(false);
      }
    }
    void fetchData();
    return () => {
      canceled = true;
    };
  }, [client, logout, refreshKey, isMock]);

  // Holds a just-created agent id whose row hasn't landed in `agents` yet,
  // so the selection-honesty effect below doesn't yank the detail pane back
  // to the builtin during the post-create refresh.
  const pendingSelectRef = useRef<string | null>(null);

  // Keep the selection honest against the loaded list: default to the first
  // agent (the builtin sorts first), and recover when the selected row
  // disappears (deleted elsewhere).
  useEffect(() => {
    if (selected === 'new') return;
    if (agents.length === 0) return;
    if (selected !== null && agents.some((a) => a.id === selected)) {
      pendingSelectRef.current = null; // selection resolved to a real row
      return;
    }
    // `selected` is null or not (yet) in the list. If it's a just-created id
    // still in flight, wait for the refresh instead of resetting.
    if (selected !== null && selected === pendingSelectRef.current) return;
    setSelected(agents[0].id);
  }, [agents, selected]);

  const refresh = useCallback(() => setRefreshKey((k) => k + 1), []);

  const handleDelete = useCallback(
    async (agent: AgentProfile) => {
      if (isMock) {
        setMutationError('Delete disabled in mock mode.');
        return;
      }
      setMutating(true);
      setMutationError(null);
      try {
        const { error: apiError, response } = await client.DELETE('/v1/agents/{agent_id}', {
          params: { path: { agent_id: agent.id } },
        });
        if (response.status === 401) {
          logout();
          return;
        }
        if (apiError || !response.ok) {
          setMutationError(apiError?.error || `HTTP Error ${response.status}`);
          return;
        }
        setPendingDelete(null);
        setSelected(null); // the list effect re-selects the first row
        refresh();
      } catch (e) {
        setMutationError(e instanceof Error ? e.message : 'Network error contacting gateway');
      } finally {
        setMutating(false);
      }
    },
    [client, isMock, logout, refresh],
  );

  const selectedAgent =
    selected !== null && selected !== 'new' ? agents.find((a) => a.id === selected) ?? null : null;

  if (loading && agents.length === 0) {
    return (
      <div className="p-8 flex flex-col items-center justify-center h-full">
        <RiLoader4Line className="animate-spin text-4xl text-ink-soft mb-3" />
        <p className="text-ink-soft font-bold uppercase tracking-wider">Loading agents…</p>
      </div>
    );
  }

  return (
    <div className="h-full flex overflow-hidden">
      {/* ── agents sidebar ── */}
      <aside className="w-64 shrink-0 border-r-2 border-black bg-canvas flex flex-col">
        <div className="px-3 py-3 border-b-2 border-black flex items-center gap-2">
          <button
            type="button"
            onClick={() => {
              setMutationError(null);
              setSelected('new');
            }}
            className="flex-1 flex items-center justify-center gap-2 px-3 py-2 bg-brand text-ink border-2 border-black rounded-md shadow-brutal-sm font-bold uppercase tracking-wider text-[0.85rem] hover:bg-brand-hover active:translate-x-[2px] active:translate-y-[2px] active:shadow-none cursor-pointer"
          >
            <RiAddLine className="text-lg" /> New agent
          </button>
          <button
            type="button"
            onClick={refresh}
            title="Refresh"
            aria-label="Refresh agents"
            className="shrink-0 flex items-center justify-center h-9 w-9 border-2 border-black rounded-md shadow-brutal-xs bg-white text-ink hover:bg-canvas active:translate-x-[1px] active:translate-y-[1px] active:shadow-none cursor-pointer"
          >
            <RiRefreshLine />
          </button>
        </div>
        <nav className="flex-1 overflow-y-auto overscroll-none px-2 py-2 flex flex-col gap-1">
          {agents.map((agent) => (
            <AgentRow
              key={agent.id}
              agent={agent}
              active={selected === agent.id}
              baseUrl={baseUrl}
              token={token}
              onSelect={() => {
                setMutationError(null);
                setSelected(agent.id);
              }}
            />
          ))}
        </nav>
      </aside>

      {/* ── detail ── */}
      <main className="flex-1 min-w-0 flex flex-col overflow-hidden bg-surface">
        {error && (
          <div className="m-4 mb-0 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
            {error}
          </div>
        )}
        {selected === 'new' ? (
          <AgentEditorPanel
            key="new"
            agent={null}
            registeredSkills={registeredSkills}
            llmNames={llmNames}
            onSaved={(createdId) => {
              if (createdId) {
                pendingSelectRef.current = createdId;
                setSelected(createdId);
              }
              refresh();
            }}
          />
        ) : selectedAgent ? (
          <AgentEditorPanel
            key={selectedAgent.id}
            agent={selectedAgent}
            registeredSkills={registeredSkills}
            llmNames={llmNames}
            onSaved={() => refresh()}
            onDelete={
              selectedAgent.builtin
                ? undefined
                : () => {
                    setMutationError(null);
                    setPendingDelete(selectedAgent);
                  }
            }
          />
        ) : (
          <div className="flex-1 flex items-center justify-center text-ink-soft font-bold uppercase tracking-wider">
            No agent selected
          </div>
        )}
      </main>

      {pendingDelete && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-6"
          role="dialog"
          aria-modal="true"
          onClick={() => setPendingDelete(null)}
        >
          <div
            className="max-w-md w-full bg-white border-[3px] border-black rounded-md shadow-brutal overflow-hidden"
            onClick={(e) => e.stopPropagation()}
          >
            <header className="px-6 py-4 border-b-2 border-black">
              <h3 className="font-bold uppercase tracking-wider">Delete agent</h3>
            </header>
            <div className="px-6 py-4 space-y-3">
              <p className="text-sm break-words">
                Delete <span className="font-bold">{pendingDelete.name}</span>? This cannot be
                undone.
              </p>
              {mutationError && (
                <p className="text-err font-mono text-sm border-2 border-err rounded-md px-3 py-2 break-words">
                  {mutationError}
                </p>
              )}
            </div>
            <footer className="flex justify-end gap-2 px-6 py-3 border-t-2 border-black bg-canvas">
              <Button onClick={() => setPendingDelete(null)}>Cancel</Button>
              <Button
                className="!bg-err !text-white !border-err hover:!bg-err/90"
                disabled={isMock || mutating}
                onClick={() => void handleDelete(pendingDelete)}
              >
                <RiDeleteBinLine /> Delete
              </Button>
            </footer>
          </div>
        </div>
      )}
    </div>
  );
}

// ── sidebar row ─────────────────────────────────────────────────────

function AgentRow({
  agent,
  active,
  baseUrl,
  token,
  onSelect,
}: {
  agent: AgentProfile;
  active: boolean;
  baseUrl: string;
  token: string | null;
  onSelect: () => void;
}) {
  const framework = FRAMEWORKS.find((f) => f.value === agent.framework)?.label ?? agent.framework;
  const subtitle = agent.llm ? `${framework} · ${agent.llm}` : framework;
  return (
    <button
      type="button"
      onClick={onSelect}
      title={agent.name}
      className={`group flex w-full items-center gap-2.5 px-2 py-1.5 text-left rounded-md border-2 cursor-pointer ${
        active
          ? 'bg-selected text-ink border-black shadow-brutal-sm'
          : 'border-transparent hover:bg-gray-100'
      }`}
    >
      <AgentFace agent={agent} baseUrl={baseUrl} token={token} size="sm" />
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-1">
          <span className={`truncate text-sm text-ink ${active ? 'font-bold' : ''}`}>
            {agent.name}
          </span>
          {agent.builtin && (
            <RiLockLine
              className={`shrink-0 text-[0.7rem] ${active ? 'text-ink/70' : 'text-ink-soft'}`}
              title="Built-in"
            />
          )}
        </div>
        <div className={`text-[0.68rem] truncate ${active ? 'text-ink/70' : 'text-ink-soft'}`}>
          {subtitle}
        </div>
      </div>
    </button>
  );
}

// ── avatar ──────────────────────────────────────────────────────────

// `<img>` can't carry the Authorization header, so fetch the blob and hand
// the bitmap over as an object URL (same pattern as the chat
// AttachmentImage). Returns null while loading, on failure, or without a
// blob id — callers fall back to their default portrait.
function useBlobUrl(blobId: string | null, baseUrl: string, token: string | null): string | null {
  const [url, setUrl] = useState<string | null>(null);
  useEffect(() => {
    setUrl(null);
    if (!blobId) return;
    let cancelled = false;
    let objectUrl: string | null = null;
    void (async () => {
      try {
        const base = (baseUrl || '').replace(/\/+$/, '');
        const res = await fetch(`${base}/v1/blobs/${encodeURIComponent(blobId)}`, {
          headers: { Authorization: `Bearer ${token ?? ''}` },
        });
        if (!res.ok) throw new Error(`blob ${res.status}`);
        const blob = await res.blob();
        if (cancelled) return;
        objectUrl = URL.createObjectURL(blob);
        setUrl(objectUrl);
      } catch {
        // Callers render their fallback.
      }
    })();
    return () => {
      cancelled = true;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [blobId, baseUrl, token]);
  return url;
}

// A character face on its tint tile: uploaded avatar > bundled brand image
// (builtin) > big monogram.
function AgentFace({
  agent,
  baseUrl,
  token,
  size,
  previewUrl,
}: {
  agent: Pick<AgentProfile, 'id' | 'name' | 'builtin'> & { avatar_blob_id?: string | null };
  baseUrl: string;
  token: string | null;
  size: 'sm' | 'lg';
  /** Local object URL that overrides the stored blob (fresh upload). */
  previewUrl?: string | null;
}) {
  const url = useBlobUrl(agent.avatar_blob_id ?? null, baseUrl, token);
  const face = previewUrl ?? url ?? (agent.builtin ? bayboAvatar : null);
  const frame =
    size === 'sm'
      ? 'h-9 w-9 rounded-md border border-black'
      : 'h-32 w-32 rounded-md border-2 border-black shadow-brutal-sm';
  if (face) {
    return <img src={face} alt={agent.name} className={`${frame} shrink-0 object-cover`} />;
  }
  return (
    <div
      className={`${frame} shrink-0 flex items-center justify-center font-bold uppercase select-none ${
        size === 'sm' ? 'text-lg' : 'text-5xl'
      } ${tileTint(agent)}`}
    >
      {agent.name.slice(0, 1) || '?'}
    </div>
  );
}

// ── detail / editor panel ───────────────────────────────────────────

function AgentEditorPanel({
  agent,
  registeredSkills,
  llmNames,
  onSaved,
  onDelete,
}: {
  agent: AgentProfile | null; // null = create
  registeredSkills: SkillInfo[];
  llmNames: string[];
  /** Called after a successful save; carries the new id on create. */
  onSaved: (createdId?: string) => void;
  /** Present only for existing non-builtin agents; hands off to the confirm dialog. */
  onDelete?: () => void;
}) {
  const client = useAdminClient();
  const { token, baseUrl, logout } = useAuth();
  const isMock = useMockMode();
  const isBuiltin = agent?.builtin ?? false;

  const [name, setName] = useState(agent?.name ?? '');
  const [description, setDescription] = useState(agent?.description ?? '');
  const [framework, setFramework] = useState<AgentFramework>(agent?.framework ?? 'baybo');
  const [systemPrompt, setSystemPrompt] = useState(agent?.system_prompt ?? '');
  const [llm, setLlm] = useState(agent?.llm ?? '');
  const [avatarBlobId, setAvatarBlobId] = useState<string | null>(agent?.avatar_blob_id ?? null);
  const [avatarPreview, setAvatarPreview] = useState<string | null>(null);
  const [uploadingAvatar, setUploadingAvatar] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [savedFlash, setSavedFlash] = useState(false);

  const externalFramework = framework !== 'baybo';
  const contentLocked = isBuiltin;
  // The avatar is the builtin's only writable field, so its Save is live
  // only once the avatar actually differs from the stored one.
  const avatarChanged = avatarBlobId !== (agent?.avatar_blob_id ?? null);

  // The old preview URL is revoked whenever a new one replaces it (the
  // cleanup sees the previous value) and on unmount.
  useEffect(() => {
    return () => {
      if (avatarPreview) URL.revokeObjectURL(avatarPreview);
    };
  }, [avatarPreview]);

  const uploadAvatar = useCallback(
    async (file: File) => {
      if (isMock) {
        setSaveError('Avatar upload disabled in mock mode.');
        return;
      }
      setUploadingAvatar(true);
      setSaveError(null);
      try {
        const base = (baseUrl || '').replace(/\/+$/, '');
        const res = await fetch(`${base}/v1/blobs`, {
          method: 'POST',
          headers: {
            Authorization: `Bearer ${token ?? ''}`,
            'content-type': file.type || 'application/octet-stream',
          },
          body: file,
        });
        if (!res.ok) throw new Error(`upload failed: ${res.status}`);
        const data = (await res.json()) as { blob_id: string };
        setAvatarBlobId(data.blob_id);
        setAvatarPreview(URL.createObjectURL(file));
      } catch (e) {
        setSaveError(e instanceof Error ? e.message : 'avatar upload failed');
      } finally {
        setUploadingAvatar(false);
      }
    },
    [baseUrl, isMock, token],
  );

  const save = useCallback(async () => {
    if (isMock) {
      setSaveError('Saving disabled in mock mode.');
      return;
    }
    setSaving(true);
    setSaveError(null);
    try {
      const content = {
        name,
        description,
        framework,
        system_prompt: systemPrompt.trim() === '' ? null : systemPrompt,
        llm: llm === '' ? null : llm,
      };
      if (agent === null) {
        const { data, error: apiError, response } = await client.POST('/v1/agents', {
          body: { ...content, avatar_blob_id: avatarBlobId },
        });
        if (response.status === 401) return logout();
        if (apiError || !response.ok) {
          setSaveError(apiError?.error || `HTTP Error ${response.status}`);
          return;
        }
        onSaved(data?.id);
        return;
      }
      if (!isBuiltin) {
        const { error: apiError, response } = await client.PUT('/v1/agents/{agent_id}', {
          params: { path: { agent_id: agent.id } },
          body: content,
        });
        if (response.status === 401) return logout();
        if (apiError || !response.ok) {
          setSaveError(apiError?.error || `HTTP Error ${response.status}`);
          return;
        }
      }
      if (avatarChanged) {
        const { error: apiError, response } = await client.PUT('/v1/agents/{agent_id}/avatar', {
          params: { path: { agent_id: agent.id } },
          body: { blob_id: avatarBlobId },
        });
        if (response.status === 401) return logout();
        if (apiError || !response.ok) {
          setSaveError(apiError?.error || `HTTP Error ${response.status}`);
          return;
        }
      }
      setSavedFlash(true);
      setTimeout(() => setSavedFlash(false), 1500);
      onSaved();
    } catch (e) {
      setSaveError(e instanceof Error ? e.message : 'Network error contacting gateway');
    } finally {
      setSaving(false);
    }
  }, [
    agent,
    avatarBlobId,
    avatarChanged,
    client,
    description,
    framework,
    isBuiltin,
    isMock,
    llm,
    logout,
    name,
    onSaved,
    systemPrompt,
  ]);

  return (
    <div className="flex-1 flex flex-col overflow-hidden">
      <div className="flex-1 overflow-y-auto overscroll-none">
        <div className="mx-auto max-w-4xl px-6 py-6 space-y-5">
          {/* ── header: portrait + identity ── */}
          <div className="flex items-center gap-5">
            <div className="flex flex-col items-center gap-2 shrink-0">
              <AgentFace
                agent={{
                  id: agent?.id ?? 'new',
                  name: name || '?',
                  builtin: isBuiltin,
                  avatar_blob_id: avatarBlobId,
                }}
                baseUrl={baseUrl}
                token={token}
                size="lg"
                previewUrl={avatarPreview}
              />
              <div className="flex items-center gap-2">
                <label className="cursor-pointer" title="Choose image">
                  <span className="inline-flex items-center gap-1.5 px-2 py-1 font-bold text-[0.7rem] uppercase border border-black rounded-md shadow-brutal-xs bg-white hover:bg-canvas">
                    {uploadingAvatar ? (
                      <RiLoader4Line className="animate-spin" />
                    ) : (
                      <RiImageAddLine />
                    )}
                    Image
                  </span>
                  <input
                    type="file"
                    accept="image/*"
                    className="hidden"
                    disabled={uploadingAvatar}
                    onChange={(e) => {
                      const file = e.target.files?.[0];
                      if (file) void uploadAvatar(file);
                      e.target.value = '';
                    }}
                  />
                </label>
                {avatarBlobId && (
                  <button
                    type="button"
                    className="text-[0.7rem] font-bold uppercase text-ink-soft underline cursor-pointer"
                    onClick={() => {
                      setAvatarBlobId(null);
                      setAvatarPreview(null);
                    }}
                  >
                    Remove
                  </button>
                )}
              </div>
            </div>
            <div className="flex-1 min-w-0 space-y-3">
              <div>
                <label className={fieldLabel}>Name</label>
                <input
                  className={`${textInput} !text-base font-bold`}
                  value={name}
                  maxLength={MAX_AGENT_NAME_CHARS}
                  disabled={contentLocked}
                  onChange={(e) => setName(e.target.value)}
                  placeholder="e.g. Code Reviewer"
                />
                {/* Always rendered with a fixed min-height so switching
                    between the builtin (badge) and a custom agent (id only)
                    or the create form (empty) never changes the header
                    height — no layout jump. */}
                <div className="min-h-6 flex flex-wrap items-center gap-2 mt-1.5">
                  {isBuiltin && (
                    <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded-md text-[0.65rem] font-bold uppercase border border-black bg-white">
                      <RiLockLine /> built-in · read-only except avatar
                    </span>
                  )}
                  {agent && (
                    <span className="text-[0.65rem] text-ink-soft font-mono truncate">
                      id: {agent.id}
                    </span>
                  )}
                </div>
              </div>
              <div>
                <label className={fieldLabel}>Description</label>
                <textarea
                  className={`${textInput} resize-y`}
                  rows={2}
                  value={description}
                  disabled={contentLocked}
                  onChange={(e) => setDescription(e.target.value)}
                  placeholder="What does this agent do?"
                />
              </div>
            </div>
          </div>

          {/* ── behavior ── */}
          <div className="flex gap-4">
            <div className="flex-1">
              <label className={fieldLabel}>Framework</label>
              <SelectBox
                className={`w-full h-10 !border ${contentLocked ? 'opacity-60' : ''}`}
                value={framework}
                disabled={contentLocked}
                onChange={(e) => setFramework(e.target.value as AgentFramework)}
              >
                {FRAMEWORKS.map((f) => (
                  <option key={f.value} value={f.value}>
                    {f.label}
                  </option>
                ))}
              </SelectBox>
            </div>
            <div className="flex-1" title={externalFramework ? BAYBO_ONLY_HINT : undefined}>
              <label className={fieldLabel}>
                Model {externalFramework && <span className="normal-case">(baybo only)</span>}
              </label>
              <SelectBox
                className={`w-full h-10 !border ${contentLocked || externalFramework ? 'opacity-60' : ''}`}
                value={llm ?? ''}
                disabled={contentLocked || externalFramework}
                onChange={(e) => setLlm(e.target.value)}
              >
                <option value="">Default model</option>
                {llmNames.map((n) => (
                  <option key={n} value={n}>
                    {n}
                  </option>
                ))}
                {/* A stale pin (entry removed from baybo.json) must stay
                    visible and deliberately clearable — a hidden stale value
                    would 400 on save while the form looks like "Default". */}
                {llm !== '' && !llmNames.includes(llm) && (
                  <option value={llm}>{llm} (unavailable)</option>
                )}
              </SelectBox>
            </div>
          </div>

          {/* ── system prompt ── */}
          <div>
            <label className={fieldLabel}>
              System prompt{' '}
              <span className="normal-case">(empty = workspace Soul default)</span>
            </label>
            <textarea
              className={`${textInput} resize-y`}
              rows={10}
              value={systemPrompt}
              disabled={contentLocked}
              onChange={(e) => setSystemPrompt(e.target.value)}
              placeholder="Markdown system prompt replacing the workspace Soul…"
            />
          </div>

          {/* ── skills (read-only, live from the registry, full width) ── */}
          <SkillsDisplay registered={registeredSkills} />

          {/* actions pinned to the bottom of the card, right-aligned; the
              card content fades out behind them as it scrolls. */}
          <div className="sticky bottom-0 -mx-6 -mb-6 flex items-center justify-end gap-2 px-6 pt-4 pb-5 bg-gradient-to-t from-surface via-surface to-transparent">
            {saveError && (
              <p className="mr-auto max-w-md text-err font-mono text-xs border border-err bg-white rounded-md px-3 py-2 break-words">
                {saveError}
              </p>
            )}
            {savedFlash && (
              <span className="text-[0.75rem] font-bold uppercase text-ok">Saved</span>
            )}
            {onDelete && (
              <button
                type="button"
                onClick={onDelete}
                title="Delete agent"
                aria-label="Delete agent"
                className="flex items-center justify-center h-9 w-9 rounded-md border border-black bg-white text-ink-soft hover:text-err hover:border-err shadow-brutal-xs cursor-pointer"
              >
                <RiDeleteBinLine />
              </button>
            )}
            <Button
              variant="primary"
              className="!px-4 !py-1.5 !text-[0.85rem]"
              disabled={
                isMock ||
                saving ||
                uploadingAvatar ||
                (agent === null && name.trim() === '') ||
                (isBuiltin && !avatarChanged)
              }
              onClick={() => void save()}
            >
              {saving ? <RiLoader4Line className="animate-spin" /> : null}
              {agent === null ? 'Create' : 'Save'}
            </Button>
          </div>
        </div>
      </div>
    </div>
  );
}

// Read-only view of the skills an agent has, each with its description.
// Not configured here: `null` (the v1 default) means the agent inherits
// every registered skill, so we list the live registry; a stored list
// resolves each name to its registry blurb.
function SkillsDisplay({ registered }: { registered: SkillInfo[] }) {
  return (
    <div>
      <label className={fieldLabel}>
        Skills{' '}
        <span className="normal-case text-ink-soft">
          (managed by the skill system — read-only)
        </span>
      </label>
      {registered.length === 0 ? (
        <p className="text-ink-soft text-sm font-mono">No skills registered.</p>
      ) : (
        <div className="border border-black rounded-md divide-y divide-black overflow-hidden">
          {registered.map((s) => (
            <div key={s.name} className="px-3 py-2 flex flex-wrap items-baseline gap-x-3">
              <span className="font-mono text-sm font-bold shrink-0">{s.name}</span>
              {s.description && (
                <span className="text-ink-soft text-xs leading-snug">{s.description}</span>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
