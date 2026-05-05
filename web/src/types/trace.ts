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
}

// ── Span ──────────────────────────────────────────────────────────────

export interface ToolCallOrigin {
  llm_span_id: string;
  tool_use_id: string;
}

export interface LlmCallBegin {
  model_id: string;
  provider: string;
  provider_config_hash: string;
  input_messages: ChatMessage[];
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

export interface ReplayJob {
  job_id: string;
  job_status_kind: JobStatusKind;
  steps: ReplayStep[];
}

export interface SessionReplay {
  session_id: string;
  jobs: ReplayJob[];
}
