import { describe, expect, it } from 'vitest';

import {
  EMPTY_VIEW,
  applySubscribeState,
  applySyncReplace,
  bundleAnswer,
  type SessionView,
  type TranscriptRow,
} from './ChatPage';

// A `subscribe_state` bundle is the whole coalesced turn, so it REPLACES the
// open block's steps. Its TRAILING prose step, though, is the answer streaming
// right now — the live view renders that as the reply bubble BELOW the block,
// not as a work step. iOS has always routed it there; the web dropped it into
// the block, so one mid-turn reconnect showed the in-progress answer as a
// caret-less work step here and as a growing reply there. These pin the fix and
// the shapes around it.

const TURN_START = '2026-08-02T04:00:00Z';

const frame = (
  steps: Array<{ kind: string; text?: string; call_id?: string; label?: string; status?: string }>,
  active = true,
) =>
  ({
    kind: 'subscribe_state',
    session_id: 's1',
    turn: { active, started_at: TURN_START },
    work_steps: steps,
    pending_approvals: [],
  }) as unknown as Parameters<typeof applySubscribeState>[1];

const openBlock = (steps: TranscriptRow['steps'] = []): TranscriptRow => ({
  key: 'work-1-0',
  role: 'system',
  text: '',
  kind: 'work',
  workActive: true,
  workStartedAt: Date.parse(TURN_START),
  steps,
});

const streamingReply = (text: string): TranscriptRow => ({
  key: 'stream-1-1',
  role: 'assistant',
  text,
  streaming: true,
  createdAt: TURN_START,
});

const view = (transcript: TranscriptRow[]): SessionView => ({
  ...EMPTY_VIEW,
  transcript,
  turn: { active: true, startedAt: Date.parse(TURN_START) },
});

const shape = (v: SessionView) =>
  v.transcript.map((r) =>
    r.kind === 'work' ? `work[${(r.steps ?? []).map((s) => s.kind).join(',')}]` : `${r.role}:${r.text}`,
  );

describe('applySubscribeState — the trailing prose step is the live reply, not a work step', () => {
  it('routes the tail prose to the streaming bubble and keeps the rest in the block', () => {
    const next = applySubscribeState(
      view([openBlock(), streamingReply('部分')]),
      frame([
        { kind: 'reasoning', text: 'thinking' },
        { kind: 'tool', call_id: 'c1', label: 'Bash', status: 'ok' },
        { kind: 'prose', text: '正在回答的这一段' },
      ]),
      false,
    );
    expect(shape(next)).toEqual(['work[reasoning,tool]', 'assistant:正在回答的这一段']);
  });

  it('grows the existing bubble IN PLACE rather than remounting it', () => {
    const before = view([openBlock(), streamingReply('正在')]);
    const next = applySubscribeState(
      before,
      frame([{ kind: 'tool', call_id: 'c1', label: 'Bash', status: 'ok' }, { kind: 'prose', text: '正在回答' }]),
      false,
    );
    const bubble = next.transcript[next.transcript.length - 1];
    // Same React key ⇒ no remount ⇒ the reply does not blank for a frame.
    expect(bubble.key).toBe('stream-1-1');
    expect(bubble.text).toBe('正在回答');
  });

  it('leaves no empty "Working" card hovering above the reply when the bundle is answer-only', () => {
    const next = applySubscribeState(
      view([openBlock(), streamingReply('部分')]),
      frame([{ kind: 'prose', text: '直接作答' }]),
      false,
    );
    expect(shape(next)).toEqual(['assistant:直接作答']);
  });

  it('keeps a bubble the bundle says nothing about — machinery alone is not evidence', () => {
    // A bundle with no answer text does NOT prove the reply on screen is stale;
    // the in-flight buffer is cleared by Message/TurnState while the gateway
    // still reports the turn active. Deleting here loses a reply mid-read.
    const next = applySubscribeState(
      view([openBlock(), streamingReply('刚流出来的一段')]),
      frame([{ kind: 'tool', call_id: 'c1', label: 'Bash', status: 'ok' }]),
      false,
    );
    expect(shape(next)).toEqual(['work[tool]', 'assistant:刚流出来的一段']);
  });

  it('keeps the live spinner card for a turn that has produced nothing yet', () => {
    const next = applySubscribeState(view([openBlock()]), frame([]), false);
    expect(shape(next)).toEqual(['work[]']);
    expect(next.transcript[0].workActive).toBe(true);
  });

  it('is a no-op on the transcript when the turn already ended (stale bundle)', () => {
    const before = view([openBlock(), streamingReply('部分')]);
    const next = applySubscribeState(before, frame([{ kind: 'prose', text: 'x' }]), true);
    expect(next.transcript).toBe(before.transcript);
  });
});

// ── The REST/sync plane carries the same in-flight answer ─────────────────
//
// `build_history_page` folds the live channel's in-flight buffer into the
// trailing work block (crates/gateway/src/api/admin/chat.rs), and an
// `AnswerDelta` becomes a `prose` step there. So a BASELINE sync taken mid-
// answer returns a page whose trailing block ends with the very text the reply
// bubble is streaming. Found by an adversarial review of this change: before
// it, that step was hidden inside the collapse; after it, `segmentWorkSteps`
// paints it at answer typography directly above the identical bubble — and it
// stays there permanently once the turn ends.

const workItem = (steps: Array<{ kind: string; text?: string }>): TranscriptRow => ({
  key: 'row-s1-w4',
  role: 'system',
  text: '',
  kind: 'work',
  workStartedAt: Date.parse(TURN_START),
  workEndedAt: Date.parse(TURN_START) + 3000,
  steps: steps.map((s, i) => ({ key: `k${i}`, ...s })) as TranscriptRow['steps'],
});

const userRow = (text: string): TranscriptRow => ({
  key: 'row-s1-m3',
  role: 'user',
  text,
  createdAt: TURN_START,
});

const ACTIVE = { active: true, startedAt: Date.parse(TURN_START) } as const;

describe('applySyncReplace — the page\'s in-flight answer is the reply, not a work step', () => {
  it('does not render the streaming answer twice on a mid-answer baseline sync', () => {
    const prev = [userRow('写个总结'), openBlock([{ key: 't0', kind: 'tool', toolCallId: 'c1' }]), streamingReply('总结如下：第一点')];
    const page = [userRow('写个总结'), workItem([{ kind: 'tool' }, { kind: 'prose', text: '总结如下：第一点' }])];
    const rows = applySyncReplace(prev, page, new Set(), ACTIVE);
    expect(
      rows.map((r) => (r.kind === 'work' ? `work[${(r.steps ?? []).map((s) => s.kind).join(',')}]` : `${r.role}:${r.text}`)),
    ).toEqual(['user:写个总结', 'work[tool]', 'assistant:总结如下：第一点']);
  });

  it('drops a block that held nothing but the in-flight answer (tool-free turn)', () => {
    const prev = [userRow('你好'), streamingReply('你好，我是')];
    const page = [userRow('你好'), workItem([{ kind: 'prose', text: '你好，我是' }])];
    const rows = applySyncReplace(prev, page, new Set(), ACTIVE);
    expect(rows.map((r) => `${r.role}:${r.text}`)).toEqual(['user:你好', 'assistant:你好，我是']);
  });

  it('keeps genuine mid-turn narration — a persisted prose step is never a block\'s last', () => {
    const prev = [userRow('查一下'), openBlock()];
    const page = [userRow('查一下'), workItem([{ kind: 'prose', text: '我先看看配置' }, { kind: 'tool' }])];
    const rows = applySyncReplace(prev, page, new Set(), ACTIVE);
    const work = rows.find((r) => r.kind === 'work');
    expect((work?.steps ?? []).map((s) => s.kind)).toEqual(['prose', 'tool']);
  });

  it('leaves a finished turn alone (no active turn ⇒ no in-flight tail to strip)', () => {
    const page = [userRow('查一下'), workItem([{ kind: 'tool' }, { kind: 'prose', text: '说了点什么' }])];
    const rows = applySyncReplace([], page, new Set(), null);
    const work = rows.find((r) => r.kind === 'work');
    expect((work?.steps ?? []).map((s) => s.kind)).toEqual(['tool', 'prose']);
  });

  // The live final `Frame::Message` can go missing (reconnect, `Frame::Gap`),
  // leaving a `streaming` bubble on screen for a turn that has already ended.
  // The next REPLACE page carries the server's finished reply, so re-overlaying
  // the local partial rendered the same answer twice — the complete row then a
  // truncated prefix of it — and the overlaid row stayed `streaming`, so it
  // doubled again on every later REPLACE. `applySyncMerge` folds the persisted
  // final into the bubble for this same case; in REPLACE the page already has
  // it, so the fold is a drop.
  it('drops a stale streaming partial when the turn has ended and the page has the reply', () => {
    const prev = [userRow('summarize this'), streamingReply('here is the first')];
    const page = [
      userRow('summarize this'),
      { key: 'row-s1-m4', role: 'assistant', text: 'here is the first point, and the second' } as TranscriptRow,
    ];
    const rows = applySyncReplace(prev, page, new Set(), null);
    expect(rows.map((r) => `${r.role}:${r.text}`)).toEqual([
      'user:summarize this',
      'assistant:here is the first point, and the second',
    ]);
  });

  it('still overlays the partial while the turn is genuinely running', () => {
    const prev = [userRow('summarize this'), streamingReply('here is the first')];
    const page = [userRow('summarize this')];
    const rows = applySyncReplace(prev, page, new Set(), ACTIVE);
    const tail = rows[rows.length - 1];
    expect(tail.streaming).toBe(true);
    expect(tail.text).toBe('here is the first');
  });
});

describe('bundleAnswer — one judgement, two consumers', () => {
  // The hoist and the pacer seed must agree on what is already painted. If they
  // ever disagreed the pacer would overwrite the reply the hoist recovered.
  it('recovers the trailing prose as the answer in flight', () => {
    expect(bundleAnswer([{ kind: 'tool' }, { kind: 'prose', text: '答案' }])).toEqual({
      kind: 'recovered',
      text: '答案',
    });
  });

  it('calls a bubble superseded when the bundle holds the text but has moved past it', () => {
    expect(bundleAnswer([{ kind: 'prose', text: '叙述' }, { kind: 'tool' }])).toEqual({
      kind: 'superseded',
    });
  });

  // The one that matters: a bundle with no answer text is NOT evidence that the
  // reply on screen is stale. Reachable without a race — the in-flight buffer is
  // cleared by Message/TurnState while the gateway still reports turn.active.
  it('says nothing about a bubble when it carries no answer text at all', () => {
    expect(bundleAnswer([])).toEqual({ kind: 'unknown' });
    expect(bundleAnswer([{ kind: 'reasoning', text: 'r' }, { kind: 'tool' }])).toEqual({
      kind: 'unknown',
    });
  });
});

describe('applySubscribeState — an uninformative bundle must not delete the reply', () => {
  it('keeps the reply when the bundle carries no answer text (finalization window)', () => {
    const next = applySubscribeState(
      view([openBlock([{ key: 't0', kind: 'tool', toolCallId: 'c1' }]), streamingReply('答案的前半段')]),
      frame([]),
      false,
    );
    expect(shape(next)).toEqual(['work[tool]', 'assistant:答案的前半段']);
  });

  it('does not wipe live steps with an empty bundle', () => {
    const next = applySubscribeState(
      view([openBlock([{ key: 'r0', kind: 'reasoning', text: '想一下' }])]),
      frame([]),
      false,
    );
    expect(shape(next)).toEqual(['work[reasoning]']);
  });

  it('still drops the reply when the bundle demonstrably moved past it', () => {
    const next = applySubscribeState(
      view([openBlock(), streamingReply('过期的一段')]),
      frame([{ kind: 'prose', text: '过期的一段' }, { kind: 'tool', call_id: 'c1', label: 'Bash', status: 'ok' }]),
      false,
    );
    expect(shape(next)).toEqual(['work[prose,tool]']);
  });
});

describe('applySubscribeState — a reply already on screen must not fork', () => {
  // Reconnect after the model narrated straight into a bubble and then called a
  // tool: the turn has no block on screen, so `applyTurnState` would append one
  // BELOW the bubble and `writeStreamingAnswer` would fork a second bubble under
  // that — the same paragraph twice, two carets, one of them never finalized.
  it('keeps one reply when the turn had no block on screen yet', () => {
    const next = applySubscribeState(
      view([
        { key: 'row-s1-m3', role: 'user', text: '查一下超时', createdAt: TURN_START },
        streamingReply('我先看看配置。'),
      ]),
      frame([
        { kind: 'prose', text: '我先看看配置。' },
        { kind: 'tool', call_id: 'c1', label: 'Read', status: 'ok' },
        { kind: 'prose', text: '超时是 30 秒。' },
      ]),
      false,
    );
    expect(shape(next)).toEqual(['user:查一下超时', 'work[prose,tool]', 'assistant:超时是 30 秒。']);
    // …and the surviving bubble is the ORIGINAL row, so React keeps the node.
    expect(next.transcript[2].key).toBe('stream-1-1');
  });
});
