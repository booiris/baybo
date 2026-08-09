import { useCallback, useEffect, useRef, useState } from 'react';
import { RiAddLine, RiCloseLine } from 'react-icons/ri';

import { useAdminClient, useAuth } from '../../api/auth';
import { ChatWs } from '../../api/chatWs';
import {
  fetchLeadConversations,
  fetchLeadMessages,
  openLeadConversation,
  type LeadConversation,
  type LeadTurn,
} from './api';

export function LeadPanel({
  projectId,
  readOnly,
  onClose,
  onBoardChanged,
}: {
  projectId: string;
  readOnly: boolean;
  onClose: () => void;
  onBoardChanged: () => void;
}) {
  const client = useAdminClient();
  const { token, baseUrl, logout } = useAuth();
  const [conversations, setConversations] = useState<LeadConversation[]>([]);
  const [active, setActive] = useState<string | null>(null);
  const [turns, setTurns] = useState<LeadTurn[]>([]);
  const [draft, setDraft] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const wsRef = useRef<ChatWs | null>(null);
  const bottom = useRef<HTMLDivElement | null>(null);

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

  useEffect(() => {
    if (active === null) {
      setTurns([]);
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
      setTurns(outcome.value);
    });
    return () => {
      canceled = true;
    };
  }, [active, client, logout]);

  useEffect(() => {
    if (token === null || active === null) return;
    const ws = new ChatWs({
      baseUrl,
      adminToken: token,
      initialSessionIds: [active],
      onFrame: (frame) => {
        if (frame.kind === 'message' && frame.session_id === active) {
          setTurns((current) => [
            ...current,
            {
              id: `live-${frame.ordinal ?? current.length}-${current.length}`,
              kind: 'message',
              role: frame.role,
              text: frame.content,
              at: new Date().toISOString(),
            },
          ]);
          if (frame.role !== 'user') onBoardChanged();
        }
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
  }, [turns]);

  const start = useCallback(async () => {
    setBusy(true);
    const outcome = await openLeadConversation(client, projectId);
    setBusy(false);
    if (outcome.kind === 'unauthorized') {
      logout();
      return;
    }
    if (outcome.kind === 'failed') {
      setError(outcome.message);
      return;
    }
    setError(null);
    setActive(outcome.value.session_id);
    await load();
  }, [client, load, logout, projectId]);

  const send = useCallback(() => {
    const text = draft.trim();
    if (text === '' || active === null || wsRef.current === null) return;
    wsRef.current.sendMessage({ sessionId: active, userId: 'owner', content: text });
    setTurns((current) => [
      ...current,
      {
        id: `local-${Date.now()}`,
        kind: 'message',
        role: 'user',
        text,
        at: new Date().toISOString(),
      },
    ]);
    setDraft('');
  }, [active, draft]);

  return (
    <aside className="w-[360px] border-l-2 border-black bg-canvas flex flex-col min-h-0">
      <header className="flex items-center gap-2 px-3 py-2 border-b-2 border-black shrink-0">
        <h2 className="font-mono text-[0.68rem] font-bold uppercase tracking-wider">@lead</h2>
        <select
          aria-label="Conversation"
          value={active ?? ''}
          onChange={(event) => {
            setActive(event.target.value === '' ? null : event.target.value);
          }}
          className="min-w-0 flex-1 border-2 border-black rounded-md bg-surface px-1.5 py-0.5 font-mono text-[0.62rem]"
        >
          {conversations.length === 0 ? <option value="">No conversations yet</option> : null}
          {conversations.map((conversation, index) => (
            <option key={conversation.session_id} value={conversation.session_id}>
              {conversation.title ??
                `Conversation ${conversations.length - index} · ${new Date(
                  conversation.last_active_ms,
                ).toLocaleDateString()}`}
            </option>
          ))}
        </select>
        {readOnly ? null : (
          <button
            type="button"
            aria-label="Start a new conversation with the lead"
            title="New conversation"
            disabled={busy}
            onClick={() => {
              void start();
            }}
            className="shrink-0 border-2 border-black rounded-md bg-surface px-1.5 py-0.5 disabled:opacity-50"
          >
            <RiAddLine className="text-xs" />
          </button>
        )}
        <button
          type="button"
          aria-label="Close the lead panel"
          onClick={onClose}
          className="shrink-0 text-ink-soft hover:text-ink"
        >
          <RiCloseLine />
        </button>
      </header>

      <div className="flex-1 overflow-y-auto p-2 flex flex-col gap-2">
        {error !== null ? (
          <p className="border-2 border-err text-err rounded-md px-2 py-1 font-mono text-[0.66rem] break-words">
            {error}
          </p>
        ) : null}
        {active === null ? (
          <p className="m-auto text-center font-mono text-[0.66rem] text-ink-soft leading-snug">
            Nothing said yet. Start a conversation to plan with the lead — it
            turns what you agree into cards on this board.
          </p>
        ) : null}
        {turns.map((turn) =>
          turn.kind === 'work' ? (
            <p
              key={turn.id}
              className="self-start font-mono text-[0.6rem] text-ink-soft italic break-words"
            >
              {turn.text === '' ? 'worked on the board' : turn.text}
            </p>
          ) : (
            <div
              key={turn.id}
              className={`max-w-[85%] rounded-md px-2 py-1.5 whitespace-pre-wrap break-words font-sans text-[0.8rem] ${
                turn.role === 'user'
                  ? 'self-end border-2 border-black bg-brand/60'
                  : 'self-start bg-surface border-2 border-black/20'
              }`}
            >
              {turn.text}
            </div>
          ),
        )}
        <div ref={bottom} />
      </div>

      {readOnly || active === null ? null : (
        <form
          className="shrink-0 border-t-2 border-black p-2 flex gap-2"
          onSubmit={(event) => {
            event.preventDefault();
            send();
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
                send();
              }
            }}
            className="flex-1 border-2 border-black rounded-md px-2 py-1 font-sans text-[0.8rem] resize-none"
          />
          <button
            type="submit"
            disabled={draft.trim() === ''}
            className="shrink-0 self-end border-2 border-black rounded-md bg-brand px-3 py-1 font-mono text-[0.68rem] font-bold disabled:opacity-50"
          >
            Send
          </button>
        </form>
      )}
    </aside>
  );
}
