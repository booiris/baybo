import { describe, expect, it } from 'vitest';
import type { components } from '../api/schema';
import {
  applySyncMerge,
  applySyncReplace,
  applyTurnState,
  compactionDividerKeys,
  finalizeMessage,
  joinKeptHead,
  pushToolStartedStep,
  rowsAboveFloor,
  shouldAutoLoadOlder,
  syncSince,
  transcriptItemToRow,
  type TranscriptRow,
} from './ChatPage';

type ApiItem = components['schemas']['ChatTranscriptItem'];

const SID = 'sess-1';

function msg(ordinal: number, role: 'user' | 'assistant', text: string, pmid?: string): ApiItem {
  return {
    id: `m${ordinal}`,
    ordinal,
    kind: 'message',
    role,
    text,
    has_attachments: false,
    platform_msg_id: pmid ?? '',
    created_at: new Date(0).toISOString(),
  } as ApiItem;
}

function work(key: string, complete: boolean): TranscriptRow {
  return {
    key,
    role: 'system',
    text: '',
    kind: 'work',
    workActive: false,
    workComplete: complete,
    workStartedAt: 1,
    workEndedAt: 2,
    steps: [{ key: `${key}-0`, kind: 'tool', tool: 'bash', toolStatus: 'ok' }],
  };
}

describe('transcriptItemToRow', () => {
  it('carries a user row platform_msg_id as clientMsgId (reconciles the optimistic bubble)', () => {
    const row = transcriptItemToRow(SID, msg(3, 'user', 'hi', 'client-uuid'));
    expect(row.key).toBe(`row-${SID}-m3`);
    expect(row.clientMsgId).toBe('client-uuid');
  });

  it('keys every row by the stable server id (m/w/n)', () => {
    expect(transcriptItemToRow(SID, msg(4, 'assistant', 'yo')).key).toBe(`row-${SID}-m4`);
    const notice = transcriptItemToRow(SID, {
      id: 'n2',
      kind: 'notice',
      role: '',
      text: 'compacted',
      has_attachments: false,
      notice_level: 'info',
      created_at: new Date(0).toISOString(),
    } as ApiItem);
    expect(notice.key).toBe(`row-${SID}-n2`);
    expect(notice.notice?.text).toBe('compacted');
  });
});

describe('applySyncReplace (baseline / rebase REPLACE + overlay)', () => {
  it('REPLACEs the thread but re-overlays an unconfirmed optimistic send absent from the page', () => {
    const prev: TranscriptRow[] = [
      { key: 'old', role: 'assistant', text: 'stale' },
      { key: 'pending-x', role: 'user', text: 'just sent', pending: true, clientMsgId: 'x' },
    ];
    const page = [transcriptItemToRow(SID, msg(0, 'user', 'hello'))];
    const out = applySyncReplace(prev, page, new Set(['x']), null);
    // The page replaces the thread; the optimistic send (unconfirmed, absent
    // from the page) is re-appended so it never vanishes from screen.
    expect(out.map((r) => r.text)).toEqual(['hello', 'just sent']);
    expect(out.find((r) => r.clientMsgId === 'x')?.pending).toBe(true);
  });

  it('does not re-overlay a send the page already carries', () => {
    const prev: TranscriptRow[] = [
      { key: 'pending-x', role: 'user', text: 'sent', pending: true, clientMsgId: 'x' },
    ];
    const page = [transcriptItemToRow(SID, msg(5, 'user', 'sent', 'x'))];
    const out = applySyncReplace(prev, page, new Set(['x']), null);
    expect(out).toHaveLength(1); // the page's persisted row stands alone
    expect(out[0].clientMsgId).toBe('x');
  });

  // The page is a SNAPSHOT. A reply persisted after it was taken arrives over
  // the socket with its own ordinal, and that frame advances the cursor — so a
  // REPLACE that drops the row loses it for good: a difference selects strictly
  // `>` the cursor the row itself set. A cold open is the one path that runs a
  // baseline, which is why this read as "the newest message never arrives".
  it('keeps a live reply whose ordinal the page predates', () => {
    const page = [
      transcriptItemToRow(SID, msg(9, 'user', 'question')),
      transcriptItemToRow(SID, msg(10, 'assistant', 'older answer')),
    ];
    const live: TranscriptRow = { key: `row-${SID}-m12`, role: 'assistant', text: 'the newest' };
    const out = applySyncReplace([...page, live], page, new Set(), null);
    expect(out.map((r) => r.text)).toEqual(['question', 'older answer', 'the newest']);
  });

  it('drops a stale row the page covers and disagrees with — that is what REPLACE means', () => {
    const page = [transcriptItemToRow(SID, msg(10, 'assistant', 'the truth'))];
    const stale: TranscriptRow = { key: `row-${SID}-m8`, role: 'assistant', text: 'rebased away' };
    expect(applySyncReplace([stale], page, new Set(), null).map((r) => r.text)).toEqual([
      'the truth',
    ]);
  });

  it('keeps nothing twice when the page already carries the row', () => {
    const page = [transcriptItemToRow(SID, msg(10, 'assistant', 'answer'))];
    expect(applySyncReplace(page, page, new Set(), null)).toHaveLength(1);
  });

  // A session's rows are never deleted, so an empty page against a thread that
  // holds rows is always a pre-persist read — the shape every fresh session's
  // baseline can produce (the gateway echoes before it writes, and a null
  // cursor keeps every sync a baseline). Applying it deleted every
  // ordinal-less row outside the kept sets — the clientMsgId-less
  // echo-appended user row most of all.
  it('treats an empty page against a non-empty thread as stale — nothing moves', () => {
    const prev: TranscriptRow[] = [
      { key: 'echoed', role: 'user', text: 'first message' },
      { key: `row-${SID}-m12`, role: 'assistant', text: 'the reply' },
    ];
    expect(applySyncReplace(prev, [], new Set(), null)).toBe(prev);
  });

  // The other half of the same hole: the kept sets return BEHIND the page, so
  // an owed ordinal-less first send re-filed below the ordinal-bearing reply
  // that outran it — [reply, user], permanent (the reply's ordinal advanced
  // the cursor, differences select strictly `>`, nothing re-orders it).
  it('never re-files an owed first send below a reply the empty page predates', () => {
    const prev: TranscriptRow[] = [
      { key: 'pending-x', role: 'user', text: 'just sent', pending: true, clientMsgId: 'x' },
      { key: `row-${SID}-m12`, role: 'assistant', text: 'the reply' },
    ];
    expect(applySyncReplace(prev, [], new Set(['x']), null).map((r) => r.text)).toEqual([
      'just sent',
      'the reply',
    ]);
  });
});

describe('finalizeMessage — the echo fall-through append', () => {
  // The append arm used to mint a key and nothing else, so an echo-appended
  // user row (this tab reloaded mid-send; a sibling tab sent) had no
  // clientMsgId — invisible to the kept-set overlay, unmatchable by a
  // re-echo, and unretirable by applySyncMerge's reconciliation.
  it('carries the echo platform_msg_id as clientMsgId, and the overlay can then hold the row', () => {
    const rows = finalizeMessage([], 'user', 'reloaded mid-send', false, [], 'client-uuid');
    expect(rows[0].clientMsgId).toBe('client-uuid');

    const page = [transcriptItemToRow(SID, msg(10, 'assistant', 'answer'))];
    expect(
      applySyncReplace(rows, page, new Set(['client-uuid']), null).map((r) => r.text),
    ).toEqual(['answer', 'reloaded mid-send']);
  });

  it('a plain append without one stays clientMsgId-less (assistant rows never get one)', () => {
    const rows = finalizeMessage([], 'assistant', 'an answer', false);
    expect(rows[0].clientMsgId).toBeUndefined();
  });
});

// A rebase says only that the difference outran the server's limit and here is
// the newest page instead — ordinals are not rewritten, so the pages a reader
// scrolled up for are still true. Dropping them costs them the history AND the
// round trips that fetched it, mid-read. iOS has kept it since af7372bc.
describe('rowsAboveFloor — a rebase must not cost the reader the history they paged in', () => {
  const none = new Set<string>();
  const at = (ordinal: number): TranscriptRow => ({
    key: `row-${SID}-m${ordinal}`,
    role: 'assistant',
    text: `r${ordinal}`,
  });
  const notice = (id: string): TranscriptRow => ({
    key: `row-${SID}-n${id}`,
    role: 'system',
    text: '',
    notice: { level: 'info', text: 'Stopped.' },
  });

  it('keeps everything before the first row the page covers', () => {
    const rows = [at(10), at(20), at(30), at(40)];
    expect(rowsAboveFloor(rows, 30, none).map((r) => r.key)).toEqual([
      `row-${SID}-m10`,
      `row-${SID}-m20`,
    ]);
  });

  it('cuts by POSITION, so an ordinal-less notice stays with its neighbours', () => {
    const rows = [at(10), notice('7'), at(30)];
    expect(rowsAboveFloor(rows, 30, none).map((r) => r.key)).toEqual([
      `row-${SID}-m10`,
      `row-${SID}-n7`,
    ]);
  });

  it('drops a row the rebuilt thread already carries — never twice', () => {
    const rows = [at(10), at(20), at(30)];
    const taken = new Set([`row-${SID}-m20`]);
    expect(rowsAboveFloor(rows, 30, taken).map((r) => r.key)).toEqual([`row-${SID}-m10`]);
  });

  it('keeps nothing when the page reaches the whole thread', () => {
    expect(rowsAboveFloor([at(30), at(40)], 30, none)).toEqual([]);
  });
});

// The cost of keeping the head: the page re-cuts a turn the head still holds. A
// block cut at its START is flushed `turn_complete: true` — `flush` only ever
// learns about a block's END — so both halves claim to be whole turns and the
// fold declines, rendering one turn as two cards.
describe('joinKeptHead — a turn the page re-cut at its start is still one turn', () => {
  const workRow = (ordinal: number, complete: boolean, callId: string): TranscriptRow => ({
    key: `row-${SID}-w${ordinal}`,
    role: 'system',
    text: '',
    kind: 'work',
    workActive: false,
    workComplete: complete,
    workStartedAt: 1_000,
    workEndedAt: 5_000,
    steps: [{ key: `${ordinal}-0`, kind: 'tool', toolCallId: callId, tool: 'bash' }],
  });

  it('fuses the head half with the page half into one card', () => {
    const head = [transcriptItemToRow(SID, msg(9, 'user', 'go')), workRow(10, true, 'c1')];
    const rebuilt = [workRow(20, true, 'c2'), transcriptItemToRow(SID, msg(30, 'assistant', 'done'))];
    const out = joinKeptHead(head, rebuilt, []);
    expect(out.map((r) => r.key)).toEqual([
      `row-${SID}-m9`,
      `row-${SID}-w10`,
      `row-${SID}-m30`,
    ]);
    expect(out[1].steps?.map((s) => s.toolCallId)).toEqual(['c1', 'c2']);
  });

  it('refuses across a compaction watermark — those halves are two turns', () => {
    const head = [workRow(10, true, 'c1')];
    const rebuilt = [workRow(20, true, 'c2')];
    const points = [{ ordinal: 15, at: '2026-08-09T10:00:00.000Z' }];
    expect(joinKeptHead(head, rebuilt, points)).toHaveLength(2);
  });

  it('leaves a head that does not end on a work block alone', () => {
    const head = [workRow(10, true, 'c1'), transcriptItemToRow(SID, msg(12, 'assistant', 'answered'))];
    const rebuilt = [workRow(20, true, 'c2')];
    expect(joinKeptHead(head, rebuilt, []).map((r) => r.key)).toEqual([
      `row-${SID}-w10`,
      `row-${SID}-m12`,
      `row-${SID}-w20`,
    ]);
  });
});

describe('applySyncMerge (difference append + dedup)', () => {
  it('appends new rows and reconciles an optimistic send by platform_msg_id', () => {
    const prev: TranscriptRow[] = [
      { key: 'pending-x', role: 'user', text: 'q', pending: true, clientMsgId: 'x' },
    ];
    const page = [
      transcriptItemToRow(SID, msg(2, 'user', 'q', 'x')), // redelivery of our send
      transcriptItemToRow(SID, msg(3, 'assistant', 'a')), // new assistant reply
    ];
    const out = applySyncMerge(prev, page, []);
    // The optimistic row is reconciled in place (pending cleared), the reply
    // appended — no duplicate user bubble.
    const users = out.filter((r) => r.role === 'user');
    expect(users).toHaveLength(1);
    expect(users[0].pending).toBeFalsy();
    expect(out.some((r) => r.role === 'assistant' && r.text === 'a')).toBe(true);
  });

  it('a redelivered row already on screen is a no-op', () => {
    const prev: TranscriptRow[] = [{ key: `row-${SID}-m3`, role: 'assistant', text: 'a' }];
    const page = [transcriptItemToRow(SID, msg(3, 'assistant', 'a'))];
    expect(applySyncMerge(prev, page, [])).toHaveLength(1);
  });

  it('reconciles a live /stop-ack notice with its durable twin by content (no double)', () => {
    // The `/stop` ack is persisted AFTER its emit, so the live frame carried no
    // `durable_id` and the row was minted with a client `notice-…` key. Its
    // synced `n<seq>` twin must adopt that row, not append a second card.
    const ack = 'Stopped.\n- Cancelled the in-progress reply.';
    const prev: TranscriptRow[] = [
      { key: `notice-${SID}-0-1700`, role: 'system', text: '', notice: { level: 'info', text: ack } },
    ];
    const synced = transcriptItemToRow(SID, {
      id: 'n4',
      kind: 'notice',
      role: '',
      text: ack,
      has_attachments: false,
      notice_level: 'info',
      created_at: new Date(0).toISOString(),
    } as ApiItem);
    const out = applySyncMerge(prev, [synced], []);
    expect(out).toHaveLength(1); // reconciled, not doubled
    expect(out[0].key).toBe(`row-${SID}-n4`); // adopted the durable key
    // A second sync now dedups by key.
    expect(applySyncMerge(out, [synced], [])).toBe(out);
  });

  it('reconciles a durable /compact command echo with the optimistic bubble by platform_msg_id', () => {
    // The Command control event now carries the send's platform_msg_id, so its
    // synced user row reconciles with the optimistic `/compact` bubble instead
    // of appending a second command bubble.
    const prev: TranscriptRow[] = [
      { key: 'pending-c', role: 'user', text: '/compact', pending: true, clientMsgId: 'c' },
    ];
    const synced = transcriptItemToRow(SID, {
      id: 'n2',
      kind: 'message',
      role: 'user',
      text: '/compact',
      has_attachments: false,
      platform_msg_id: 'c',
      created_at: new Date(0).toISOString(),
    } as ApiItem);
    const out = applySyncMerge(prev, [synced], []);
    const commands = out.filter((r) => r.text === '/compact');
    expect(commands).toHaveLength(1); // reconciled, not doubled
    expect(commands[0].pending).toBeFalsy();
  });

  it('never fuses across a compaction boundary at the merge seam', () => {
    // The cursor fell mid-turn, so the thread's trailing cut-off block meets
    // this page's leading one — but a compaction watermark landed in the GAP
    // between the two windows, which neither reconstruction split. They are two
    // turns; fusing them takes the older key and eats the divider's anchor row.
    const points = [{ ordinal: 30, at: 't' }];
    const prev: TranscriptRow[] = [
      { key: `row-${SID}-m10`, role: 'assistant', text: 'a' },
      work(`row-${SID}-w20`, false),
    ];
    const page = [work(`row-${SID}-w40`, true)];
    const out = applySyncMerge(prev, page, points);
    expect(out.map((r) => r.key)).toEqual([
      `row-${SID}-m10`,
      `row-${SID}-w20`,
      `row-${SID}-w40`,
    ]);
    // …and the seam the divider keys off is still on screen.
    expect([...compactionDividerKeys(out, points).keys()]).toEqual([`row-${SID}-w40`]);
    // No boundary between them ⇒ one turn the page edge cut, still one card.
    expect(applySyncMerge(prev, page, [])).toHaveLength(2);
  });
});

// A sync that runs WHILE the turn is running (a revisit, a `Frame::Gap`) gets
// back the gateway's partial reconstruction of that same turn, deliberately
// stamped with the live turn's `started_at` so it lands on the open block. That
// row is `workActive: false` like every REST row — adopting it wholesale closed
// a block whose turn had not ended, and the next progress frame then found a
// frozen tail and opened a SECOND card with no row between them. Nothing healed
// it: `foldAdjacentWork` runs at the end of the merge, not on later live frames.
describe('applySyncMerge — a mid-turn difference must not close the turn it is watching', () => {
  const T = 1_700_000_000_000;

  const partial = (ordinal: number, complete: boolean): TranscriptRow => ({
    ...work(`row-${SID}-w${ordinal}`, complete),
    workStartedAt: T,
    workEndedAt: T + 5_000,
    steps: [{ key: 'srv-0', kind: 'tool', toolCallId: 'c1', tool: 'bash', toolStatus: 'ok' }],
  });

  const live = (): TranscriptRow[] =>
    pushToolStartedStep(applyTurnState([], true, T), 'c1', 'bash', 'Bash(ls)', true);

  it('keeps the block live and unions the steps when the page says the turn continues', () => {
    const out = applySyncMerge(live(), [partial(7, false)], []);
    expect(out).toHaveLength(1);
    expect(out[0].workActive).toBe(true);
    expect(out[0].workComplete).toBe(false);
    expect(out[0].steps?.map((s) => s.toolCallId)).toEqual(['c1']);
  });

  it('so a progress frame landing after it extends that block instead of forking a card', () => {
    const merged = applySyncMerge(live(), [partial(7, false)], []);
    const after = pushToolStartedStep(merged, 'c2', 'read', 'Read(x)', true);
    expect(after.filter((r) => r.kind === 'work')).toHaveLength(1);
    expect(after[0].steps?.map((s) => s.toolCallId)).toEqual(['c1', 'c2']);
  });

  it('still lets a COMPLETE page row supersede — that turn ended while frames were lost', () => {
    const out = applySyncMerge(live(), [partial(7, true)], []);
    expect(out).toHaveLength(1);
    expect(out[0].workActive).toBe(false);
    expect(out[0].key).toBe(`row-${SID}-w7`);
  });
});

// A cold open applies the baseline REPLACE before the `subscribe_state` bundle
// arrives, so `turn` is still null and the page's in-flight block stays closed.
// A progress frame landing in that window used to fork a second card, because
// `ensureWork`'s reuse test rejected the `null` tri-state. The server's own
// cut-off flag answers the question the turn state cannot yet.
describe('ensureWork — an unknown turn state must not fork the block the page just delivered', () => {
  it('extends a cut-off block when the turn state is not known yet', () => {
    const page = [transcriptItemToRow(SID, msg(6, 'user', 'go')), work(`row-${SID}-w7`, false)];
    const rows = applySyncReplace([], page, new Set(), null);
    const after = pushToolStartedStep(rows, 'c9', 'read', 'Read(x)', null);
    expect(after.filter((r) => r.kind === 'work')).toHaveLength(1);
    // The page's own step is still there — the frame extended that block.
    expect(after.at(-1)?.steps).toHaveLength(2);
    expect(after.at(-1)?.steps?.at(-1)?.toolCallId).toBe('c9');
  });

  it('but a COMPLETE block is a finished turn, and the frame opens its own card', () => {
    const page = [transcriptItemToRow(SID, msg(6, 'user', 'go')), work(`row-${SID}-w7`, true)];
    const rows = applySyncReplace([], page, new Set(), null);
    const after = pushToolStartedStep(rows, 'c9', 'read', 'Read(x)', null);
    expect(after.filter((r) => r.kind === 'work')).toHaveLength(2);
  });
});

describe('syncSince — a difference page must EXTEND the thread, never overlap it', () => {
  // The iOS scar, which this client shares: the cursor is a COVERAGE watermark,
  // and `loadOlder` renders rows without advancing it. Ask for `since = cursor`
  // from such a thread and the server answers a correct DIFFERENCE (no rebase,
  // so nothing self-heals) that `applySyncMerge` welds onto the bottom.
  const rendered = (): TranscriptRow[] => [
    { key: `row-${SID}-m10`, role: 'assistant', text: 'older' },
    { key: `row-${SID}-m20`, role: 'assistant', text: 'newer' },
  ];

  it('takes the baseline when a rendered row sits above the cursor', () => {
    expect(syncSince(5, rendered())).toBeNull();
  });

  it('keeps differencing when the cursor covers the thread', () => {
    expect(syncSince(20, rendered())).toBe(20);
    expect(syncSince(99, rendered())).toBe(99);
  });

  it('is quiet on rows carrying no ordinal — an optimistic send is not durable coverage', () => {
    const rows: TranscriptRow[] = [
      ...rendered(),
      { key: 'pending-x', role: 'user', text: 'just sent', pending: true, clientMsgId: 'x' },
      { key: `notice-${SID}-0-1700`, role: 'system', text: '', notice: { level: 'info', text: 'n' } },
    ];
    expect(syncSince(20, rows)).toBe(20);
  });

  it('stays null with no cursor at all (the fresh-tab baseline)', () => {
    expect(syncSince(null, rendered())).toBeNull();
  });

  it('is what stands between the overlap and the merge', () => {
    // What the merge does with the page the guard now refuses: rows 11..19 land
    // BELOW m20 instead of between m10 and it.
    const page = [transcriptItemToRow(SID, msg(18, 'assistant', 'missing'))];
    expect(applySyncMerge(rendered(), page, []).map((r) => r.key)).toEqual([
      `row-${SID}-m10`,
      `row-${SID}-m20`,
      `row-${SID}-m18`,
    ]);
  });
});

describe('compactionDividerKeys — the pre-compaction seam', () => {
  const row = (key: string): TranscriptRow => ({ key, role: 'user', text: '' });
  // Under Philosophy B the machinery ordinals (3..6) are hidden, so the
  // displayed thread jumps 2 → 7 across the compaction watermark at ordinal 3.
  const thread = [
    row('row-s-m0'),
    row('row-s-m1'),
    row('row-s-m2'),
    row('row-s-m7'),
    row('row-s-m8'),
  ];

  it('marks the first row at/after the watermark, once', () => {
    const keys = compactionDividerKeys(thread, [{ ordinal: 3, at: '2026-07-22T10:00:00Z' }]);
    expect([...keys.entries()]).toEqual([['row-s-m7', '2026-07-22T10:00:00Z']]);
  });

  it('draws nothing when the boundary is above every loaded row (not paged in yet)', () => {
    // Only post-compaction rows loaded — the originals below the seam aren't here.
    const keys = compactionDividerKeys([row('row-s-m7'), row('row-s-m8')], [
      { ordinal: 3, at: '2026-07-22T10:00:00Z' },
    ]);
    expect(keys.size).toBe(0);
  });

  it('is empty when the session was never compacted', () => {
    expect(compactionDividerKeys(thread, []).size).toBe(0);
  });

  it('handles two compactions at their own seams', () => {
    const twice = [row('row-s-m0'), row('row-s-m4'), row('row-s-m9')];
    const keys = compactionDividerKeys(twice, [
      { ordinal: 2, at: 't1' },
      { ordinal: 6, at: 't2' },
    ]);
    expect([...keys.entries()]).toEqual([
      ['row-s-m4', 't1'],
      ['row-s-m9', 't2'],
    ]);
  });

  it('ignores rows with no message/work ordinal (interleaved notices)', () => {
    // A notice (`n<seq>`) between the last original and the first post-row must
    // not swallow or misplace the seam — the divider still lands on m7.
    const withNotice = [row('row-s-m2'), row('row-s-n5'), row('row-s-m7')];
    const keys = compactionDividerKeys(withNotice, [{ ordinal: 3, at: 't' }]);
    expect([...keys.keys()]).toEqual(['row-s-m7']);
  });
});

describe('shouldAutoLoadOlder — reach older history when the thread underfills', () => {
  const base = {
    hasMore: true,
    olderLoading: false,
    historyLoading: false,
    scrollHeight: 300,
    clientHeight: 800,
    slackPx: 4,
  };

  it('fires when there is more history and no scrollable overflow', () => {
    // The exact stuck case: a compacted session whose post-compaction tail folds
    // to a couple of cards, too short to scroll — auto-load must page it back.
    expect(shouldAutoLoadOlder(base)).toBe(true);
  });
  it('does not fire once the content overflows (normal scroll works)', () => {
    expect(shouldAutoLoadOlder({ ...base, scrollHeight: 1200 })).toBe(false);
  });
  it('never fires while in flight, during the initial load, or at the floor', () => {
    expect(shouldAutoLoadOlder({ ...base, olderLoading: true })).toBe(false);
    expect(shouldAutoLoadOlder({ ...base, historyLoading: true })).toBe(false);
    expect(shouldAutoLoadOlder({ ...base, hasMore: false })).toBe(false);
  });
});
