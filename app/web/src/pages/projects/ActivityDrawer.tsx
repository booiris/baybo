import { useEffect, useState } from 'react';
import { RiCloseLine } from 'react-icons/ri';

import { useAdminClient, useAuth } from '../../api/auth';
import { fetchFeed } from './api';
import { describeFeedEntry, eventAgo, feedActorLabel, type FeedEntry } from './timelineModel';

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
      <div className="flex-1 overflow-y-auto p-2 flex flex-col gap-1">
        {error !== null ? (
          <p className="border-2 border-err text-err rounded-md px-2 py-1 font-mono text-[0.66rem] break-words">
            {error}
          </p>
        ) : null}
        {!loading && error === null && events.length === 0 ? (
          <p className="m-auto text-center font-mono text-[0.66rem] text-ink-soft">
            Nothing has happened on this board yet.
          </p>
        ) : null}
        {events.map((event, index) => {
          const card = event.number;
          const line = (
            <>
              <div className="flex items-baseline gap-1.5 font-mono text-[0.6rem] text-ink-soft">
                {card != null ? <span className="font-bold text-ink">#{card}</span> : null}
                <span>{feedActorLabel(event)}</span>
                <span className="ml-auto tabular-nums">{eventAgo(event.created_at_ms, now)}</span>
              </div>
              <p className="font-mono text-[0.66rem] leading-snug break-words">
                {describeFeedEntry(event)}
              </p>
            </>
          );
          // A hire belongs to the board, so there is no card to open — it
          // renders as a plain row rather than a button that goes nowhere.
          return card == null ? (
            <div
              key={`${event.created_at_ms}-${index}`}
              className="border-2 border-black/15 rounded-md px-2 py-1.5 bg-surface"
            >
              {line}
            </div>
          ) : (
            <button
              key={`${event.created_at_ms}-${index}`}
              type="button"
              onClick={() => {
                onOpenIssue(card);
              }}
              className="text-left border-2 border-black/15 hover:border-black rounded-md px-2 py-1.5 bg-surface"
            >
              {line}
            </button>
          );
        })}
      </div>
    </aside>
  );
}
