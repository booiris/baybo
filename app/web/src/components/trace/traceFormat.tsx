/**
 * Shared presentational helpers for the trace viewer — visual maps, time /
 * token / outcome formatting, and the per-kind summary text. Extracted so the
 * step/span tree (`TraceTree`), the right-hand detail panels, and the page
 * header all render from one source of truth (no duplicated visual constants).
 */
import type { IconType } from 'react-icons';
import {
  RiArchiveLine,
  RiAttachment2,
  RiBookmark3Line,
  RiBroadcastLine,
  RiBrainLine,
  RiChat3Line,
  RiCpuLine,
  RiPriceTag3Line,
  RiSave3Line,
  RiSearchEyeLine,
  RiTeamLine,
  RiToolsLine,
  RiUser3Line,
} from 'react-icons/ri';
import type {
  ChatMessage,
  ContentBlock,
  ExternalAgentKind,
  LifecycleState,
  LlmCallResult,
  ReplayStep,
  SessionMessageRow,
  Span,
  SpanKindTag,
  Step,
  StepKindTag,
  TraceTurnSummary,
  TurnTrace,
} from '../../types/trace';
import { resolveInputMessages } from '../../types/trace';
import type { TranscriptNodeKind } from './transcriptModel';

// ── Visual mapping per StepKind / SpanKind ───────────────────────────

/** The colour groups the legend is keyed on — one per hue, plus the failure
 *  overlay. Clicking a legend entry highlights the nodes in that group. */
export type TraceGroup = 'llm' | 'tool' | 'memory' | 'subagent' | 'compression' | 'meta' | 'failed';

/** The tool whose call spawns a subagent. A `spawn_subagent` tool_call is what a
 *  subagent actually looks like in a trace — `StepKind::Subagent` /
 *  `SpanKind::SubagentStub` exist on the wire but nothing records them. */
export const SPAWN_SUBAGENT_TOOL = 'spawn_subagent';

export interface KindVisual {
  icon: IconType; // used by the right-hand detail-panel headers
  group: TraceGroup; // which legend entry this kind belongs to
  glyph: string; // compact one-char marker for the dense tree rows (bench-web style)
  accent: string; // tailwind text color
  bg: string; // tailwind bg color (for the icon disc)
  // Left-edge stripe class and solid overview-minimap cell class. Both kept as
  // LITERALS (not derived from `accent` at runtime) so Tailwind's source
  // scanner actually generates the utility — a `.replace()`-built class name is
  // invisible to the scanner and renders colourless.
  stripe: string;
  cell: string;
  label: string;
}

// Colour groups (kept distinct so adjacent minimap cells don't blur together;
// see TRACE_LEGEND below): LLM = green, Tool = orange, Memory = blue,
// Subagent = gold, everything else (aux/meta steps) = gray. Failure is a red
// overlay, applied on top wherever an outcome failed. The glyph, left stripe,
// and minimap cell of a kind all share its hue so the legend reads across the UI.
export const STEP_VISUALS: Record<StepKindTag, KindVisual> = {
  llm_iteration: { group: 'llm', icon: RiBrainLine, glyph: 'L', accent: 'text-ok', bg: 'bg-ok/10', stripe: 'border-l-ok', cell: 'bg-ok', label: 'LLM iteration' },
  compression: { group: 'compression', icon: RiArchiveLine, glyph: 'C', accent: 'text-violet', bg: 'bg-violet/10', stripe: 'border-l-violet', cell: 'bg-violet', label: 'Compression' },
  memory_recall: { group: 'memory', icon: RiSearchEyeLine, glyph: 'M', accent: 'text-info', bg: 'bg-info/10', stripe: 'border-l-info', cell: 'bg-info', label: 'Memory recall' },
  memory_write: { group: 'memory', icon: RiSave3Line, glyph: 'W', accent: 'text-info', bg: 'bg-info/10', stripe: 'border-l-info', cell: 'bg-info', label: 'Memory write' },
  skill_selection: { group: 'meta', icon: RiBookmark3Line, glyph: 'S', accent: 'text-ink-soft', bg: 'bg-gray-100', stripe: 'border-l-ink-soft', cell: 'bg-ink-soft', label: 'Skill selection' },
  progress_observer: { group: 'meta', icon: RiBroadcastLine, glyph: 'P', accent: 'text-ink-soft', bg: 'bg-gray-100', stripe: 'border-l-ink-soft', cell: 'bg-ink-soft', label: 'Progress observer' },
  title_generation: { group: 'meta', icon: RiPriceTag3Line, glyph: 'T', accent: 'text-ink-soft', bg: 'bg-gray-100', stripe: 'border-l-ink-soft', cell: 'bg-ink-soft', label: 'Title generation' },
};

export const SPAN_VISUALS: Record<SpanKindTag, KindVisual> = {
  llm_call: { group: 'llm', icon: RiBrainLine, glyph: 'L', accent: 'text-ok', bg: 'bg-ok/10', stripe: 'border-l-ok', cell: 'bg-ok', label: 'LLM call' },
  tool_call: { group: 'tool', icon: RiToolsLine, glyph: 't', accent: 'text-warn', bg: 'bg-warn/10', stripe: 'border-l-warn', cell: 'bg-warn', label: 'Tool call' },
};

// Colour key for the overview minimap + tree glyphs/stripes. One entry per hue
// group above (not per kind), plus the failure overlay.
export const TRACE_LEGEND: { group: TraceGroup; label: string; cell: string }[] = [
  { group: 'llm', label: 'LLM', cell: 'bg-ok' },
  { group: 'tool', label: 'Tool', cell: 'bg-warn' },
  { group: 'memory', label: 'Memory', cell: 'bg-info' },
  { group: 'subagent', label: 'Subagent', cell: 'bg-brand' },
  { group: 'compression', label: 'Compression', cell: 'bg-violet' },
  { group: 'meta', label: 'Meta', cell: 'bg-ink-soft' },
  { group: 'failed', label: 'Failed', cell: 'bg-err' },
];

/** A node's legend group. A failed/cancelled outcome wins over its kind, matching
 *  how the minimap paints failures red regardless of kind. `toolName` promotes a
 *  `spawn_subagent` call out of the generic Tool group — that call IS the
 *  subagent, and it is the only form a subagent takes in a real trace. */
export function nodeGroup(
  visual: KindVisual,
  outcome: LifecycleState,
  toolName?: string,
): TraceGroup {
  if (outcome.outcome === 'failed' || outcome.outcome === 'cancelled') return 'failed';
  if (toolName === SPAWN_SUBAGENT_TOOL) return 'subagent';
  return visual.group;
}

// A kind the frontend doesn't know yet (wire drift, or a new Rust variant
// shipped ahead of this map) must degrade to a generic row — never let a
// `STEP_VISUALS[kind].icon` on `undefined` throw and white-screen the whole
// trace view. The raw tag becomes the label so it's still legible.
const FALLBACK_VISUAL: KindVisual = {
  group: 'meta',
  icon: RiCpuLine,
  glyph: '·',
  accent: 'text-ink-soft',
  bg: 'bg-gray-100',
  stripe: 'border-l-ink-soft',
  cell: 'bg-ink-soft',
  label: 'step',
};

// The lookups are widened to `| undefined` on purpose: the maps are keyed by the
// tags this build knows, but the wire can carry a newer variant, and that must
// degrade to `FALLBACK_VISUAL` instead of returning undefined into a render.
export function stepVisual(kind: StepKindTag): KindVisual {
  const known: KindVisual | undefined = (STEP_VISUALS as Partial<Record<string, KindVisual>>)[kind];
  return known ?? { ...FALLBACK_VISUAL, label: kind };
}

export function spanVisual(kind: SpanKindTag): KindVisual {
  const known: KindVisual | undefined = (SPAN_VISUALS as Partial<Record<string, KindVisual>>)[kind];
  return known ?? { ...FALLBACK_VISUAL, label: kind };
}

/** Subagent is not a `StepKind`/`SpanKind` — it is the identity of a
 *  `spawn_subagent` tool call, so it carries its own visual rather than
 *  borrowing a variant's. */
const SUBAGENT_VISUAL: KindVisual = {
  group: 'subagent',
  icon: RiTeamLine,
  glyph: 'A',
  accent: 'text-brand',
  bg: 'bg-brand/10',
  stripe: 'border-l-brand',
  cell: 'bg-brand',
  label: 'Subagent',
};

/** Visual for the jump to a child session's own trace. It shares the Subagent
 *  identity with the `spawn_subagent` span that opened it. */
export const CHILD_SESSION_VISUAL: KindVisual = SUBAGENT_VISUAL;

/**
 * Visuals for an external agent's transcript rows. An external run has no
 * step/span tree, so its transcript stands in for one — these keep it speaking
 * the same colour language as a real tree: its tool calls are Tool-orange like
 * any other tool call, its assistant output and reasoning are LLM-green, and
 * the human/attachment rows sit in the neutral Meta group. No new legend entry:
 * every row still lands in a group the legend already names.
 */
export const TRANSCRIPT_VISUALS: Record<TranscriptNodeKind, KindVisual> = {
  user: { group: 'meta', icon: RiUser3Line, glyph: 'U', accent: 'text-ink-soft', bg: 'bg-gray-100', stripe: 'border-l-ink-soft', cell: 'bg-ink-soft', label: 'User' },
  assistant: { group: 'llm', icon: RiChat3Line, glyph: 'a', accent: 'text-ok', bg: 'bg-ok/10', stripe: 'border-l-ok', cell: 'bg-ok', label: 'Assistant' },
  thinking: { group: 'llm', icon: RiBrainLine, glyph: '~', accent: 'text-ok', bg: 'bg-ok/10', stripe: 'border-l-ok', cell: 'bg-ok', label: 'Thinking' },
  tool: { group: 'tool', icon: RiToolsLine, glyph: 't', accent: 'text-warn', bg: 'bg-warn/10', stripe: 'border-l-warn', cell: 'bg-warn', label: 'Tool call' },
  attachment: { group: 'meta', icon: RiAttachment2, glyph: '@', accent: 'text-ink-soft', bg: 'bg-gray-100', stripe: 'border-l-ink-soft', cell: 'bg-ink-soft', label: 'Attachment' },
};

/** Human-facing name of an external agent backend. */
export function externalAgentLabel(kind: ExternalAgentKind): string {
  return EXTERNAL_AGENT_LABELS[kind] ?? kind;
}

const EXTERNAL_AGENT_LABELS: Partial<Record<string, string>> = {
  claude: 'Claude Code',
  codex: 'Codex',
};

/** The tool name a span calls, when it is a tool call. */
export function spanToolName(span: Span): string | undefined {
  return span.kind.kind === 'tool_call' ? span.kind.begin.tool_name : undefined;
}

/** Visual for a span, promoting a `spawn_subagent` tool call to the Subagent
 *  identity — that call is the subagent, so it should not read as a plain tool. */
export function spanVisualOf(span: Span): KindVisual {
  if (spanToolName(span) === SPAWN_SUBAGENT_TOOL) {
    return SUBAGENT_VISUAL;
  }
  return spanVisual(span.kind.kind);
}

// The kind's left-edge stripe class (a literal so Tailwind generates it).
export function stripeClass(v: KindVisual): string {
  return v.stripe;
}

// Token totals across a set of llm_call spans — one span (a span row), a
// step's spans (its rolled-up badge), or a whole turn's (see `traceTokens`).
// `input` already contains the cache buckets; see [`TurnTokenTotals`].
export function sumLlmTokens(spans: Span[]): TurnTokenTotals {
  let input = 0;
  let output = 0;
  let cached = 0;
  let cacheCreate = 0;
  for (const s of spans) {
    if (s.kind.kind === 'llm_call' && s.kind.result) {
      input += s.kind.result.input_tokens ?? 0;
      output += s.kind.result.output_tokens ?? 0;
      cached += s.kind.result.cached_input_tokens ?? 0;
      cacheCreate += s.kind.result.cache_creation_input_tokens ?? 0;
    }
  }
  return { input, output, cached, cacheCreate };
}

/** Tokens a compaction consumed and produced. Cache reads and writes are part
 *  of `input_tokens` already (see [`TurnTokenTotals`]), so the compacted window
 *  is that field alone. The step summary and the overview's CONTEXT chips both
 *  read this, so the two figures cannot drift apart. */
export function compressionTokens(result: LlmCallResult): { input: number; output: number } {
  return {
    input: result.input_tokens ?? 0,
    output: result.output_tokens ?? 0,
  };
}

// ── Time / duration ──────────────────────────────────────────────────

export function durationMs(span: { started_at: string; ended_at?: string | null }): number | null {
  if (span.ended_at == null) return null;
  return Math.max(0, new Date(span.ended_at).getTime() - new Date(span.started_at).getTime());
}

export function formatDuration(ms: number | null): string {
  if (ms === null) return '…';
  if (ms < 1_000) return `${ms} ms`;
  if (ms < 60_000) return `${(ms / 1_000).toFixed(2)} s`;
  return `${(ms / 60_000).toFixed(2)} min`;
}

export function formatTime(iso: string): string {
  const d = new Date(iso);
  return d.toLocaleTimeString([], {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  });
}

// ── Outcome ──────────────────────────────────────────────────────────

export function outcomeColor(state: LifecycleState): string {
  switch (state.outcome) {
    case 'pending':
      return 'text-info';
    case 'ok':
      return 'text-ok';
    case 'failed':
      return 'text-err';
    case 'cancelled':
      return 'text-ink-soft';
  }
}

function outcomeLabel(state: LifecycleState): string {
  switch (state.outcome) {
    case 'pending':
      return 'pending';
    case 'ok':
      return 'ok';
    case 'failed':
      return 'failed';
    case 'cancelled':
      return `cancelled (${state.reason})`;
  }
}

export function OutcomeBadge({ state }: { state: LifecycleState }) {
  const cls = outcomeColor(state);
  return (
    <span className={`inline-flex items-center text-[0.7rem] font-bold uppercase tracking-wider ${cls}`}>
      {state.outcome === 'pending' && (
        <span className="inline-block w-2 h-2 rounded-full bg-info animate-pulse mr-1.5" />
      )}
      {outcomeLabel(state)}
    </span>
  );
}

// ── Step summary text per StepKind ───────────────────────────────────

export function stepSummaryText(step: Step, spans: Span[]): string {
  const k = step.kind.kind;
  switch (k) {
    case 'llm_iteration': {
      const llm = spans.find((s) => s.kind.kind === 'llm_call');
      if (llm && llm.kind.kind === 'llm_call') {
        const model = llm.kind.begin.model_id;
        const tools = spans.filter((s) => s.kind.kind === 'tool_call').length;
        return tools > 0 ? `${model} • ${tools} tool ${tools === 1 ? 'call' : 'calls'}` : model;
      }
      return 'llm iteration';
    }
    case 'compression': {
      // `trigger` (why it ran) leads; the token figures come off the
      // summarizer's own span, which a failed compaction does not have.
      const { trigger } = step.kind;
      const parts: string[] = [];
      if (trigger != null) parts.push(trigger);
      const llm = spans.find((s) => s.kind.kind === 'llm_call');
      if (llm?.kind.kind === 'llm_call' && llm.kind.result) {
        const { input, output } = compressionTokens(llm.kind.result);
        parts.push(`${input.toLocaleString()} → ${output.toLocaleString()} tokens`);
      }
      return parts.length > 0 ? parts.join(' · ') : 'compression';
    }
    case 'memory_recall': {
      const tool = spans.find((s) => s.kind.kind === 'tool_call');
      if (tool && tool.kind.kind === 'tool_call') {
        return `recall via ${tool.kind.begin.tool_name}`;
      }
      return 'memory recall';
    }
    case 'memory_write':
      return `wrote ${spans.length} memor${spans.length === 1 ? 'y' : 'ies'}`;
    case 'skill_selection': {
      const llm = spans.find((s) => s.kind.kind === 'llm_call');
      const out = llm?.kind.kind === 'llm_call' ? llm.kind.result?.output_content : undefined;
      if (out != null && out !== '') return out.slice(0, 80);
      return 'skill selection';
    }
    case 'progress_observer': {
      const llm = spans.find((s) => s.kind.kind === 'llm_call');
      const out = llm?.kind.kind === 'llm_call' ? llm.kind.result?.output_content : undefined;
      if (out != null && out !== '') return out.slice(0, 80);
      return 'progress update';
    }
    case 'title_generation': {
      const llm = spans.find((s) => s.kind.kind === 'llm_call');
      const out = llm?.kind.kind === 'llm_call' ? llm.kind.result?.output_content : undefined;
      if (out != null && out !== '') return out.slice(0, 80);
      return 'conversation title';
    }
  }
}

// ── Turn-level token / duration / io helpers ─────────────────────────

// Total turn execution time. Prefers the backend's `started_at`/`ended_at`
// (covers setup, teardown, and gaps that fall outside any step) and falls
// back to deriving from step timestamps for older traces. For in-flight turns
// we use `now` as the upper bound so the counter keeps ticking.
export function turnDurationMs(
  turn: { started_at?: string | null; ended_at?: string | null },
  trace: TurnTrace | undefined,
): number | null {
  if (turn.started_at != null) {
    const start = new Date(turn.started_at).getTime();
    const end = turn.ended_at != null ? new Date(turn.ended_at).getTime() : Date.now();
    return Math.max(0, end - start);
  }
  if (!trace) return null;
  let minStart = Infinity;
  let maxEnd = -Infinity;
  let inFlight = false;
  for (const rs of trace.steps) {
    const start = new Date(rs.step.started_at).getTime();
    if (start < minStart) minStart = start;
    if (rs.step.ended_at != null) {
      const end = new Date(rs.step.ended_at).getTime();
      if (end > maxEnd) maxEnd = end;
    } else {
      inFlight = true;
    }
  }
  if (minStart === Infinity) return null;
  const end = inFlight ? Date.now() : maxEnd;
  if (end === -Infinity) return null;
  return Math.max(0, end - minStart);
}

// Time the turn sat in queue before execution started. Only meaningful when
// both timestamps are present; null for legacy traces or turns never started.
export function turnQueuedMs(turn: { created_at?: string | null; started_at?: string | null }): number | null {
  if (turn.created_at == null || turn.started_at == null) return null;
  return Math.max(0, new Date(turn.started_at).getTime() - new Date(turn.created_at).getTime());
}

/** `input` is the WHOLE prompt: the LLM layer normalises every provider so
 *  `input_tokens` already contains the prompt-cache buckets (Anthropic reports
 *  them disjoint and `fold_anthropic_cache_into_total` folds them back in;
 *  OpenAI/Gemini report them as a subset natively). `cached` / `cacheCreate` are
 *  therefore a BREAKDOWN of `input`, never addends — adding them back double-
 *  counts every cache hit. Billing agrees: `compute_cost_usd` bills
 *  `input - cached - cacheCreate` at the full rate. */
export interface TurnTokenTotals {
  input: number;
  output: number;
  cached: number;
  cacheCreate: number;
}

export function summaryTokens(summary: TraceTurnSummary): TurnTokenTotals {
  return {
    input: summary.input_tokens,
    output: summary.output_tokens,
    cached: summary.cached_input_tokens,
    cacheCreate: summary.cache_creation_input_tokens,
  };
}

/**
 * A turn's token totals, from whichever source actually has them.
 *
 * The span tree is preferred once loaded — it is live, while the summary's
 * figures come from a grouped cost query — but only when it has spans to sum.
 * A turn with a **stepless** trace is not a turn that spent nothing: an
 * external agent records no spans at all, yet its usage is written to
 * `cost_records` and arrives on the summary. Summing its (empty) tree would
 * report a hard zero next to a transcript full of work.
 */
export function turnTokens(summary: TraceTurnSummary, trace: TurnTrace | undefined): TurnTokenTotals {
  return trace && trace.steps.length > 0 ? traceTokens(trace) : summaryTokens(summary);
}

// Derive token totals from a loaded TurnTrace's spans. Prefer `turnTokens`,
// which falls back to the summary when the tree is empty.
export function traceTokens(trace: TurnTrace): TurnTokenTotals {
  return sumLlmTokens(trace.steps.flatMap((rs) => rs.spans));
}

// Flatten a message's content blocks into a single display string (text
// verbatim; non-text blocks as a `[kind …]` placeholder).
export function contentText(content: ContentBlock[]): string {
  const parts: string[] = [];
  for (const block of content) {
    if ('Text' in block) parts.push(block.Text);
    else if ('ToolResult' in block) parts.push(`[tool_result ${block.ToolResult.tool_use_id}]`);
    else if ('Image' in block) parts.push(`[image ${block.Image.mime_type}]`);
    else if ('Audio' in block) parts.push(`[audio ${block.Audio.mime_type}]`);
    else if ('File' in block) parts.push(`[file ${block.File.filename}]`);
  }
  return parts.join('\n');
}

/** The step kind that IS the conversation — the agent loop's own iterations. */
const CONVERSATION_STEP_KIND: StepKindTag = 'llm_iteration';

/** Step kinds that never constitute a turn's own work: detached side passes
 *  recorded under the turn they observe, each sending a prompt of its own. A
 *  turn left holding nothing else (it died before recording an iteration)
 *  has no user input to show — not the side pass's template. */
const SIDE_PASS_STEP_KINDS = new Set<StepKindTag>(['title_generation', 'progress_observer']);

// Which steps speak for the turn. "The turn's first/last LLM span" does not:
// title generation is spawned before the turn's first iteration and the
// observer/a compaction land after its last, so a new session's turn preview
// showed the title prompt template instead of what the user asked. Prefer the
// agent loop's steps; a turn that has none (a standalone `/compact`) falls back
// to the work it did do.
function conversationSteps(trace: TurnTrace): ReplayStep[] {
  const own = trace.steps.filter((rs) => !SIDE_PASS_STEP_KINDS.has(rs.step.kind.kind));
  const loop = own.filter((rs) => rs.step.kind.kind === CONVERSATION_STEP_KIND);
  return loop.length > 0 ? loop : own;
}

// Derive the user-facing input that kicked off the turn: the last message in
// the *first* conversation LLM call's input_messages whose `source` is 'user'.
/**
 * The prompt a turn was given, read straight off its transcript.
 *
 * `turnInputText` recovers it from an LLM span's resolved input, which an
 * external-agent turn does not have — its loop is out of process and records no
 * spans. Its task is simply the first genuine user row the spawn router wrote
 * before handing off, so that is what this returns. Agent-injected `user`-role
 * rows (framing, reminders) are skipped: they are not what was asked.
 */
export function transcriptInputText(rows: SessionMessageRow[]): string | null {
  for (const row of rows) {
    if (row.message.role !== 'user') continue;
    if (row.message.source !== 'user' && row.message.source !== 'agent') continue;
    const text = contentText(row.message.content);
    if (text.length > 0) return text;
  }
  return null;
}

export function turnInputText(trace: TurnTrace, messageLog: SessionMessageRow[]): string | null {
  for (const rs of conversationSteps(trace)) {
    for (const span of rs.spans) {
      if (span.kind.kind === 'llm_call') {
        const messages: ChatMessage[] = resolveInputMessages(
          span.kind.begin.input_messages,
          messageLog,
          span.started_at,
        );
        for (let i = messages.length - 1; i >= 0; i--) {
          if (messages[i].source === 'user') {
            const text = contentText(messages[i].content);
            return text.length > 0 ? text : null;
          }
        }
        return null;
      }
    }
  }
  return null;
}

// Derive the final output text of the turn: the most recent conversation LLM
// call's output_content (walking steps/spans back-to-front).
export function turnOutputText(trace: TurnTrace): string | null {
  const steps = conversationSteps(trace);
  for (let i = steps.length - 1; i >= 0; i--) {
    const rs = steps[i];
    for (let j = rs.spans.length - 1; j >= 0; j--) {
      const span = rs.spans[j];
      const out = span.kind.kind === 'llm_call' ? span.kind.result?.output_content : undefined;
      if (out != null && out !== '') return out;
    }
  }
  return null;
}

export function formatTok(n: number): string {
  if (n < 1_000) return n.toString();
  if (n < 1_000_000) return `${(n / 1_000).toFixed(1)}k`;
  return `${(n / 1_000_000).toFixed(2)}M`;
}
