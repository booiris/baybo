import { describe, expect, it } from 'vitest';
import type {
  ChatMessage,
  PersistedToolCallOutput,
  SessionMessageRow,
} from './trace';
import { hydratePersistedInput, resolveToolCallOutput } from './trace';

function toolResultRow(
  ordinal: number,
  createdAt: string,
  toolUseId: string,
  content: string,
): SessionMessageRow {
  return {
    ordinal,
    created_at: createdAt,
    message: {
      role: 'tool',
      source: 'agent',
      content: [{ ToolResult: { tool_use_id: toolUseId, content } }],
    },
  };
}

describe('resolveToolCallOutput', () => {
  it('resolves a text result to the raw wrapped transcript string', () => {
    const wrapped = '<tool_output name="Read">\nbody </tool_output> text\n</tool_output>';
    const log = [toolResultRow(7, '2026-07-15T00:00:02Z', 'tool-7', wrapped)];
    const reference: PersistedToolCallOutput = {
      $baybo_ref: 'session_tool_result',
      tool_use_id: 'tool-7',
    };

    expect(resolveToolCallOutput(reference, log, '2026-07-15T00:00:01Z')).toBe(wrapped);
  });

  it('wraps a media result so the blob list survives', () => {
    const log = [toolResultRow(7, '2026-07-15T00:00:02Z', 'tool-7', 'shown')];
    const reference: PersistedToolCallOutput = {
      $baybo_ref: 'session_tool_result',
      tool_use_id: 'tool-7',
      attachments: [{ Text: 'blob-ref-stand-in' } as never],
    };

    expect(
      resolveToolCallOutput(reference, log, '2026-07-15T00:00:01Z'),
    ).toEqual({
      type: 'with_attachments',
      text: 'shown',
      attachments: [{ Text: 'blob-ref-stand-in' }],
    });
  });

  it('surfaces a visible marker when no transcript row carries the tool_use_id', () => {
    const reference: PersistedToolCallOutput = {
      $baybo_ref: 'session_tool_result',
      tool_use_id: 'missing',
    };
    expect(resolveToolCallOutput(reference, [], '2026-07-15T00:00:01Z')).toMatchObject({
      type: 'trace_reconstruction_error',
      tool_use_id: 'missing',
    });
  });
});

describe('hydratePersistedInput', () => {
  it('uses an explicit repaired ordinal subset and order', () => {
    const message = (text: string): ChatMessage => ({
      role: 'user',
      source: 'agent',
      content: [{ Text: text }],
    });
    const at = '2026-07-15T00:00:01Z';
    const a = message('a');
    const quarantined = message('orphan');
    const b = message('b');
    const suffix = message('suffix');
    const log: SessionMessageRow[] = [a, quarantined, b].map((entry, ordinal) => ({
      ordinal,
      created_at: at,
      message: entry,
    }));

    expect(
      hydratePersistedInput(log, 2, '2026-07-15T00:00:02Z', 2, [2, 0], [suffix]),
    ).toEqual([b, a, suffix]);
  });
});
