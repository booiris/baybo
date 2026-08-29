/**
 * The incremental-poll rules for `GET /v1/traces/{session_id}`.
 *
 * A poll asks only for rows above the cursor it already holds
 * (`since_ordinal`), so the client must decide what to do with the page it
 * gets back. That decision is subtle enough to live here as a pure function
 * rather than inline in the page's fetch effect.
 */
import type { SessionMessageRow, TraceOverview } from '../../types/trace';

/** Highest ordinal in a transcript page, or `undefined` when empty — the
 *  `since_ordinal` cursor for the next incremental poll. */
export function maxOrdinal(rows: SessionMessageRow[]): number | undefined {
  if (rows.length === 0) return undefined;
  let max = rows[0].ordinal;
  for (const r of rows) if (r.ordinal > max) max = r.ordinal;
  return max;
}

/** The cursor to poll a held overview with — `undefined` for a cold start or
 *  a different session, which pulls the full transcript. */
export function pollCursor(held: TraceOverview | null | undefined, sessionId: string): number | undefined {
  if (!held || held.session_id !== sessionId) return undefined;
  return maxOrdinal(held.session_messages);
}

export type OverviewMerge =
  | { action: 'replace'; overview: TraceOverview }
  | { action: 'append'; overview: TraceOverview }
  /** The cached prefix went stale — re-pull the whole transcript. */
  | { action: 'reload' };

/**
 * Fold an overview page onto what the client already holds.
 *
 * A moved `supersede_watermark` means a compaction re-marked rows the client
 * may already be holding, so its cached prefix can no longer be trusted and
 * the whole transcript has to come again. Otherwise delta rows are strictly
 * newer than everything held and simply append; `turns` and the watermark are
 * always taken fresh (they are tiny and always complete).
 */
export function mergeOverviewPage(
  held: TraceOverview | null | undefined,
  fresh: TraceOverview,
  sinceOrdinal: number | undefined,
): OverviewMerge {
  if (sinceOrdinal == null || !held) return { action: 'replace', overview: fresh };
  if (fresh.supersede_watermark !== held.supersede_watermark) return { action: 'reload' };
  return {
    action: 'append',
    overview: { ...fresh, session_messages: [...held.session_messages, ...fresh.session_messages] },
  };
}
