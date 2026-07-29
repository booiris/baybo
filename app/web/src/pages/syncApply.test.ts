import { describe, expect, it } from 'vitest';
import type { components } from '../api/schema';
import {
  applySyncMerge,
  applySyncReplace,
  compactionDividerKeys,
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
