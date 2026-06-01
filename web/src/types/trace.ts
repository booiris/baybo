/**
 * TypeScript mirror of the `aura-trace` domain types as they appear on
 * the `GET /v1/traces/{session_id}` wire (untyped JSON on the Rust side
 * by design — see `crates/gateway/src/api/admin/traces.rs`).
 *
 * Keep these in sync with `crates/trace/src/{step,span,event,outcome}.rs`
 * and `crates/model/src/{message,security_types,approval}.rs`. The wire
 * shape was probed against `serde_json::to_string` output, so the
 * nested `{"kind": ...}` and `{"outcome": ...}` shapes are intentional.
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

// Alias — Step.outcome and Span.outcome carry the full state including
// `pending`. The wire shape is identical for both; the type just makes
// intent clearer at call sites.
export type LifecycleState = LifecycleOutcome;

export function isTerminal(state: LifecycleState): boolean {
  return state.outcome !== 'pending';
}

// ── Step ──────────────────────────────────────────────────────────────

export type StepKind =
  | { kind: 'llm_iteration' }
  | { kind: 'compression' }
  | { kind: 'memory_recall' }
  | { kind: 'memory_write' }
  | { kind: 'skill_selection' }
  | { kind: 'progress_observer' }
  | { kind: 'subagent'; child_session_id: string };

export type StepKindTag = StepKind['kind'];

export interface Step {
  id: string;
  job_id: string;
  kind: StepKind;
  started_at: string;
  ended_at?: string | null;
  outcome: LifecycleState;
}

// ── Message content (mirrors aura_model::ContentBlock) ────────────────

export type Role = 'system' | 'user' | 'assistant' | 'tool';

// Provenance of a ChatMessage row (mirrors `aura_model::MessageSource`).
// Several origins ride as a `user` role, so role alone can't tell a genuine
// prompt from a cron fire or an agent-injected reminder — this distinguishes
// them. 'user' = human channel input; 'user_interjection' = a human message
// that arrived mid-turn (steering) — also a user bubble, but framed wire-side;
// 'cron' = a cron fire's framed prompt; 'recalled_memory' = memories recalled
// from long-term storage, injected (framed) to inform the turn; 'agent' =
// everything else the agent injects/produces.
export type MessageSource =
  | 'user'
  | 'user_interjection'
  | 'cron'
  | 'recalled_memory'
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
  /**
   * Provenance of the row. Distinguishes a genuine user prompt ('user') from
   * a cron fire ('cron') and from the `user`-role messages the agent injects
   * ('agent': skill reminders, subagent tasks, the system prompt, etc.).
   */
  source: MessageSource;
}

// ── Span ──────────────────────────────────────────────────────────────

export interface ToolCallOrigin {
  llm_span_id: string;
  tool_use_id: string;
}

/**
 * Mirrors `aura_trace::LlmCallInputs` (`#[serde(untagged)]`). The
 * inline array variant matches the long-standing wire shape; the
 * `{ last_ordinal }` object variant is what the per-job trace endpoint
 * returns for main-agent spans whose transcript lives in
 * `session_messages`. Use `resolveInputMessages` to flatten either
 * form into a `ChatMessage[]` for rendering.
 */
export type LlmCallInputs =
  | ChatMessage[]
  | { last_ordinal: number };

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
}

export type SpanKind =
  | { kind: 'llm_call'; begin: LlmCallBegin; result?: LlmCallResult | null }
  | { kind: 'tool_call'; begin: ToolCallBegin; result?: ToolCallResult | null }
  | { kind: 'subagent_stub'; child_session_id: string };

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

export type JobStatusKind =
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

// ── Trace overview / per-job split (matches aura_query) ──────────────

/**
 * Mirrors `aura_session::StoredMessage` on the wire. Used to hydrate
 * `LlmCallInputs::Persisted` slices in the client without the server
 * re-inlining the same prefix into every span.
 */
export interface SessionMessageRow {
  ordinal: number;
  superseded_by?: number | null;
  created_at: string;
  message: ChatMessage;
}

/**
 * Per-job row in `TraceOverview`. Carries everything the sidebar +
 * job-summary panel need before the user drills into a specific job —
 * notably the `↑in ↓out` token chips, aggregated server-side from the
 * cost store so the client doesn't need the span tree to render them.
 * `is_inherited === true` rows live in the parent of a UserFork session.
 */
export interface TraceJobSummary {
  job_id: string;
  session_id: string;
  job_status_kind: JobStatusKind;
  created_at: string;
  started_at?: string | null;
  ended_at?: string | null;
  is_inherited: boolean;
  inherited_from_session_id?: string | null;
  input_tokens: number;
  output_tokens: number;
  cached_input_tokens: number;
  cache_creation_input_tokens: number;
}

/** Response shape of `GET /v1/traces/{session_id}`. */
export interface TraceOverview {
  session_id: string;
  session_messages: SessionMessageRow[];
  jobs: TraceJobSummary[];
}

/** Response shape of `GET /v1/traces/{session_id}/jobs/{job_id}`. */
export interface JobTrace {
  job_id: string;
  session_id: string;
  job_status_kind: JobStatusKind;
  created_at?: string | null;
  started_at?: string | null;
  ended_at?: string | null;
  steps: ReplayStep[];
}

/**
 * Slice the active-as-of-`lastOrdinal` window out of
 * `session_messages`, mirroring `QueryApi::hydrate_persisted_inputs`:
 *
 *   WHERE ordinal <= lastOrdinal
 *     AND (superseded_by IS NULL OR superseded_by > lastOrdinal)
 *
 * If any candidate row was written *after* the span started, the
 * current log is from a different epoch (parent session reset, ordinal
 * reuse) and we return an empty slice rather than misleading content.
 */
export function hydratePersistedInput(
  log: SessionMessageRow[],
  lastOrdinal: number,
  spanStartedAt: string,
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
  return candidates.map((c) => c.message);
}

/**
 * Flatten an `LlmCallBegin.input_messages` field to a `ChatMessage[]`
 * regardless of which variant the wire used. `log` is the
 * `session_messages` array from the overview call; `spanStartedAt`
 * comes from the owning `Span.started_at`.
 */
export function resolveInputMessages(
  input: LlmCallInputs,
  log: SessionMessageRow[],
  spanStartedAt: string,
): ChatMessage[] {
  if (Array.isArray(input)) return input;
  return hydratePersistedInput(log, input.last_ordinal, spanStartedAt);
}
