import { describe, expect, it } from 'vitest';
import type { components } from '../api/schema';
import {
  applySyncMerge,
  applySyncReplace,
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
    const out = applySyncMerge(prev, page);
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
    expect(applySyncMerge(prev, page)).toHaveLength(1);
  });
});
