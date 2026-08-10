import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { RiAddLine, RiCloseLine, RiMenuLine } from 'react-icons/ri';

import { useAdminClient, useAuth } from '../../api/auth';
import { ChatWs } from '../../api/chatWs';
import { formatWorkedLabel } from '../ChatPage';
import {
  fetchLeadConversations,
  fetchLeadMessages,
  openLeadConversation,
  type LeadConversation,
  type LeadTurn,
} from './api';
import {
  ACTION_LABEL,
  conversationLabel,
  isMissingLead,
  leadItems,
  mergeLiveRow,
  type LeadItem,
} from './leadModel';

/// The lead's own tool calls, rendered where the mockup puts them: an amber
/// left bar under the reply, linking to the card that appeared on the board
/// beside it.
function EventCard({
  item,
  projectId,
  onOpenIssue,
}: {
  item: Extract<LeadItem, { kind: 'event' }>;
  projectId: string;
  onOpenIssue: () => void;
}) {
  return (
    <div className="self-start w-full border-l-4 border-brand bg-brand/10 rounded-r-md px-2 py-1.5">
      <p className="font-mono text-[0.58rem] font-bold uppercase tracking-wider text-ink-soft">
        ＋ {ACTION_LABEL[item.action]}
        {item.count > 1 ? ` ×${String(item.count)}` : ''}
      </p>
      {item.issues.length > 0 ? (
        <p className="mt-0.5 flex flex-wrap gap-x-2 font-mono text-[0.66rem]">
          {item.issues.map((number) => (
            <Link
              key={number}
              to={`/projects/${encodeURIComponent(projectId)}/issues/${String(number)}`}
              onClick={onOpenIssue}
              className="font-bold underline"
            >
              #{number}
            </Link>
          ))}
        </p>
      ) : null}
    </div>
  );
}

/// The collapsed work block, using the chat page's own label rule so a turn
/// reads identically in both places.
function WorkBlock({ row }: { row: LeadTurn }) {
  const [expanded, setExpanded] = useState(false);
  const steps = row.steps ?? [];
  const started = row.work_started_at == null ? null : Date.parse(row.work_started_at);
  const ended = row.work_ended_at == null ? null : Date.parse(row.work_ended_at);
  const elapsed = started != null && ended != null ? ended - started : 0;
  const label = formatWorkedLabel(elapsed, row.cancelled ?? false);

  return (
    <div className="self-start w-full">
      <button
        type="button"
        disabled={steps.length === 0}
        onClick={() => {
          setExpanded((value) => !value);
        }}
        className="font-mono text-[0.6rem] text-ink-soft italic cursor-pointer disabled:cursor-default hover:text-ink"
      >
        {steps.length === 0 ? '' : expanded ? '▾ ' : '▸ '}
        {label}
        {steps.length === 0 ? '' : ` · ${String(steps.length)} steps`}
      </button>
      {expanded ? (
        <ul className="mt-1 flex flex-col gap-0.5 border-l-2 border-black/15 pl-2">
          {steps.map((step, index) => (
            <li
              key={`${row.id}-${String(index)}`}
              className="font-mono text-[0.58rem] text-ink-soft break-words"
            >
              {step.tool ?? step.kind}
              {step.tool_summary == null ? '' : ` · ${step.tool_summary}`}
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

export function LeadPanel({
  projectId,
  projectName,
  readOnly,
  onClose,
  onBoardChanged,
  onHireLead,
}: {
  projectId: string;
  /// Shown in the header as `<project> · lead`, which is the conversation's
  /// ownership label — the panel is per board, and a bare "@lead" does not
  /// say which one.
  projectName: string;
  readOnly: boolean;
  onClose: () => void;
  onBoardChanged: () => void;
  /// Open the ordinary new-agent form so the operator can add the missing
  /// coordinator themselves. Only reachable on a board that has none.
  onHireLead?: () => void;
}) {
  const client = useAdminClient();
  const { token, baseUrl, logout } = useAuth();
  const [conversations, setConversations] = useState<LeadConversation[]>([]);
  const [active, setActive] = useState<string | null>(null);
  const [rows, setRows] = useState<LeadTurn[]>([]);
  const [older, setOlder] = useState<{ has: boolean; cursor: number | null }>({
    has: false,
    cursor: null,
  });
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [draft, setDraft] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [showHistory, setShowHistory] = useState(false);
  const wsRef = useRef<ChatWs | null>(null);
  const bottom = useRef<HTMLDivElement | null>(null);

  const items = useMemo(() => leadItems(rows), [rows]);

  const load = useCallback(async () => {
    const outcome = await fetchLeadConversations(client, projectId);
    if (outcome.kind === 'unauthorized') {
      logout();
      return;
    }
    if (outcome.kind === 'failed') {
      setError(outcome.message);
      return;
    }
    setError(null);
    setConversations(outcome.value);
    const newest = outcome.value.length > 0 ? outcome.value[0].session_id : null;
    setActive((current) => current ?? newest);
  }, [client, logout, projectId]);

  useEffect(() => {
    void load();
  }, [load]);

  // Esc closes the panel, like every other layer on this board.
  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      if (event.key !== 'Escape') return;
      if (showHistory) {
        setShowHistory(false);
        return;
      }
      onClose();
    }
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('keydown', onKeyDown);
    };
  }, [onClose, showHistory]);

  useEffect(() => {
    if (active === null) {
      setRows([]);
      setOlder({ has: false, cursor: null });
      return;
    }
    let canceled = false;
    void fetchLeadMessages(client, active).then((outcome) => {
      if (canceled) return;
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        return;
      }
      setError(null);
      setRows(outcome.value.rows);
      setOlder({ has: outcome.value.hasMoreOlder, cursor: outcome.value.oldestOrdinal });
    });
    return () => {
      canceled = true;
    };
  }, [active, client, logout]);

  const loadOlder = useCallback(async () => {
    if (active === null || !older.has || loadingOlder) return;
    setLoadingOlder(true);
    const outcome = await fetchLeadMessages(client, active, older.cursor);
    setLoadingOlder(false);
    if (outcome.kind === 'unauthorized') {
      logout();
      return;
    }
    if (outcome.kind === 'failed') {
      setError(outcome.message);
      return;
    }
    setRows((current) => [...outcome.value.rows, ...current]);
    setOlder({ has: outcome.value.hasMoreOlder, cursor: outcome.value.oldestOrdinal });
  }, [active, client, loadingOlder, logout, older]);

  useEffect(() => {
    if (token === null || active === null) return;
    const ws = new ChatWs({
      baseUrl,
      adminToken: token,
      initialSessionIds: [active],
      onFrame: (frame) => {
        if (frame.kind !== 'message' || frame.session_id !== active) return;
        setRows((current) =>
          mergeLiveRow(current, {
            id: `live-${String(frame.ordinal ?? current.length)}`,
            kind: 'message',
            role: frame.role,
            text: frame.content,
            created_at: new Date().toISOString(),
            has_attachments: false,
            ordinal: frame.ordinal ?? null,
          } as LeadTurn),
        );
        if (frame.role !== 'user') onBoardChanged();
      },
    });
    wsRef.current = ws;
    return () => {
      ws.close();
      wsRef.current = null;
    };
  }, [active, baseUrl, onBoardChanged, token]);

  useEffect(() => {
    bottom.current?.scrollIntoView({ block: 'end' });
  }, [items]);

  /// Open a conversation, either because the operator asked for a new one or
  /// because they are about to say something and there is none yet.
  const start = useCallback(async (): Promise<string | null> => {
    setBusy(true);
    const outcome = await openLeadConversation(client, projectId);
    setBusy(false);
    if (outcome.kind === 'unauthorized') {
      logout();
      return null;
    }
    if (outcome.kind === 'failed') {
      setError(outcome.message);
      return null;
    }
    setError(null);
    setActive(outcome.value.session_id);
    await load();
    return outcome.value.session_id;
  }, [client, load, logout, projectId]);

  const send = useCallback(async () => {
    const text = draft.trim();
    if (text === '') return;
    // The first conversation is created lazily, on the first message — the
    // panel opening must not mint a session nobody has spoken into, and the
    // composer must not be hidden until one exists.
    let session = active;
    if (session === null) {
      session = await start();
      if (session === null) return;
    }
    setDraft('');
    setRows((current) => [
      ...current,
      {
        id: `local-${String(Date.now())}`,
        kind: 'message',
        role: 'user',
        text,
        created_at: new Date().toISOString(),
        has_attachments: false,
        ordinal: null,
      } as LeadTurn,
    ]);
    // A session opened a moment ago has no socket yet; the reload below
    // picks the message up either way.
    wsRef.current?.sendMessage({ sessionId: session, userId: 'owner', content: text });
  }, [active, draft, start]);

  return (
    <aside className="w-[360px] border-l-2 border-black bg-canvas flex flex-col min-h-0">
      <header className="flex items-center gap-2 px-3 py-2 border-b-2 border-black shrink-0 relative">
        <h2 className="min-w-0 truncate font-mono text-[0.68rem] font-bold uppercase tracking-wider">
          {projectName} · lead
        </h2>
        <div className="ml-auto flex items-center gap-1.5 shrink-0">
          {readOnly ? null : (
            <button
              type="button"
              aria-label="Start a new conversation with the lead"
              title="New conversation"
              disabled={busy}
              onClick={() => {
                void start();
              }}
              className="border-2 border-black rounded-md bg-surface px-1.5 py-0.5 disabled:opacity-50"
            >
              <RiAddLine className="text-xs" />
            </button>
          )}
          <button
            type="button"
            aria-label="Conversation history"
            title="History"
            onClick={() => {
              setShowHistory((value) => !value);
            }}
            className="border-2 border-black rounded-md bg-surface px-1.5 py-0.5"
          >
            <RiMenuLine className="text-xs" />
          </button>
          <button
            type="button"
            aria-label="Close the lead panel"
            onClick={onClose}
            className="text-ink-soft hover:text-ink"
          >
            <RiCloseLine />
          </button>
        </div>

        {showHistory ? (
          <ul className="absolute right-2 top-[calc(100%+4px)] z-40 w-[280px] max-h-[320px] overflow-y-auto bg-surface border-2 border-black rounded-md shadow-brutal">
            {conversations.length === 0 ? (
              <li className="px-3 py-2 font-mono text-[0.66rem] text-ink-soft">
                No conversations yet
              </li>
            ) : null}
            {conversations.map((conversation, index) => (
              <li key={conversation.session_id}>
                <button
                  type="button"
                  onClick={() => {
                    setActive(conversation.session_id);
                    setShowHistory(false);
                  }}
                  className={`w-full text-left flex items-baseline gap-2 px-3 py-2 border-b border-black/20 font-mono text-[0.66rem] ${
                    conversation.session_id === active
                      ? 'bg-selected font-bold'
                      : 'hover:bg-canvas'
                  }`}
                >
                  <span className="truncate">
                    {conversationLabel(conversation, index, conversations.length)}
                  </span>
                  <span className="ml-auto shrink-0 text-[0.56rem] text-ink-soft tabular-nums">
                    {new Date(conversation.last_active_ms).toLocaleDateString()}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        ) : null}
      </header>

      <div className="flex-1 overflow-y-auto p-2 flex flex-col gap-2">
        {error !== null && isMissingLead(error) ? (
          // Not a failure the operator can retry past: this board predates
          // the lead being seeded with the project, so there is nobody for
          // the conversation to bind to. Say that, and offer the fix.
          <div className="border-2 border-warn bg-warn/10 rounded-md px-2 py-2">
            <p className="font-mono text-[0.66rem] font-bold text-warn">This board has no lead.</p>
            <p className="mt-1 font-mono text-[0.62rem] leading-snug">
              It was opened before boards came with a coordinator, so there is no <b>@lead</b> to
              talk to — and unassigned cards are not being triaged either.
            </p>
            {readOnly || onHireLead === undefined ? null : (
              <button
                type="button"
                onClick={onHireLead}
                className="mt-2 border-2 border-black bg-brand rounded-md px-2 py-0.5 font-mono text-[0.66rem] font-bold"
              >
                Add one
              </button>
            )}
            <p className="mt-1.5 font-mono text-[0.58rem] text-ink-soft leading-snug">
              Name it <b>Lead</b> and its handle becomes <b>@lead</b>.
            </p>
          </div>
        ) : error !== null ? (
          <p className="border-2 border-err text-err rounded-md px-2 py-1 font-mono text-[0.66rem] break-words">
            {error}
          </p>
        ) : null}
        {older.has ? (
          <button
            type="button"
            disabled={loadingOlder}
            onClick={() => {
              void loadOlder();
            }}
            className="self-center font-mono text-[0.62rem] text-ink-soft underline cursor-pointer disabled:opacity-50"
          >
            {loadingOlder ? 'Loading…' : 'Load earlier messages'}
          </button>
        ) : null}
        {items.length === 0 ? (
          <p className="m-auto text-center font-mono text-[0.66rem] text-ink-soft leading-snug">
            Nothing said yet. Start a conversation to plan with the lead — it turns what you
            agree into cards on this board.
          </p>
        ) : null}
        {items.map((item) =>
          item.kind === 'work' ? (
            <WorkBlock key={item.id} row={item.row} />
          ) : item.kind === 'event' ? (
            <EventCard
              key={item.id}
              item={item}
              projectId={projectId}
              onOpenIssue={onClose}
            />
          ) : (
            <div
              key={item.id}
              className={`max-w-[85%] rounded-md px-2 py-1.5 whitespace-pre-wrap break-words font-sans text-[0.8rem] ${
                item.role === 'user'
                  ? 'self-end border-2 border-black bg-brand/60'
                  : 'self-start bg-surface border-2 border-black/20'
              }`}
            >
              {item.text}
            </div>
          ),
        )}
        <div ref={bottom} />
      </div>

      {readOnly ? null : (
        <form
          className="shrink-0 border-t-2 border-black p-2 flex gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            void send();
          }}
        >
          <textarea
            value={draft}
            rows={2}
            placeholder="Ask the lead to plan something…"
            onChange={(event) => {
              setDraft(event.target.value);
            }}
            onKeyDown={(event) => {
              if (event.key === 'Enter' && !event.shiftKey) {
                event.preventDefault();
                void send();
              }
            }}
            className="flex-1 border-2 border-black rounded-md px-2 py-1 font-sans text-[0.8rem] resize-none"
          />
          <button
            type="submit"
            disabled={draft.trim() === '' || busy}
            className="shrink-0 self-end border-2 border-black rounded-md bg-brand px-3 py-1 font-mono text-[0.68rem] font-bold disabled:opacity-50"
          >
            Send
          </button>
        </form>
      )}
    </aside>
  );
}
