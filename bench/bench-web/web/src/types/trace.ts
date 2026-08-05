/**
 * TypeScript mirror of the `baybo-trace` domain types as they appear on
 * the wire. Ported verbatim from the gateway dashboard
 * (`web/src/types/trace.ts`) — the bench `trace.json` / `messages.json`
 * files ARE this serialization, so the viewer renders them unchanged.
 * The bench-web backend reshapes the file envelope (`{session, turns}` +
 * `{messages}`) into `{session_id, session_messages, turns}` to match
 * `TraceOverview` below.
 *
 * Keep in sync with `crates/trace/src/{step,span,event,outcome}.rs` and
 * `crates/model/src/{message,security_types,approval}.rs`.
 */

// ── Lifecycle ─────────────────────────────────────────────────────────

export type CancelReason =
  | 'user_preempt'
  | 'user_stopped'
  | 'system_crash'
  | 'parent_cancelled'
  | 'timeout'
  | 'cost_limit'
  | 'security_block'
  | string;

export type LifecycleOutcome =
  | { outcome: 'pending' }
  | { outcome: 'ok' }
  | { outcome: 'failed'; reason: string }
  | { outcome: 'cancelled'; reason: CancelReason };

export type LifecycleState = LifecycleOutcome;

export function isTerminal(state: LifecycleState): boolean {
  return state.outcome !== 'pending';
}

// ── Step ──────────────────────────────────────────────────────────────

export type StepKind =
  | { kind: 'llm_iteration' }
  | {
      kind: 'compression';
      trigger?: CompressionTrigger | null;
      applied?: CompressionApplied | null;
    }
  | { kind: 'memory_recall' }
  | { kind: 'memory_write' }
  | { kind: 'skill_selection' }
  | { kind: 'progress_observer' }
  | { kind: 'title_generation' };

/** Why a compaction ran (`baybo_trace::CompressionTrigger`). */
export type CompressionTrigger = 'threshold' | 'forced';

/** How a compaction shrank the context (`baybo_trace::CompressionApplied`). */
export type CompressionApplied = 'live_summary' | 'truncate';

export type StepKindTag = StepKind['kind'];

export interface Step {
  id: string;
  turn_id: string;
  kind: StepKind;
  started_at: string;
  ended_at?: string | null;
  outcome: LifecycleState;
}

// ── Message content (mirrors baybo_model::ContentBlock) ────────────────

export type Role = 'system' | 'user' | 'assistant' | 'tool';

export type MessageSource =
  | 'user'
  | 'user_interjection'
  | 'cron'
  | 'issue_brief'
  | 'cron_notification'
  | 'recalled_memory'
  | 'system_prompt_update'
  | 'skill_listing'
  | 'skills_update'
  | 'agent';

export interface BlobRef {
  blob_id: string;
}

export type ThinkingContent =
  | { kind: 'text'; text: string; signature?: string | null }
  | { kind: 'summary'; text: string }
  | { kind: 'redacted'; data: string };

export type ContentBlock =
  | { Text: string }
  | { Image: { blob: BlobRef; mime_type: string } }
  | { Audio: { blob: BlobRef; mime_type: string } }
  | { File: { blob: BlobRef; filename: string; mime_type: string } }
  | {
      ToolUse: {
        id: string;
        name: string;
        input: unknown;
        signature?: string | null;
      };
    }
  | { ToolResult: { tool_use_id: string; content: string } }
  | { Thinking: { id?: string | null; content: ThinkingContent[] } };

export interface ChatMessage {
  role: Role;
  content: ContentBlock[];
  source: MessageSource;
}

// ── Span ──────────────────────────────────────────────────────────────

export interface ToolCallOrigin {
  llm_span_id: string;
  tool_use_id: string;
}

export type LlmCallInputs =
  | ChatMessage[]
  | { last_ordinal: number; prefix_len: number; suffix?: ChatMessage[] };

export interface LlmCallBegin {
  model_id: string;
  provider: string;
  provider_config_hash: string;
  input_messages: LlmCallInputs;
  temperature?: number | null;
}

export interface LlmToolCallRecord {
  id: string;
  name: string;
  arguments: unknown;
}

export interface LlmCallResult {
  output_content?: string;
  thinking?: string | null;
  tool_calls?: LlmToolCallRecord[];
  input_tokens?: number;
  output_tokens?: number;
  cached_input_tokens?: number;
  cache_creation_input_tokens?: number;
}

export interface ToolCallBegin {
  tool_name: string;
  tool_artifact_hash: string;
  triggered_by?: ToolCallOrigin | null;
  params: unknown;
}

export interface ToolCallResult {
  output: unknown;
  success: boolean;
  /**
   * Serialized byte length of the untruncated output, present only when the
   * span's copy was capped (at the same budget the LLM transcript uses). The
   * model never saw more than the cap either. Absent = stored verbatim.
   */
  output_truncated_from?: number;
}

export type SpanKind =
  | { kind: 'llm_call'; begin: LlmCallBegin; result?: LlmCallResult | null }
  | { kind: 'tool_call'; begin: ToolCallBegin; result?: ToolCallResult | null };


export type SpanKindTag = SpanKind['kind'];

export interface Span {
  id: string;
  step_id: string;
  kind: SpanKind;
  parallel_group?: string | null;
  started_at: string;
  ended_at?: string | null;
  outcome: LifecycleState;
  events?: SpanEvent[];
}

// ── SpanEvent ─────────────────────────────────────────────────────────

export type SecretKind =
  | 'api_key'
  | 'bearer_token'
  | 'aws_access_key'
  | 'aws_secret_key'
  | 'private_key'
  | 'password'
  | 'high_entropy'
  | 'other';

export type ApprovalDecision = 'approve' | 'approve_always' | 'deny';

export type ResourceAccess =
  | { kind: 'read_file'; path: string }
  | { kind: 'write_file'; path: string }
  | { kind: 'http'; host: string }
  | { kind: 'exec_command'; command: string };

export type ToolEventPayload =
  | { type: 'phase'; duration_ms: number }
  | {
      type: 'http_fetch';
      status: number;
      bytes: number;
      content_type: string | null;
      body_preview: string | null;
    }
  | {
      type: 'llm_call';
      model: string;
      input: string;
      output: string;
    }
  | {
      type: 'parse_failure';
      command: string;
    };

export type SpanEventKind =
  | {
      kind: 'sanitize_hit';
      hits_count: number;
      kinds: SecretKind[];
      placeholder_ids: string[];
    }
  | {
      kind: 'approval';
      decision: ApprovalDecision;
      resource: ResourceAccess;
    }
  | {
      kind: 'tool_event';
      action: string;
      payload: ToolEventPayload;
    };

export interface SpanEvent {
  span_id: string;
  seq: number;
  at: string;
  kind: SpanEventKind;
}

// ── Replay (per-session export wire shape) ────────────────────────────

export type TurnStatusKind =
  | 'pending'
  | 'in_progress'
  | 'stuck'
  | 'cancelled'
  | 'failed'
  | 'completed';

export interface ReplayStep {
  step: Step;
  spans: Span[];
}

// ── Trace overview / per-turn split (matches baybo_query) ─────────────

export interface SessionMessageRow {
  ordinal: number;
  superseded_by?: number | null;
  created_at: string;
  message: ChatMessage;
}

export interface TraceTurnSummary {
  turn_id: string;
  session_id: string;
  turn_status_kind: TurnStatusKind;
  created_at: string;
  started_at?: string | null;
  ended_at?: string | null;
  input_tokens: number;
  output_tokens: number;
  cached_input_tokens: number;
  cache_creation_input_tokens: number;
}

export interface TraceOverview {
  session_id: string;
  session_messages: SessionMessageRow[];
  turns: TraceTurnSummary[];
}

export interface TurnTrace {
  turn_id: string;
  session_id: string;
  turn_status_kind: TurnStatusKind;
  created_at?: string | null;
  started_at?: string | null;
  ended_at?: string | null;
  steps: ReplayStep[];
}

export function hydratePersistedInput(
  log: SessionMessageRow[],
  lastOrdinal: number,
  spanStartedAt: string,
  prefixLen: number,
  suffix: ChatMessage[] = [],
): ChatMessage[] {
  const candidates = log.filter(
    (m) =>
      m.ordinal <= lastOrdinal &&
      (m.superseded_by == null || m.superseded_by > lastOrdinal),
  );
  const spanStart = new Date(spanStartedAt).getTime();
  if (candidates.some((m) => new Date(m.created_at).getTime() > spanStart)) {
    return [];
  }
  const prefix = candidates.map((c) => c.message);
  if (prefix.length !== prefixLen) {
    return [
      reconstructionWarning(prefixLen, prefix.length),
      ...prefix,
      ...suffix,
    ];
  }
  return [...prefix, ...suffix];
}

function reconstructionWarning(
  expected: number,
  reconstructed: number,
): ChatMessage {
  return {
    role: 'system',
    source: 'agent',
    content: [
      {
        Text:
          `⚠️ trace reconstruction inconsistent: expected ${expected} prefix ` +
          `message(s) from session_messages, reconstructed ${reconstructed}. ` +
          `The log drifted under this span's ordinal reference — the input ` +
          `shown may be incomplete or wrong.`,
      },
    ],
  };
}

export function resolveInputMessages(
  input: LlmCallInputs,
  log: SessionMessageRow[],
  spanStartedAt: string,
): ChatMessage[] {
  if (Array.isArray(input)) return input;
  return hydratePersistedInput(
    log,
    input.last_ordinal,
    spanStartedAt,
    input.prefix_len,
    input.suffix ?? [],
  );
}
