import { useEffect, useState } from 'react';
import { RiCloseLine } from 'react-icons/ri';

import { useAdminClient, useAuth } from '../../api/auth';
import { fetchFeed } from './api';
import { eventAgo, feedLine, feedTone, TONE_DOT, type FeedEntry } from './timelineModel';

export function ActivityDrawer({
  projectId,
  refreshKey,
  onClose,
  onOpenIssue,
}: {
  projectId: string;
  refreshKey: number;
  onClose: () => void;
  onOpenIssue: (number: number) => void;
}) {
  const client = useAdminClient();
  const { logout } = useAuth();
  const [events, setEvents] = useState<FeedEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let canceled = false;
    void fetchFeed(client, projectId, null).then((outcome) => {
      if (canceled) return;
      setLoading(false);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        return;
      }
      setError(null);
      setEvents(outcome.value);
    });
    return () => {
      canceled = true;
    };
  }, [client, logout, projectId, refreshKey]);

  const now = Date.now();
  return (
    <aside className="w-[320px] border-l-2 border-black bg-canvas flex flex-col min-h-0">
      <header className="flex items-center gap-2 px-3 py-2 border-b-2 border-black shrink-0">
        <h2 className="font-mono text-[0.68rem] font-bold uppercase tracking-wider">Activity</h2>
        <button
          type="button"
          aria-label="Close the activity feed"
          onClick={onClose}
          className="ml-auto text-ink-soft hover:text-ink"
        >
          <RiCloseLine />
        </button>
      </header>
      <div className="flex-1 overflow-y-auto overscroll-none flex flex-col">
        {error !== null ? (
          <p className="m-2 border-2 border-err text-err rounded-md px-2 py-1 font-mono text-[0.66rem] break-words">
            {error}
          </p>
        ) : null}
        {!loading && error === null && events.length === 0 ? (
          <p className="m-auto px-4 text-center font-mono text-[0.66rem] text-ink-soft">
            Nothing has happened on this board yet.
          </p>
        ) : null}
        {events.map((event, index) => {
          const card = event.number;
          // A stream, not a stack of cards. Each line is one sentence with a
          // tone dot in front of it, so a failure and a hire are told apart
          // down the left edge before either is read — the same alphabet the
          // card's own rail uses.
          const line = (
            <>
              <span
                aria-hidden
                className={`mt-1 h-2 w-2 shrink-0 rounded-full ${TONE_DOT[feedTone(event)]}`}
              />
              <span className="min-w-0 font-mono text-[0.66rem] leading-snug break-words">
                {/* The plain runs stay bare text nodes. Wrapped in spans they
                    each become their own contribution to the accessible name,
                    which is trimmed per element — the row announced itself as
                    "@dev-1opened#7", one run-on token. */}
                {feedLine(event).map((span, at) =>
                  span.strong === true ? (
                    <b key={at} className="font-bold">
                      {span.text}
                    </b>
                  ) : (
                    span.text
                  ),
                )}
              </span>
              {/* The time is right-aligned away from the sentence, but the
                  accessible name has no columns — without this it announces
                  "#7" and "2h" as "#72h". */}
              <span className="sr-only">, </span>
              <span className="ml-auto shrink-0 font-mono text-[0.56rem] text-ink-soft tabular-nums">
                {eventAgo(event.created_at_ms, now)}
              </span>
            </>
          );
          const shape = 'flex gap-2 px-3 py-2 border-b border-black/15 text-left';
          // A hire belongs to the board, so there is no card to open — it
          // renders as a plain row rather than a button that goes nowhere.
          return card == null ? (
            <div key={`${event.created_at_ms}-${index}`} className={shape}>
              {line}
            </div>
          ) : (
            <button
              key={`${event.created_at_ms}-${index}`}
              type="button"
              onClick={() => {
                onOpenIssue(card);
              }}
              className={`${shape} hover:bg-brand/15`}
            >
              {line}
            </button>
          );
        })}
      </div>
    </aside>
  );
}
