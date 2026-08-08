import { useRef, useState } from 'react';

import { Button } from '../../components/Button';
import type { Agent, Issue, IssueRun } from './boardModel';
import {
  applyMention,
  mentionCandidates,
  mentionHint,
  mentionQuery,
} from './mentionModel';
import {
  actorLabel,
  commentHint,
  describeEvent,
  eventShape,
  eventTime,
  pendingApprovals,
  type IssueEvent,
} from './timelineModel';

function Note({ event, now }: { event: IssueEvent; now: number }) {
  const sentence = describeEvent(event.body);
  if (sentence == null) return null;
  return (
    <li className="flex items-baseline gap-2 py-1 font-mono text-[0.7rem] text-ink-soft">
      <span className="font-bold text-ink">{actorLabel(event)}</span>
      <span className="min-w-0 break-words">{sentence}</span>
      <span className="ml-auto shrink-0 tabular-nums text-[0.62rem] opacity-60">
        {eventTime(event.created_at_ms, now)}
      </span>
    </li>
  );
}

function Comment({ event, now }: { event: IssueEvent; now: number }) {
  const text = event.body.kind === 'comment' ? event.body.text : '';
  return (
    <li className="py-1.5">
      <div className="border-2 border-black rounded-md bg-surface px-3 py-2 shadow-brutal-sm">
        <div className="flex items-baseline gap-2 font-mono text-[0.66rem]">
          <span className="font-bold">{actorLabel(event)}</span>
          <span className="ml-auto tabular-nums text-[0.62rem] text-ink-soft">
            {eventTime(event.created_at_ms, now)}
          </span>
        </div>
        <p className="mt-1 whitespace-pre-wrap break-words font-sans text-[0.82rem]">{text}</p>
      </div>
    </li>
  );
}

function PendingApproval({
  approval,
  onResolve,
  busy,
}: {
  approval: { callId: string; tool: string; summary: string };
  onResolve: (callId: string, decision: 'approve' | 'approve_always' | 'deny') => void;
  busy: boolean;
}) {
  return (
    <div className="mt-3 border-[3px] border-warn rounded-md bg-warn/10 px-3 py-2 shadow-brutal-sm">
      <p className="font-mono text-[0.62rem] font-bold uppercase tracking-wider text-warn">
        Waiting on you — {approval.tool}
      </p>
      <p className="mt-1 whitespace-pre-wrap break-words font-mono text-[0.74rem]">
        {approval.summary}
      </p>
      <div className="mt-2 flex flex-wrap gap-2">
        <button
          type="button"
          disabled={busy}
          onClick={() => {
            onResolve(approval.callId, 'approve');
          }}
          className="border-2 border-black rounded-md px-2 py-0.5 font-mono text-[0.66rem] font-bold bg-brand text-ink disabled:opacity-50"
        >
          Approve
        </button>
        <button
          type="button"
          disabled={busy}
          onClick={() => {
            onResolve(approval.callId, 'approve_always');
          }}
          className="border-2 border-black rounded-md px-2 py-0.5 font-mono text-[0.66rem] bg-surface disabled:opacity-50"
        >
          Approve always
        </button>
        <button
          type="button"
          disabled={busy}
          onClick={() => {
            onResolve(approval.callId, 'deny');
          }}
          className="border-2 border-err rounded-md px-2 py-0.5 font-mono text-[0.66rem] text-err bg-surface disabled:opacity-50"
        >
          Deny
        </button>
      </div>
    </div>
  );
}

export function Timeline({
  events,
  issue,
  runs,
  onComment,
  onResolveApproval,
  team = [],
  busy,
}: {
  events: IssueEvent[];
  issue: Issue;
  runs: IssueRun[];
  onComment: (text: string) => void;
  onResolveApproval: (callId: string, decision: 'approve' | 'approve_always' | 'deny') => void;
  team?: Agent[];
  busy: boolean;
}) {
  const [draft, setDraft] = useState('');
  const [caret, setCaret] = useState(0);
  const box = useRef<HTMLTextAreaElement | null>(null);
  const query = mentionQuery(draft, caret);
  const candidates = query === null ? [] : mentionCandidates(team, query.prefix);

  function complete(handle: string) {
    if (query === null) return;
    const next = applyMention(draft, query, handle);
    setDraft(next.text);
    setCaret(next.caret);
    requestAnimationFrame(() => {
      box.current?.setSelectionRange(next.caret, next.caret);
      box.current?.focus();
    });
  }
  const now = Date.now();
  const trimmed = draft.trim();
  const waiting = pendingApprovals(events);

  return (
    <section className="mt-6 border-t-2 border-black/20 pt-4">
      <h2 className="font-mono text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft">
        Timeline
      </h2>

      {waiting.map((approval) => (
        <PendingApproval
          key={approval.callId}
          approval={approval}
          onResolve={onResolveApproval}
          busy={busy}
        />
      ))}

      {events.length === 0 ? (
        <p className="mt-2 font-mono text-[0.7rem] text-ink-soft">Nothing has happened yet.</p>
      ) : (
        <ul className="mt-2">
          {events.map((event) =>
            eventShape(event) === 'comment' ? (
              <Comment key={event.id} event={event} now={now} />
            ) : (
              <Note key={event.id} event={event} now={now} />
            ),
          )}
        </ul>
      )}

      <div className="mt-3">
        <textarea
          ref={box}
          className="w-full min-h-[72px] bg-canvas border-2 border-black rounded-md px-3 py-2 font-sans text-[0.82rem] outline-none resize-y"
          placeholder="Say something about this issue…"
          value={draft}
          onChange={(event) => {
            setDraft(event.target.value);
            setCaret(event.target.selectionStart);
          }}
          onSelect={(event) => {
            setCaret(event.currentTarget.selectionStart);
          }}
          onKeyDown={(event) => {
            if (event.key === 'Tab' && candidates.length > 0) {
              event.preventDefault();
              complete(candidates[0].handle);
              return;
            }
            if (event.key === 'Enter' && (event.metaKey || event.ctrlKey) && trimmed.length > 0) {
              event.preventDefault();
              onComment(trimmed);
              setDraft('');
            }
          }}
        />
        {candidates.length === 0 ? null : (
          <ul className="mt-1 flex flex-wrap gap-1">
            {candidates.map((agent, index) => (
              <li key={agent.id}>
                <button
                  type="button"
                  onClick={() => {
                    complete(agent.handle);
                  }}
                  className={`border-2 rounded-md px-1.5 py-0.5 font-mono text-[0.62rem] ${
                    index === 0 ? 'border-black bg-brand/40' : 'border-black/30 bg-surface'
                  }`}
                >
                  @{agent.handle}
                </button>
              </li>
            ))}
          </ul>
        )}
        <div className="mt-2 flex items-center gap-3">
          <Button
            className="!px-4 !py-1.5 !text-[0.75rem]"
            variant="primary"
            disabled={trimmed.length === 0 || busy}
            onClick={() => {
              onComment(trimmed);
              setDraft('');
            }}
          >
            {busy ? 'Sending…' : 'Comment'}
          </Button>
          <span className="font-mono text-[0.66rem] text-ink-soft">
            {mentionHint(issue, draft, team) ?? commentHint(issue, runs)}
          </span>
        </div>
      </div>
    </section>
  );
}
