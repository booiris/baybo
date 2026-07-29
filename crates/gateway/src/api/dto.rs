//! Shared request / response DTOs for the v1 API.
//!
//! Every type that appears in an admin-API response lives here and
//! derives [`utoipa::ToSchema`] so `crate::openapi` can emit a spec
//! matching the wire format. Domain types in `baybo-model`, `baybo-turn`,
//! `baybo-cron`, `baybo-tools` deliberately stay free of utoipa so
//! changes over there don't silently reshape the HTTP surface — every
//! field crossing the API boundary has an explicit DTO mirror plus a
//! `From` conversion, and the drift test picks up any schema change.
//!
//! When a domain type changes:
//!
//! * If the DTO should follow, update the mirror here and its `From`
//!   impl, then run `UPDATE_OPENAPI=1 cargo test -p baybo-gateway
//!   --test all openapi_json_is_in_sync` + `pnpm --filter baybo-web gen:api`.
//! * If the DTO should stay fixed (back-compat), the conversion
//!   absorbs the rename/removal here, keeping clients stable.
//!
//! **v1 stability:** the v1 surface is in active development and has
//! no published external consumers yet. Breaking shape changes
//! (e.g. the `TurnStatus` `kind`/`reason`/`cancel_reason` envelope
//! introduced with the trace redesign) land directly on `/v1/*`
//! without a parallel `/v2/*`. Once an external consumer is on the
//! record we'll switch to additive-only changes here.
//!
//! Channel-side routes reuse [`ListResponse`] too; the generic
//! `ToSchema` impl only applies when `T: ToSchema`, so a channel route
//! using `ListResponse<Session>` still compiles even though `Session`
//! stays outside the OpenAPI surface.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Envelope for list endpoints. `next_cursor` is opaque — clients
/// pass it back as `?cursor=` to fetch the next page, and treat
/// `None` as "no more pages." The cursor's internal scheme may change
/// across releases; clients must not parse it.
#[derive(Debug, Serialize, ToSchema)]
pub struct ListResponse<T> {
    pub items: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl<T> ListResponse<T> {
    pub fn new(items: Vec<T>) -> Self {
        Self {
            items,
            next_cursor: None,
        }
    }

    pub fn with_next_cursor(items: Vec<T>, next_cursor: Option<String>) -> Self {
        Self { items, next_cursor }
    }
}

/// Uniform error envelope for every non-2xx admin response.
///
/// `GatewayError::into_response` already emits `{"error": "..."}`; this
/// type just pins the shape in the spec so clients can discriminate on
/// it rather than on status codes alone.
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: String,
}

// ── ChannelType ──────────────────────────────────────────────────────

/// Admin-surface mirror of [`baybo_model::ChannelType`]. Transparent
/// wrapper around a snake_case string so the OpenAPI surface stays
/// stable while the core type is open-ended (runtime-registered
/// sidecars like `"slack"` pass through unchanged).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String)]
pub struct ChannelType(pub String);

impl From<baybo_model::ChannelType> for ChannelType {
    fn from(v: baybo_model::ChannelType) -> Self {
        Self(v.into_string())
    }
}

impl From<ChannelType> for baybo_model::ChannelType {
    fn from(v: ChannelType) -> Self {
        baybo_model::ChannelType::from(v.0)
    }
}

impl std::fmt::Display for ChannelType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ── Gateway envelopes ────────────────────────────────────────────────

/// Read-only snapshot of a single registered channel.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChannelEntry {
    pub channel_type: ChannelType,
    pub status: String,
}

/// Minimal gateway health/status payload.
#[derive(Debug, Serialize, ToSchema)]
pub struct StatusResponse {
    pub version: String,
    pub bind_address: String,
    pub sessions: usize,
    pub turns_in_flight: usize,
}

/// One in-flight background job (detached subagent or `Bash` command),
/// across all sessions — the cross-session twin of the `JobList` tool.
#[derive(Debug, Serialize, ToSchema)]
pub struct BackgroundJob {
    /// User-facing handle (`bg-…`), as shown by `JobList`/`JobStop`.
    pub handle: String,
    /// Parent session that dispatched it.
    pub session_id: String,
    /// Subagent profile name, or `"command"` for a detached `Bash` command.
    pub kind: String,
    /// Task summary (subagent) or the command line (command).
    pub summary: String,
}

/// Response body for `GET /v1/background-jobs`.
#[derive(Debug, Serialize, ToSchema)]
pub struct BackgroundJobsResponse {
    pub jobs: Vec<BackgroundJob>,
}

/// Response body for `PUT` / `DELETE /v1/config`.
#[derive(Debug, Serialize, ToSchema)]
pub struct MutateResponse {
    pub path: String,
    pub written_to: String,
    pub requires_restart: bool,
}

/// `PUT /v1/config` body.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetConfigRequest {
    pub path: String,
    /// JSON value written at `path`. Shape validated by `BayboConfig`.
    #[schema(value_type = Object)]
    pub value: serde_json::Value,
}

/// `DELETE /v1/config` body.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UnsetConfigRequest {
    pub path: String,
}

/// Current LLM provider descriptor.
#[derive(Debug, Serialize, ToSchema)]
pub struct LlmInfo {
    pub model_id: String,
    pub provider: String,
}

// ── Models dashboard ─────────────────────────────────────────────────

/// Per-entry pricing override fields. Wire shape mirrors
/// [`baybo_model::LlmPricingOverride`]; each field is integer micro-USD
/// per 1M tokens (1 USD = 1_000_000). The DTO exists separately from
/// the canonical struct only so we can derive `ToSchema` for utoipa
/// without leaking that derive into `baybo-model`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, ToSchema)]
pub struct LlmPricingOverrideDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub input_per_1m_tokens: Option<baybo_model::MicroUsd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub output_per_1m_tokens: Option<baybo_model::MicroUsd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub cached_input_per_1m_tokens: Option<baybo_model::MicroUsd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub cache_write_per_1m_tokens: Option<baybo_model::MicroUsd>,
}

impl From<baybo_model::LlmPricingOverride> for LlmPricingOverrideDto {
    fn from(v: baybo_model::LlmPricingOverride) -> Self {
        Self {
            input_per_1m_tokens: v.input_per_1m_tokens,
            output_per_1m_tokens: v.output_per_1m_tokens,
            cached_input_per_1m_tokens: v.cached_input_per_1m_tokens,
            cache_write_per_1m_tokens: v.cache_write_per_1m_tokens,
        }
    }
}

impl From<LlmPricingOverrideDto> for baybo_model::LlmPricingOverride {
    fn from(v: LlmPricingOverrideDto) -> Self {
        Self {
            input_per_1m_tokens: v.input_per_1m_tokens,
            output_per_1m_tokens: v.output_per_1m_tokens,
            cached_input_per_1m_tokens: v.cached_input_per_1m_tokens,
            cache_write_per_1m_tokens: v.cache_write_per_1m_tokens,
        }
    }
}

/// Effective per-token pricing for a model after applying any operator
/// overrides — micro-USD per 1M tokens.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, ToSchema)]
pub struct LlmModelPricingDto {
    #[schema(value_type = i64)]
    pub input_per_1m_tokens: baybo_model::MicroUsd,
    #[schema(value_type = i64)]
    pub output_per_1m_tokens: baybo_model::MicroUsd,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub cached_input_per_1m_tokens: Option<baybo_model::MicroUsd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub cache_write_per_1m_tokens: Option<baybo_model::MicroUsd>,
}

/// One model an entry serves, with the operator's per-model overrides.
/// Read-only mirror of `baybo_config::LlmModelSpec` — `model_list` is
/// edited in `baybo.json`, not through this API.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LlmModelSpecDto {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<LlmPricingOverrideDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
}

impl From<baybo_config::LlmModelSpec> for LlmModelSpecDto {
    fn from(v: baybo_config::LlmModelSpec) -> Self {
        Self {
            model: v.model,
            context_window: v.context_window,
            pricing: v.pricing.map(LlmPricingOverrideDto::from),
            supports_vision: v.supports_vision,
        }
    }
}

/// One row in `GET /v1/llm/models`. Carries both the raw config (so the
/// edit form can populate "override or unset" toggles) and the
/// **effective** values that result from layering overrides over the
/// factory / OpenRouter snapshot defaults.
#[derive(Debug, Serialize, ToSchema)]
pub struct LlmModelEntry {
    pub name: String,
    pub provider: String,
    pub model: String,
    /// Every model this entry serves, in picker order, each with its own
    /// overrides. Always contains `model`. Read-only here.
    #[serde(default)]
    pub model_list: Vec<LlmModelSpecDto>,
    /// Cheaper/faster model this entry uses for lightweight auxiliary
    /// calls. Names one of `model_list`. `None` when unconfigured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lite_model: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    /// `true` when an API key is currently resolvable for this entry
    /// (vault entry, explicit `api_key_env`, or provider-default env
    /// var). The literal value never leaves the gateway.
    pub api_key_configured: bool,
    pub reasoning_effort: Option<String>,
    pub is_default: bool,

    /// The **default** model's `supports_vision` override. `None` =
    /// factory default. Mirrors `model_list[default].supports_vision`;
    /// overrides for the entry's other models are file-edited only.
    pub supports_vision_override: Option<bool>,
    /// The default model's `context_window` override. `None` = factory
    /// default.
    pub context_window_override: Option<usize>,
    /// The default model's pricing override. `None` = factory default.
    pub pricing_override: Option<LlmPricingOverrideDto>,

    /// Effective `context_window` of the entry's **default** model, given
    /// its overrides and the snapshot/factory defaults.
    pub effective_context_window: usize,
    /// Effective vision flag of the entry's default model.
    pub effective_supports_vision: bool,
    /// Effective pricing of the entry's default model.
    pub effective_pricing: LlmModelPricingDto,
}

/// `GET /v1/llm/models` response.
#[derive(Debug, Serialize, ToSchema)]
pub struct LlmModelsResponse {
    pub default_name: String,
    pub items: Vec<LlmModelEntry>,
}

/// `PUT /v1/llm/models/{name}` body. Every field is optional — only
/// present fields are applied. To clear an override pass an explicit
/// `null` (handled by including the key in the request JSON; serde
/// can't distinguish "missing key" from "key with null" by default, so
/// the handler treats `Some(None)` as clear and an absent key as keep).
#[derive(Debug, Deserialize, Default, ToSchema)]
pub struct UpdateLlmModelRequest {
    /// Set the provider id (e.g. `"openai"`). Requires gateway restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Set the model id (e.g. `"gpt-4o"`). Requires gateway restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Custom base URL or `null` to clear.
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub base_url: Option<Option<String>>,
    /// Environment variable name holding the API key, or `null` to clear.
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub api_key_env: Option<Option<String>>,
    /// Reasoning-effort override, or `null` to clear.
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub reasoning_effort: Option<Option<String>>,
    /// `supports_vision` override for the entry's **default** model, or
    /// `null` to clear. Lands in that model's `model_list` spec; the
    /// entry's other models are file-edited only.
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub supports_vision: Option<Option<bool>>,
    /// `context_window` override for the default model, or `null` to clear.
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub context_window: Option<Option<usize>>,
    /// Pricing override for the default model, or `null` to clear.
    #[serde(default, deserialize_with = "deserialize_optional_field")]
    pub pricing: Option<Option<LlmPricingOverrideDto>>,
    /// Set the literal API key in the vault (`llm.entry.<name>.api_key`).
    /// Pass `""` to remove the vault entry, or omit the field to leave
    /// the vault untouched. Never echoed back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

/// Helper for `Option<Option<T>>` — distinguishes "absent" from "null".
/// Without this, serde collapses both into `None` and the handler can't
/// tell "keep current" from "clear override".
fn deserialize_optional_field<'de, T, D>(de: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<T>::deserialize(de)?))
}

/// `PUT /v1/llm/default` body.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetDefaultLlmRequest {
    pub name: String,
}

/// `POST /v1/llm/models/{name}/test` response. Carries the latency and
/// token usage from a one-shot `ping` probe.
#[derive(Debug, Serialize, ToSchema)]
pub struct LlmModelTestResult {
    pub ok: bool,
    /// On `ok: false`, a human-readable error suitable for direct
    /// display (auth failure, network error, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Round-trip latency in ms (only on success).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<usize>,
    /// Provider id used for the probe (may differ from the request's
    /// `name` if the entry's provider was just edited).
    pub provider: String,
    pub model: String,
}

/// One row in `GET /v1/llm/usage` — aggregated cost / token volume for
/// a single configured entry, joined by `entry.model == record.model`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LlmModelUsage {
    /// Entry `name`. May be unique per (provider, model) pair.
    pub name: String,
    /// The `model` id used to look up records.
    pub model: String,
    pub call_count: usize,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_creation_input_tokens: usize,
    #[schema(value_type = i64)]
    pub cost_micro_usd: baybo_model::MicroUsd,
}

/// `GET /v1/llm/usage` response.
#[derive(Debug, Serialize, ToSchema)]
pub struct LlmUsageResponse {
    /// Inclusive lower bound of the aggregation window.
    pub since: DateTime<Utc>,
    /// Exclusive upper bound.
    pub until: DateTime<Utc>,
    pub items: Vec<LlmModelUsage>,
}

/// `GET /v1/llm/usage` query params.
#[derive(Debug, Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LlmUsageQuery {
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
}

/// `POST /v1/sessions` body.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateSessionRequest {
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub user_name: Option<String>,
    #[serde(default)]
    pub channel: Option<ChannelType>,
}

/// `POST /v1/sessions/:id/messages` body.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SendMessageRequest {
    pub text: String,
}

/// Response for `POST /v1/sessions/:id/messages`.
#[derive(Debug, Serialize, ToSchema)]
pub struct SendMessageResponse {
    pub message_id: String,
}

/// `POST /v1/cron` body. Schedule format is the standard 5-field cron
/// string accepted by [`baybo_cron`].
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateCronRequest {
    pub schedule: String,
    pub user_id: String,
    #[serde(default)]
    pub channel: Option<ChannelType>,
    /// Short human name for the job. Names the conversation a recurring fire
    /// opens and heads the result a one-shot reports back to the conversation
    /// that scheduled it.
    pub title: String,
    pub text: String,
    /// IANA timezone (e.g. `"Asia/Shanghai"`) used to evaluate the cron
    /// expression and to render time fields in responses. Required —
    /// every time the API speaks is anchored to this zone, so callers
    /// must commit to one explicitly.
    pub timezone: String,
    #[serde(default)]
    pub origin_session_id: Option<String>,
}

/// `PATCH /v1/cron/{id}` body: a partial edit of the job's authored fields.
/// Every field is optional and named exactly as it is on [`CronJob`], so a
/// client patches by sending back the subset it changed on the job it fetched.
/// An absent field keeps the value the job already has, and a body that sets
/// nothing at all is rejected — there is nothing to write, and answering with
/// the unchanged job would tell the user their edit landed.
///
/// The scheduler's own fields (`status`, `next_trigger_at`,
/// `last_triggered_at`) are not patchable: they are derived from the schedule,
/// or moved by `POST /v1/cron/{id}/{pause,resume}`.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct UpdateCronRequest {
    pub title: Option<String>,
    pub prompt: Option<String>,
    /// The new schedule, in the same tagged shape [`CronJob`] reports:
    /// `{"kind":"cron","expr":"0 8 * * *"}` or
    /// `{"kind":"at","time":"2026-07-15T08:00:00Z"}`. Swapping one kind for the
    /// other is allowed — that is how a fired one-shot gets a new moment.
    pub schedule: Option<CronSchedule>,
    /// IANA timezone the cron expression is evaluated in. Changing it alone
    /// reschedules the job: the same expression names a different instant in a
    /// different zone.
    pub timezone: Option<String>,
}

impl From<UpdateCronRequest> for baybo_cron::CronJobPatch {
    fn from(v: UpdateCronRequest) -> Self {
        Self {
            title: v.title,
            prompt: v.prompt,
            schedule: v.schedule.map(Into::into),
            timezone: v.timezone,
        }
    }
}

// ── Turn ──────────────────────────────────────────────────────────────

/// Wire mirror of [`baybo_turn::TurnStatus`]. Carries the same payload
/// the domain enum carries (cancel reason, partial-artifact span IDs);
/// the wire shape collapses inner-variant content into `Option`-typed
/// fields so HTTP clients can decode without needing the full Rust
/// enum machinery.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct TurnStatus {
    pub kind: TurnStatusKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_reason: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub partial_artifacts: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatusKind {
    Pending,
    InProgress,
    Stuck,
    Cancelled,
    Failed,
    Completed,
}

impl From<baybo_turn::TurnStatusKind> for TurnStatusKind {
    fn from(v: baybo_turn::TurnStatusKind) -> Self {
        match v {
            baybo_turn::TurnStatusKind::Pending => Self::Pending,
            baybo_turn::TurnStatusKind::InProgress => Self::InProgress,
            baybo_turn::TurnStatusKind::Stuck => Self::Stuck,
            baybo_turn::TurnStatusKind::Cancelled => Self::Cancelled,
            baybo_turn::TurnStatusKind::Failed => Self::Failed,
            baybo_turn::TurnStatusKind::Completed => Self::Completed,
        }
    }
}

impl From<TurnStatusKind> for baybo_turn::TurnStatusKind {
    fn from(v: TurnStatusKind) -> Self {
        match v {
            TurnStatusKind::Pending => Self::Pending,
            TurnStatusKind::InProgress => Self::InProgress,
            TurnStatusKind::Stuck => Self::Stuck,
            TurnStatusKind::Cancelled => Self::Cancelled,
            TurnStatusKind::Failed => Self::Failed,
            TurnStatusKind::Completed => Self::Completed,
        }
    }
}

impl From<baybo_turn::TurnStatus> for TurnStatus {
    fn from(v: baybo_turn::TurnStatus) -> Self {
        let kind = TurnStatusKind::from(v.kind());
        match v {
            baybo_turn::TurnStatus::Pending
            | baybo_turn::TurnStatus::InProgress
            | baybo_turn::TurnStatus::Completed => Self {
                kind,
                reason: None,
                cancel_reason: None,
                partial_artifacts: Vec::new(),
            },
            baybo_turn::TurnStatus::Stuck { reason }
            | baybo_turn::TurnStatus::Failed { reason } => Self {
                kind,
                reason: Some(reason),
                cancel_reason: None,
                partial_artifacts: Vec::new(),
            },
            baybo_turn::TurnStatus::Cancelled {
                reason,
                partial_artifacts,
            } => Self {
                kind,
                reason: None,
                cancel_reason: Some(reason.as_snake_case().to_string()),
                partial_artifacts: partial_artifacts
                    .into_iter()
                    .map(|s| s.to_string())
                    .collect(),
            },
        }
    }
}

/// Wire mirror of [`baybo_turn::TurnInputKind`] — what payload fed the turn.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnInputKind {
    UserChat,
    Cron,
    CronNotification,
    Compact,
    Spawned,
    SubagentNotification,
}

impl From<baybo_turn::TurnInputKind> for TurnInputKind {
    fn from(v: baybo_turn::TurnInputKind) -> Self {
        match v {
            baybo_turn::TurnInputKind::UserChat => Self::UserChat,
            baybo_turn::TurnInputKind::Cron => Self::Cron,
            baybo_turn::TurnInputKind::CronNotification => Self::CronNotification,
            baybo_turn::TurnInputKind::Compact => Self::Compact,
            baybo_turn::TurnInputKind::Spawned => Self::Spawned,
            baybo_turn::TurnInputKind::SubagentNotification => Self::SubagentNotification,
        }
    }
}

/// Wire mirror of a turn's origin (the owning session's root trigger,
/// [`baybo_model::TriggerKind`]).
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnOrigin {
    User,
    Cron,
    Spawned,
}

impl From<baybo_model::TriggerKind> for TurnOrigin {
    fn from(v: baybo_model::TriggerKind) -> Self {
        match v {
            baybo_model::TriggerKind::User => Self::User,
            baybo_model::TriggerKind::Cron => Self::Cron,
            baybo_model::TriggerKind::Spawned => Self::Spawned,
        }
    }
}

/// Wire mirror of [`baybo_turn::Turn`]. Inner shape reflects the new
/// state machine (Q6) — `final_result` replaces `output`/`error`,
/// `emitted_span_ids` replaces `trace_span_id`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Turn {
    pub id: String,
    pub session_id: String,
    pub parent_turn_id: Option<String>,
    /// What payload fed the turn (display-only projection of the input).
    pub input_kind: TurnInputKind,
    /// The owning session's root trigger.
    pub origin: TurnOrigin,
    pub status: TurnStatus,
    #[schema(value_type = Option<Object>)]
    pub final_result: Option<serde_json::Value>,
    pub emitted_span_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl From<baybo_turn::Turn> for Turn {
    fn from(v: baybo_turn::Turn) -> Self {
        Self {
            id: v.id.to_string(),
            session_id: v.session_id.to_string(),
            parent_turn_id: v.parent_turn_id.map(|p| p.to_string()),
            input_kind: v.input_kind().into(),
            origin: v.origin.into(),
            status: v.status.into(),
            final_result: v
                .final_result
                .map(|o| serde_json::to_value(&o).unwrap_or(serde_json::Value::Null)),
            emitted_span_ids: v
                .emitted_span_ids
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
            created_at: v.created_at,
            started_at: v.started_at,
            ended_at: v.ended_at,
        }
    }
}

// ── CronJob ──────────────────────────────────────────────────────────

/// Mirror of [`baybo_cron::CronStatus`].
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CronStatus {
    Enabled,
    Disabled,
    Executed,
}

impl From<baybo_cron::CronStatus> for CronStatus {
    fn from(v: baybo_cron::CronStatus) -> Self {
        match v {
            baybo_cron::CronStatus::Enabled => Self::Enabled,
            baybo_cron::CronStatus::Disabled => Self::Disabled,
            baybo_cron::CronStatus::Executed => Self::Executed,
        }
    }
}

/// Mirror of [`baybo_cron::CronSchedule`].
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CronSchedule {
    Cron { expr: String },
    At { time: DateTime<Utc> },
}

impl From<baybo_cron::CronSchedule> for CronSchedule {
    fn from(v: baybo_cron::CronSchedule) -> Self {
        match v {
            baybo_cron::CronSchedule::Cron { expr } => Self::Cron { expr },
            baybo_cron::CronSchedule::At { time } => Self::At { time },
        }
    }
}

impl From<CronSchedule> for baybo_cron::CronSchedule {
    fn from(v: CronSchedule) -> Self {
        match v {
            CronSchedule::Cron { expr } => Self::Cron { expr },
            CronSchedule::At { time } => Self::At { time },
        }
    }
}

/// Mirror of [`baybo_cron::CronJob`].
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CronJob {
    pub id: String,
    pub user_id: String,
    pub channel: ChannelType,
    /// Short human name for the turn. Empty for rows created before the field
    /// existed — clients fall back to the prompt.
    pub title: String,
    pub schedule: CronSchedule,
    pub prompt: String,
    pub timezone: String,
    pub status: CronStatus,
    pub last_triggered_at: Option<DateTime<Utc>>,
    pub next_trigger_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// When the job was moved to the recycle bin. Absent on a live job.
    /// A present value means the job never fires and is listed only by
    /// `GET /v1/cron?deleted=true`, until `POST /v1/cron/{id}/restore`
    /// brings it back. `status` is orthogonal — a deleted one-shot that
    /// already fired stays `executed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_session_id: Option<String>,
    /// Whether this job's **cron group** — the chat-list row collapsing its
    /// fires — is pinned to the top of the list (`docs/cron-groups.md`).
    /// `#[serde(default)]` so it is not required on the wire (defaults false),
    /// matching `cron_group_pinned` on the chat-list summary.
    #[serde(default)]
    pub pinned: bool,
}

impl From<baybo_cron::CronJob> for CronJob {
    fn from(v: baybo_cron::CronJob) -> Self {
        Self {
            id: v.id,
            user_id: v.user_id,
            channel: v.channel.into(),
            title: v.title,
            schedule: v.schedule.into(),
            prompt: v.prompt,
            timezone: v.timezone,
            status: v.status.into(),
            last_triggered_at: v.last_triggered_at,
            next_trigger_at: v.next_trigger_at,
            created_at: v.created_at,
            updated_at: v.updated_at,
            deleted_at: v.deleted_at,
            origin_session_id: v.origin_session_id.map(|s| s.into_inner()),
            pinned: v.pinned,
        }
    }
}

/// Request body for `PUT /v1/cron/{id}/pin`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct SetCronPinRequest {
    /// `true` to pin this job's cron group to the top of the chat list,
    /// `false` to release it back to recency order.
    pub pinned: bool,
}

// ── Log records ──────────────────────────────────────────────────────

/// Mirror of [`crate::log_buffer::LogLevel`]. Snake-cased on the wire to
/// match the rest of the admin surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl From<crate::log_buffer::LogLevel> for LogLevel {
    fn from(v: crate::log_buffer::LogLevel) -> Self {
        match v {
            crate::log_buffer::LogLevel::Error => Self::Error,
            crate::log_buffer::LogLevel::Warn => Self::Warn,
            crate::log_buffer::LogLevel::Info => Self::Info,
            crate::log_buffer::LogLevel::Debug => Self::Debug,
            crate::log_buffer::LogLevel::Trace => Self::Trace,
        }
    }
}

impl From<LogLevel> for crate::log_buffer::LogLevel {
    fn from(v: LogLevel) -> Self {
        match v {
            LogLevel::Error => Self::Error,
            LogLevel::Warn => Self::Warn,
            LogLevel::Info => Self::Info,
            LogLevel::Debug => Self::Debug,
            LogLevel::Trace => Self::Trace,
        }
    }
}

/// Single structured tracing field (`k=v`) captured alongside a log
/// record. Kept as free-form strings — formatting / unquoting happens
/// in the emitter.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LogField {
    pub name: String,
    pub value: String,
}

/// Mirror of [`crate::log_buffer::LogRecord`]. Used as the item type of
/// [`LogsResponse`].
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LogEntry {
    pub id: u64,
    pub timestamp: DateTime<Utc>,
    pub level: LogLevel,
    /// Tracing target that emitted the record (e.g.
    /// `baybo_gateway::server`). Rendered as "source" in the UI.
    pub target: String,
    pub message: String,
    pub fields: Vec<LogField>,
}

impl From<crate::log_buffer::LogRecord> for LogEntry {
    fn from(v: crate::log_buffer::LogRecord) -> Self {
        Self {
            id: v.id,
            timestamp: v.timestamp,
            level: v.level.into(),
            target: v.target,
            message: v.message,
            fields: v
                .fields
                .into_iter()
                .map(|(name, value)| LogField { name, value })
                .collect(),
        }
    }
}

/// `GET /v1/logs` query params.
#[derive(Debug, Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LogsQuery {
    #[serde(default)]
    pub level: Option<LogLevel>,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// Envelope for `GET /v1/logs`. Unlike [`ListResponse`] we expose
/// `total` so the UI can render pagination (`Showing X of N`).
#[derive(Debug, Serialize, ToSchema)]
pub struct LogsResponse {
    pub items: Vec<LogEntry>,
    /// Total number of records matching the filters — independent of
    /// `limit`/`offset`, so clients can size the pager without asking
    /// for the full list.
    pub total: usize,
}

// ── Trace session summary (list view) ───────────────────────────────

/// Wire mirror of [`baybo_query::SessionKind`]. Coarse trigger/lineage
/// label for the trace browser list view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    User,
    Cron,
    Subagent,
}

impl From<baybo_query::SessionKind> for SessionKind {
    fn from(v: baybo_query::SessionKind) -> Self {
        match v {
            baybo_query::SessionKind::User => Self::User,
            baybo_query::SessionKind::Cron => Self::Cron,
            baybo_query::SessionKind::Subagent => Self::Subagent,
        }
    }
}

impl From<SessionKind> for baybo_query::SessionKind {
    fn from(v: SessionKind) -> Self {
        match v {
            SessionKind::User => Self::User,
            SessionKind::Cron => Self::Cron,
            SessionKind::Subagent => Self::Subagent,
        }
    }
}

/// `GET /v1/traces` query params. All fields are optional; `None`
/// removes that constraint.
#[derive(Debug, Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TracesListQuery {
    /// Filter on the latest turn's status (snake_case enum).
    #[serde(default)]
    pub status: Option<TurnStatusKind>,
    /// Inclusive lower bound on `last_active`.
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    /// Exclusive upper bound on `last_active`.
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
    /// Case-insensitive substring on session id.
    #[serde(default)]
    pub q: Option<String>,
    /// Filter on coarse trigger/lineage label.
    #[serde(default)]
    pub kind: Option<SessionKind>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TraceOverviewQuery {
    /// When set, `session_messages` carries only rows with a greater
    /// ordinal; the client already holds the prefix and re-validates it
    /// via the response's `supersede_watermark`.
    #[serde(default)]
    pub since_ordinal: Option<i64>,
}

/// One row of the trace browser list view. Mirrors
/// [`baybo_query::SessionSummary`] for the wire.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TraceSessionSummary {
    pub session_id: String,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    /// `None` when the session has no turns (those rows are filtered
    /// out, but the type stays Option to keep the wire shape stable
    /// if the policy ever flips).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_turn_status: Option<TurnStatus>,
    pub kind: SessionKind,
    pub turn_count: usize,
    pub span_count: usize,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_creation_input_tokens: usize,
}

impl From<baybo_query::SessionSummary> for TraceSessionSummary {
    fn from(v: baybo_query::SessionSummary) -> Self {
        Self {
            session_id: v.session_id.to_string(),
            created_at: v.created_at,
            last_active: v.last_active,
            latest_turn_status: v.latest_turn_status.map(Into::into),
            kind: v.kind.into(),
            turn_count: v.turn_count,
            span_count: v.span_count,
            input_tokens: v.input_tokens,
            output_tokens: v.output_tokens,
            cached_input_tokens: v.cached_input_tokens,
            cache_creation_input_tokens: v.cache_creation_input_tokens,
        }
    }
}

/// Envelope for `GET /v1/traces`. Carries `total` for "Showing X of N"
/// pagers, matching the shape of [`LogsResponse`].
#[derive(Debug, Serialize, ToSchema)]
pub struct TracesListResponse {
    pub items: Vec<TraceSessionSummary>,
    pub total: usize,
}

// ── Analytics ────────────────────────────────────────────────────────

/// `GET /v1/analytics` query params. Defaults to the last 30 UTC days
/// when no range is supplied.
#[derive(Debug, Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AnalyticsQuery {
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
}

/// One bucket per UTC day for the analytics chart.
///
/// `cost_micro_usd` is integer micro-USD (USD × 10^6). Rendering layers
/// divide by 1_000_000 to get USD; on-wire arithmetic stays exact.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AnalyticsDayBucket {
    /// `YYYY-MM-DD` (UTC).
    pub date: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_creation_input_tokens: usize,
    /// Spend for the day, in **micro-USD** (1 USD = 1_000_000).
    #[schema(value_type = i64)]
    pub cost_micro_usd: baybo_model::MicroUsd,
    pub sessions_created: usize,
}

impl From<baybo_query::AnalyticsDayBucket> for AnalyticsDayBucket {
    fn from(v: baybo_query::AnalyticsDayBucket) -> Self {
        Self {
            date: v.date,
            input_tokens: v.input_tokens,
            output_tokens: v.output_tokens,
            cached_input_tokens: v.cached_input_tokens,
            cache_creation_input_tokens: v.cache_creation_input_tokens,
            cost_micro_usd: v.cost_usd,
            sessions_created: v.sessions_created,
        }
    }
}

/// Per-model breakdown row for the analytics dashboard.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AnalyticsModelBucket {
    pub model: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_creation_input_tokens: usize,
    /// Spend for the model, in **micro-USD** (1 USD = 1_000_000).
    #[schema(value_type = i64)]
    pub cost_micro_usd: baybo_model::MicroUsd,
    pub call_count: usize,
}

impl From<baybo_query::AnalyticsModelBucket> for AnalyticsModelBucket {
    fn from(v: baybo_query::AnalyticsModelBucket) -> Self {
        Self {
            model: v.model,
            input_tokens: v.input_tokens,
            output_tokens: v.output_tokens,
            cached_input_tokens: v.cached_input_tokens,
            cache_creation_input_tokens: v.cache_creation_input_tokens,
            cost_micro_usd: v.cost_usd,
            call_count: v.call_count,
        }
    }
}

/// Per-reason breakdown row for the analytics dashboard. `reason` is the
/// `CallReason` wire token (`chat`, `compression`, `tool`, …).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AnalyticsReasonBucket {
    pub reason: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_creation_input_tokens: usize,
    /// Spend for the reason, in **micro-USD** (1 USD = 1_000_000).
    #[schema(value_type = i64)]
    pub cost_micro_usd: baybo_model::MicroUsd,
    pub call_count: usize,
}

impl From<baybo_query::AnalyticsReasonBucket> for AnalyticsReasonBucket {
    fn from(v: baybo_query::AnalyticsReasonBucket) -> Self {
        Self {
            reason: v.reason.to_token().into_owned(),
            input_tokens: v.input_tokens,
            output_tokens: v.output_tokens,
            cached_input_tokens: v.cached_input_tokens,
            cache_creation_input_tokens: v.cache_creation_input_tokens,
            cost_micro_usd: v.cost_usd,
            call_count: v.call_count,
        }
    }
}

/// `GET /v1/analytics` response body.
#[derive(Debug, Serialize, ToSchema)]
pub struct AnalyticsResponse {
    /// Inclusive lower bound used for the aggregation (UTC).
    pub since: DateTime<Utc>,
    /// Exclusive upper bound used for the aggregation (UTC).
    pub until: DateTime<Utc>,
    pub total_input_tokens: usize,
    pub total_output_tokens: usize,
    pub total_cached_input_tokens: usize,
    pub total_cache_creation_input_tokens: usize,
    /// Total spend across the window, in **micro-USD** (1 USD = 1_000_000).
    #[schema(value_type = i64)]
    pub total_cost_micro_usd: baybo_model::MicroUsd,
    pub total_record_count: usize,
    pub daily: Vec<AnalyticsDayBucket>,
    pub by_model: Vec<AnalyticsModelBucket>,
    pub by_reason: Vec<AnalyticsReasonBucket>,
}

// ── ToolDefinition ───────────────────────────────────────────────────

/// Mirror of [`baybo_tools::ToolDefinition`].
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    #[schema(value_type = Object)]
    pub parameters_schema: serde_json::Value,
}

impl From<baybo_tools::ToolDefinition> for ToolDefinition {
    fn from(v: baybo_tools::ToolDefinition) -> Self {
        Self {
            name: v.name,
            description: v.description,
            parameters_schema: v.parameters_schema,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `PATCH` body names only what it changes, and the fields it leaves out
    /// have to arrive as unset — not as `Some(default)`, which would blank the
    /// job's title and rewrite its schedule with an empty cron expression. The
    /// `Option`s carry that on their own; nothing here may make them required.
    #[test]
    fn a_patch_body_that_names_one_field_leaves_the_others_unset() {
        let req: UpdateCronRequest = serde_json::from_str(r#"{"prompt": "new prompt"}"#).unwrap();
        let patch: baybo_cron::CronJobPatch = req.into();

        assert_eq!(patch.prompt.as_deref(), Some("new prompt"));
        assert!(patch.title.is_none());
        assert!(patch.schedule.is_none());
        assert!(patch.timezone.is_none());
        assert!(!patch.is_empty());
        assert!(
            !patch.reschedules(),
            "a prompt edit does not move the fire times",
        );
    }

    #[test]
    fn an_empty_patch_body_sets_nothing() {
        let req: UpdateCronRequest = serde_json::from_str("{}").unwrap();
        assert!(baybo_cron::CronJobPatch::from(req).is_empty());
    }

    /// The schedule crosses the wire in the tagged shape `CronJob` reports it
    /// in, so a client patches with the value it was given.
    #[test]
    fn a_patch_body_carries_either_kind_of_schedule() {
        let cron: UpdateCronRequest =
            serde_json::from_str(r#"{"schedule": {"kind": "cron", "expr": "0 8 * * *"}}"#).unwrap();
        let patch = baybo_cron::CronJobPatch::from(cron);
        assert_eq!(
            patch.schedule,
            Some(baybo_cron::CronSchedule::cron("0 8 * * *")),
        );
        assert!(patch.reschedules());

        let at: UpdateCronRequest =
            serde_json::from_str(r#"{"schedule": {"kind": "at", "time": "2026-07-15T08:00:00Z"}}"#)
                .unwrap();
        let patch = baybo_cron::CronJobPatch::from(at);
        assert_eq!(
            patch.schedule,
            Some(baybo_cron::CronSchedule::at(
                "2026-07-15T08:00:00Z".parse().unwrap()
            )),
        );
    }
}
