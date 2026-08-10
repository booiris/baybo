import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { RiSendPlane2Line } from 'react-icons/ri';

import { MarkdownBody } from '../ChatPage';
import { Avatar } from './Avatar';
import { caretPoint, type CaretPoint } from './caret';
import { generatedPortrait, type Portrait } from './portrait';
import type { Agent, Issue, IssueRun } from './boardModel';
import {
  applyMention,
  mentionCandidates,
  mentionHint,
  mentionQuery,
} from './mentionModel';
import {
  actorLabel,
  approvalAsks,
  commentHint,
  describeEvent,
  eventShape,
  eventTime,
  eventTone,
  pendingApprovals,
  type IssueEvent,
  type PendingApproval as PendingApprovalRow,
  type Tone,
} from './timelineModel';

/// How tall the composer may grow before it scrolls instead. The chat
/// composer's number, so the two boxes behave the same.
const COMPOSER_MAX_PX = 200;

const TONE_DOT: Record<Tone, string> = {
  ok: 'bg-ok',
  err: 'bg-err',
  warn: 'bg-warn',
  info: 'bg-info',
  brand: 'bg-brand-hover',
  muted: 'bg-ink-soft',
};

/// One node on the rail. The dot is a real element rather than a `::before`
/// so its colour can come from the entry it belongs to.
function RailDot({ tone, size, top }: { tone: Tone; size: 'sm' | 'lg'; top: string }) {
  return (
    <span
      aria-hidden
      className={`absolute rounded-full ${TONE_DOT[tone]} ${
        size === 'lg' ? '-left-[6px] w-2.5 h-2.5' : '-left-[5px] w-2 h-2'
      }`}
      style={{ top }}
    />
  );
}

function Note({ event, now }: { event: IssueEvent; now: number }) {
  const sentence = describeEvent(event.body);
  if (sentence == null) return null;
  return (
    <li className="relative flex items-baseline gap-2.5 py-1.5 pl-3.5 font-mono text-[0.7rem] text-ink-soft">
      <RailDot tone={eventTone(event.body)} size="sm" top="12px" />
      <span className="font-bold text-ink">{actorLabel(event)}</span>
      <span className="min-w-0 break-words">{sentence}</span>
      <span className="ml-auto shrink-0 tabular-nums text-[0.62rem] opacity-60">
        {eventTime(event.created_at_ms, now)}
      </span>
    </li>
  );
}

function Comment({
  event,
  now,
  portrait,
}: {
  event: IssueEvent;
  now: number;
  portrait: Portrait;
}) {
  const text = event.body.kind === 'comment' ? event.body.text : '';
  // The operator's own words carry the chat page's amber user bubble; an
  // agent's report is surface. Rendering both identically made a timeline
  // where you could not tell what you had asked for from what came back.
  const mine = event.actor.kind === 'user';
  const who =
    event.actor.kind === 'agent' ? event.actor.handle : event.actor.kind === 'user' ? 'you' : 'board';
  return (
    <li className="relative flex gap-2.5 py-2 pl-3.5">
      <RailDot tone="muted" size="lg" top="14px" />
      <Avatar handle={who} src={portrait(event.actor.kind === 'agent' ? event.actor.id : null)} />
      {/* Three quarters of the timeline, and a maximum rather than a width: a
          run report is hundreds of words and needs the cap, while "ok" has no
          business being stretched to meet it.
          The cap sits on this column, not on the bubble inside it. A
          percentage resolves against the containing block, and the bubble's
          containing block is this column — which is a flex item sized by its
          own content. Capping the bubble at a fraction of a box that was
          measured *from* the bubble is circular, and browsers settle it by
          squeezing the bubble inside a column that keeps the full width. The
          column's own containing block is the row, which is block-level and
          full width, so the fraction here means what it says. */}
      <div className="min-w-0 max-w-3/4">
        <div className="flex items-baseline gap-2 font-mono text-[0.6rem] font-bold text-ink-soft">
          <span>{actorLabel(event)} · comment</span>
          <span className="tabular-nums font-normal">{eventTime(event.created_at_ms, now)}</span>
        </div>
        <div
          className={`mt-1 border-2 border-black rounded-md px-3 py-2 shadow-brutal-xs chat-prose timeline-prose break-words ${
            mine ? 'bg-brand/60' : 'bg-surface'
          }`}
        >
          <MarkdownBody text={text} />
        </div>
      </div>
    </li>
  );
}

/// The command a run was gated on, in its own box. What is being approved is
/// the whole question, so it does not get to share a paragraph with the ask.
function ApprovalCommand({ tool, summary }: { tool: string; summary: string }) {
  return (
    <pre className="mt-1.5 overflow-x-auto rounded border border-black/30 bg-canvas px-2.5 py-1.5 font-mono text-[0.7rem] whitespace-pre-wrap break-words">
      {`${tool} · ${summary}`}
    </pre>
  );
}

/// A settled approval, frozen where it happened — including what was let
/// through. The mockup's second state: the decision stays on the timeline
/// rather than collapsing into a sentence, because "who approved what, and
/// when" is the question a reader comes back to the card with.
function ResolvedApproval({
  event,
  ask,
  now,
}: {
  event: IssueEvent;
  ask: PendingApprovalRow | undefined;
  now: number;
}) {
  if (event.body.kind !== 'approval_resolved') return null;
  const approved = event.body.decision !== 'deny';
  return (
    <li className="relative py-2 pl-3.5">
      <RailDot tone={approved ? 'ok' : 'err'} size="lg" top="16px" />
      <div
        className={`max-w-[480px] border-2 border-black rounded-md bg-surface px-3.5 py-2.5 shadow-brutal-xs ${
          approved ? 'border-l-[6px] border-l-ok' : 'border-l-[6px] border-l-err'
        }`}
      >
        <p
          className={`font-mono text-[0.62rem] font-bold uppercase tracking-wider ${
            approved ? 'text-ok' : 'text-err'
          }`}
        >
          {approved ? '✓ Approved · run continued' : '✕ Denied'}
        </p>
        {ask === undefined ? null : <ApprovalCommand tool={ask.tool} summary={ask.summary} />}
        <p className="mt-1.5 font-mono text-[0.56rem] text-ink-soft">
          {actorLabel(event)} · {eventTime(event.created_at_ms, now)}
        </p>
      </div>
    </li>
  );
}

function PendingApproval({
  approval,
  onResolve,
  busy,
}: {
  approval: PendingApprovalRow;
  onResolve: (callId: string, decision: 'approve' | 'approve_always' | 'deny') => void;
  busy: boolean;
}) {
  return (
    <li className="relative py-2 pl-3.5">
      <RailDot tone="warn" size="lg" top="16px" />
      <div className="max-w-[480px] border-2 border-black border-l-[6px] border-l-warn rounded-md bg-surface px-3.5 py-2.5 shadow-brutal-sm">
        <p className="font-mono text-[0.62rem] font-bold uppercase tracking-wider text-warn">
          ⚑ Waiting on you
          {approval.attempt == null ? '' : ` · run #${approval.attempt}`}
        </p>
        <ApprovalCommand tool={approval.tool} summary={approval.summary} />
        <div className="mt-2 flex flex-wrap items-center gap-2">
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
          <span className="font-mono text-[0.56rem] text-ink-soft">
            Mirrored into the activity feed · closes itself if it times out
          </span>
        </div>
      </div>
    </li>
  );
}

export function Timeline({
  events,
  issue,
  runs,
  onComment,
  onResolveApproval,
  team = [],
  portrait = generatedPortrait,
  busy,
}: {
  events: IssueEvent[];
  issue: Issue;
  runs: IssueRun[];
  onComment: (text: string) => void;
  onResolveApproval: (callId: string, decision: 'approve' | 'approve_always' | 'deny') => void;
  team?: Agent[];
  /// Resolved faces from the page that owns the roster. Left alone, every
  /// speaker gets the face generated from its id — right for an agent
  /// nobody has uploaded one for, which is most of them.
  portrait?: Portrait;
  busy: boolean;
}) {
  const [draft, setDraft] = useState('');
  const [caret, setCaret] = useState(0);
  // Esc closes the picker without closing anything else. Cleared whenever the
  // caret moves onto a different mention, so dismissing one does not silence
  // the next.
  const [dismissed, setDismissed] = useState(false);
  const [point, setPoint] = useState<CaretPoint | null>(null);
  const box = useRef<HTMLTextAreaElement | null>(null);
  const query = mentionQuery(draft, caret);
  const candidates = query === null || dismissed ? [] : mentionCandidates(team, query.prefix);

  // Auto-grow up to a cap, as the chat composer does: idle is one line, a
  // multi-paragraph draft grows into it, and past the cap it scrolls rather
  // than eating the timeline above.
  useLayoutEffect(() => {
    const field = box.current;
    if (field === null) return;
    field.style.height = 'auto';
    field.style.height = `${String(Math.min(field.scrollHeight, COMPOSER_MAX_PX))}px`;
  }, [draft]);

  // Measured after layout, from the field itself: the popup follows the caret
  // through wraps, scrolls and resizes, none of which can be worked out from
  // the offset alone.
  //
  // The dependency is the mention's **offset**, not the query object —
  // `mentionQuery` builds a fresh one every render, so depending on it made
  // the effect run on every render, and the effect's own `setPoint` caused
  // the next one.
  const mentionAt = query?.start ?? null;
  useLayoutEffect(() => {
    if (mentionAt === null || box.current === null) {
      setPoint(null);
      return;
    }
    setPoint(caretPoint(box.current, mentionAt));
  }, [draft, caret, mentionAt]);

  // A dismissal belongs to the mention it dismissed, not to the composer.
  useEffect(() => {
    setDismissed(false);
  }, [mentionAt]);

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
  function send() {
    onComment(draft.trim());
    setDraft('');
  }
  const now = Date.now();
  const trimmed = draft.trim();
  // Approvals render where they happened rather than hoisted above the
  // history: a decision pulled out of its run loses the run it belonged to.
  const asks = approvalAsks(events);
  const open = new Set(pendingApprovals(events).map((approval) => approval.callId));
  const mention = mentionHint(issue, draft, team);
  const hint = mention ?? commentHint(issue, runs, team);

  return (
    // A column that takes whatever height is left in the pane, so the entry
    // list can absorb the slack and leave the composer at the foot even on a
    // card with two lines of history. `sticky` alone only pins once the page
    // is long enough to scroll.
    <section className="mt-2 flex flex-1 flex-col border-t border-black/20 pt-4">
      <h2 className="font-mono text-[0.62rem] font-bold uppercase tracking-[0.14em] text-ink-soft">
        Activity
      </h2>

      <div className="flex-1">
        {events.length === 0 ? (
          <p className="mt-2 font-mono text-[0.7rem] text-ink-soft">Nothing has happened yet.</p>
        ) : (
          <ul
            // `pr-4` is the composer's own right inset — its 2px border plus its
            // 14px padding — so a row's timestamp lands on the same line as the
            // send button rather than 16px past it.
            className="mt-2 ml-[5px] pr-4 border-l-2 border-black/20"
          >
            {events.map((event) => {
              if (eventShape(event) === 'comment') {
                return <Comment key={event.id} event={event} now={now} portrait={portrait} />;
              }
              if (event.body.kind === 'approval_resolved') {
                return (
                  <ResolvedApproval
                    key={event.id}
                    event={event}
                    ask={asks.get(event.body.call_id)}
                    now={now}
                  />
                );
              }
              if (event.body.kind === 'approval_requested') {
                // Answered prompts are not narrated twice: the resolved card
                // below carries the same command with its verdict on it.
                if (!open.has(event.body.call_id)) return null;
                const approval = asks.get(event.body.call_id);
                return approval === undefined ? null : (
                  <PendingApproval
                    key={event.id}
                    approval={approval}
                    onResolve={onResolveApproval}
                    busy={busy}
                  />
                );
              }
              return <Note key={event.id} event={event} now={now} />;
            })}
          </ul>
        )}
      </div>

      {/* Pinned to the bottom of the pane: the timeline is the long thing on
          this page, and a composer that scrolls away with it means reading
          the history and replying to it are two different scroll positions. */}
      <div className="sticky bottom-0 z-10 mt-8">
        {/* The timeline scrolls *behind* the composer, and a flat opaque band
            above it cuts entries off mid-line — a white edge across the page.
            The chat thread's answer, reused: a page-colour gradient, opaque
            under and below the box and fading out above it, so an entry
            dissolves as it slides in. `-bottom-5` reaches under the pane's own
            bottom padding so nothing shows below the box; `-inset-x-2`
            overhangs the card's 2px shadow. */}
        <div
          aria-hidden
          className="pointer-events-none absolute -inset-x-2 -bottom-5 -top-16 bg-linear-to-t from-surface from-45% to-transparent"
        />
        <div className="relative border-2 border-black rounded-2xl bg-white shadow-brutal">
          <div className="relative">
            <textarea
              ref={box}
              rows={1}
              className="w-full resize-none bg-transparent px-4 pt-3 pb-1.5 font-sans text-sm leading-relaxed focus:outline-none placeholder:text-ink-soft/70"
              placeholder="Comment…  (@ a teammate, Shift+Enter for newline)"
              value={draft}
              onChange={(event) => {
                setDraft(event.target.value);
                setCaret(event.target.selectionStart);
              }}
              onSelect={(event) => {
                setCaret(event.currentTarget.selectionStart);
              }}
              onKeyDown={(event) => {
                if (event.key === 'Escape' && candidates.length > 0) {
                  event.preventDefault();
                  setDismissed(true);
                  return;
                }
                if ((event.key === 'Tab' || event.key === 'Enter') && candidates.length > 0) {
                  event.preventDefault();
                  complete(candidates[0].handle);
                  return;
                }
                if (event.key === 'Enter' && !event.shiftKey && trimmed.length > 0) {
                  event.preventDefault();
                  send();
                }
              }}
            />
            {/* At the caret, not under the box. A list pinned to the bottom of
                the composer makes you look away from what you are typing to
                find out what you are choosing between. It opens **upward**:
                the composer is at the bottom of the page, so downward is off
                the screen. */}
            {candidates.length === 0 || point === null ? null : (
              <ul
                className="absolute z-30 max-h-40 min-w-[9rem] overflow-y-auto border-2 border-black rounded-md bg-surface py-0.5 shadow-brutal-sm"
                style={{ bottom: point.fieldHeight - point.top + 4, left: point.left }}
              >
                {candidates.map((agent, index) => (
                  <li key={agent.id}>
                    <button
                      type="button"
                      onMouseDown={(event) => {
                        // The textarea must keep focus, or completing puts the
                        // caret back at the start of the draft.
                        event.preventDefault();
                      }}
                      onClick={() => {
                        complete(agent.handle);
                      }}
                      className={`flex w-full items-baseline gap-2 px-2 py-1 text-left font-mono text-[0.66rem] ${
                        index === 0 ? 'bg-brand/40 font-bold' : 'hover:bg-canvas'
                      }`}
                    >
                      <span>@{agent.handle}</span>
                      <span className="truncate text-[0.58rem] text-ink-soft">{agent.name}</span>
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </div>
          <div className="flex items-center justify-end gap-2 px-2.5 pb-2 pt-0.5">
            <button
              type="button"
              aria-label="Comment"
              title={hint}
              disabled={trimmed.length === 0 || busy}
              onClick={send}
              className="shrink-0 h-8 w-8 flex items-center justify-center bg-brand text-ink border-2 border-black rounded-full shadow-brutal-xs hover:bg-brand-hover active:translate-x-[1px] active:translate-y-[1px] active:shadow-none disabled:opacity-50 disabled:cursor-not-allowed cursor-pointer"
            >
              <RiSendPlane2Line className="text-base" />
            </button>
          </div>
        </div>
      </div>
    </section>
  );
}
