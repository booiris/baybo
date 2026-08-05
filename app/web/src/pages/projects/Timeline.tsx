import { useState } from 'react';

import { Button } from '../../components/Button';
import type { Issue } from './boardModel';
import {
  actorLabel,
  commentHint,
  describeEvent,
  eventShape,
  eventTime,
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

/**
 * The issue's history and the place to add to it.
 *
 * Comments are shown; everything else is narrated in one line. The two
 * read differently on purpose — a wall of identically-shaped rows is how a
 * timeline stops being read at all.
 */
export function Timeline({
  events,
  issue,
  onComment,
  busy,
}: {
  events: IssueEvent[];
  issue: Issue;
  onComment: (text: string) => void;
  busy: boolean;
}) {
  const [draft, setDraft] = useState('');
  const now = Date.now();
  const trimmed = draft.trim();

  return (
    <section className="mt-6 border-t-2 border-black/20 pt-4">
      <h2 className="font-mono text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft">
        Timeline
      </h2>

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
          className="w-full min-h-[72px] bg-canvas border-2 border-black rounded-md px-3 py-2 font-sans text-[0.82rem] outline-none resize-y"
          placeholder="Say something about this issue…"
          value={draft}
          onChange={(event) => {
            setDraft(event.target.value);
          }}
          onKeyDown={(event) => {
            // ⌘/Ctrl+↵ sends, matching the create modal. A bare ↵ is a
            // newline: a comment is prose, and losing a half-written one to
            // a stray keystroke is worse than one extra key to send.
            if (event.key === 'Enter' && (event.metaKey || event.ctrlKey) && trimmed.length > 0) {
              event.preventDefault();
              onComment(trimmed);
              setDraft('');
            }
          }}
        />
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
          {/* What sending will do, before it is sent — the two outcomes look
              identical in the composer otherwise. */}
          <span className="font-mono text-[0.66rem] text-ink-soft">{commentHint(issue)}</span>
        </div>
      </div>
    </section>
  );
}
