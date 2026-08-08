/**
 * Projects a session transcript into trace-shaped rows.
 *
 * An external-agent session (claude / codex) records no step/span
 * tree — its loop runs out of process and baybo never sees the individual LLM
 * calls. Its `session_messages` log is therefore the only trace it has, and
 * this module reshapes that log into the same vocabulary the tree already
 * speaks: one row per thing the agent did, with a tool call and its result
 * folded into a single row the way a `ToolCall` span carries both its params
 * and its output.
 *
 * Pure — no React, no fetching — so the pairing and ordering rules are
 * unit-testable and `TraceTree` stays layout.
 */
import type { ChatMessage, ContentBlock, SessionMessageRow } from '../../types/trace';

export type TranscriptNodeKind = 'user' | 'assistant' | 'thinking' | 'tool' | 'attachment';

export interface TranscriptToolCall {
  tool_use_id: string;
  input: unknown;
  /** `null` while the call is still outstanding — the agent is mid-tool. */
  output: string | null;
}

/** One row in an external agent's transcript-as-trace. */
export interface TranscriptNode {
  /**
   * Stable within a session: the row's ordinal plus its index inside that
   * row's content blocks. Survives polling (ordinals never renumber) so it
   * works as a URL selection token and a React key.
   */
  id: string;
  kind: TranscriptNodeKind;
  ordinal: number;
  created_at: string;
  /** Row heading — the tool name, or the speaking role. */
  title: string;
  /** Full body, for the detail panel. Empty for a pure tool row. */
  text: string;
  tool?: TranscriptToolCall;
}

function blockText(block: ContentBlock): string | null {
  if ('Text' in block) return block.Text;
  if ('Thinking' in block) {
    // A redacted block carries no readable text — say so rather than rendering
    // an empty row that looks like the agent thought nothing.
    //
    // BLANK line between segments, not a single newline. Each summary segment
    // is its own section and typically opens with a `**Headline**`; CommonMark
    // folds a lone newline into a space, so joining with `\n` glues that
    // headline onto the tail of the previous segment's last sentence
    // (`…I need!**Inspecting the repo**`). The segment boundary is real — it is
    // a separate array entry, and flattening is the only place it can be lost.
    return block.Thinking.content
      .map((c) => (c.kind === 'redacted' ? '[redacted reasoning]' : c.text))
      .join('\n\n');
  }
  return null;
}

function roleTitle(message: ChatMessage): string {
  if (message.role === 'user') return message.source === 'user' ? 'User' : `User (${message.source})`;
  return 'Assistant';
}

function attachmentLabel(block: ContentBlock): string | null {
  if ('Image' in block) return `image · ${block.Image.mime_type}`;
  if ('Audio' in block) return `audio · ${block.Audio.mime_type}`;
  if ('File' in block) return `${block.File.filename} · ${block.File.mime_type}`;
  return null;
}

/**
 * Flatten a turn's transcript rows into ordered transcript nodes.
 *
 * A `ToolUse` block is joined to the `ToolResult` that quotes its
 * `tool_use_id` — which arrives in a *later* row, so results are indexed up
 * front rather than looked ahead to per block. An unmatched `ToolUse` is a
 * call still in flight (the common case at the head of a live stream) and
 * keeps `output: null`; an unmatched `ToolResult` — a transcript whose head
 * was compacted away, or a page boundary — still gets its own row instead of
 * being silently dropped.
 *
 * Rows are assumed to be in ordinal order, as both the overview and its
 * incremental pages deliver them.
 */
export function buildTranscriptNodes(rows: SessionMessageRow[]): TranscriptNode[] {
  const resultByUseId = new Map<string, string>();
  for (const row of rows) {
    for (const block of row.message.content) {
      if ('ToolResult' in block) resultByUseId.set(block.ToolResult.tool_use_id, block.ToolResult.content);
    }
  }

  const consumed = new Set<string>();
  const out: TranscriptNode[] = [];

  for (const row of rows) {
    row.message.content.forEach((block, i) => {
      const id = `${row.ordinal}:${i}`;
      const base = { id, ordinal: row.ordinal, created_at: row.created_at };

      if ('ToolUse' in block) {
        const { id: useId, name, input } = block.ToolUse;
        consumed.add(useId);
        const output = resultByUseId.get(useId) ?? null;
        out.push({ ...base, kind: 'tool', title: name, text: '', tool: { tool_use_id: useId, input, output } });
        return;
      }

      if ('ToolResult' in block) {
        // Already folded into its ToolUse row; only an orphan survives here.
        if (consumed.has(block.ToolResult.tool_use_id)) return;
        out.push({
          ...base,
          kind: 'tool',
          title: 'tool result',
          text: '',
          tool: { tool_use_id: block.ToolResult.tool_use_id, input: null, output: block.ToolResult.content },
        });
        return;
      }

      const attachment = attachmentLabel(block);
      if (attachment != null) {
        out.push({ ...base, kind: 'attachment', title: attachment, text: '' });
        return;
      }

      const text = blockText(block);
      if (text == null || text.trim() === '') return;
      if ('Thinking' in block) {
        out.push({ ...base, kind: 'thinking', title: 'Thinking', text });
        return;
      }
      out.push({
        ...base,
        kind: row.message.role === 'user' ? 'user' : 'assistant',
        title: roleTitle(row.message),
        text,
      });
    });
  }

  return out;
}

/** One-line summary of a node for its collapsed row. */
export function transcriptPreview(node: TranscriptNode): string {
  if (node.tool) {
    if (node.tool.output != null) return collapse(node.tool.output);
    if (node.tool.input != null) return collapse(safeStringify(node.tool.input) ?? '') || 'in flight';
    return 'in flight';
  }
  return collapse(node.text);
}

/**
 * Everything in a node the tree's text filter should search.
 *
 * Deliberately NOT `transcriptPreview`: a preview shows a tool's output once it
 * lands, so searching the preview meant a call stopped matching its own
 * arguments the moment it returned — filtering an external agent's transcript
 * for `cargo test` found the row only while it was in flight. Both sides are
 * included here, uncollapsed and untruncated, because a match deep in a long
 * output is exactly what a filter is for.
 */
export function transcriptSearchText(node: TranscriptNode): string {
  if (node.tool) {
    return `${node.title} ${safeStringify(node.tool.input) ?? ''} ${node.tool.output ?? ''}`;
  }
  return `${node.title} ${node.text}`;
}

function safeStringify(value: unknown): string | null {
  if (value == null) return null;
  try {
    return JSON.stringify(value);
  } catch {
    // Circular or otherwise unserializable params — the row still renders.
    return null;
  }
}

/**
 * A row's preview never shows more than a couple of hundred characters, so only
 * that much is worth normalising. Collapsing an untruncated 32 KiB tool result
 * ran a regex over the whole thing on every row of every 2s poll tick.
 */
const PREVIEW_SCAN_LIMIT = 400;

function collapse(s: string): string {
  return s.slice(0, PREVIEW_SCAN_LIMIT).replace(/\s+/g, ' ').trim();
}
