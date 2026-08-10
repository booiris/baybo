import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
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
import { useBlobUrl } from '../api/blobs';
import { botttsFace } from '../components/botttsFace';
import { useMockMode, MOCK_AGENT_PROFILES } from '../api/mock';
import type { components } from '../api/schema';
// 256² webp squeezed from assets/baybo.png — the builtin profile's default
// face when no avatar blob is set (avatar_blob_id stays NULL in the DB).
import bayboAvatar from '../assets/baybo-avatar.webp';

type AgentProfile = components['schemas']['AgentProfileDto'];
type AgentFramework = components['schemas']['AgentFrameworkDto'];
// `GET /v1/skills` inlines its item schema, so this mirrors it by hand.
// `universal` marks a skill every agent has regardless of persona.
type SkillInfo = { name: string; description: string; universal: boolean };

const FRAMEWORKS: { value: AgentFramework; label: string }[] = [
  { value: 'baybo', label: 'Baybo' },
  { value: 'claude', label: 'Claude Code' },
  { value: 'codex', label: 'Codex' },
];

const BAYBO_ONLY_HINT = 'baybo framework only — ignored by external frameworks';
const BUILTIN_FOLLOWS_DEFAULT_HINT =
  'the built-in agent always follows default-llm — change it on the LLM page';

// Mirrors `baybo_model::MAX_AGENT_PROFILE_NAME_CHARS` (the server cap).
const MAX_AGENT_NAME_CHARS = 64;

const fieldLabel = 'block text-[0.7rem] font-bold uppercase text-ink-soft mb-1';
const textInput =
  'w-full px-3 py-2 bg-white border border-black rounded-md font-mono text-sm outline-none focus:shadow-brutal-xs disabled:opacity-60 disabled:bg-canvas disabled:cursor-not-allowed';

// The tile behind the monogram, which only the create form still shows:
// every saved agent has an id, and an id is all a generated face needs.
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
  // The shared listing, kept only to seed the create form: a not-yet-created
  // agent has no id to scope by, and what it will start with is exactly the
  // universal subset of this.
  const [registeredSkills, setRegisteredSkills] = useState<SkillInfo[]>([]);
  const [llmNames, setLlmNames] = useState<string[]>([]);
  // Which entry `default-llm` currently points at, so the unpinned option can
  // name it instead of saying "Default model" and leaving the operator to go
  // look it up.
  const [defaultLlmName, setDefaultLlmName] = useState('');
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refreshKey, setRefreshKey] = useState(0);
  // What a not-yet-created agent will start with: the universal subset of
  // the shared listing, computed here so the panel needs no hard-coded names.
  const universalSkills = useMemo(
    () => registeredSkills.filter((s) => s.universal),
    [registeredSkills],
  );
  // Sidebar selection: an agent id, or 'new' for the create form.
  const [selected, setSelected] = useState<string | 'new' | null>(null);
  const [pendingDelete, setPendingDelete] = useState<AgentProfile | null>(null);
  const [mutationError, setMutationError] = useState<string | null>(null);
  const [mutating, setMutating] = useState(false);

  useEffect(() => {
    const alive = { current: true };
    async function fetchData() {
      if (isMock) {
        setAgents(MOCK_AGENT_PROFILES);
        setRegisteredSkills([
          {
            name: 'baybo-cli',
            description: 'Introspect the running instance through the baybo CLI.',
            universal: true,
          },
          {
            name: 'brainstorm',
            description: 'Generate and expand ideas quickly.',
            universal: false,
          },
          {
            name: 'commit-helper',
            description: 'Draft conventional-commit messages from a diff.',
            universal: false,
          },
          {
            name: 'weekly-report',
            description: "Summarize the week's work into a report.",
            universal: false,
          },
        ]);
        setLlmNames(['primary', 'fast']);
        setDefaultLlmName('primary');
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
        if (!alive.current) return;
        for (const r of [agentsRes, skillsRes, llmRes]) {
          if (r.response.status === 401) {
            logout();
            return;
          }
        }
        if (agentsRes.error || !agentsRes.response.ok) {
          setError(agentsRes.error?.error ?? `HTTP Error ${agentsRes.response.status}`);
          return;
        }
        setAgents(agentsRes.data.items);
        setRegisteredSkills(
          (skillsRes.data?.items ?? []).slice().sort((a, b) => a.name.localeCompare(b.name)),
        );
        setLlmNames((llmRes.data?.items ?? []).map((m) => m.name));
        setDefaultLlmName(llmRes.data?.default_name ?? '');
        // A dead picker source must not masquerade as "nothing registered".
        const failed: string[] = [];
        if (skillsRes.error || !skillsRes.response.ok) failed.push('skills');
        if (llmRes.error || !llmRes.response.ok) failed.push('models');
        if (failed.length > 0) {
          setError(`Failed to load ${failed.join(', ')} for the editor — retry Refresh.`);
        }
      } catch (e) {
        if (!alive.current) return;
        setError(e instanceof Error ? `Network error: ${e.message}` : 'Network error contacting gateway');
      } finally {
        if (alive.current) setLoading(false);
      }
    }
    void fetchData();
    return () => {
      alive.current = false;
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
          setMutationError(apiError?.error ?? `HTTP Error ${response.status}`);
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
        {error !== null && (
          <div className="m-4 mb-0 bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
            {error}
          </div>
        )}
        {selected === 'new' ? (
          <AgentEditorPanel
            key="new"
            agent={null}
            llmNames={llmNames}
            defaultLlmName={defaultLlmName}
            universalSkills={universalSkills}
            onSaved={(createdId) => {
              if (createdId !== undefined) {
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
            llmNames={llmNames}
            defaultLlmName={defaultLlmName}
            universalSkills={universalSkills}
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
            className="max-w-md w-full bg-white border-[3px] border-black rounded-md shadow-brutal overflow-hidden max-h-full flex flex-col"
            onClick={(e) => e.stopPropagation()}
          >
            <header className="shrink-0 px-6 py-4 border-b-2 border-black">
              <h3 className="font-bold uppercase tracking-wider">Delete agent</h3>
            </header>
            <div className="px-6 py-4 space-y-3 overflow-y-auto min-h-0">
              <p className="text-sm break-words">
                Delete <span className="font-bold">{pendingDelete.name}</span>? This cannot be
                undone.
              </p>
              {mutationError !== null && (
                <p className="text-err font-mono text-sm border-2 border-err rounded-md px-3 py-2 break-words">
                  {mutationError}
                </p>
              )}
            </div>
            <footer className="shrink-0 flex justify-end gap-2 px-6 py-3 border-t-2 border-black bg-canvas">
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
  const subtitle =
    agent.llm !== undefined && agent.llm !== '' ? `${framework} · ${agent.llm}` : framework;
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

// Uploaded avatar > the bundled brand image (builtin) > a face generated
// from the agent's id. The monogram is left for the create form alone, where
// there is no id yet to generate from — and picking a face off the typed name
// would show one portrait while filling the form and a different one after
// saving.
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
  const url = useBlobUrl(agent.avatar_blob_id, baseUrl, token);
  const generated = agent.id === '' ? null : botttsFace(agent.id);
  const face = previewUrl ?? url ?? (agent.builtin ? bayboAvatar : generated);
  const frame =
    size === 'sm'
      ? 'h-9 w-9 rounded-md border border-black'
      : 'h-32 w-32 rounded-md border border-black shadow-brutal-sm';
  if (face !== null) {
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
  llmNames,
  defaultLlmName,
  universalSkills,
  onSaved,
  onDelete,
}: {
  agent: AgentProfile | null; // null = create
  llmNames: string[];
  defaultLlmName: string;
  universalSkills: SkillInfo[];
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
  // The soul is a file the agent owns (`personas/<id>/SOUL.md`), not a
  // profile field, so it loads and saves on its own endpoint; its path is
  // shown because that file lives in a git repo the operator may want to
  // commit. `IDENTITY.md` is deliberately not surfaced — the only thing this
  // page wants from it is the name, which has its own field.
  //
  // The page never polls or subscribes, so what it shows can be stale — the
  // agent rewrites this file mid-conversation. `version` is what keeps that
  // safe: it rides back on Save, and the server refuses a write whose base
  // has moved rather than deleting the agent's own edit.
  const [soul, setSoul] = useState('');
  const [soulPath, setSoulPath] = useState<string | null>(null);
  const [soulVersion, setSoulVersion] = useState<string | null>(null);
  // A custom agent does not inherit the workspace's skills, so the readout
  // has to be its scope — the page-wide list would be a different agent's.
  const [scopedSkills, setScopedSkills] = useState<SkillInfo[]>(
    universalSkills,
  );
  const [filesLoaded, setFilesLoaded] = useState(false);
  const [soulDirty, setSoulDirty] = useState(false);
  const [llm, setLlm] = useState(agent?.llm ?? '');
  const [avatarBlobId, setAvatarBlobId] = useState<string | null>(agent?.avatar_blob_id ?? null);
  const [avatarPreview, setAvatarPreview] = useState<string | null>(null);
  const [uploadingAvatar, setUploadingAvatar] = useState(false);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [savedFlash, setSavedFlash] = useState(false);

  const externalFramework = framework !== 'baybo';
  // Two things about the builtin are fixed by what it *is*: it runs on baybo,
  // and it follows `default-llm` (pinning a model here would duplicate that
  // setting). Name, description, soul and avatar are ordinary content.
  const builtinPinned = isBuiltin;
  const avatarChanged = avatarBlobId !== (agent?.avatar_blob_id ?? null);

  // The old preview URL is revoked whenever a new one replaces it (the
  // cleanup sees the previous value) and on unmount.
  useEffect(() => {
    return () => {
      if (avatarPreview !== null) URL.revokeObjectURL(avatarPreview);
    };
  }, [avatarPreview]);

  // Load the selected agent's soul from its own file. The panel is keyed by
  // selection, so this runs once per agent; a create form starts empty and
  // lets the server seed the template.
  useEffect(() => {
    if (agent === null) {
      setScopedSkills(universalSkills);
      setFilesLoaded(true);
      return;
    }
    if (isMock) {
      setSoul(`# ${agent.name}\n\n${agent.description}`);
      setSoulPath(`personas/${agent.id}/SOUL.md`);
      setFilesLoaded(true);
      return;
    }
    const alive = { current: true };
    void (async () => {
      const { data, error: apiError, response } = await client.GET('/v1/agents/{agent_id}/soul', {
        params: { path: { agent_id: agent.id } },
      });
      if (!alive.current) return;
      if (response.status === 401) return logout();
      if (!response.ok || apiError !== undefined) {
        setSaveError(apiError?.error ?? `HTTP Error ${response.status}`);
        return;
      }
      setSoul(data.content);
      setSoulPath(data.path);
      setSoulVersion(data.version);
      setSoulDirty(false);
      setFilesLoaded(true);

      const skills = await client.GET('/v1/skills', {
        params: { query: { agent_id: agent.id } },
      });
      if (!alive.current) return;
      setScopedSkills(
        (skills.data?.items ?? [])
          .slice()
          .sort((a, b) => a.name.localeCompare(b.name)),
      );
    })();
    return () => {
      alive.current = false;
    };
  }, [agent, client, isMock, logout, universalSkills]);

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
      // What the row still owns. Name and llm ride targeted endpoints (the
      // builtin lock does not reach those), so they are absent here.
      const content = { description, framework };
      if (agent === null) {
        const { data, error: apiError, response } = await client.POST('/v1/agents', {
          body: {
            ...content,
            name,
            llm: llm === '' ? null : llm,
            avatar_blob_id: avatarBlobId,
            soul: soul.trim() === '' ? null : soul,
          },
        });
        if (response.status === 401) return logout();
        if (apiError || !response.ok) {
          setSaveError(apiError?.error ?? `HTTP Error ${response.status}`);
          return;
        }
        onSaved(data.id);
        return;
      }
      if (soulDirty) {
        const { data, error: apiError, response } = await client.PUT(
          '/v1/agents/{agent_id}/soul',
          {
            params: { path: { agent_id: agent.id } },
            body: { content: soul, version: soulVersion },
          },
        );
        if (response.status === 401) return logout();
        if (response.status === 409) {
          setSaveError(
            'The soul was rewritten by the agent since this page loaded. Reopen the agent to ' +
              'see the current version, then reapply your edit — saving now would delete it.',
          );
          return;
        }
        if (!response.ok || apiError !== undefined) {
          setSaveError(apiError?.error ?? `HTTP Error ${response.status}`);
          return;
        }
        // Adopt the version we just created, so a second Save from this same
        // open editor does not conflict with its own write.
        setSoulVersion(data.version);
      }

      // Targeted endpoints, so these run for the builtin too.
      for (const targeted of [
        {
          dirty: name !== agent.name,
          path: '/v1/agents/{agent_id}/name' as const,
          body: { name } as Record<string, unknown>,
        },
        {
                // The builtin's pin is fixed empty, so there is nothing to send.
          dirty: !isBuiltin && (llm === '' ? null : llm) !== (agent.llm ?? null),
          path: '/v1/agents/{agent_id}/model' as const,
          body: { llm: llm === '' ? null : llm } as Record<string, unknown>,
        },
      ]) {
        if (!targeted.dirty) continue;
        const { error: apiError, response } = await client.PUT(targeted.path, {
          params: { path: { agent_id: agent.id } },
          body: targeted.body,
        });
        if (response.status === 401) return logout();
        if (apiError || !response.ok) {
          setSaveError(apiError?.error ?? `HTTP Error ${response.status}`);
          return;
        }
      }

      // Only when the row's own fields moved. An unconditional PUT bumped
      // `updated_at` on every Save, including one that touched nothing but
      // the soul.
      if (description !== agent.description || framework !== agent.framework) {
        const { error: apiError, response } = await client.PUT('/v1/agents/{agent_id}', {
          params: { path: { agent_id: agent.id } },
          body: content,
        });
        if (response.status === 401) return logout();
        if (apiError || !response.ok) {
          setSaveError(apiError?.error ?? `HTTP Error ${response.status}`);
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
          setSaveError(apiError?.error ?? `HTTP Error ${response.status}`);
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
    isMock,
    llm,
    logout,
    name,
    onSaved,
    soul,
    soulDirty,
    soulVersion,
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
                {avatarBlobId !== null && (
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
                      <RiLockLine /> built-in · framework fixed
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
                className={`w-full h-10 !border ${builtinPinned ? 'opacity-60' : ''}`}
                value={framework}
                disabled={builtinPinned}
                onChange={(e) => setFramework(e.target.value as AgentFramework)}
              >
                {FRAMEWORKS.map((f) => (
                  <option key={f.value} value={f.value}>
                    {f.label}
                  </option>
                ))}
              </SelectBox>
            </div>
            <div
              className="flex-1"
              title={
                externalFramework
                  ? BAYBO_ONLY_HINT
                  : builtinPinned
                    ? BUILTIN_FOLLOWS_DEFAULT_HINT
                    : undefined
              }
            >
              <label className={fieldLabel}>
                Model {externalFramework && <span className="normal-case">(baybo only)</span>}
              </label>
              <SelectBox
                className={`w-full h-10 !border ${externalFramework || builtinPinned ? 'opacity-60' : ''}`}
                value={llm}
                disabled={externalFramework || builtinPinned}
                onChange={(e) => setLlm(e.target.value)}
              >
                {/* Not a model — the "don't pin one" choice. Naming the
                    entry `default-llm` currently points at saves the operator
                    a trip to the LLM page to find out what it resolves to. */}
                <option value="">
                  {defaultLlmName === ''
                    ? 'Follow default'
                    : `Follow default (${defaultLlmName})`}
                </option>
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

          {/* ── soul ── */}
          <div>
            <label className={fieldLabel}>
              Soul{' '}
              <span className="normal-case">
                {soulPath
                  ? `(${soulPath})`
                  : agent === null
                    ? '(seeded from a template if left empty)'
                    : '(loading…)'}
              </span>
            </label>
            <textarea
              className={`${textInput} resize-y`}
              rows={14}
              value={soul}
              disabled={agent !== null && !filesLoaded}
              onChange={(e) => {
                setSoul(e.target.value);
                setSoulDirty(true);
              }}
              placeholder="Markdown persona: who this agent is, how it should come across, what it refuses to do…"
            />
            <p className="mt-1 text-xs text-muted">
              This agent&apos;s personality and tone. The agent can rewrite it during a
              conversation.
            </p>
          </div>

          {/* ── skills (read-only, live from the registry, full width) ── */}
          <SkillsDisplay registered={scopedSkills} isNew={agent === null} />

          {/* actions pinned to the bottom of the card, right-aligned; the
              card content fades out behind them as it scrolls. */}
          <div className="sticky bottom-0 -mx-6 -mb-6 flex items-center justify-end gap-2 px-6 pt-4 pb-5 bg-gradient-to-t from-surface via-surface to-transparent">
            {saveError !== null && (
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
                (agent === null && name.trim() === '')
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
function SkillsDisplay({ registered, isNew }: { registered: SkillInfo[]; isNew: boolean }) {
  return (
    <div>
      <label className={fieldLabel}>
        Skills{' '}
        <span className="normal-case text-ink-soft">
          (this agent&apos;s own — read-only here)
        </span>
      </label>
      {registered.length === 0 ? (
        <p className="text-ink-soft text-sm font-mono">No skills.</p>
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
      <p className="mt-1 text-xs text-muted">
        {isNew
          ? 'A new agent starts with only the universal skills above — it does not inherit the workspace’s. Add one by putting it in the agent’s personas/<id>/skills/ folder.'
          : 'This agent does not inherit the workspace’s skills. Add one by putting it in its personas/<id>/skills/ folder.'}
      </p>
    </div>
  );
}
