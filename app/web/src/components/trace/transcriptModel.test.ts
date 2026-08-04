/**
 * Pins the transcript-as-trace projection for an external agent
 * (claude / codex): `buildTranscriptNodes` + `transcriptPreview`.
 *
 * Source of truth for the input shape is `ContentBlock` / `ChatMessage` /
 * `SessionMessageRow` in `src/types/trace.ts`, which mirror
 * `crates/model/src/message.rs` and `baybo_session::StoredMessage`. Such a
 * session records no step/span tree at all, so these rows ARE its trace —
 * the two rules worth defending are (a) a `ToolUse` and the `ToolResult`
 * that quotes its id fold into ONE row even though the result arrives in a
 * LATER transcript row, and (b) nothing is ever silently dropped: a call
 * still in flight renders with `output: null` (the live case) and a result
 * whose call is missing still gets a row of its own.
 */
import { describe, expect, it } from 'vitest';
import type {
  ChatMessage,
  ContentBlock,
  MessageSource,
  Role,
  SessionMessageRow,
  ThinkingContent,
} from '../../types/trace';
import { buildTranscriptNodes, transcriptPreview } from './transcriptModel';
import type { TranscriptNode } from './transcriptModel';

const T0 = '2026-01-01T00:00:00.000Z';
const T1 = '2026-01-01T00:00:01.000Z';
const T2 = '2026-01-01T00:00:02.000Z';
const T3 = '2026-01-01T00:00:03.000Z';

function msg(role: Role, source: MessageSource, content: ContentBlock[]): ChatMessage {
  return { role, content, source };
}

function row(ordinal: number, createdAt: string, message: ChatMessage): SessionMessageRow {
  return { ordinal, created_at: createdAt, message };
}

function toolUse(id: string, name: string, input: unknown): ContentBlock {
  return { ToolUse: { id, name, input } };
}

function toolResult(toolUseId: string, content: string): ContentBlock {
  return { ToolResult: { tool_use_id: toolUseId, content } };
}

function thinking(content: ThinkingContent[]): ContentBlock {
  return { Thinking: { content } };
}

/** A whole external-agent turn: prompt → reply + two calls → results → answer. */
function realisticLog(): SessionMessageRow[] {
  return [
    row(11, T0, msg('user', 'user', [{ Text: 'fix the build' }])),
    row(12, T1, msg('assistant', 'agent', [
      { Text: 'Looking at CI.' },
      toolUse('tu_1', 'bash', { command: 'cargo test' }),
      toolUse('tu_2', 'read_file', { path: 'ci.yml' }),
    ])),
    row(13, T2, msg('user', 'agent', [
      toolResult('tu_1', 'test failed'),
      toolResult('tu_2', 'jobs: [lint]'),
    ])),
    row(14, T3, msg('assistant', 'agent', [
      thinking([{ kind: 'text', text: 'lint is the only job' }]),
      { Text: 'The lint job failed.' },
    ])),
  ];
}

describe('buildTranscriptNodes — folding a call into its result', () => {
  it('joins a ToolUse to the ToolResult that arrives in a LATER row', () => {
    const nodes = buildTranscriptNodes([
      row(1, T0, msg('assistant', 'agent', [toolUse('tu_1', 'bash', { command: 'cargo test' })])),
      row(2, T1, msg('user', 'agent', [toolResult('tu_1', 'test failed')])),
    ]);

    expect(nodes).toEqual<TranscriptNode[]>([
      {
        id: '1:0',
        kind: 'tool',
        ordinal: 1,
        created_at: T0,
        title: 'bash',
        text: '',
        tool: { tool_use_id: 'tu_1', input: { command: 'cargo test' }, output: 'test failed' },
      },
    ]);
  });

  it('keeps the CALL row timestamp on the folded node, not the result row one', () => {
    const nodes = buildTranscriptNodes([
      row(1, T0, msg('assistant', 'agent', [toolUse('tu_1', 'bash', {})])),
      row(2, T2, msg('user', 'agent', [toolResult('tu_1', 'ok')])),
    ]);
    expect(nodes).toHaveLength(1);
    expect(nodes[0].created_at).toBe(T0);
  });

  it('leaves output null while the call is still outstanding (the live case)', () => {
    const nodes = buildTranscriptNodes([
      row(1, T0, msg('assistant', 'agent', [toolUse('tu_1', 'bash', { command: 'sleep 30' })])),
    ]);

    expect(nodes).toHaveLength(1);
    expect(nodes[0].kind).toBe('tool');
    expect(nodes[0].tool).toEqual({
      tool_use_id: 'tu_1',
      input: { command: 'sleep 30' },
      output: null,
    });
  });

  it('folds only the matching id — a second, unanswered call stays in flight', () => {
    const nodes = buildTranscriptNodes([
      row(1, T0, msg('assistant', 'agent', [
        toolUse('tu_1', 'bash', { command: 'cargo test' }),
        toolUse('tu_2', 'read_file', { path: 'ci.yml' }),
      ])),
      row(2, T1, msg('user', 'agent', [toolResult('tu_2', 'jobs: [lint]')])),
    ]);

    expect(nodes.map((n) => n.tool?.output)).toEqual([null, 'jobs: [lint]']);
  });

  it('gives an orphan ToolResult its own row instead of dropping it', () => {
    // Head of the transcript compacted away / a turn boundary between the
    // call and its result: the result must still be visible.
    const nodes = buildTranscriptNodes([
      row(7, T2, msg('user', 'agent', [toolResult('tu_gone', 'stdout: 3 passed')])),
    ]);

    expect(nodes).toEqual<TranscriptNode[]>([
      {
        id: '7:0',
        kind: 'tool',
        ordinal: 7,
        created_at: T2,
        title: 'tool result',
        text: '',
        tool: { tool_use_id: 'tu_gone', input: null, output: 'stdout: 3 passed' },
      },
    ]);
  });

  it('emits nothing for a result row whose calls were all folded', () => {
    const nodes = buildTranscriptNodes(realisticLog());
    expect(nodes.filter((n) => n.ordinal === 13)).toEqual([]);
    expect(nodes.filter((n) => n.title === 'tool result')).toEqual([]);
  });
});

describe('buildTranscriptNodes — ids and ordering', () => {
  it('ids are `${ordinal}:${blockIndex}`, unique across a realistic log', () => {
    const nodes = buildTranscriptNodes(realisticLog());
    const ids = nodes.map((n) => n.id);

    expect(ids).toEqual(['11:0', '12:0', '12:1', '12:2', '14:0', '14:1']);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it('indexes by BLOCK position, so a folded result row leaves no id gap', () => {
    // The index is the block's slot in its own row, never a running counter —
    // that is what keeps an id stable while later rows stream in.
    const nodes = buildTranscriptNodes(realisticLog());
    const byId = new Map(nodes.map((n) => [n.id, n]));
    expect(byId.get('12:2')?.title).toBe('read_file');
    expect(byId.get('14:1')?.title).toBe('Assistant');
  });

  it('survives a re-run over a longer log — earlier ids do not renumber', () => {
    const log = realisticLog();
    const before = buildTranscriptNodes(log.slice(0, 2)).map((n) => n.id);
    const after = buildTranscriptNodes(log).map((n) => n.id);
    expect(after.slice(0, before.length)).toEqual(before);
  });

  it('emits nodes in row order, and in block order within a row', () => {
    const nodes = buildTranscriptNodes(realisticLog());

    expect(nodes.map((n) => [n.ordinal, n.kind, n.title])).toEqual([
      [11, 'user', 'User'],
      [12, 'assistant', 'Assistant'],
      [12, 'tool', 'bash'],
      [12, 'tool', 'read_file'],
      [14, 'thinking', 'Thinking'],
      [14, 'assistant', 'Assistant'],
    ]);
  });
});

describe('buildTranscriptNodes — thinking blocks', () => {
  it('joins text and summary segments with a newline', () => {
    const nodes = buildTranscriptNodes([
      row(1, T0, msg('assistant', 'agent', [
        thinking([
          { kind: 'text', text: 'first the lint job', signature: 'sig' },
          { kind: 'summary', text: 'then the tests' },
        ]),
      ])),
    ]);

    expect(nodes).toHaveLength(1);
    expect(nodes[0].kind).toBe('thinking');
    expect(nodes[0].title).toBe('Thinking');
    expect(nodes[0].text).toBe('first the lint job\nthen the tests');
  });

  it('renders a redaction placeholder rather than an empty row', () => {
    // An all-redacted block would otherwise trim to '' and vanish, reading as
    // "the agent thought nothing" instead of "this reasoning is sealed".
    const nodes = buildTranscriptNodes([
      row(1, T0, msg('assistant', 'agent', [thinking([{ kind: 'redacted', data: 'AAAA' }])])),
    ]);

    expect(nodes).toHaveLength(1);
    expect(nodes[0].text).toBe('[redacted reasoning]');
  });

  it('keeps the readable segments beside a redacted one', () => {
    const nodes = buildTranscriptNodes([
      row(1, T0, msg('assistant', 'agent', [
        thinking([
          { kind: 'text', text: 'checking CI' },
          { kind: 'redacted', data: 'AAAA' },
          { kind: 'summary', text: 'lint failed' },
        ]),
      ])),
    ]);

    expect(nodes[0].text).toBe('checking CI\n[redacted reasoning]\nlint failed');
  });

  it('drops a thinking block with no segments at all', () => {
    expect(buildTranscriptNodes([row(1, T0, msg('assistant', 'agent', [thinking([])]))])).toEqual([]);
  });
});

describe('buildTranscriptNodes — text rows', () => {
  it('drops empty and whitespace-only text blocks', () => {
    const nodes = buildTranscriptNodes([
      row(1, T0, msg('assistant', 'agent', [
        { Text: '' },
        { Text: '   \n\t ' },
        { Text: 'the lint job failed' },
      ])),
    ]);

    expect(nodes.map((n) => n.id)).toEqual(['1:2']);
    expect(nodes[0].text).toBe('the lint job failed');
  });

  it('keeps the raw text (no collapsing) for the detail panel', () => {
    const nodes = buildTranscriptNodes([
      row(1, T0, msg('user', 'user', [{ Text: '  fix\n\n  the build  ' }])),
    ]);
    expect(nodes[0].text).toBe('  fix\n\n  the build  ');
  });

  it('titles a genuine prompt "User" and names the source otherwise', () => {
    const sources: MessageSource[] = ['user', 'agent', 'cron', 'user_interjection'];
    const nodes = buildTranscriptNodes(
      sources.map((source, i) => row(i + 1, T0, msg('user', source, [{ Text: 'go' }]))),
    );

    expect(nodes.map((n) => n.title)).toEqual([
      'User',
      'User (agent)',
      'User (cron)',
      'User (user_interjection)',
    ]);
    expect(new Set(nodes.map((n) => n.kind))).toEqual(new Set(['user']));
  });

  it('titles an assistant row "Assistant" whatever its source', () => {
    const nodes = buildTranscriptNodes([
      row(1, T0, msg('assistant', 'agent', [{ Text: 'done' }])),
      row(2, T1, msg('assistant', 'cron_notification', [{ Text: 'done' }])),
    ]);
    expect(nodes.map((n) => n.title)).toEqual(['Assistant', 'Assistant']);
    expect(nodes.map((n) => n.kind)).toEqual(['assistant', 'assistant']);
  });

  it('currently folds every non-user role into the assistant row', () => {
    // Documents today's behaviour, not an endorsement: a `system` row (the
    // leading prompt) and a `tool` row both read as something the agent said.
    const nodes = buildTranscriptNodes([
      row(1, T0, msg('system', 'agent', [{ Text: 'You are baybo.' }])),
      row(2, T1, msg('tool', 'agent', [{ Text: 'exit 0' }])),
    ]);
    expect(nodes.map((n) => [n.kind, n.title])).toEqual([
      ['assistant', 'Assistant'],
      ['assistant', 'Assistant'],
    ]);
  });
});

describe('buildTranscriptNodes — attachments', () => {
  it('labels image / audio / file blocks from their mime type and name', () => {
    const nodes = buildTranscriptNodes([
      row(1, T0, msg('user', 'user', [
        { Image: { blob: { blob_id: 'b1' }, mime_type: 'image/png' } },
        { Audio: { blob: { blob_id: 'b2' }, mime_type: 'audio/mpeg' } },
        { File: { blob: { blob_id: 'b3' }, filename: 'ci.log', mime_type: 'text/plain' } },
      ])),
    ]);

    expect(nodes.map((n) => [n.id, n.kind, n.title, n.text])).toEqual([
      ['1:0', 'attachment', 'image · image/png', ''],
      ['1:1', 'attachment', 'audio · audio/mpeg', ''],
      ['1:2', 'attachment', 'ci.log · text/plain', ''],
    ]);
    expect(nodes.map((n) => n.tool)).toEqual([undefined, undefined, undefined]);
  });

  it('renders an attachment beside the text of the same row', () => {
    const nodes = buildTranscriptNodes([
      row(4, T0, msg('user', 'user', [
        { Text: 'what is wrong here?' },
        { Image: { blob: { blob_id: 'b1' }, mime_type: 'image/jpeg' } },
      ])),
    ]);
    expect(nodes.map((n) => n.kind)).toEqual(['user', 'attachment']);
  });
});

describe('buildTranscriptNodes — degenerate input', () => {
  it('returns nothing for an empty log', () => {
    expect(buildTranscriptNodes([])).toEqual([]);
  });

  it('returns nothing for a row with no content blocks', () => {
    expect(buildTranscriptNodes([row(1, T0, msg('assistant', 'agent', []))])).toEqual([]);
  });
});

describe('transcriptPreview', () => {
  function node(over: Partial<TranscriptNode>): TranscriptNode {
    return { id: '1:0', kind: 'assistant', ordinal: 1, created_at: T0, title: 'Assistant', text: '', ...over };
  }

  it('collapses every run of whitespace into one space and trims', () => {
    expect(transcriptPreview(node({ text: '  the lint\n\njob   failed\t\n' }))).toBe(
      'the lint job failed',
    );
  });

  it('is empty for a node with no text', () => {
    expect(transcriptPreview(node({ text: '' }))).toBe('');
  });

  it('prefers a completed tool output over its input', () => {
    expect(
      transcriptPreview(
        node({
          kind: 'tool',
          title: 'bash',
          tool: { tool_use_id: 'tu_1', input: { command: 'cargo test' }, output: ' 3 passed\n' },
        }),
      ),
    ).toBe('3 passed');
  });

  it('falls back to the JSON input while the call is in flight', () => {
    expect(
      transcriptPreview(
        node({
          kind: 'tool',
          title: 'bash',
          tool: { tool_use_id: 'tu_1', input: { command: 'cargo test' }, output: null },
        }),
      ),
    ).toBe('{"command":"cargo test"}');
  });

  it('says "in flight" when there is neither an output nor an input', () => {
    expect(
      transcriptPreview(
        node({ kind: 'tool', title: 'bash', tool: { tool_use_id: 'tu_1', input: null, output: null } }),
      ),
    ).toBe('in flight');
  });

  it('says "in flight" rather than throwing on an unserializable input', () => {
    const circular: Record<string, unknown> = {};
    circular.self = circular;
    expect(
      transcriptPreview(
        node({ kind: 'tool', title: 'bash', tool: { tool_use_id: 'tu_1', input: circular, output: null } }),
      ),
    ).toBe('in flight');
  });

  it('ignores a tool node text field entirely', () => {
    expect(
      transcriptPreview(
        node({
          kind: 'tool',
          title: 'bash',
          text: 'should never surface',
          tool: { tool_use_id: 'tu_1', input: null, output: 'exit 0' },
        }),
      ),
    ).toBe('exit 0');
  });

  it('previews an empty tool output as empty, not as in flight', () => {
    // `output: ''` is a call that answered with nothing — distinct from a call
    // still running, which the tree badges with a spinner.
    expect(
      transcriptPreview(
        node({ kind: 'tool', title: 'bash', tool: { tool_use_id: 'tu_1', input: { a: 1 }, output: '' } }),
      ),
    ).toBe('');
  });

  it('reads a folded node straight out of buildTranscriptNodes', () => {
    const nodes = buildTranscriptNodes(realisticLog());
    expect(nodes.map(transcriptPreview)).toEqual([
      'fix the build',
      'Looking at CI.',
      'test failed',
      'jobs: [lint]',
      'lint is the only job',
      'The lint job failed.',
    ]);
  });
});
