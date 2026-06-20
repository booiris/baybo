import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ChangeEvent,
  type RefObject,
} from 'react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { ChatWs, type Frame, type TaskView, type WireAttachment } from '@aura/chat-core';
import {
  admin,
  createSession,
  hideSession,
  listModels,
  listSessions,
  refreshToken,
  setSessionModel,
  slashManifest,
  uploadBlob,
  type SessionSummary,
  type SlashCommand,
} from '../api/admin';
import type { AdminClient } from '../api/client';
import type { Connection } from '../api/connection';
import { Select } from '../components/Select';
import { MicIcon, NewChatIcon, PlusIcon, SendArrowIcon, SettingsIcon } from '../components/icons';
import { SettingsView } from '../settings/Settings';
import { Sidebar, type MainView } from './Sidebar';
import { AutomationView, PluginsView } from './SystemViews';

// Milestone 4/5 chat workspace, restyled to the Codex layout (aaa.png) on the
// warm neo-brutalist tokens: one merged sidebar (nav + project→conversation
// tree + 设置), a centered reading-column thread (user-right, agent-centered),
// and a floating composer that hovers over the messages.

const OPERATOR = 'web-operator';

interface ToolStep {
  callId: string;
  tool: string;
  label?: string;
  status?: string;
  summary?: string;
}

interface MsgItem {
  kind: 'message';
  id: string;
  role: string;
  text: string;
  ordinal?: number;
  // True while this assistant bubble is still being built from
  // `answer_delta` frames. Cleared when the terminal `message` frame folds
  // the streamed text into its finalized form. Without it the final reply
  // would render as a second, duplicate bubble next to the streamed one.
  streaming?: boolean;
}
interface WorkItem {
  kind: 'work';
  id: string;
  reasoning: string;
  tools: ToolStep[];
  streaming: boolean;
}
interface NoticeItem {
  kind: 'notice';
  id: string;
  level: string;
  text: string;
}
type Item = MsgItem | WorkItem | NoticeItem;

interface Approval {
  callId: string;
  tool: string;
  preview: string;
  description?: string | null;
}

interface TranscriptItem {
  ordinal: number;
  kind: string;
  role: string;
  text: string;
  notice_level?: string | null;
}

export function Workspace({ conn }: { conn: Connection }) {
  const client = useMemo<AdminClient>(() => admin(conn), [conn]);
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [models, setModels] = useState<string[]>([]);
  const [slash, setSlash] = useState<SlashCommand[]>([]);
  const [view, setView] = useState<MainView>('chat');

  const refreshSessions = useCallback(async () => {
    try {
      setSessions(await listSessions(client));
    } catch {
      /* sidebar tolerates a failed refresh */
    }
  }, [client]);

  useEffect(() => {
    refreshSessions();
    listModels(client).then(setModels, () => undefined);
    slashManifest(client).then(setSlash, () => undefined);
  }, [client, refreshSessions]);

  const newChat = useCallback(async () => {
    const { sessionId } = await createSession(client);
    await refreshSessions();
    setView('chat');
    setActiveId(sessionId);
  }, [client, refreshSessions]);

  const selectConversation = useCallback((id: string) => {
    setView('chat');
    setActiveId(id);
  }, []);

  const hide = useCallback(
    async (id: string) => {
      await hideSession(client, id);
      if (activeId === id) setActiveId(null);
      await refreshSessions();
    },
    [client, activeId, refreshSessions],
  );

  const activeTitle = useMemo(
    () => sessions.find((s) => s.session_id === activeId)?.last_user_text ?? undefined,
    [sessions, activeId],
  );

  return (
    <div className="flex h-screen bg-canvas">
      {/* Far-left app rail (gold): brand/home + settings — app-level nav, kept
          distinct from the conversation sidebar to its right. The top 32px is a
          seamless cream title-bar strip shared with the sidebar (no rail→sidebar
          divider runs through it) so the macOS overlay traffic lights sit on a
          clean field instead of straddling the gold/cream seam. The gold fill
          and its `border-r` divider start below the strip. */}
      <nav data-tauri-drag-region="deep" className="flex w-12 shrink-0 flex-col bg-canvas">
        <div data-tauri-drag-region="deep" className="h-[var(--titlebar)] shrink-0" />
        <div
          data-tauri-drag-region="deep"
          className="flex flex-1 flex-col items-center gap-3 border-r-[3px] border-border bg-rail pb-3 pt-3"
        >
          <button
            onClick={() => setView('chat')}
            title="对话"
            className={`flex h-8 w-8 items-center justify-center rounded-brutal border-2 border-border font-mono text-sm font-bold transition-all hover:-translate-y-0.5 hover:shadow-brutal-sm active:translate-y-0 active:shadow-none ${
              view === 'settings' ? 'bg-surface' : 'bg-selected shadow-brutal-sm'
            }`}
          >
            A
          </button>
          <button
            onClick={() => setView('settings')}
            title="设置"
            className={`group mt-auto flex h-8 w-8 items-center justify-center rounded-brutal border-2 border-border transition-all hover:-translate-y-0.5 hover:shadow-brutal-sm active:translate-y-0 active:shadow-none ${
              view === 'settings' ? 'bg-selected shadow-brutal-sm' : 'bg-surface'
            }`}
          >
            <SettingsIcon className="h-4 w-4 transition-transform duration-300 group-hover:rotate-90" />
          </button>
        </div>
      </nav>

      <Sidebar
        sessions={sessions}
        activeId={activeId}
        view={view}
        onNewChat={() => void newChat()}
        onSelect={selectConversation}
        onHide={(id) => void hide(id)}
        onView={setView}
      />

      <main className="relative flex min-w-0 flex-1 flex-col overflow-hidden">
        {view === 'settings' ? (
          <SettingsView conn={conn} />
        ) : view === 'plugins' ? (
          <PluginsView client={client} />
        ) : view === 'automation' ? (
          <AutomationView client={client} />
        ) : activeId ? (
          <Thread
            key={activeId}
            client={client}
            conn={conn}
            sessionId={activeId}
            title={activeTitle}
            models={models}
            slash={slash}
            onActivity={refreshSessions}
          />
        ) : (
          <EmptyState onNew={() => void newChat()} />
        )}
      </main>
    </div>
  );
}

function EmptyState({ onNew }: { onNew: () => void }) {
  return (
    <div className="flex flex-1 flex-col items-center justify-center gap-5 px-8">
      <div className="flex h-14 w-14 items-center justify-center rounded-brutal border-[3px] border-border bg-rail font-mono text-2xl font-bold shadow-brutal">
        A
      </div>
      <p className="text-center font-mono text-sm text-ink-soft">
        选择一个对话，或开始新对话。
      </p>
      <button
        onClick={onNew}
        className="flex items-center gap-2 rounded-brutal border-[3px] border-border bg-accent px-4 py-2 text-sm font-bold text-surface shadow-brutal-sm transition-all hover:-translate-y-0.5 hover:shadow-brutal active:translate-y-0 active:shadow-none"
      >
        <NewChatIcon className="h-4 w-4" />
        新对话
      </button>
    </div>
  );
}

function Thread({
  client,
  conn,
  sessionId,
  title,
  models,
  slash,
  onActivity,
}: {
  client: AdminClient;
  conn: Connection;
  sessionId: string;
  title?: string;
  models: string[];
  slash: SlashCommand[];
  onActivity: () => void;
}) {
  const [items, setItems] = useState<Item[]>([]);
  const [approvals, setApprovals] = useState<Approval[]>([]);
  const [tasks, setTasks] = useState<TaskView[]>([]);
  // Connection health (from the WS) is kept separate from the agent's
  // transient turn phase (e.g. "compacting") — conflating them is what made
  // a steady "connected" sit in the header. `link` only surfaces on a
  // problem; `phase` only while a turn emits one.
  const [link, setLink] = useState<'connecting' | 'connected' | 'disconnected'>('connecting');
  const [phase, setPhase] = useState('');
  const [model, setModel] = useState<string>('');
  const [input, setInput] = useState('');
  const [pending, setPending] = useState<WireAttachment[]>([]);
  const wsRef = useRef<ChatWs | null>(null);
  const liveRef = useRef<string | null>(null);
  const newestOrdinalRef = useRef<number>(-1);
  const tokenRef = useRef<string>('');
  const fileRef = useRef<HTMLInputElement | null>(null);
  const threadRef = useRef<HTMLDivElement | null>(null);
  const taRef = useRef<HTMLTextAreaElement | null>(null);

  useEffect(() => {
    threadRef.current?.scrollTo({ top: threadRef.current.scrollHeight });
  }, [items, approvals]);

  // Load history, pin the model picker, then open the live stream.
  useEffect(() => {
    let ws: ChatWs | null = null;
    let cancelled = false;

    const updateLive = (fn: (w: WorkItem) => WorkItem) => {
      let id = liveRef.current;
      if (!id) {
        id = crypto.randomUUID();
        liveRef.current = id;
        const fresh: WorkItem = { kind: 'work', id, reasoning: '', tools: [], streaming: true };
        setItems((xs) => [...xs, fn(fresh)]);
        return;
      }
      const liveId = id;
      setItems((xs) => xs.map((it) => (it.kind === 'work' && it.id === liveId ? fn(it) : it)));
    };

    const handleFrame = (f: Frame) => {
      switch (f.kind) {
        case 'message': {
          if (f.ordinal !== undefined && f.ordinal <= newestOrdinalRef.current) return; // dup vs history
          if (f.ordinal !== undefined) newestOrdinalRef.current = f.ordinal;
          liveRef.current = null;
          if (f.role === 'assistant') setPhase(''); // turn ended — drop any phase hint
          setItems((xs) => {
            // The gateway stamps the persisted ordinal onto the SAME reply
            // we just streamed via `answer_delta`, so fold it into the live
            // bubble. Appending would render the finalized copy as a second,
            // duplicate reply bubble.
            if (f.role === 'assistant') {
              const last = xs[xs.length - 1];
              if (last && last.kind === 'message' && last.role === 'assistant' && last.streaming) {
                return xs.map((it, i) =>
                  i === xs.length - 1
                    ? { ...last, text: f.content, ordinal: f.ordinal, streaming: false }
                    : it,
                );
              }
            }
            return [
              ...xs,
              { kind: 'message', id: crypto.randomUUID(), role: f.role, text: f.content, ordinal: f.ordinal },
            ];
          });
          break;
        }
        case 'answer_delta':
          // deltas accumulate into a streaming message bubble
          setItems((xs) => {
            const last = xs[xs.length - 1];
            if (last && last.kind === 'message' && last.role === 'assistant' && last.streaming) {
              return xs.map((it, i) => (i === xs.length - 1 ? { ...last, text: last.text + f.text } : it));
            }
            return [
              ...xs,
              { kind: 'message', id: crypto.randomUUID(), role: 'assistant', text: f.text, streaming: true },
            ];
          });
          break;
        case 'reasoning':
          updateLive((w) => ({ ...w, reasoning: w.reasoning + f.text }));
          break;
        case 'tool_started':
          updateLive((w) => ({ ...w, tools: [...w.tools, { callId: f.call_id, tool: f.tool, label: f.label }] }));
          break;
        case 'tool_completed':
          updateLive((w) => ({
            ...w,
            tools: w.tools.map((t) => (t.callId === f.call_id ? { ...t, status: f.status, summary: f.summary } : t)),
          }));
          break;
        case 'status':
          setPhase(
            f.phase === 'compacting'
              ? 'Compacting context…'
              : f.phase === 'compacted'
                ? 'Context compacted'
                : f.phase,
          );
          break;
        case 'notice':
          if (!f.transient)
            setItems((xs) => [...xs, { kind: 'notice', id: crypto.randomUUID(), level: f.level, text: f.text }]);
          break;
        case 'task_list':
          setTasks(f.tasks);
          break;
        case 'approval_requested':
          setApprovals((xs) => [
            ...xs,
            { callId: f.call_id, tool: f.tool, preview: f.params_preview, description: f.description },
          ]);
          break;
        case 'approval_resolved':
          setApprovals((xs) => xs.filter((a) => a.callId !== f.call_id));
          break;
        case 'pending_approvals_snapshot':
          setApprovals((xs) => xs.filter((a) => f.call_ids.includes(a.callId)));
          break;
        case 'session_activity':
          onActivity();
          break;
        default:
          break;
      }
    };

    (async () => {
      // History via REST, then seed the live cursor so the WS only streams new.
      const { data } = await client.GET('/v1/chat/sessions/{session_id}', {
        params: { path: { session_id: sessionId } },
      });
      if (cancelled) return;
      if (data) {
        setModel(data.last_llm ?? '');
        const transcript = (data.transcript ?? []) as TranscriptItem[];
        const loaded: Item[] = transcript
          .filter((t) => t.kind === 'message' || t.kind === 'notice')
          .map((t) =>
            t.kind === 'notice'
              ? { kind: 'notice', id: `h${t.ordinal}`, level: t.notice_level ?? 'info', text: t.text }
              : { kind: 'message', id: `h${t.ordinal}`, role: t.role, text: t.text, ordinal: t.ordinal },
          );
        setItems(loaded);
        newestOrdinalRef.current = data.newest_ordinal ?? -1;
      }
      const token = await refreshToken(client, sessionId);
      if (cancelled) return;
      tokenRef.current = token;
      ws = new ChatWs({
        baseUrl: conn.baseUrl,
        channelToken: token,
        initialSessionIds: [sessionId],
        onStatus: (s) => setLink(s.state),
        onFrame: handleFrame,
        onTokenRejected: () => {
          // The channel token expired or was revoked (the common cause is the
          // gateway restarting on a config reload). Without this the socket
          // suspends and the chat sits dead — mint a fresh token and resume.
          void (async () => {
            try {
              const fresh = await refreshToken(client, sessionId);
              if (cancelled) return;
              tokenRef.current = fresh;
              wsRef.current?.replaceToken(fresh);
            } catch {
              /* stay disconnected; the header badge surfaces it */
            }
          })();
        },
      });
      if (newestOrdinalRef.current >= 0) ws.recordOrdinal(sessionId, newestOrdinalRef.current);
      wsRef.current = ws;
    })();

    return () => {
      cancelled = true;
      ws?.close();
      wsRef.current = null;
    };
  }, [client, conn, sessionId, onActivity]);

  const send = () => {
    const text = input.trim();
    if ((!text && pending.length === 0) || !wsRef.current) return;
    wsRef.current.sendMessage({
      sessionId,
      userId: OPERATOR,
      content: text,
      attachments: pending.length ? pending : undefined,
    });
    setInput('');
    setPending([]);
    liveRef.current = null;
    const ta = taRef.current;
    if (ta) ta.style.height = 'auto';
  };

  const onInputChange = (e: ChangeEvent<HTMLTextAreaElement>) => {
    setInput(e.target.value);
    const ta = e.target;
    ta.style.height = 'auto';
    ta.style.height = `${Math.min(ta.scrollHeight, 160)}px`;
  };

  const onAttach = async (file: File) => {
    try {
      const blobId = await uploadBlob(conn, tokenRef.current, file);
      const kind: WireAttachment['kind'] = file.type.startsWith('image/')
        ? 'image'
        : file.type.startsWith('audio/')
          ? 'audio'
          : 'file';
      setPending((p) => [
        ...p,
        {
          kind,
          blob_id: blobId,
          mime_type: file.type || 'application/octet-stream',
          size: file.size,
          filename: file.name,
        },
      ]);
    } catch {
      /* leave the composer unchanged on upload failure */
    }
  };

  const resolve = (callId: string, decision: 'approve' | 'approve_always' | 'deny') => {
    wsRef.current?.resolveApproval(callId, decision);
    setApprovals((xs) => xs.filter((a) => a.callId !== callId));
  };

  const onModelChange = async (m: string) => {
    setModel(m);
    try {
      await setSessionModel(client, sessionId, m || null);
    } catch {
      /* keep the optimistic selection */
    }
  };

  const slashOpen = input.startsWith('/') && !input.includes(' ');
  const slashMatches = slashOpen
    ? slash.filter((c) => c.command.startsWith(input.slice(1).toLowerCase()))
    : [];

  return (
    <>
      <header
        data-tauri-drag-region="deep"
        className="flex items-center justify-between gap-3 border-b-[3px] border-border bg-canvas px-6 py-3"
      >
        <h1 className="min-w-0 flex-1 truncate text-sm font-bold">{title || '新对话'}</h1>
        <ConnState link={link} phase={phase} />
      </header>

      {tasks.length > 0 && <TasksPanel tasks={tasks} />}

      <div ref={threadRef} className="no-scrollbar relative flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-3xl space-y-6 px-6 pb-44 pt-6">
          {items.map((it) =>
            it.kind === 'message' ? (
              <MessageBubble key={it.id} role={it.role} text={it.text} />
            ) : it.kind === 'notice' ? (
              <div
                key={it.id}
                className="rounded-brutal border-2 border-info bg-surface px-3 py-2 text-sm font-bold text-info"
              >
                {it.text}
              </div>
            ) : (
              <WorkBlock key={it.id} item={it} />
            ),
          )}

          {approvals.map((a) => (
            <ApprovalCard key={a.callId} approval={a} onResolve={resolve} />
          ))}
        </div>
      </div>

      <Composer
        input={input}
        pending={pending}
        slashMatches={slashMatches}
        model={model}
        models={models}
        taRef={taRef}
        fileRef={fileRef}
        onInputChange={onInputChange}
        onSend={send}
        onPickSlash={(cmd) => setInput(`/${cmd} `)}
        onAttach={onAttach}
        onRemovePending={(i) => setPending((p) => p.filter((_, j) => j !== i))}
        onModelChange={(v) => void onModelChange(v)}
      />
    </>
  );
}

function Composer({
  input,
  pending,
  slashMatches,
  model,
  models,
  taRef,
  fileRef,
  onInputChange,
  onSend,
  onPickSlash,
  onAttach,
  onRemovePending,
  onModelChange,
}: {
  input: string;
  pending: WireAttachment[];
  slashMatches: SlashCommand[];
  model: string;
  models: string[];
  taRef: RefObject<HTMLTextAreaElement | null>;
  fileRef: RefObject<HTMLInputElement | null>;
  onInputChange: (e: ChangeEvent<HTMLTextAreaElement>) => void;
  onSend: () => void;
  onPickSlash: (command: string) => void;
  onAttach: (file: File) => void;
  onRemovePending: (index: number) => void;
  onModelChange: (value: string) => void;
}) {
  const canSend = input.trim().length > 0 || pending.length > 0;
  return (
    // Overlay: the card floats over the thread (messages scroll behind it).
    // pointer-events-none on the wrapper + fade; only the card wrapper re-enables
    // events so the thread stays scrollable/selectable in the side gutters.
    <div className="pointer-events-none absolute inset-x-0 bottom-0 z-20 flex justify-center px-6 pb-6">
      <div className="pointer-events-none absolute inset-x-0 bottom-0 h-36 bg-gradient-to-t from-canvas via-canvas/90 to-transparent" />
      <div className="pointer-events-auto relative w-full max-w-3xl">
        {slashMatches.length > 0 && (
          <div className="absolute bottom-full left-0 mb-2 w-80 rounded-brutal border-[3px] border-border bg-surface shadow-brutal">
            {slashMatches.map((c) => (
              <button
                key={c.command}
                onClick={() => onPickSlash(c.command)}
                className="block w-full px-3 py-2 text-left text-sm hover:bg-canvas"
              >
                <span className="font-mono font-bold">/{c.command}</span>{' '}
                <span className="text-ink-soft">{c.description}</span>
              </button>
            ))}
          </div>
        )}

        <div className="rounded-brutal border-[3px] border-border bg-surface shadow-brutal">
          <div className="px-3 pt-3">
            {pending.length > 0 && (
              <div className="mb-2 flex flex-wrap gap-2">
                {pending.map((a, i) => (
                  <span
                    key={i}
                    className="rounded-brutal border-2 border-border bg-canvas px-2 py-1 font-mono text-xs"
                  >
                    📎 {a.filename ?? a.kind}
                    <button onClick={() => onRemovePending(i)} className="ml-1 text-ink-soft">
                      ✕
                    </button>
                  </span>
                ))}
              </div>
            )}
            <input
              ref={fileRef}
              type="file"
              className="hidden"
              onChange={(e) => {
                const f = e.target.files?.[0];
                if (f) onAttach(f);
                e.target.value = '';
              }}
            />
            <textarea
              ref={taRef}
              value={input}
              onChange={onInputChange}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && !e.shiftKey) {
                  e.preventDefault();
                  onSend();
                }
              }}
              rows={1}
              placeholder="有什么需要我做的…  （输入 / 调用命令）"
              className="max-h-40 w-full resize-none bg-transparent font-sans text-sm leading-relaxed placeholder:text-ink-soft focus:outline-none"
            />
          </div>

          <div className="flex items-center gap-2 px-3 pb-2.5 pt-1">
            <button
              onClick={() => fileRef.current?.click()}
              title="添加附件"
              className="flex h-7 w-7 shrink-0 items-center justify-center rounded-brutal border-2 border-border bg-canvas shadow-brutal-sm transition-all hover:-translate-y-0.5 active:translate-y-0 active:shadow-none"
            >
              <PlusIcon className="h-3.5 w-3.5" />
            </button>
            <span className="flex shrink-0 items-center gap-1 rounded-brutal border-2 border-border bg-rail px-2.5 py-1 font-mono text-xs font-bold">
              自动智能体 <span className="text-[10px]">▾</span>
            </span>

            <div className="flex-1" />

            <Select
              size="sm"
              value={model}
              onChange={onModelChange}
              options={[{ value: '', label: 'default-llm' }, ...models.map((m) => ({ value: m, label: m }))]}
              className="w-40"
            />
            <button
              title="语音"
              className="flex h-7 w-7 shrink-0 items-center justify-center rounded-brutal text-ink-soft hover:text-ink"
            >
              <MicIcon className="h-4 w-4" />
            </button>
            <button
              onClick={onSend}
              disabled={!canSend}
              title="发送"
              className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full border-[3px] border-border bg-accent text-surface shadow-brutal-sm transition-all hover:-translate-y-0.5 active:translate-y-0 active:shadow-none disabled:opacity-50 disabled:hover:translate-y-0 disabled:hover:shadow-brutal-sm"
            >
              <SendArrowIcon className="h-4 w-4" />
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

// Header status: stays quiet when the connection is healthy (no "connected"
// noise). A colored dot + label appears only while connecting or after a
// drop; once connected it yields to the agent's transient turn phase, if any.
function ConnState({
  link,
  phase,
}: {
  link: 'connecting' | 'connected' | 'disconnected';
  phase: string;
}) {
  if (link === 'connecting') {
    return (
      <span className="flex shrink-0 items-center gap-2 font-mono text-xs text-ink-soft">
        <span className="h-2 w-2 rounded-full bg-warn" />
        连接中…
      </span>
    );
  }
  if (link === 'disconnected') {
    return (
      <span className="flex shrink-0 items-center gap-2 font-mono text-xs font-bold text-err">
        <span className="h-2 w-2 rounded-full bg-err" />
        已断开 — 重连中…
      </span>
    );
  }
  return <span className="shrink-0 font-mono text-xs text-ink-soft">{phase}</span>;
}

function MessageBubble({ role, text }: { role: string; text: string }) {
  if (role === 'user') {
    // Right-aligned, washed-gold chip — lighter than work/approval cards but
    // unmistakably Aura (gold, not Codex grey).
    return (
      <div className="flex justify-end">
        <div className="max-w-[80%] rounded-brutal border-2 border-border bg-rail/60 px-4 py-2.5 text-sm font-medium shadow-brutal-sm">
          {text}
        </div>
      </div>
    );
  }
  // Agent: borderless prose flowing in the centered reading column (the core
  // Codex shift). Identity lives in the bordered code/pre, not a prose frame.
  return (
    <div className="markdown reading-col w-full text-ink">
      <ReactMarkdown remarkPlugins={[remarkGfm]}>{text}</ReactMarkdown>
    </div>
  );
}

function WorkBlock({ item }: { item: WorkItem }) {
  const [open, setOpen] = useState(false);
  if (!item.reasoning && item.tools.length === 0) return null;
  return (
    <div className="w-full space-y-2">
      <button
        onClick={() => setOpen((v) => !v)}
        className="flex items-center gap-1.5 font-mono text-xs text-ink-soft hover:text-ink"
      >
        <span className={`text-[9px] transition-transform ${open ? 'rotate-90' : ''}`}>▸</span>
        {open ? '隐藏过程' : `已使用 ${item.tools.length} 个工具`}
      </button>
      {open && (
        <div className="space-y-1.5 border-l-2 border-border/25 pl-3">
          {item.reasoning && (
            <pre className="whitespace-pre-wrap font-mono text-xs text-ink-soft">{item.reasoning}</pre>
          )}
          {item.tools.map((t) => (
            <div key={t.callId} className="flex items-start gap-2 font-mono text-xs">
              <span className="text-ink-soft">
                {t.status === 'completed' || t.status === 'ok'
                  ? '✓'
                  : t.status?.includes('err')
                    ? '✕'
                    : t.status
                      ? '…'
                      : '○'}
              </span>
              <span>
                <span className="font-bold">{t.tool}</span>
                {t.label ? ` ${t.label}` : ''}
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

function ApprovalCard({
  approval,
  onResolve,
}: {
  approval: Approval;
  onResolve: (callId: string, d: 'approve' | 'approve_always' | 'deny') => void;
}) {
  return (
    <div className="rounded-brutal border-[3px] border-warn bg-surface p-4 shadow-brutal-sm">
      <p className="mb-1 font-bold">
        批准 <span className="font-mono">{approval.tool}</span>？
      </p>
      {approval.description && <p className="mb-2 text-sm text-ink-soft">{approval.description}</p>}
      <pre className="mb-3 overflow-x-auto rounded-brutal border-2 border-border bg-canvas p-2 font-mono text-xs">
        {approval.preview}
      </pre>
      <div className="flex gap-2">
        <button
          onClick={() => onResolve(approval.callId, 'approve')}
          className="rounded-brutal border-2 border-border bg-ok px-3 py-1 text-sm font-bold text-surface"
        >
          批准
        </button>
        <button
          onClick={() => onResolve(approval.callId, 'approve_always')}
          className="rounded-brutal border-2 border-border bg-canvas px-3 py-1 text-sm font-bold"
        >
          总是
        </button>
        <button
          onClick={() => onResolve(approval.callId, 'deny')}
          className="rounded-brutal border-2 border-border bg-err px-3 py-1 text-sm font-bold text-surface"
        >
          拒绝
        </button>
      </div>
    </div>
  );
}

function TasksPanel({ tasks }: { tasks: TaskView[] }) {
  return (
    <div className="border-b-[3px] border-border bg-surface px-6 py-2">
      <p className="mb-1 text-xs font-bold uppercase tracking-wider text-ink-soft">任务</p>
      <ul className="space-y-1">
        {tasks.map((t) => (
          <li key={t.id} className="flex items-center gap-2 text-sm">
            <span className="font-mono text-xs text-ink-soft">
              {t.status === 'completed' ? '✓' : t.status === 'in_progress' ? '…' : '○'}
            </span>
            <span className={t.status === 'completed' ? 'line-through text-ink-soft' : ''}>{t.subject}</span>
          </li>
        ))}
      </ul>
    </div>
  );
}
