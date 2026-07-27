/**
 * Shared presentational helpers for the trace viewer — visual maps, time /
 * token / outcome formatting, and the per-kind summary text. Extracted so the
 * step/span tree (`TraceTree`), the right-hand detail panels, and the page
 * header all render from one source of truth (no duplicated visual constants).
 */
import type { IconType } from 'react-icons';
import {
  RiArchiveLine,
  RiBookmark3Line,
  RiBroadcastLine,
  RiBrainLine,
  RiCpuLine,
  RiPriceTag3Line,
  RiSave3Line,
  RiSearchEyeLine,
  RiTeamLine,
  RiToolsLine,
} from 'react-icons/ri';
import type {
  ChatMessage,
  ContentBlock,
  JobTrace,
  LifecycleState,
  LlmCallResult,
  SessionMessageRow,
  Span,
  SpanKindTag,
  Step,
  StepKindTag,
  TraceJobSummary,
} from '../../types/trace';
import { resolveInputMessages } from '../../types/trace';

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

// Total input (incl. cache) and output tokens across the llm_call spans of a
// step — for the rolled-up token badge on a collapsed/summary Step row.
export function sumLlmTokens(spans: Span[]): { input: number; output: number } {
  let input = 0;
  let output = 0;
  for (const s of spans) {
    if (s.kind.kind === 'llm_call' && s.kind.result) {
      input +=
        (s.kind.result.input_tokens ?? 0) +
        (s.kind.result.cached_input_tokens ?? 0) +
        (s.kind.result.cache_creation_input_tokens ?? 0);
      output += s.kind.result.output_tokens ?? 0;
    }
  }
  return { input, output };
}

/** Tokens a compaction consumed and produced. Input counts cache reads and cache
 *  writes too: they occupied the context window that was compacted. The step
 *  summary and the overview's CONTEXT chips both read this, so the two figures
 *  cannot drift apart. */
export function compressionTokens(result: LlmCallResult): { input: number; output: number } {
  return {
    input:
      (result.input_tokens ?? 0) +
      (result.cached_input_tokens ?? 0) +
      (result.cache_creation_input_tokens ?? 0),
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
      // `trigger` (why) leads, `applied` (how) follows. A truncate compaction
      // makes no LLM call, so it has no span and no token figures — it must
      // still say what it did, because that spanless row is the compaction
      // that discarded the most.
      const { trigger, applied } = step.kind;
      const parts: string[] = [];
      if (trigger != null) parts.push(trigger);
      if (applied != null) parts.push(applied.replace('_', ' '));
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

// ── Job-level token / duration / io helpers ──────────────────────────

// Total job execution time. Prefers the backend's `started_at`/`ended_at`
// (covers setup, teardown, and gaps that fall outside any step) and falls
// back to deriving from step timestamps for older traces. For in-flight jobs
// we use `now` as the upper bound so the counter keeps ticking.
export function jobDurationMs(
  job: { started_at?: string | null; ended_at?: string | null },
  trace: JobTrace | undefined,
): number | null {
  if (job.started_at != null) {
    const start = new Date(job.started_at).getTime();
    const end = job.ended_at != null ? new Date(job.ended_at).getTime() : Date.now();
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

// Time the job sat in queue before execution started. Only meaningful when
// both timestamps are present; null for legacy traces or jobs never started.
export function jobQueuedMs(job: { created_at?: string | null; started_at?: string | null }): number | null {
  if (job.created_at == null || job.started_at == null) return null;
  return Math.max(0, new Date(job.started_at).getTime() - new Date(job.created_at).getTime());
}

export interface JobTokenTotals {
  input: number;
  output: number;
  cached: number;
  cacheCreate: number;
  inputTotal: number;
}

export function summaryTokens(summary: TraceJobSummary): JobTokenTotals {
  return {
    input: summary.input_tokens,
    output: summary.output_tokens,
    cached: summary.cached_input_tokens,
    cacheCreate: summary.cache_creation_input_tokens,
    inputTotal:
      summary.input_tokens + summary.cached_input_tokens + summary.cache_creation_input_tokens,
  };
}

// Derive token totals from a loaded JobTrace's spans. Used when we have the
// full step/span tree; otherwise prefer `summaryTokens`.
export function traceTokens(trace: JobTrace): JobTokenTotals {
  let input = 0;
  let output = 0;
  let cached = 0;
  let cacheCreate = 0;
  for (const rs of trace.steps) {
    for (const span of rs.spans) {
      if (span.kind.kind === 'llm_call' && span.kind.result) {
        input += span.kind.result.input_tokens ?? 0;
        output += span.kind.result.output_tokens ?? 0;
        cached += span.kind.result.cached_input_tokens ?? 0;
        cacheCreate += span.kind.result.cache_creation_input_tokens ?? 0;
      }
    }
  }
  return { input, output, cached, cacheCreate, inputTotal: input + cached + cacheCreate };
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

// Derive the user-facing input that kicked off the job: the last message in
// the *first* LLM call's input_messages whose `source` is 'user'.
export function jobInputText(trace: JobTrace, messageLog: SessionMessageRow[]): string | null {
  for (const rs of trace.steps) {
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

// Derive the final output text of the job: the most recent LLM call's
// output_content (walking steps/spans back-to-front).
export function jobOutputText(trace: JobTrace): string | null {
  for (let i = trace.steps.length - 1; i >= 0; i--) {
    const rs = trace.steps[i];
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
