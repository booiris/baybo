import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from 'react';
import { Link, useLocation, useParams } from 'react-router-dom';
import {
  RiArrowDownLine,
  RiArrowLeftLine,
  RiGitMergeLine,
  RiLoader4Line,
  RiPushpin2Fill,
  RiPushpin2Line,
  RiRefreshLine,
} from 'react-icons/ri';

import { useAdminClient, useAuth } from '../../api/auth';
import { atBottom, useHoldBottomEdge } from '../../components/scrollPin';
import {
  cancelRun,
  fetchTeam,
  markIssueRead,
  resolveApproval,
  fetchIssue,
  fetchIssueRuns,
  fetchIssues,
  fetchTimeline,
  moveIssue,
  patchIssue,
  postComment,
  retryRun,
  type IssueAttachmentRequest,
} from './api';
import {
  backFrom,
  COLUMN_LABEL,
  RUN_LOG_HEAD,
  STATUS_PILL,
  assignableAgents,
  issuePath,
  runDuration,
  runLogView,
  tokensOf,
  unsettledRun,
  type Agent,
  type Issue,
  type IssueRun,
  type RunLog,
  type IssuePriority,
  type IssueStatus,
} from './boardModel';
import {
  AttachButton,
  AttachmentTray,
  MAX_ISSUE_ATTACHMENTS,
  anyUploading,
  readyRequests,
  useAttachmentDraft,
} from './Attachments';
import { IssueRefScope } from './issueRefs';
import { useIssueRefPlugins } from './issueRefsProse';
import { MarkdownEditor } from './MarkdownEditor';
import { Avatar } from './Avatar';
import { useTeamPortraits } from './portrait';
import { Picker } from './Picker';
import { SubIssues } from './SubIssues';
import { Timeline } from './Timeline';
import type { IssueEvent } from './timelineModel';
import { useBoardStream } from './useBoardStream';
import { invalidateAttention } from './useAttention';
import { formatTokens, formatUsd } from './budgetModel';
import { handleOf } from './teamModel';
import {
  assigneeOptions,
  priorityOptions,
  statusChipShape,
  statusOptions,
  PRIORITY_LABEL,
  PRIORITY_TONE,
} from './issueFields';

const railLabel = 'font-mono text-[0.6rem] font-bold uppercase tracking-[0.12em] text-ink-soft';

/// The skin a rail value wears once it is pressable. Three of the five rows
/// set something, and until they looked like this the rail read as a table of
/// facts with three of its lines secretly controls.
const railPickerChip =
  'rounded-md border border-black/30 bg-canvas pl-1.5 pr-0.5 py-[1px] hover:border-black hover:bg-brand/25';

/// The rail's status chip, minus its horizontal padding — the trigger and the
/// panel's rows need different amounts, and everything else about them has to
/// match. Same square corner as the priority chip beside it; the tone is the
/// only thing a status says differently, because it is the one property on
/// this rail worth reading without stopping to read it.
const railBox = 'border-2 border-black rounded-md bg-surface px-3 py-2.5';

/// One `label ── value` line of the property table.
///
/// `w-full` is load-bearing. The row is not always a block: it has sat inside
/// a flex container, where without this it is sized by its own content and
/// `justify-between` has no slack left to push the value into — which is what
/// left three of these five values beside their labels while the other two
/// sat flush right.
///
/// The height is pinned for the same kind of reason: the Assignee row carries
/// an 18px face and the rest carry one line of 11px mono, so content-sized
/// rows gave a five-row table five different line spacings. Centred rather
/// than baselined, because an inline-flex box holding an avatar does not have
/// a baseline anybody would choose.
function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="flex w-full min-h-[26px] items-center justify-between gap-2.5 py-[2px] font-mono text-[0.68rem]">
      <span className="shrink-0 text-ink-soft">{label}</span>
      <span className="min-w-0 text-right font-bold break-words">{children}</span>
    </div>
  );
}

const RUN_TONE: Record<IssueRun['status'], string> = {
  held: 'border-warn/50 bg-warn/12 text-warn',
  queued: 'border-black/35 bg-canvas text-ink-soft',
  running: 'border-black bg-brand/40 text-ink',
  done: 'border-ok/50 bg-ok/15 text-ok',
  failed: 'border-err/45 bg-err/12 text-err',
  cancelled: 'border-black/35 bg-canvas text-ink-soft',
};

/// Why this run happened. The mockup writes the first one "drag", which is
/// only ever one of the three ways a card reaches In Progress — a REST move
/// and an agent's own tool call produce the same trigger, and a log that
/// blames a drag for either is lying about who started the work.
const RUN_TRIGGER_LABEL: Record<IssueRun['trigger'], string> = {
  started: 'moved to In Progress',
  assigned: 'assigned',
  retry: 'retry',
  comment: 'comment',
  promoted: 'the board had room',
  triage: 'nobody assigned',
  stage_barrier: 'stage barrier',
  review: 'awaiting review',
  stalled: 'work stopped',
  blocked: 'blocked, needs a decision',
};

function RunRow({
  run,
  onCancel,
  busy,
}: {
  run: IssueRun;
  onCancel: () => void;
  busy: boolean;
}) {
  const duration = runDuration(run, Date.now());
  const live = run.status === 'queued' || run.status === 'running' || run.status === 'held';
  // Subscription-priced runs fall back to token usage.
  const consumed = tokensOf(run);
  const cost =
    run.cost_micros != null && run.cost_micros > 0
      ? formatUsd(run.cost_micros)
      : consumed > 0
        ? `${formatTokens(consumed)} tok`
        : null;
  // A live run can be stopped; every run has a transcript once it has a
  // session to open. Nothing here starts one — work begins by moving the
  // card, putting somebody on it, commenting, a stage barrier, or the board
  // taking it off the top of Todo.
  //
  // Two fixed lines rather than one wrapping one: seven fields in a 340px
  // rail meant `flex-wrap` broke each run at whatever point its own trigger
  // label ran out of room, so no two rows in the log lined up. What the row
  // *is* stays on the first line, what it *cost* and what you can *do* on
  // the second, flush right.
  const meta = duration != null || cost != null || live || run.session_id != null;
  return (
    <li className="py-1.5 border-b border-black/15 last:border-0 font-mono text-[0.62rem]">
      <div className="flex items-center gap-2">
        <span className="shrink-0 font-bold">#{run.attempt}</span>
        <span
          className="min-w-0 flex-1 truncate text-ink-soft"
          title={RUN_TRIGGER_LABEL[run.trigger]}
        >
          {RUN_TRIGGER_LABEL[run.trigger]}
        </span>
        <span
          className={`shrink-0 rounded-full border px-2 font-bold uppercase tracking-wider ${RUN_TONE[run.status]}`}
        >
          {run.status}
        </span>
      </div>
      {meta ? (
        <div className="mt-1 flex items-center justify-end gap-2.5 text-ink-soft">
          {duration != null ? <span className="tabular-nums">{duration}</span> : null}
          {cost != null ? (
            <span
              className="tabular-nums"
              title={
                run.cost_micros != null && run.cost_micros > 0
                  ? "What this run's model calls cost"
                  : 'Tokens this run spent — its model calls were billed at $0'
              }
            >
              {cost}
            </span>
          ) : null}
          {live ? (
            <button
              type="button"
              className="font-bold text-err underline cursor-pointer disabled:opacity-50"
              disabled={busy}
              onClick={onCancel}
            >
              Cancel
            </button>
          ) : null}
          {run.session_id != null ? (
            // The trace page's own icon, so the link wears the face of where
            // it lands rather than a second symbol for the same place.
            <a
              aria-label={`Transcript of run #${run.attempt}`}
              title="Open this run's transcript"
              href={`#/traces/${encodeURIComponent(run.session_id)}`}
              className="flex h-[18px] w-[18px] shrink-0 items-center justify-center rounded-md border border-black/40 bg-surface text-info hover:border-black hover:bg-brand hover:text-ink"
            >
              <RiGitMergeLine aria-hidden />
            </a>
          ) : null}
        </div>
      ) : null}
    </li>
  );
}

export function IssueDetailPage() {
  const { pid, num } = useParams<{ pid: string; num: string }>();
  const projectId = pid ?? '';
  const number = Number(num ?? '');
  const client = useAdminClient();
  const { logout } = useAuth();
  // Router state survives a reload and is absent on a pasted URL, which is
  // exactly when the board is the honest answer. `backFrom` refuses
  // anything that is not a stage of this project.
  const back = backFrom(projectId, (useLocation().state as { from?: unknown } | null)?.from);

  const [issue, setIssue] = useState<Issue | null>(null);
  const [board, setBoard] = useState<Issue[]>([]);
  // The board is what a `#12` is read against, in the description's editor as
  // much as in a comment.
  const issueRefPlugins = useIssueRefPlugins(projectId, board);
  const [agents, setAgents] = useState<Agent[]>([]);
  const portrait = useTeamPortraits(agents);
  const [runLog, setRunLog] = useState<RunLog | null>(null);
  const runs = runLog?.items ?? [];
  const [allRuns, setAllRuns] = useState(false);
  const runView = runLogView(runs, allRuns);
  const [events, setEvents] = useState<IssueEvent[]>([]);
  const [refreshKey, setRefreshKey] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [title, setTitle] = useState('');
  const [saving, setSaving] = useState(false);
  const [showMore, setShowMore] = useState(false);
  const [confirmCancel, setConfirmCancel] = useState(false);
  const [blocking, setBlocking] = useState(false);
  const [blockReason, setBlockReason] = useState('');
  const [editingTitle, setEditingTitle] = useState(false);
  const serverTitle = useRef('');
  /// What the editor last reported, for a blur listener that was registered
  /// once and cannot see React state — and whether it ever reported at all.
  /// A blur fires whether or not anything was typed, and the draft starts
  /// empty, so committing unconditionally would wipe the description of any
  /// card you merely clicked into.
  const draftDescription = useRef('');
  const descriptionTouched = useRef(false);
  const pane = useRef<HTMLElement | null>(null);
  /// Whether the reader is parked at the foot of the card. The chat thread's
  /// posture, and this pane is the same act: a card is read downwards and
  /// answered at the bottom, so an arrival follows a reader who is already
  /// there and leaves one who is not exactly where they were.
  const pinned = useRef(true);
  const [newBelow, setNewBelow] = useState(false);
  /// What the foot of the timeline looked like last commit. A board frame
  /// re-fetches the whole card, so a fresh `events` array is not by itself
  /// news — only a different last entry is.
  const tail = useRef('');
  const holdTimelineEdge = useHoldBottomEdge(pane, pinned);
  /// Whether this card's read cursor has been moved. Reset per card: opening a
  /// card is the act that reads it, and a refresh is not a second opening.
  const stamped = useRef(false);

  useEffect(() => {
    stamped.current = false;
    draftDescription.current = '';
    descriptionTouched.current = false;
    // A fresh card is read from its foot again, whatever posture the last one
    // was left in. This pane is routed, not remounted, so none of this resets
    // itself — including the unfolded execution log, which would otherwise
    // open the next card's whole history because somebody read this one's.
    pinned.current = true;
    tail.current = '';
    setNewBelow(false);
    setAllRuns(false);
  }, [projectId, number]);

  // A card opens at the foot of its own history — the newest entries and the
  // composer — rather than at a title you have already read, and stays there
  // as entries land. Before paint, so it lands rather than jumps. A reader in
  // scroll-back is never yanked: what they get is the pill, and only when
  // something actually arrived below them.
  useLayoutEffect(() => {
    const signature = `${String(events.length)}|${events.at(-1)?.id ?? ''}`;
    const moved = signature !== tail.current;
    tail.current = signature;
    if (loading || issue === null) return;
    const box = pane.current;
    if (box === null) return;
    if (pinned.current) {
      box.scrollTop = box.scrollHeight;
      setNewBelow(false);
    } else if (moved) {
      setNewBelow(true);
    }
  }, [loading, issue, events]);

  const onPaneScroll = useCallback(() => {
    const box = pane.current;
    if (box === null) return;
    pinned.current = atBottom(box);
    if (pinned.current) setNewBelow(false);
  }, []);

  /// Smooth, unlike every other move this pane makes: the others are the page
  /// holding an edge the reader never left, and this one is a "take me there"
  /// they pressed.
  const jumpToLatest = useCallback(() => {
    const box = pane.current;
    if (box === null) return;
    box.scrollTo({ top: box.scrollHeight, behavior: 'smooth' });
    pinned.current = true;
    setNewBelow(false);
  }, []);

  useEffect(() => {
    let canceled = false;
    async function load() {
      const [outcome, agentsOutcome, runsOutcome, timelineOutcome, boardOutcome] =
        await Promise.all([
          fetchIssue(client, projectId, number),
          fetchTeam(client, projectId),
          fetchIssueRuns(client, projectId, number),
          fetchTimeline(client, projectId, number),
          fetchIssues(client, projectId),
        ]);
      if (canceled) return;
      if (boardOutcome.kind === 'ok') setBoard(boardOutcome.value);
      if (agentsOutcome.kind === 'ok') setAgents(assignableAgents(agentsOutcome.value));
      if (runsOutcome.kind === 'ok') setRunLog(runsOutcome.value);
      if (timelineOutcome.kind === 'ok') setEvents(timelineOutcome.value);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        setLoading(false);
        return;
      }
      setIssue(outcome.value);
      // Live refreshes must not overwrite an edit in progress — so the
      // editors only follow the server while they still hold what the server
      // last sent.
      //
      // The comparison reads a **snapshot** taken before the ref moves. A
      // lazy updater runs during the next render, by which time an assignment
      // further down this function has already advanced `serverProse`, so
      // reading the ref inside the updater compared the old draft against the
      // *new* server value — never equal on a first load, which left both
      // editors empty on every card and armed Save to write those blanks
      // back.
      const previousTitle = serverTitle.current;
      serverTitle.current = outcome.value.title;
      setTitle((current) => (current === previousTitle ? outcome.value.title : current));
      setError(null);
      setLoading(false);
      // The stamp rides the tail of a load that actually put the timeline on
      // screen, never the mount. Every early return above is a card the
      // operator has not read, and the cursor is irreversible — a stamp on a
      // failed load buries the very comments the badge existed to point at.
      // Once per card: an agent commenting into an open card raises the count
      // again, and the operator has not read that one either.
      if (timelineOutcome.kind === 'ok' && !stamped.current) {
        stamped.current = true;
        void markIssueRead(client, projectId, number).then(() => {
          // The rail's dot is the same fact read from further away. Without
          // this it trails the card by a poll interval, which is what made
          // it look like a dot nothing could clear.
          invalidateAttention(client);
        });
      }
    }
    void load();
    return () => {
      canceled = true;
    };
  }, [client, logout, number, projectId, refreshKey]);

  /// Refetch this card, and ask the badge again.
  ///
  /// The two go together everywhere on this page. Every write here can move
  /// one of the four counts behind the rail's dot — an answer discharges an
  /// approval, a stop discharges a held run, a cancel or a block discharges
  /// a failure — and bumping only the local refresh is how a dot outlives
  /// the act that cleared it. An answered approval is the case that has no
  /// other way home: it emits no board frame at all, so nothing else would
  /// ever ask.
  const refetch = useCallback(() => {
    setRefreshKey((key) => key + 1);
    invalidateAttention(client);
  }, [client]);

  useBoardStream(projectId, number, refetch);

  // What the URL asks for right now, readable from inside a promise that was
  // started before it changed.
  const currentCard = useRef(`${projectId}:${String(number)}`);
  currentCard.current = `${projectId}:${String(number)}`;

  const apply = useCallback(
    async (body: Parameters<typeof patchIssue>[3]) => {
      setSaving(true);
      const wrote = `${projectId}:${String(number)}`;
      const outcome = await patchIssue(client, projectId, number, body);
      setSaving(false);
      // The page does not unmount when the route parameter changes, so a save
      // for the card you just left can answer while the next one is on screen
      // — and the row it carries would replace it, leaving the previous card's
      // title and description sitting at the new card's URL until reload.
      if (wrote !== `${currentCard.current}`) return;
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        return;
      }
      setError(null);
      setIssue(outcome.value);
      setTitle(outcome.value.title);
      serverTitle.current = outcome.value.title;
    },
    [client, logout, number, projectId],
  );

  const moveChild = useCallback(
    async (childNumber: number, status: IssueStatus) => {
      setSaving(true);
      const latest = await fetchIssues(client, projectId);
      if (latest.kind === 'unauthorized') {
        logout();
        setSaving(false);
        return;
      }
      if (latest.kind === 'failed') {
        setError(latest.message);
        setSaving(false);
        return;
      }
      const ordered = latest.value
        .filter((candidate) => candidate.status === status && candidate.number !== childNumber)
        .sort((a, b) => a.position - b.position)
        .map((candidate) => candidate.number);
      ordered.push(childNumber);
      const outcome = await moveIssue(client, projectId, childNumber, status, ordered);
      setSaving(false);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        return;
      }
      setError(null);
      refetch();
    },
    [client, logout, projectId, refetch],
  );

  const relocate = useCallback(
    async (status: IssueStatus) => {
      setSaving(true);
      const board = await fetchIssues(client, projectId);
      if (board.kind === 'unauthorized') {
        logout();
        setSaving(false);
        return;
      }
      if (board.kind === 'failed') {
        setError(board.message);
        setSaving(false);
        return;
      }
      const ordered = board.value
        .filter((candidate) => candidate.status === status && candidate.number !== number)
        .sort((a, b) => a.position - b.position)
        .map((candidate) => candidate.number);
      ordered.push(number);

      const outcome = await moveIssue(client, projectId, number, status, ordered);
      setSaving(false);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        return;
      }
      setError(null);
      setIssue(outcome.value);
    },
    [client, logout, number, projectId],
  );

  const stopRun = useCallback(async () => {
    setSaving(true);
    const outcome = await cancelRun(client, projectId, number);
    setSaving(false);
    if (outcome.kind === 'unauthorized') {
      logout();
      return;
    }
    if (outcome.kind === 'failed') {
      setError(outcome.message);
      return;
    }
    setError(null);
    refetch();
  }, [client, logout, number, projectId, refetch]);

  /// The one action that discharges a failed card without destroying or
  /// hiding it. The endpoint shipped with the feature and nothing in the
  /// dashboard had ever called it, so an operator staring at a lit badge
  /// could only reach zero by finishing, cancelling or blocking the card.
  const runAgain = useCallback(async () => {
    setSaving(true);
    const outcome = await retryRun(client, projectId, number);
    setSaving(false);
    if (outcome.kind === 'unauthorized') {
      logout();
      return;
    }
    if (outcome.kind === 'failed') {
      setError(outcome.message);
      return;
    }
    setError(null);
    refetch();
  }, [client, logout, number, projectId, refetch]);

  const comment = useCallback(
    async (text: string, attachments: IssueAttachmentRequest[]) => {
      setSaving(true);
      const outcome = await postComment(client, projectId, number, text, attachments);
      setSaving(false);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        return;
      }
      setError(null);
      // Sending is a "take me there" the same as pressing the pill: your own
      // comment lands in view even if you had scrolled back up to write it.
      pinned.current = true;
      setNewBelow(false);
      setEvents((current) => [...current, outcome.value]);
    },
    [client, logout, number, projectId],
  );

  const answerApproval = useCallback(
    async (callId: string, decision: 'approve' | 'approve_always' | 'deny') => {
      setSaving(true);
      const outcome = await resolveApproval(client, projectId, number, callId, decision);
      setSaving(false);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        return;
      }
      setError(null);
      refetch();
    },
    [client, logout, number, projectId, refetch],
  );

  /// Put somebody on a step without leaving the parent. Goes through the
  /// same patch the card's own rail uses, so the trigger predicate sees an
  /// assignment identically whichever page made it.
  const assignChild = useCallback(
    async (childNumber: number, assignee: string | null) => {
      setSaving(true);
      const outcome = await patchIssue(client, projectId, childNumber, { assignee });
      setSaving(false);
      if (outcome.kind === 'unauthorized') {
        logout();
        return;
      }
      if (outcome.kind === 'failed') {
        setError(outcome.message);
        return;
      }
      setError(null);
      refetch();
    },
    [client, logout, projectId, refetch],
  );

  /// Leaving a field is what saves it. With no Save button the write has to
  /// ride the gesture that ends the edit, so blur and Enter commit — and
  /// Escape puts the server's text back and commits nothing, which is the
  /// only way left to change your mind.
  ///
  /// A title erased to nothing is treated as a slip rather than an
  /// instruction: it reverts. Blanking a card's title from a field you were
  /// only passing through is not an edit anybody means to make, and the
  /// board would show the card as `Untitled` with no way to tell that from a
  /// card that never had one.
  const commitTitle = useCallback(
    (next: string) => {
      setEditingTitle(false);
      if (next.trim() === '') {
        setTitle(issue?.title ?? '');
        return;
      }
      if (issue != null && next !== issue.title) void apply({ title: next });
    },
    [apply, issue],
  );

  const commitDescription = useCallback(
    (next: string) => {
      if (issue != null && next !== issue.description) void apply({ description: next });
    },
    [apply, issue],
  );

  // A file lands on the card the moment its upload finishes, rather than
  // waiting for the description editor to blur: the two are edited
  // independently, and an operator who attaches a screenshot and navigates
  // away should not lose it because they never clicked into the prose.
  //
  // The draft is seeded **once per card**, not from every `issue` the page
  // re-reads: `apply` sets a fresh `issue` on each save, and re-adopting
  // from that would wipe an upload still in flight beside it.
  const files = useAttachmentDraft(projectId);
  const adopt = files.adopt;
  const seededFor = files.seededFor;
  const card = `${projectId}:${String(number)}`;
  // The card the loaded row IS, which is not always the one the URL asks
  // for: this page does not unmount when the route parameter changes, so
  // between `#7` becoming `#8` and the fetch answering, `issue` is still
  // #7. Seeding then would hand #8's draft #7's files, and the save below
  // would write them onto #8 — a probe caught exactly that.
  const loaded = issue == null ? null : `${issue.project_id}:${String(issue.number)}`;
  useEffect(() => {
    if (issue == null || loaded !== card || seededFor === card) return;
    adopt(card, issue.attachments ?? []);
  }, [card, loaded, issue, adopt, seededFor]);

  // What the draft settled on, and what the card says, as one comparable
  // string each — a save is owed exactly when they differ. Driving it off
  // the *ids* rather than off an add/remove callback is what makes an
  // upload that lands seconds after the pick save itself.
  const draftRequests = readyRequests(files.attachments);
  const draftIds = draftRequests.map((a) => a.blob_id).join(',');
  const savedIds = (issue?.attachments ?? []).map((a) => a.blob_id).join(',');
  const settling = anyUploading(files.attachments);
  const pending = useRef(draftRequests);
  pending.current = draftRequests;
  useEffect(() => {
    // `seededFor` is the guard, and it is state rather than a ref for the
    // reason `useAttachmentDraft` spells out: on the commit where the card
    // first loads, a ref would already say "seeded" while the draft still
    // read empty — and this would save that emptiness over the card's files.
    if (issue == null || loaded !== card || seededFor !== card || settling || draftIds === savedIds)
      return;
    // From the ref, so the saved list keeps each file's NAME — rebuilding it
    // from `draftIds` would send bare blob ids and rename every file on the
    // card to nothing on the first unrelated save.
    void apply({ attachments: pending.current });
    // Keyed on the ids rather than on the request objects: `readyRequests`
    // builds a fresh array every render, so depending on it would save on
    // every keystroke anywhere on the page.
  }, [card, loaded, draftIds, savedIds, settling, issue, apply, seededFor]);

  const moreRoot = useRef<HTMLDivElement | null>(null);

  const closeMore = useCallback(() => {
    setShowMore(false);
    setConfirmCancel(false);
    setBlocking(false);
    setBlockReason('');
  }, []);

  // A menu that only closes through its own button is a menu you have to
  // remember to put away. Same handling as the project switcher: a pointer
  // anywhere outside dismisses it, and so does Escape.
  useEffect(() => {
    if (!showMore) return;
    function onPointerDown(event: PointerEvent) {
      if (moreRoot.current === null) return;
      if (!moreRoot.current.contains(event.target as Node)) closeMore();
    }
    function onKeyDown(event: KeyboardEvent) {
      if (event.key === 'Escape') closeMore();
    }
    document.addEventListener('pointerdown', onPointerDown);
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('pointerdown', onPointerDown);
      document.removeEventListener('keydown', onKeyDown);
    };
  }, [showMore, closeMore]);

  /// Record what this card is waiting on, and close the panel.
  const block = useCallback(() => {
    const reason = blockReason.trim();
    if (reason === '') return;
    setBlocking(false);
    setShowMore(false);
    setBlockReason('');
    void apply({ blocked_reason: reason });
  }, [apply, blockReason]);

  if (loading) {
    return (
      <div className="flex flex-1 items-center justify-center">
        <RiLoader4Line className="text-3xl text-ink-soft animate-spin" />
      </div>
    );
  }

  if (issue === null) {
    return (
      <div className="p-6">
        <div className="bg-white border-[3px] border-err text-err rounded-md shadow-brutal-sm px-4 py-3 font-mono text-sm break-words">
          {error ?? 'Issue not found'}
        </div>
      </div>
    );
  }

  const cancelled = issue.cancelled_at_ms != null;
  const blocked = issue.blocked_reason != null;
  const live = unsettledRun(runs);
  const children = board.filter((candidate) => candidate.parent === issue.number);

  const assignees = assigneeOptions(agents, portrait);

  return (
    <div className="flex flex-col h-full min-h-0">
      <header className="h-12 shrink-0 px-4 border-b-2 border-black flex items-center gap-3 bg-canvas">
        {/* Back to whichever surface opened this card, not always the
            board: from a maximized stage that was two steps from where the
            operator was, and it dropped that stage's filter on the way. */}
        <Link
          to={back.to}
          className="inline-flex items-center gap-1 font-mono text-[0.72rem] text-ink-soft hover:text-ink"
        >
          <RiArrowLeftLine /> {back.label}
        </Link>
        <span className="shrink-0 border-2 border-black bg-brand/35 rounded px-2 font-mono text-[0.6rem] font-bold uppercase tracking-wider">
          {COLUMN_LABEL[issue.status]}
        </span>
        {cancelled ? (
          <span className="shrink-0 border-2 border-black bg-canvas rounded px-2 font-mono text-[0.6rem] font-bold uppercase tracking-wider text-ink-soft">
            Cancelled
          </span>
        ) : null}
        <span className="shrink-0 font-mono text-[0.85rem] font-bold">#{issue.number}</span>
        {/* The card opens at the foot of its timeline, which puts the heading
            off-screen — so the header carries the name too. Tracks the editor
            rather than the loaded row, so a title being typed reads the same
            in both places. */}
        <span
          title={title}
          className={`min-w-0 flex-1 truncate font-mono text-[0.78rem] ${
            cancelled ? 'line-through text-ink-soft' : ''
          }`}
        >
          {title.trim() === '' ? <span className="text-ink-soft">Untitled</span> : title}
        </span>
        {live !== null && issue.assignee != null ? (
          // The same run-status frame the board card and the team strip
          // read, so the shimmer means one thing everywhere.
          <span
            className="ml-auto flex items-center gap-2 font-mono text-[0.66rem] text-ink-soft"
            title={`run #${live.attempt} · ${RUN_TRIGGER_LABEL[live.trigger]} · ${live.status}`}
          >
            <Avatar
              handle={handleOf(agents, issue.assignee)}
              src={portrait(issue.assignee)}
              run={live.status === 'running' ? 'running' : live.status === 'held' ? 'held' : 'queued'}
              size="lg"
            />
            @{handleOf(agents, issue.assignee)}{' '}
            {live.status === 'running'
              ? 'is working'
              : live.status === 'held'
                ? 'is held on budget'
                : 'is queued'}{' '}
            · run #{live.attempt}
          </span>
        ) : null}
      </header>

      <div className="flex-1 min-h-0 flex overflow-hidden bg-surface">
        {/* Positioning context for the jump pill. It cannot live inside the
            pane: an absolute box in a scroller is placed against the content
            and scrolls away with it, and the composer it would sit above is
            itself only stuck while the timeline is on screen. */}
        <div className="relative flex flex-1 min-w-0">
        <main
          ref={pane}
          onScroll={onPaneScroll}
          className="flex flex-col flex-1 min-w-0 overflow-y-auto overscroll-none py-5 pl-20 pr-20"
        >
          {error != null ? (
            <div className="mb-4 bg-white border-2 border-err text-err rounded-md px-3 py-2 font-mono text-[0.78rem] break-words">
              {error}
            </div>
          ) : null}

          {/* A heading that becomes a field, not a field wearing a heading's
              clothes. The title sat in a permanently-live borderless input,
              which read as a nameless text box stacked on the description's
              nameless text box — a two-field form where the mockup has a
              document. Both halves now share one read-then-edit gesture, and
              leaving the field is what writes it. */}
          <h1 className="font-mono text-[1.4rem] font-bold leading-[2rem]">
            {editingTitle ? (
              <input
                className="block w-full bg-transparent border-b-2 border-black outline-none font-mono text-[1.4rem] font-bold leading-[2rem]"
                value={title}
                autoFocus
                aria-label="Issue title"
                onBlur={() => {
                  commitTitle(title);
                }}
                onKeyDown={(event) => {
                  if (event.key === 'Enter') commitTitle(title);
                  if (event.key === 'Escape') {
                    setTitle(issue.title);
                    setEditingTitle(false);
                  }
                }}
                onChange={(event) => {
                  setTitle(event.target.value);
                }}
              />
            ) : (
              <button
                type="button"
                title="Click to edit"
                onClick={() => {
                  setEditingTitle(true);
                }}
                className={`block w-full text-left border-b-2 border-transparent leading-[2rem] cursor-text underline-offset-4 hover:underline hover:decoration-dashed hover:decoration-black/40 ${
                  cancelled ? 'line-through text-ink-soft' : ''
                }`}
              >
                {title.trim() === '' ? (
                  <span className="font-normal text-ink-soft">Untitled</span>
                ) : (
                  title
                )}
              </button>
            )}
          </h1>
          {/* One always-on editor, keyed to the card so opening another
              one does not inherit this one's document. There is no read mode
              to swap to any more, which is what every height-jump on this
              block was: two boxes pretending to be one.

              No minimum height: an empty document is one empty paragraph,
              which is one line, which is what an empty description should
              look like. `shrink-0` stays — the pane is a flex column and a
              flex item is otherwise free to be squeezed below its content. */}
          <MarkdownEditor
            key={`${projectId}:${String(issue.number)}`}
            className="mt-1 w-full shrink-0 md-editor"
            initialValue={issue.description}
            plugins={issueRefPlugins}
            ariaLabel="Issue description"
            placeholder="Add description…"
            onChange={(markdown) => {
              draftDescription.current = markdown;
              descriptionTouched.current = true;
            }}
            onBlur={() => {
              // From the ref, not from state: Milkdown registers this listener
              // once, so a closure over state would commit whatever the
              // description was when the editor mounted.
              if (!descriptionTouched.current) return;
              commitDescription(draftDescription.current);
            }}
          />

          {/* Under the editor, not inside it. Milkdown owns a ProseMirror
              document, and a blob-backed image is not something its markdown
              serialiser could round-trip — the files are the card's, beside
              its prose. Paste and drop are wired on the wrapper so dropping a
              screenshot anywhere over the description works. */}
          {/* The files first, the control that adds them under them — the
              same order the composers use. `gap` on a flex column and a tray
              that renders null when empty means a card with no files pays no
              height for the arrangement. */}
          <div
            className="mt-2 flex shrink-0 flex-col gap-2"
            onPaste={files.onPaste}
            onDrop={files.onDrop}
            onDragOver={files.onDragOver}
          >
            <AttachmentTray attachments={files.attachments} onRemove={files.remove} />
            <AttachButton
              subtle
              onPick={files.add}
              disabled={saving}
              full={files.attachments.length >= MAX_ISSUE_ATTACHMENTS}
            />
          </div>

          <SubIssues
            projectId={projectId}
            children={children}
            team={agents}
            portrait={portrait}
            disabled={saving}
            onStatus={(childNumber, status) => {
              void moveChild(childNumber, status);
            }}
            onAssignee={(childNumber, assignee) => {
              void assignChild(childNumber, assignee);
            }}
          />

          {/* The board is what a `#12` in a comment is read against — the same list
              the parent picker resolves from, which the page already holds. */}
          <IssueRefScope projectId={projectId} issues={board}>
            <Timeline
              events={events}
              issue={issue}
              runs={runs}
              busy={saving}
              contentRef={holdTimelineEdge}
              onComment={(text, attachments) => {
                void comment(text, attachments);
              }}
              team={agents}
              portrait={portrait}
              onResolveApproval={(callId, decision) => {
                void answerApproval(callId, decision);
              }}
            />
          </IssueRefScope>
        </main>

        {newBelow ? (
          <button
            type="button"
            onClick={jumpToLatest}
            title="Jump to the newest entry"
            className="absolute bottom-32 left-1/2 z-30 flex -translate-x-1/2 items-center gap-1.5 border-2 border-black rounded-md bg-white px-3 py-1.5 font-mono text-[0.62rem] font-bold uppercase tracking-wider shadow-brutal-sm hover:bg-canvas"
          >
            <RiArrowDownLine className="text-sm" />
            New activity
          </button>
        ) : null}
        </div>

        <aside className="w-[340px] shrink-0 border-l-2 border-black bg-canvas p-3.5 flex flex-col gap-3.5 overflow-y-auto overscroll-none">
          <section className={railBox}>
            <div className="flex items-center gap-2">
              <h2 className={railLabel}>Properties</h2>
              <div ref={moreRoot} className="ml-auto relative">
                <button
                  type="button"
                  aria-label="More actions"
                  aria-expanded={showMore}
                  title="Block, cancel…"
                  onClick={() => {
                    setConfirmCancel(false);
                    setBlocking(false);
                    setBlockReason('');
                    setShowMore((value) => !value);
                  }}
                  className="flex h-6 w-6 items-center justify-center rounded-md border-2 border-black bg-surface font-mono text-[0.8rem] font-bold leading-none text-ink cursor-pointer hover:bg-brand"
                >
                  ⋯
                </button>
                {showMore ? (
                  <div className="absolute right-0 top-[calc(100%+4px)] z-30 w-[230px] bg-surface border-2 border-black rounded-md shadow-brutal py-1">
                    {blocking ? (
                      // The reason is asked for in the panel rather than by
                      // `window.prompt`: a browser dialog blocks the page,
                      // cannot be styled, and on a board that streams updates
                      // it freezes everything behind it until it is answered.
                      <div className="px-3 py-1.5">
                        <label
                          htmlFor="block-reason"
                          className="block font-mono text-[0.58rem] font-bold uppercase tracking-wider text-ink-soft"
                        >
                          What is it blocked on?
                        </label>
                        <textarea
                          id="block-reason"
                          autoFocus
                          value={blockReason}
                          placeholder="Waiting on the staging key…"
                          onChange={(event) => {
                            setBlockReason(event.target.value);
                          }}
                          onKeyDown={(event) => {
                            if (event.key === 'Escape') {
                              // Back out to the menu, not out of it: the
                              // document-level handler would close everything.
                              event.stopPropagation();
                              setBlocking(false);
                              return;
                            }
                            if (event.key === 'Enter' && !event.shiftKey) {
                              event.preventDefault();
                              if (blockReason.trim() !== '') block();
                            }
                          }}
                          className="mt-1 w-full min-h-[52px] resize-none rounded-md border-2 border-black bg-canvas px-2 py-1 font-sans text-[0.75rem] outline-none"
                        />
                        <div className="mt-2 flex gap-2">
                          <button
                            type="button"
                            disabled={blockReason.trim() === ''}
                            onClick={block}
                            className="border-2 border-black bg-brand text-ink rounded-md px-2 py-0.5 font-mono text-[0.66rem] font-bold cursor-pointer disabled:opacity-40 disabled:cursor-not-allowed"
                          >
                            Block
                          </button>
                          <button
                            type="button"
                            onClick={() => {
                              setBlocking(false);
                            }}
                            className="border-2 border-black bg-surface rounded-md px-2 py-0.5 font-mono text-[0.66rem] cursor-pointer"
                          >
                            Never mind
                          </button>
                        </div>
                      </div>
                    ) : confirmCancel ? (
                      // What cancelling costs, asked at the moment it is being
                      // decided. It used to sit in the menu as standing prose
                      // under the button, which is where nobody reads it and
                      // where it made two ordinary actions look alarming.
                      <div className="px-3 py-1.5">
                        <p className="font-mono text-[0.62rem] leading-snug">
                          Cancel #{issue.number}? It keeps its number and its whole history — it
                          just stops counting as live work, and nothing runs on it until you
                          reopen it.
                        </p>
                        <div className="mt-2 flex gap-2">
                          <button
                            type="button"
                            className="border-2 border-err bg-err text-white rounded-md px-2 py-0.5 font-mono text-[0.66rem] font-bold cursor-pointer"
                            onClick={() => {
                              setConfirmCancel(false);
                              setShowMore(false);
                              void apply({ cancelled: true });
                            }}
                          >
                            Cancel it
                          </button>
                          <button
                            type="button"
                            className="border-2 border-black bg-surface rounded-md px-2 py-0.5 font-mono text-[0.66rem] cursor-pointer"
                            onClick={() => {
                              setConfirmCancel(false);
                            }}
                          >
                            Keep
                          </button>
                        </div>
                      </div>
                    ) : (
                      <>
                    {blocked ? (
                      <button
                        type="button"
                        className="w-full text-left px-3 py-1.5 font-mono text-[0.7rem] cursor-pointer hover:bg-canvas"
                        onClick={() => {
                          setShowMore(false);
                          void apply({ blocked_reason: null });
                        }}
                      >
                        Unblock
                      </button>
                    ) : (
                      <button
                        type="button"
                        className="w-full text-left px-3 py-1.5 font-mono text-[0.7rem] cursor-pointer hover:bg-canvas"
                        onClick={() => {
                          setBlocking(true);
                        }}
                      >
                        Block…
                      </button>
                    )}
                    <button
                      type="button"
                      className={`w-full text-left px-3 py-1.5 font-mono text-[0.7rem] cursor-pointer hover:bg-canvas ${
                        cancelled ? '' : 'text-err'
                      }`}
                      onClick={() => {
                        // Reopening is not destructive, so it just happens.
                        if (cancelled) {
                          setShowMore(false);
                          void apply({ cancelled: false });
                          return;
                        }
                        setConfirmCancel(true);
                      }}
                    >
                      {cancelled ? 'Reopen issue' : 'Cancel issue'}
                    </button>
                      </>
                    )}
                  </div>
                ) : null}
              </div>
            </div>
            <div className="mt-1.5">
              <Row label="Status">
                <Picker
                  label="Status"
                  title="Move this card to another column"
                  value={issue.status}
                  disabled={saving}
                  options={statusOptions}
                  onPick={(picked) => {
                    void relocate(picked as IssueStatus);
                  }}
                  triggerClassName={`${statusChipShape} pl-2 pr-0.5 ${
                    STATUS_PILL[issue.status]
                  } hover:border-black`}
                >
                  {COLUMN_LABEL[issue.status]}
                </Picker>
              </Row>
              <Row label="Priority">
                <Picker
                  label="Priority"
                  title="Change this card's priority"
                  value={issue.priority}
                  disabled={saving}
                  options={priorityOptions}
                  onPick={(picked) => {
                    void apply({ priority: picked as IssuePriority });
                  }}
                  triggerClassName={`${railPickerChip} ${PRIORITY_TONE[issue.priority]}`}
                >
                  {PRIORITY_LABEL[issue.priority]}
                </Picker>
              </Row>
              <Row label="Assignee">
                <Picker
                  label="Assignee"
                  title="Hand this card to somebody"
                  value={issue.assignee ?? ''}
                  disabled={saving}
                  options={assignees}
                  onPick={(picked) => {
                    void apply({ assignee: picked.length > 0 ? picked : null });
                  }}
                  triggerClassName={railPickerChip}
                >
                  {issue.assignee == null ? (
                    <span className="font-normal text-ink-soft">Unassigned</span>
                  ) : (
                    <span className="inline-flex items-center gap-1.5">
                      <Avatar
                        handle={handleOf(agents, issue.assignee)}
                        src={portrait(issue.assignee)}
                        size="sm"
                      />
                      @{handleOf(agents, issue.assignee)}
                    </span>
                  )}
                </Picker>
              </Row>
              {/* A press, like the three rows above it — and unlike Parent
                  and Blocked, which are facts. The board's own pin is a
                  hover-revealed glyph on a card; this one is always on
                  screen, which is what a touch screen has to reach for. */}
              <Row label="Pinned">
                <button
                  type="button"
                  disabled={saving}
                  aria-pressed={issue.pinned}
                  title={
                    issue.pinned
                      ? 'Stop keeping this card at the top of its column'
                      : 'Keep this card at the top of its column'
                  }
                  onClick={() => {
                    void apply({ pinned: !issue.pinned });
                  }}
                  className={`${railPickerChip} inline-flex items-center gap-1 pr-1.5 disabled:opacity-40 disabled:cursor-not-allowed ${
                    issue.pinned ? 'text-brand-hover' : 'font-normal text-ink-soft'
                  }`}
                >
                  {issue.pinned ? (
                    <>
                      <RiPushpin2Fill aria-hidden /> Pinned
                    </>
                  ) : (
                    <>
                      <RiPushpin2Line aria-hidden /> Pin to top
                    </>
                  )}
                </button>
              </Row>
              <Row label="Parent">
                {issue.parent == null ? (
                  <span className="font-normal text-ink-soft">—</span>
                ) : (
                  <Link
                    to={issuePath(projectId, issue.parent)}
                    className="underline"
                  >
                    #{issue.parent} · stage {issue.stage}
                  </Link>
                )}
              </Row>
              <Row label="Blocked">
                {blocked ? (
                  <span className="text-warn">⚑ {issue.blocked_reason ?? ''}</span>
                ) : (
                  <span className="font-normal text-ink-soft">—</span>
                )}
              </Row>
            </div>
          </section>

          {issue.branch != null ? (
            <section className={railBox}>
              <h2 className={railLabel}>Branch</h2>
              <div className="mt-1.5 flex items-center gap-2 font-mono text-[0.66rem]">
                <span aria-hidden className="shrink-0 text-ink-soft">
                  ⎇
                </span>
                <code className="min-w-0 flex-1 truncate" title={issue.branch}>
                  {issue.branch}
                </code>
              </div>
              <p className="mt-1.5 font-mono text-[0.56rem] text-ink-soft leading-snug">
                No diff or merge here. The worktree is reclaimed at Done.
              </p>
            </section>
          ) : null}

          <section className={railBox}>
            <h2 className={railLabel}>Execution log</h2>
            {/* Read-only, apart from stopping what is running: the log
                reports the board's work rather than commanding it. */}
            {runs.length === 0 ? (
              <p className="mt-1.5 font-mono text-[0.62rem] text-ink-soft leading-snug">
                No runs yet — this card starts working when it reaches In Progress with an
                assignee.
              </p>
            ) : (
              <ul className="mt-1.5 flex flex-col">
                {runView.shown.map((run) => (
                  <RunRow
                    key={run.attempt}
                    run={run}
                    busy={saving}
                    onCancel={() => {
                      void stopRun();
                    }}
                  />
                ))}
              </ul>
            )}
            {/* The fold, and what it is holding. A hidden failure is the one
                thing this collapse could cost, so the count of them rides on
                the toggle in the error tone — the same reason the board's
                filter trigger carries a badge. */}
            {runView.foldable ? (
              <button
                type="button"
                onClick={() => {
                  setAllRuns((open) => !open);
                }}
                className="mt-1.5 w-full text-left font-mono text-[0.6rem] text-ink-soft hover:text-ink"
              >
                {allRuns ? (
                  `Fold to the newest ${String(RUN_LOG_HEAD)}`
                ) : (
                  <>
                    ＋{runView.hidden} earlier {runView.hidden === 1 ? 'run' : 'runs'}
                    {runView.hiddenFailed > 0 ? (
                      <span className="font-bold text-err"> · {runView.hiddenFailed} failed</span>
                    ) : null}
                  </>
                )}
              </button>
            ) : null}
            {/* Gated on the same fact the board's badge and the card's own
                marker read, so the button is present exactly when the
                thing it clears is being counted. */}
            {issue.last_run_failed ? (
              <button
                type="button"
                disabled={saving}
                onClick={() => {
                  void runAgain();
                }}
                className="mt-2 inline-flex items-center gap-1 border-2 border-black rounded-md bg-surface px-2 py-0.5 font-mono text-[0.62rem] font-bold hover:bg-brand/25 disabled:opacity-50"
              >
                <RiRefreshLine aria-hidden /> Run again
              </button>
            ) : null}
          </section>

          {/* Shown even at zero: "this card has cost nothing yet" is an
              answer, and a block that appears only once there is a bill
              cannot be looked at to ask the question. */}
          <section className={railBox}>
            <h2 className={railLabel}>Tokens</h2>
            <div className="mt-1.5">
              <Row label="input / output">
                <span className="tabular-nums">
                  {formatTokens(runLog?.total_input_tokens ?? 0)} /{' '}
                  {formatTokens(runLog?.total_output_tokens ?? 0)}
                </span>
              </Row>
              <Row label="cost (all runs)">
                <span className="tabular-nums">{formatUsd(runLog?.total_cost_micros ?? 0)}</span>
              </Row>
            </div>
          </section>
        </aside>
      </div>
    </div>
  );
}
