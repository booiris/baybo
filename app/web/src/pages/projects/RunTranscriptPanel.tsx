import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { Link } from 'react-router-dom';
import { RiCloseLine, RiGitMergeLine, RiLoader4Line } from 'react-icons/ri';

import { useAdminClient, useAuth } from '../../api/auth';
import { atBottom, useHoldBottomEdge } from '../../components/scrollPin';
import {
  OLDER_SCROLL_SLACK_PX,
  ThreadView,
  UNDERFILL_SLACK_PX,
  compactionDividerKeys,
  shouldAutoLoadOlder,
  foldAdjacentWork,
  joinKeptHead,
  openLiveTail,
  rowsAboveFloor,
  transcriptItemToRow,
  type TranscriptRow,
} from '../ChatPage';
import { fetchRunTranscript } from './api';
import {
  RUN_CHIP,
  RUN_POLL_MS,
  RUN_TONE,
  RUN_TRANSCRIPT_PAGE,
  RUN_TRIGGER_LABEL,
  runDuration,
  runIsLive,
  unsettledRun,
  type Agent,
  type IssueRun,
} from './boardModel';
import { formatUsd } from './budgetModel';
import { Avatar } from './Avatar';
import type { Portrait } from './portrait';
import { runsInSession, splitRowsByRun } from './runThread';
import { handleOf } from './teamModel';

/// The rows' geometry before there are rows — the same reason the activity
/// drawer has one. The panel slides in ahead of its first answer, so without
/// it the frame and the conversation land as two separate events.
const SKELETON_WIDTHS = ['64%', '38%', '81%', '52%'];

function TranscriptSkeleton() {
  return (
    <div role="status" className="flex flex-col gap-3">
      <span className="sr-only">Loading this run’s conversation…</span>
      {SKELETON_WIDTHS.map((width, index) => (
        <div key={index} aria-hidden className="motion-safe:animate-pulse">
          <span className="block h-3 rounded-sm bg-ink-soft/15" style={{ width }} />
        </div>
      ))}
    </div>
  );
}

/// The seam between two runs of one session — a rule with the next attempt's
/// name on it.
///
/// In flow, deliberately not `sticky`. Stuck to the top it was a bar floating
/// over the conversation with the text sliding under it, and it said what the
/// panel's own header two rows above already said. A boundary marker's whole
/// job is to sit AT the boundary; there is nothing for it to do once that
/// boundary has scrolled past.
function RunHeader({ run, now }: { run: IssueRun; now: number }) {
  const duration = runDuration(run, now);
  const cost = run.cost_micros != null && run.cost_micros > 0 ? formatUsd(run.cost_micros) : null;
  return (
    <div className="mb-2 flex items-center gap-2 border-b border-black/15 pb-1 font-mono text-[0.62rem]">
      <span className="shrink-0 font-bold">#{run.attempt}</span>
      <span className="min-w-0 flex-1 truncate text-ink-soft">
        {RUN_TRIGGER_LABEL[run.trigger]}
        {run.error != null && run.error.length > 0 ? (
          <span className="text-err"> · {run.error}</span>
        ) : null}
      </span>
      {duration != null ? (
        <span className="shrink-0 tabular-nums text-ink-soft">{duration}</span>
      ) : null}
      {cost != null ? <span className="shrink-0 tabular-nums text-ink-soft">{cost}</span> : null}
      <span className={`${RUN_CHIP} ${RUN_TONE[run.status]}`}>{run.status}</span>
    </div>
  );
}

/// How a run ended, when it ended without saying anything.
///
/// A turn that dies mid-work leaves a transcript that simply stops — the last
/// thing on screen is a tool step, and nothing says whether that is the end or
/// just the end of the page. The ledger beside it knows exactly: a status and,
/// when there is one, the error that killed it.
///
/// This is why the panel hands `ThreadView` no `turn`. Its own inference
/// (`isCancelledWorkAt`) paints one generic "Cancelled" over every settled
/// turn whose tail is an open block — right in the chat, where the only way
/// that happens is `/stop`, and wrong here, where a run reaches this shape by
/// being rate-limited, losing its process, or being cancelled, and the three
/// are not the same news.
function RunOutcome({ run }: { run: IssueRun }) {
  const [tone, text] =
    run.error != null && run.error.length > 0
      ? (['text-err', `Run #${String(run.attempt)} ${run.status} — ${run.error}`] as const)
      : run.status === 'cancelled'
        ? (['text-warn', `Run #${String(run.attempt)} was cancelled before it replied`] as const)
        : (['text-ink-soft', `Run #${String(run.attempt)} ended without a reply`] as const);
  return (
    <p role="status" className={`mt-1 font-mono text-[0.6rem] leading-snug ${tone}`}>
      {text}
    </p>
  );
}

/// A run, read as the conversation it was.
///
/// Same rows as the trace viewer, in the shape the agent actually worked in:
/// the ask, then what it thought and ran, then what it said back. The trace
/// page keeps everything this drops — spans, per-call tokens, the context
/// window, raw tool arguments — and is one press away in the header, because
/// this view is for following the work and that one is for auditing it.
///
/// Read-only in the strict sense: no composer, and no approve/deny. A prompt
/// waiting on an answer shows here as the step it is, and is answered on the
/// card's own timeline — which is the pane directly behind this one.
export function RunTranscriptPanel({
  projectId,
  number,
  attempt,
  sessionId,
  runs,
  agents,
  portrait,
  onClose,
}: {
  projectId: string;
  number: number;
  attempt: number;
  /// The session this run worked in — the panel's identity, and what makes
  /// pressing a second run of the same agent a swap rather than a reload.
  sessionId: string;
  /// Every run of the card. The panel keeps the ones sharing its session.
  runs: readonly IssueRun[];
  agents: Agent[];
  portrait: Portrait;
  onClose: () => void;
}) {
  const client = useAdminClient();
  const { baseUrl, token, logout } = useAuth();
  const sessionRuns = useMemo(() => runsInSession(runs, sessionId), [runs, sessionId]);
  const clicked = sessionRuns.find((run) => run.attempt === attempt) ?? null;
  /// When the work that is still going began. `null` when nothing in this
  /// session is running — and also while a live run has yet to be claimed,
  /// where there is no turn to re-open and no instant by which to tell this
  /// run's block from the previous run's.
  const liveFrom = unsettledRun(sessionRuns)?.started_at_ms ?? null;

  const [rows, setRows] = useState<TranscriptRow[]>([]);
  const [compactionPoints, setCompactionPoints] = useState<{ ordinal: number; at: string }[]>([]);
  const [oldestOrdinal, setOldestOrdinal] = useState<number | null>(null);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [olderLoading, setOlderLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /// A backfill's failure, kept apart from the one above and reported beside
  /// the button that caused it. Replacing a conversation the reader is looking
  /// at with an error box is a worse answer than the box.
  const [olderError, setOlderError] = useState<string | null>(null);
  /// The settle read's failure, reported at the foot for the same reason and
  /// in the same shape. That read is the LAST one — the poll is already torn
  /// down by the time it runs — so a blip there is not a blip: it is the
  /// run's closing words missing for good, and silence would present a
  /// truncated conversation as the whole of it.
  const [tailError, setTailError] = useState<string | null>(null);
  /// Whether the reader has paged back at all. `has_more` from the newest page
  /// is only the truth until they do — after a backfill the older page owns
  /// it, and a poll answering about its own window would put the affordance
  /// back or take it away wrongly.
  const backfilled = useRef(false);

  const pane = useRef<HTMLDivElement | null>(null);
  const panel = useRef<HTMLElement | null>(null);
  /// Each run's slice, by attempt — what the jump below aims at.
  const sliceOf = useRef(new Map<number, HTMLElement>());
  /// The attempt still owed a jump. A page opens on the run whose icon was
  /// pressed, not at the foot of a session that may hold four of them: the
  /// press names a run, and landing anywhere else makes the reader find it.
  /// Spent once, so a later page or poll does not drag them back to it.
  const owedJump = useRef<number | null>(attempt);
  /// Whether the reader is parked at the foot. A run is read downwards and
  /// arrives from below, so a poll follows a reader who is already there and
  /// leaves one who has scrolled back exactly where they were.
  const pinned = useRef(true);
  const holdEdge = useHoldBottomEdge(pane, pinned);
  // The panel is mounted BEFORE the rail that opens it, so without this the
  // keyboard would have to walk backwards through the entire card to reach it.
  //
  // `preventScroll` is load-bearing. This effect runs in the mount commit,
  // while the panel is still parked at `translate-x-full` — entirely to the
  // right of the `overflow-hidden` wrapper it slides into. A default focus
  // would scroll that wrapper across to reveal it and leave it there, taking
  // the card's own pane off screen with no scrollbar to bring it back.
  useEffect(() => {
    panel.current?.focus({ preventScroll: true });
  }, []);

  /// Re-read the newest page and rebuild the thread from it.
  ///
  /// A REPLACE rather than a merge, and the same stitch the chat does on a
  /// rebase: whatever the reader had paged in above this window is kept
  /// (`rowsAboveFloor`), and `joinKeptHead` fuses a turn the window cut in
  /// half instead of showing it as two cards.
  const readNewest = useCallback(
    async (first: boolean): Promise<boolean> => {
      const outcome = await fetchRunTranscript(
        client,
        projectId,
        number,
        attempt,
        null,
        RUN_TRANSCRIPT_PAGE,
      );
      if (outcome.kind === 'unauthorized') {
        logout();
        return false;
      }
      if (outcome.kind === 'failed') {
        // Only the first read replaces the thread: a poll that blips must not
        // swap a conversation the reader is looking at for an error box. What
        // a later read owes is a line, and its caller decides.
        if (first) setError(outcome.message);
        return false;
      }
      const page = outcome.value;
      setError(null);
      setTailError(null);
      const points = page.compaction_points ?? [];
      setCompactionPoints(points);
      const rebuilt = foldAdjacentWork(
        page.transcript.map((item) => transcriptItemToRow(sessionId, item)),
        points,
      );
      const taken = new Set(rebuilt.map((row) => row.key));
      setRows((prev) =>
        joinKeptHead(
          rowsAboveFloor(prev, page.oldest_ordinal ?? Number.MAX_SAFE_INTEGER, taken),
          rebuilt,
          points,
        ),
      );
      // Only ever downwards. A kept head still describes the window it was
      // paged in under, so a page whose floor has since risen must not pull
      // the backfill cursor up past rows already on screen — the next press
      // would fetch them a second time.
      setOldestOrdinal((prev) => {
        const floor = page.oldest_ordinal ?? prev;
        if (floor === null) return prev;
        return prev === null ? floor : Math.min(prev, floor);
      });
      // Gated on the backfill, not on `first`: a first read that failed set
      // nothing, and gating on it would leave `Load earlier` switched off for
      // the life of the panel with rows still above the window.
      if (!backfilled.current) setHasMore(page.has_more);
      return true;
    },
    [attempt, client, logout, number, projectId, sessionId],
  );

  useEffect(() => {
    let canceled = false;
    setLoading(true);
    void readNewest(true).then(() => {
      if (!canceled) setLoading(false);
    });
    return () => {
      canceled = true;
    };
    // The identity of the SESSION, deliberately — not `readNewest`, which is
    // rebuilt whenever a page lands, and not `attempt`, because every run of
    // one session resolves to the same page on the server. Either would blank
    // a conversation already on screen in order to re-read the very same rows.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projectId, number, sessionId]);

  // Following a live run. There is no socket here — the transcript arrives
  // over REST — so this poll IS the live view, and it stops the moment the
  // ledger settles or the tab goes away.
  useEffect(() => {
    if (liveFrom === null) return;
    const timer = window.setInterval(() => {
      if (document.hidden) return;
      void readNewest(false);
    }, RUN_POLL_MS);
    return () => {
      window.clearInterval(timer);
    };
  }, [liveFrom, readNewest]);

  // A run settles between two polls, so the words that ended it land after the
  // last one. Without this read the panel would keep whatever the final poll
  // saw: a turn the ledger has already called done, with its reply missing.
  const wasLive = useRef(liveFrom !== null);
  useEffect(() => {
    const justSettled = wasLive.current && liveFrom === null;
    wasLive.current = liveFrom !== null;
    if (!justSettled) return;
    void readNewest(false).then((ok) => {
      if (!ok) setTailError('Could not read how this run ended — reopen to try again.');
    });
  }, [liveFrom, readNewest]);

  const loadOlder = useCallback(() => {
    if (oldestOrdinal === null || olderLoading) return;
    setOlderLoading(true);
    setOlderError(null);
    // Asking for older rows is the one act that says the reader has left the
    // foot. Without this the bottom-edge hold answers the prepend by scrolling
    // straight past everything it just fetched.
    pinned.current = false;
    void fetchRunTranscript(
      client,
      projectId,
      number,
      attempt,
      oldestOrdinal,
      RUN_TRANSCRIPT_PAGE,
    ).then((outcome) => {
      setOlderLoading(false);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setOlderError(outcome.message);
        return;
      }
      const page = outcome.value;
      const older = foldAdjacentWork(
        page.transcript.map((item) => transcriptItemToRow(sessionId, item)),
        compactionPoints,
      );
      backfilled.current = true;
      // Measured HERE, beside the write it corrects, rather than when the
      // button was pressed: a live poll can commit rows in the round trip
      // between the two, and its growth would be added to the correction.
      const before = pane.current?.scrollHeight ?? 0;
      setRows((prev) => joinKeptHead(older, prev, compactionPoints));
      setOldestOrdinal(page.oldest_ordinal ?? oldestOrdinal);
      setHasMore(page.has_more);
      // Hold the reader on the row they were reading: the prepend grows the
      // box above them, and a scroller keeps its offset from the TOP.
      requestAnimationFrame(() => {
        const box = pane.current;
        if (box === null) return;
        box.scrollTop += box.scrollHeight - before;
      });
    });
  }, [
    attempt,
    client,
    compactionPoints,
    logout,
    number,
    oldestOrdinal,
    olderLoading,
    projectId,
    sessionId,
  ]);

  useEffect(() => {
    owedJump.current = attempt;
  }, [attempt]);

  const onScroll = useCallback(() => {
    const box = pane.current;
    if (box === null) return;
    pinned.current = atBottom(box);
    // Reaching the top IS the request for more, the same way the chat reads
    // one. `loadOlder` no-ops while a page is in flight or there is nothing
    // above, so firing it on every scroll event is safe.
    if (box.scrollTop <= OLDER_SCROLL_SLACK_PX) void loadOlder();
  }, [loadOlder]);

  // Land on the run that was pressed. Skipped while that run is the live one:
  // its own foot is where the work is arriving, and the panel is already held
  // there. `scrollTop` rather than `scrollIntoView`, which walks every
  // scrollable ancestor and would take the card's pane with it.
  useLayoutEffect(() => {
    const want = owedJump.current;
    const box = pane.current;
    if (want === null || loading || box === null) return;
    if (liveFrom !== null && unsettledRun(sessionRuns)?.attempt === want) {
      owedJump.current = null;
      return;
    }
    const slice = sliceOf.current.get(want);
    if (slice === undefined) return;
    owedJump.current = null;
    pinned.current = false;
    box.scrollTop = slice.offsetTop - box.offsetTop;
  }, [attempt, liveFrom, loading, rows, sessionRuns]);

  // A thread shorter than its pane cannot be scrolled, so the trigger above can
  // never fire for it — and a run that folds to two `Worked …` cards is exactly
  // that shape. Keep pulling until it fills or there is nothing left above.
  //
  // Stopped by a failure, because nothing else would stop it: `has_more` is
  // still true after one, the pane is still underfilled, and the effect runs
  // on every commit — which is a retry loop against an endpoint that just
  // said no. Scrolling to the top is what asks again.
  useLayoutEffect(() => {
    const box = pane.current;
    if (box === null || olderError !== null) return;
    if (
      shouldAutoLoadOlder({
        hasMore,
        olderLoading,
        historyLoading: loading,
        scrollHeight: box.scrollHeight,
        clientHeight: box.clientHeight,
        slackPx: UNDERFILL_SLACK_PX,
      })
    ) {
      void loadOlder();
    }
  }, [rows, hasMore, olderLoading, loading, olderError, loadOlder]);

  const now = Date.now();
  /// The thread as it should read right now.
  ///
  /// Derived, never stored. A page of persisted rows says nothing about what
  /// is in flight, so a running turn arrives collapsed and the run ledger
  /// beside it is what re-opens the block — and the moment that ledger
  /// settles, the same derivation closes it again. Held in state instead, the
  /// block would keep spinning "Working" under a header that says `done`.
  const shown = useMemo(
    () => (liveFrom === null ? rows : openLiveTail(rows, liveFrom)),
    [rows, liveFrom],
  );
  const dividers = useMemo(
    () => compactionDividerKeys(shown, compactionPoints),
    [shown, compactionPoints],
  );
  const slices = useMemo(() => splitRowsByRun(shown, sessionRuns), [shown, sessionRuns]);
  const handle = clicked === null ? '' : handleOf(agents, clicked.agent_id);
  const elapsed = clicked === null ? null : runDuration(clicked, now);

  return (
    <section
      ref={panel}
      tabIndex={-1}
      aria-label={`Run #${String(attempt)} conversation`}
      className="flex w-[min(760px,calc(100vw-26rem))] min-h-0 flex-col border-l-2 border-black bg-surface outline-none"
    >
      <header className="flex shrink-0 items-center gap-2 border-b-2 border-black bg-canvas px-3 py-2">
        {clicked !== null ? (
          <Avatar handle={handle} src={portrait(clicked.agent_id)} size="sm" />
        ) : null}
        <h2 className="font-mono text-[0.68rem] font-bold uppercase tracking-wider">
          Run #{attempt}
        </h2>
        {clicked !== null ? (
          <span className={`${RUN_CHIP} font-mono text-[0.6rem] ${RUN_TONE[clicked.status]}`}>
            {clicked.status}
          </span>
        ) : null}
        {elapsed != null ? (
          <span className="font-mono text-[0.6rem] tabular-nums text-ink-soft">{elapsed}</span>
        ) : null}
        {/* The old destination, kept as a destination rather than dropped: the
            spans, per-call tokens and context window are only there. */}
        <Link
          to={`/traces/${encodeURIComponent(sessionId)}`}
          title="Open the full trace — steps, spans, tokens"
          className="ml-auto flex items-center gap-1 font-mono text-[0.6rem] text-ink-soft hover:text-ink"
        >
          <RiGitMergeLine aria-hidden /> full trace
        </Link>
        <button
          type="button"
          aria-label="Close this run’s conversation"
          onClick={onClose}
          className="text-ink-soft hover:text-ink"
        >
          <RiCloseLine />
        </button>
      </header>

      <div
        ref={pane}
        onScroll={onScroll}
        className="relative min-h-0 flex-1 overflow-y-auto overscroll-none px-6 py-4"
      >
        {error !== null ? (
          <div className="rounded-md border-2 border-err bg-err/10 px-3 py-2 font-mono text-[0.66rem] text-err">
            {error}
          </div>
        ) : loading ? (
          <TranscriptSkeleton />
        ) : shown.length === 0 ? (
          <p className="font-mono text-[0.66rem] text-ink-soft leading-snug">
            This run has not said anything yet.
          </p>
        ) : (
          // `run-thread` is the panel's type scale, in `index.css` — the thread
          // one step down from /chat's.
          <div ref={holdEdge} className="run-thread flex flex-col gap-4">
            {/* No button: reaching the top is the request. What is left is the
                sign that it was heard — and, if it was not, why. */}
            {olderLoading ? (
              <div
                role="status"
                className="flex items-center justify-center gap-1.5 py-1 font-mono text-[0.6rem] uppercase tracking-wider text-ink-soft"
              >
                <RiLoader4Line className="animate-spin" aria-hidden />
                Earlier
              </div>
            ) : olderError !== null ? (
              <span className="text-center font-mono text-[0.58rem] text-err">{olderError}</span>
            ) : null}
            {slices.map((slice, index) => (
              <div
                key={
                  slice.run === null ? `lead-${String(index)}` : `attempt-${String(slice.run.attempt)}`
                }
                ref={(node) => {
                  const at = slice.run?.attempt;
                  if (at === undefined) return;
                  if (node === null) sliceOf.current.delete(at);
                  else sliceOf.current.set(at, node);
                }}
                className="flex flex-col"
              >
                {/* Only where it separates something. A session with one run
                    is already named by the panel's header, and a rule over the
                    first row of a page divides it from nothing. */}
                {slice.run !== null && (slices.length > 1 || index > 0) ? (
                  <RunHeader run={slice.run} now={now} />
                ) : null}
                <ThreadView
                  rows={slice.rows}
                  // Deliberately none — see `RunOutcome`. The ledger says how
                  // this run ended; the chat's guess would say "Cancelled".
                  turn={null}
                  compactionDividerBeforeKey={dividers}
                  baseUrl={baseUrl}
                  adminToken={token}
                />
                {/* Only where the conversation just stops: a run that signed
                    off with a reply has already said how it went. */}
                {slice.run !== null &&
                !runIsLive(slice.run) &&
                slice.rows[slice.rows.length - 1]?.kind === 'work' ? (
                  <RunOutcome run={slice.run} />
                ) : null}
              </div>
            ))}
            {tailError !== null ? (
              <p className="text-center font-mono text-[0.58rem] text-err">{tailError}</p>
            ) : null}
          </div>
        )}
      </div>
    </section>
  );
}
