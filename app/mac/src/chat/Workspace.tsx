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
import {
  ChatWs,
  EMPTY_VIEW,
  closeActiveWork,
  formatWorkedLabel,
  historyRowToTranscript,
  isStopCancellationNotice,
  isStopCommand,
  routeInboundFrame,
  workBlockDisplay,
  type HistoryRowDto,
  type PendingApproval,
  type SessionView,
  type TaskView,
  type TranscriptRow,
  type WireAttachment,
  type WorkStep,
} from '@aura/chat-core';
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
//
// The live-turn state model (SessionView, work blocks, server-authoritative
// turn_state, optimistic send + echo reconciliation) lives in @aura/chat-core
// and is shared with the web dashboard — this file owns only the mac-styled
// rendering of that shared view-model.

const OPERATOR = 'web-operator';

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
  // One per-session bucket map (mirrors web). The Thread is keyed by
  // `sessionId` so it only ever holds the active session, but routing
  // through the shared `routeInboundFrame` keeps the reducer identical to
  // web's — the mac UI just renders the resulting `SessionView`.
  const [views, setViews] = useState<Record<string, SessionView>>({});
  const v = views[sessionId] ?? EMPTY_VIEW;
  // A turn is in flight either optimistically (between send and the first
  // response) or per the server's authoritative TurnState. While busy the
  // composer's send button becomes a stop button and new sends are blocked.
  const busy = v.awaitingReply || (v.turn?.active ?? false);
  const [link, setLink] = useState<'connecting' | 'connected' | 'disconnected'>('connecting');
  const [model, setModel] = useState<string>('');
  const [input, setInput] = useState('');
  const [pending, setPending] = useState<WireAttachment[]>([]);
  const wsRef = useRef<ChatWs | null>(null);
  const tokenRef = useRef<string>('');
  const newestOrdinalRef = useRef<number>(-1);
  const lastConnectedAtRef = useRef<number>(0);
  // Sessions just `/stop`'d locally: a late `answer_delta` the server had
  // already put on the wire is dropped instead of spilling a stray bubble
  // below the now-collapsed work block. Cleared when a new turn starts.
  const stoppedRef = useRef<Set<string>>(new Set());
  const fileRef = useRef<HTMLInputElement | null>(null);
  const threadRef = useRef<HTMLDivElement | null>(null);
  const taRef = useRef<HTMLTextAreaElement | null>(null);

  useEffect(() => {
    threadRef.current?.scrollTo({ top: threadRef.current.scrollHeight });
  }, [v.transcript, v.pendingApproval, v.awaitingReply]);

  // Load history, pin the model picker, then open the live stream.
  useEffect(() => {
    let ws: ChatWs | null = null;
    let cancelled = false;

    (async () => {
      // History via REST, then seed the live cursor so the WS only streams new.
      const { data } = await client.GET('/v1/chat/sessions/{session_id}', {
        params: { path: { session_id: sessionId } },
      });
      if (cancelled) return;
      if (data) {
        setModel(data.last_llm ?? '');
        const rows = ((data.transcript ?? []) as unknown as HistoryRowDto[]).map((r) =>
          historyRowToTranscript(sessionId, r),
        );
        setViews((prev) => ({
          ...prev,
          [sessionId]: {
            ...EMPTY_VIEW,
            transcript: rows,
            historyLoaded: true,
            oldestOrdinal: data.oldest_ordinal ?? null,
            hasMore: data.has_more ?? false,
            model: data.last_llm ?? null,
          },
        }));
        newestOrdinalRef.current = data.newest_ordinal ?? -1;
      }
      const token = await refreshToken(client, sessionId);
      if (cancelled) return;
      tokenRef.current = token;
      ws = new ChatWs({
        baseUrl: conn.baseUrl,
        channelToken: token,
        initialSessionIds: [sessionId],
        onStatus: (s) => {
          setLink(s.state);
          // The snapshot-approval reconciliation uses this cutoff to tell a
          // card from before the reconnect (droppable) from one that raced in
          // after it (kept).
          if (s.state === 'connected') lastConnectedAtRef.current = Date.now();
        },
        onFrame: (frame) => {
          if (frame.kind === 'session_activity') onActivity();
          // A fresh turn supersedes any prior local `/stop`.
          if (frame.kind === 'turn_state' && frame.active) {
            stoppedRef.current.delete(frame.session_id);
          }
          // Drop a delta that raced in after a local `/stop` — the partial
          // answer is already settled inside the collapsed work block.
          if (frame.kind === 'answer_delta' && stoppedRef.current.has(frame.session_id)) {
            return;
          }
          // A broadcast `/stop` cancellation notice stops this tab's stream
          // too (an observer never ran the local `/stop`).
          if (frame.kind === 'notice' && !frame.transient && isStopCancellationNotice(frame.text)) {
            stoppedRef.current.add(frame.session_id);
          }
          routeInboundFrame(frame, setViews, () => undefined, lastConnectedAtRef.current);
        },
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

  // Optimistically close the live work block as Cancelled and tell the server.
  // A typed `/stop` and the composer's stop button both land here.
  const doStop = (text: string) => {
    const ws = wsRef.current;
    if (!ws) return;
    ws.sendMessage({ sessionId, userId: OPERATOR, content: text || '/stop' });
    stoppedRef.current.add(sessionId);
    setViews((prev) => {
      const view = prev[sessionId] ?? EMPTY_VIEW;
      return {
        ...prev,
        [sessionId]: {
          ...view,
          transcript: closeActiveWork(view.transcript, true),
          awaitingReply: false,
          turn: { active: false, startedAt: null },
        },
      };
    });
    setInput('');
    const ta = taRef.current;
    if (ta) ta.style.height = 'auto';
  };

  const send = () => {
    const text = input.trim();
    const ws = wsRef.current;
    if ((!text && pending.length === 0) || !ws) return;
    // A `/stop` is a control command, not a chat turn — no optimistic bubble.
    if (isStopCommand(text)) {
      doStop(text);
      return;
    }
    // The agent is already working — block new turns; only `/stop` gets through.
    if (busy) return;
    const clientMsgId = crypto.randomUUID();
    const attachments = pending.length ? pending : undefined;
    // Optimistic user bubble keyed by `clientMsgId`; the server's echo
    // reconciles against it in `routeInboundFrame` (clears `pending`, keeps
    // the row identity) instead of appending a duplicate.
    setViews((prev) => {
      const view = prev[sessionId] ?? EMPTY_VIEW;
      const row: TranscriptRow = {
        key: `local-${clientMsgId}`,
        role: 'user',
        text,
        pending: true,
        clientMsgId,
        hasAttachments: attachments ? true : undefined,
        attachments,
        createdAt: new Date().toISOString(),
      };
      return {
        ...prev,
        [sessionId]: { ...view, transcript: [...view.transcript, row], awaitingReply: true },
      };
    });
    ws.sendMessage({ sessionId, userId: OPERATOR, content: text, attachments, clientMsgId });
    setInput('');
    setPending([]);
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
    setViews((prev) => {
      const view = prev[sessionId];
      if (!view?.pendingApproval) return prev;
      return { ...prev, [sessionId]: { ...view, pendingApproval: null } };
    });
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
        <ConnState link={link} />
      </header>

      {v.tasks.length > 0 && <TasksPanel tasks={v.tasks} />}

      <div ref={threadRef} className="no-scrollbar relative flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-3xl space-y-6 px-6 pb-44 pt-6">
          {v.transcript.map((row) =>
            row.kind === 'work' ? (
              <WorkBlock key={row.key} row={row} />
            ) : row.notice ? (
              <div key={row.key} className={noticeClass(row.notice.level)}>
                {row.notice.text}
              </div>
            ) : (
              <MessageBubble key={row.key} role={row.role} text={row.text} pending={row.pending} />
            ),
          )}

          {v.awaitingReply && <WorkingIndicator />}

          {v.pendingApproval && <ApprovalCard approval={v.pendingApproval} onResolve={resolve} />}
        </div>
      </div>

      <Composer
        input={input}
        pending={pending}
        slashMatches={slashMatches}
        model={model}
        models={models}
        busy={busy}
        taRef={taRef}
        fileRef={fileRef}
        onInputChange={onInputChange}
        onSend={send}
        onStop={() => doStop('/stop')}
        onPickSlash={(cmd) => setInput(`/${cmd} `)}
        onAttach={onAttach}
        onRemovePending={(i) => setPending((p) => p.filter((_, j) => j !== i))}
        onModelChange={(value) => void onModelChange(value)}
      />
    </>
  );
}

function noticeClass(level: 'info' | 'warn' | 'error'): string {
  const tone =
    level === 'error'
      ? 'border-err text-err'
      : level === 'warn'
        ? 'border-warn text-warn'
        : 'border-info text-info';
  return `rounded-brutal border-2 bg-surface px-3 py-2 text-sm font-bold ${tone}`;
}

function Composer({
  input,
  pending,
  slashMatches,
  model,
  models,
  busy,
  taRef,
  fileRef,
  onInputChange,
  onSend,
  onStop,
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
  busy: boolean;
  taRef: RefObject<HTMLTextAreaElement | null>;
  fileRef: RefObject<HTMLInputElement | null>;
  onInputChange: (e: ChangeEvent<HTMLTextAreaElement>) => void;
  onSend: () => void;
  onStop: () => void;
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
            {busy ? (
              <button
                onClick={onStop}
                title="停止"
                className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full border-[3px] border-border bg-err text-surface shadow-brutal-sm transition-all hover:-translate-y-0.5 active:translate-y-0 active:shadow-none"
              >
                <span className="h-3 w-3 rounded-[2px] bg-surface" />
              </button>
            ) : (
              <button
                onClick={onSend}
                disabled={!canSend}
                title="发送"
                className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full border-[3px] border-border bg-accent text-surface shadow-brutal-sm transition-all hover:-translate-y-0.5 active:translate-y-0 active:shadow-none disabled:opacity-50 disabled:hover:translate-y-0 disabled:hover:shadow-brutal-sm"
              >
                <SendArrowIcon className="h-4 w-4" />
              </button>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

// Header status: stays quiet when the connection is healthy. A colored dot +
// label appears only while connecting or after a drop (turn-phase narration
// now lives inside the work block as a status step, so the header no longer
// surfaces it).
function ConnState({ link }: { link: 'connecting' | 'connected' | 'disconnected' }) {
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
  return null;
}

function MessageBubble({ role, text, pending }: { role: string; text: string; pending?: boolean }) {
  if (role === 'user') {
    // Right-aligned, washed-gold chip — lighter than work/approval cards but
    // unmistakably Aura (gold, not Codex grey). Dimmed while the optimistic
    // row awaits the server's echo.
    return (
      <div className="flex justify-end">
        <div
          className={`max-w-[80%] rounded-brutal border-2 border-border bg-rail/60 px-4 py-2.5 text-sm font-medium shadow-brutal-sm ${
            pending ? 'opacity-60' : ''
          }`}
        >
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

// One step inside a turn's work block: streamed reasoning, a status line, a
// mid-turn prose chunk, or a tool call with its result summary.
function WorkStepView({ step }: { step: WorkStep }) {
  if (step.kind === 'reasoning') {
    return (
      <div className="flex items-start gap-2 whitespace-pre-wrap font-mono text-xs text-ink-soft">
        <span className="select-none">✻</span>
        <span className="italic">{step.text}</span>
      </div>
    );
  }
  if (step.kind === 'status') {
    return (
      <div className="flex items-center gap-2 font-mono text-xs text-ink-soft">
        <span className="select-none">⟳</span>
        <span>{step.text}</span>
      </div>
    );
  }
  if (step.kind === 'prose') {
    return (
      <div className="markdown font-mono text-xs text-ink-soft">
        <ReactMarkdown remarkPlugins={[remarkGfm]}>{step.text ?? ''}</ReactMarkdown>
      </div>
    );
  }
  const statusColor =
    step.toolStatus === 'error'
      ? 'text-err'
      : step.toolStatus === 'denied'
        ? 'text-warn'
        : 'text-ink-soft';
  return (
    <div className="flex flex-col gap-0.5 font-mono text-xs">
      <div className="flex items-center gap-1.5">
        <span className="text-info">⏺</span>
        <span className="font-bold text-ink">{step.tool}</span>
        {step.toolLabel ? <span className="text-ink-soft">({step.toolLabel})</span> : null}
        {step.toolStatus === 'running' ? <span className="text-ink-soft">…</span> : null}
      </div>
      {step.toolSummary ? (
        <div className={`flex items-start gap-1.5 pl-1 ${statusColor}`}>
          <span className="select-none">⎿</span>
          <span className="whitespace-pre-wrap">{step.toolSummary}</span>
        </div>
      ) : null}
    </div>
  );
}

// The turn's aggregated progress. A live turn shows a pulsing "处理中" header
// with a live elapsed timer; on completion it collapses to a dim
// `Worked Xs ▸` line (click to re-expand) — or "Cancelled" when `/stop`'d.
function WorkBlock({ row }: { row: TranscriptRow }) {
  const active = !!row.workActive;
  const steps = row.steps ?? [];
  const [expanded, setExpanded] = useState(false);
  if (!active && steps.length === 0) return null;
  const { panelOpen } = workBlockDisplay(active, steps.length > 0, expanded);
  const secs =
    row.workEndedAt && row.workStartedAt
      ? Math.max(0, Math.round((row.workEndedAt - row.workStartedAt) / 1000))
      : 0;
  const label = formatWorkedLabel(secs, !!row.workCancelled);
  return (
    <div className="w-full space-y-2">
      <button
        onClick={() => {
          if (!active) setExpanded((e) => !e);
        }}
        className={`flex items-center gap-1.5 font-mono text-xs ${
          active ? 'text-ink' : 'text-ink-soft hover:text-ink'
        }`}
      >
        {active ? (
          <>
            <span className="h-2 w-2 animate-pulse rounded-full bg-accent" />
            <span className="font-bold uppercase tracking-wider">处理中</span>
            {row.workStartedAt ? (
              <span className="tabular-nums text-ink-soft">
                <LiveElapsed startedAt={row.workStartedAt} />
              </span>
            ) : null}
          </>
        ) : (
          <>
            <span className={`text-[9px] transition-transform ${expanded ? 'rotate-90' : ''}`}>▸</span>
            <span className={row.workCancelled ? 'text-warn' : ''}>{label}</span>
          </>
        )}
      </button>
      {panelOpen && (
        <div className="space-y-1.5 border-l-2 border-border/25 pl-3">
          {steps.map((s) => (
            <WorkStepView key={s.key} step={s} />
          ))}
        </div>
      )}
      {!active && !expanded ? (
        <div aria-hidden className="w-full border-t border-border/20" />
      ) : null}
    </div>
  );
}

// Live-ticking elapsed seconds for the active work header. Self-contained 1s
// interval so the rest of the transcript doesn't re-render on the tick; held
// back below 1s so a just-started turn never reads "0s".
function LiveElapsed({ startedAt }: { startedAt: number }) {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, []);
  const secs = Math.max(0, Math.floor((now - startedAt) / 1000));
  return secs < 1 ? null : <>{secs}s</>;
}

// Shown between sending a turn and the agent's first output frame
// (`SessionView.awaitingReply`), before any work block exists.
function WorkingIndicator() {
  return (
    <div className="flex items-center gap-2 font-mono text-xs text-ink-soft">
      <span className="h-2 w-2 animate-pulse rounded-full bg-accent" />
      <span className="font-bold uppercase tracking-wider">处理中…</span>
    </div>
  );
}

function ApprovalCard({
  approval,
  onResolve,
}: {
  approval: PendingApproval;
  onResolve: (callId: string, d: 'approve' | 'approve_always' | 'deny') => void;
}) {
  return (
    <div className="rounded-brutal border-[3px] border-warn bg-surface p-4 shadow-brutal-sm">
      <p className="mb-1 font-bold">
        批准 <span className="font-mono">{approval.tool}</span>？
      </p>
      {approval.description && <p className="mb-2 text-sm text-ink-soft">{approval.description}</p>}
      <pre className="mb-3 overflow-x-auto rounded-brutal border-2 border-border bg-canvas p-2 font-mono text-xs">
        {approval.paramsPreview}
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
