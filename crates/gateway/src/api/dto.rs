//! Shared request / response DTOs for the v1 API.
//!
//! Every type that appears in an admin-API response lives here and
//! derives [`utoipa::ToSchema`] so `crate::openapi` can emit a spec
//! matching the wire format. Domain types in `aura-model`, `aura-job`,
//! `aura-cron`, `aura-tools` deliberately stay free of utoipa so
//! changes over there don't silently reshape the HTTP surface — every
//! field crossing the API boundary has an explicit DTO mirror plus a
//! `From` conversion, and the drift test picks up any schema change.
//!
//! When a domain type changes:
//!
//! * If the DTO should follow, update the mirror here and its `From`
//!   impl, then run `UPDATE_OPENAPI=1 cargo test -p aura-gateway
//!   --test openapi_spec_sync` + `cd web && npm run gen:api`.
//! * If the DTO should stay fixed (back-compat), the conversion
//!   absorbs the rename/removal here, keeping clients stable.
//!
//! **v1 stability:** the v1 surface is in active development and has
//! no published external consumers yet. Breaking shape changes
//! (e.g. the `JobStatus` `kind`/`reason`/`cancel_reason` envelope
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

/// Admin-surface mirror of [`aura_model::ChannelType`]. Transparent
/// wrapper around a snake_case string so the OpenAPI surface stays
/// stable while the core type is open-ended (runtime-registered
/// sidecars like `"slack"` pass through unchanged).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String)]
pub struct ChannelType(pub String);

impl From<aura_model::ChannelType> for ChannelType {
    fn from(v: aura_model::ChannelType) -> Self {
        Self(v.into_string())
    }
}

impl From<ChannelType> for aura_model::ChannelType {
    fn from(v: ChannelType) -> Self {
        aura_model::ChannelType::from(v.0)
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
    pub jobs_in_flight: usize,
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
    /// JSON value written at `path`. Shape validated by `AuraConfig`.
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

/// `POST /v1/memory` body.
#[derive(Debug, Deserialize, ToSchema)]
pub struct StoreMemoryRequest {
    pub content: String,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub importance: Option<f32>,
}

/// `GET /v1/memory` query params.
#[derive(Debug, Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MemoryListQuery {
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `POST /v1/cron` body. Schedule format is the standard 5-field cron
/// string accepted by [`aura_cron`].
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateCronRequest {
    pub schedule: String,
    pub user_id: String,
    #[serde(default)]
    pub channel: Option<ChannelType>,
    pub text: String,
    /// IANA timezone (e.g. `"Asia/Shanghai"`) used to evaluate the cron
    /// expression and to render time fields in responses. Required —
    /// every time the API speaks is anchored to this zone, so callers
    /// must commit to one explicitly.
    pub timezone: String,
    #[serde(default)]
    pub origin_session_id: Option<String>,
}

// ── MemoryEntry ──────────────────────────────────────────────────────

/// Mirror of [`aura_model::MemoryCategory`]. Serde uses an adjacently
/// tagged shape where unit variants collapse to `{"type":"User"}`;
/// utoipa's derive can't express that, so the schema is hand-written.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum MemoryCategory {
    User,
    Feedback,
    Project,
    Reference,
}

impl utoipa::PartialSchema for MemoryCategory {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::Schema> {
        use utoipa::openapi::{ObjectBuilder, Type, schema::SchemaType};
        ObjectBuilder::new()
            .property(
                "type",
                ObjectBuilder::new()
                    .schema_type(SchemaType::Type(Type::String))
                    .enum_values(Some(vec!["User", "Feedback", "Project", "Reference"])),
            )
            .required("type")
            .build()
            .into()
    }
}

impl utoipa::ToSchema for MemoryCategory {
    fn name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("MemoryCategory")
    }
}

impl From<aura_model::MemoryCategory> for MemoryCategory {
    fn from(v: aura_model::MemoryCategory) -> Self {
        match v {
            aura_model::MemoryCategory::User => Self::User,
            aura_model::MemoryCategory::Feedback => Self::Feedback,
            aura_model::MemoryCategory::Project => Self::Project,
            aura_model::MemoryCategory::Reference => Self::Reference,
        }
    }
}

/// Mirror of [`aura_model::MemoryEntry`].
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MemoryEntry {
    pub id: String,
    pub user_id: String,
    pub content: String,
    pub category: MemoryCategory,
    pub importance: f32,
    pub embedding: Option<Vec<f32>>,
    pub created_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
    pub source_session_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl From<aura_model::MemoryEntry> for MemoryEntry {
    fn from(v: aura_model::MemoryEntry) -> Self {
        Self {
            id: v.id,
            user_id: v.user_id,
            content: v.content,
            category: v.category.into(),
            importance: v.importance,
            embedding: v.embedding,
            created_at: v.created_at,
            last_accessed: v.last_accessed,
            source_session_id: v.source_session_id,
            expires_at: v.expires_at,
        }
    }
}

// ── Job ──────────────────────────────────────────────────────────────

/// Wire mirror of [`aura_job::JobStatus`]. Carries the same payload
/// the domain enum carries (cancel reason, partial-artifact span IDs);
/// the wire shape collapses inner-variant content into `Option`-typed
/// fields so HTTP clients can decode without needing the full Rust
/// enum machinery.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub struct JobStatus {
    pub kind: JobStatusKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cancel_reason: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub partial_artifacts: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobStatusKind {
    Pending,
    InProgress,
    Stuck,
    Cancelled,
    Failed,
    Completed,
}

impl From<aura_job::JobStatusKind> for JobStatusKind {
    fn from(v: aura_job::JobStatusKind) -> Self {
        match v {
            aura_job::JobStatusKind::Pending => Self::Pending,
            aura_job::JobStatusKind::InProgress => Self::InProgress,
            aura_job::JobStatusKind::Stuck => Self::Stuck,
            aura_job::JobStatusKind::Cancelled => Self::Cancelled,
            aura_job::JobStatusKind::Failed => Self::Failed,
            aura_job::JobStatusKind::Completed => Self::Completed,
        }
    }
}

impl From<JobStatusKind> for aura_job::JobStatusKind {
    fn from(v: JobStatusKind) -> Self {
        match v {
            JobStatusKind::Pending => Self::Pending,
            JobStatusKind::InProgress => Self::InProgress,
            JobStatusKind::Stuck => Self::Stuck,
            JobStatusKind::Cancelled => Self::Cancelled,
            JobStatusKind::Failed => Self::Failed,
            JobStatusKind::Completed => Self::Completed,
        }
    }
}

impl From<aura_job::JobStatus> for JobStatus {
    fn from(v: aura_job::JobStatus) -> Self {
        let kind = JobStatusKind::from(v.kind());
        match v {
            aura_job::JobStatus::Pending
            | aura_job::JobStatus::InProgress
            | aura_job::JobStatus::Completed => Self {
                kind,
                reason: None,
                cancel_reason: None,
                partial_artifacts: Vec::new(),
            },
            aura_job::JobStatus::Stuck { reason } | aura_job::JobStatus::Failed { reason } => {
                Self {
                    kind,
                    reason: Some(reason),
                    cancel_reason: None,
                    partial_artifacts: Vec::new(),
                }
            }
            aura_job::JobStatus::Cancelled {
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

/// Wire mirror of [`aura_job::JobKind`].
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    UserChat,
    Cron,
    System,
    Spawned,
}

impl From<aura_job::JobKind> for JobKind {
    fn from(v: aura_job::JobKind) -> Self {
        match v {
            aura_job::JobKind::UserChat => Self::UserChat,
            aura_job::JobKind::Cron => Self::Cron,
            aura_job::JobKind::System => Self::System,
            aura_job::JobKind::Spawned => Self::Spawned,
        }
    }
}

/// Wire mirror of [`aura_model::SystemReason`]. Surfaced on `Job` so
/// frontend can distinguish self_improvement system jobs from
/// history-review system jobs without parsing `JobInput.payload`.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SystemReason {
    HistoryReview,
    SelfImprovement,
}

impl From<aura_model::SystemReason> for SystemReason {
    fn from(v: aura_model::SystemReason) -> Self {
        match v {
            aura_model::SystemReason::HistoryReview => Self::HistoryReview,
            aura_model::SystemReason::SelfImprovement => Self::SelfImprovement,
        }
    }
}

/// Wire mirror of [`aura_job::Job`]. Inner shape reflects the new
/// state machine (Q6) — `final_result` replaces `output`/`error`,
/// `emitted_span_ids` replaces `trace_span_id`.
///
/// `system_reason` is populated when `kind == System`; it lets the
/// trace-page cross-link badge identify a self_improvement child of a
/// user-chat job without exposing the full `JobInput` over the wire.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Job {
    pub id: String,
    pub session_id: String,
    pub parent_job_id: Option<String>,
    pub kind: JobKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_reason: Option<SystemReason>,
    pub status: JobStatus,
    #[schema(value_type = Option<Object>)]
    pub final_result: Option<serde_json::Value>,
    pub emitted_span_ids: Vec<String>,
    pub effective_soul_version: String,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl From<aura_job::Job> for Job {
    fn from(v: aura_job::Job) -> Self {
        let system_reason = match &v.input {
            aura_job::JobInput::System { reason, .. } => Some(reason.clone().into()),
            _ => None,
        };
        Self {
            id: v.id.to_string(),
            session_id: v.session_id.to_string(),
            parent_job_id: v.parent_job_id.map(|p| p.to_string()),
            kind: v.kind.into(),
            system_reason,
            status: v.status.into(),
            final_result: v
                .final_result
                .map(|o| serde_json::to_value(&o).unwrap_or(serde_json::Value::Null)),
            emitted_span_ids: v
                .emitted_span_ids
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
            effective_soul_version: v.effective_soul_version,
            created_at: v.created_at,
            started_at: v.started_at,
            ended_at: v.ended_at,
        }
    }
}

// ── CronJob ──────────────────────────────────────────────────────────

/// Mirror of [`aura_cron::CronStatus`].
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CronStatus {
    Enabled,
    Disabled,
    Executed,
}

impl From<aura_cron::CronStatus> for CronStatus {
    fn from(v: aura_cron::CronStatus) -> Self {
        match v {
            aura_cron::CronStatus::Enabled => Self::Enabled,
            aura_cron::CronStatus::Disabled => Self::Disabled,
            aura_cron::CronStatus::Executed => Self::Executed,
        }
    }
}

/// Mirror of [`aura_cron::CronSchedule`].
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CronSchedule {
    Cron { expr: String },
    At { time: DateTime<Utc> },
}

impl From<aura_cron::CronSchedule> for CronSchedule {
    fn from(v: aura_cron::CronSchedule) -> Self {
        match v {
            aura_cron::CronSchedule::Cron { expr } => Self::Cron { expr },
            aura_cron::CronSchedule::At { time } => Self::At { time },
        }
    }
}

/// Mirror of [`aura_cron::CronJob`].
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CronJob {
    pub id: String,
    pub user_id: String,
    pub channel: ChannelType,
    pub schedule: CronSchedule,
    pub prompt: String,
    pub timezone: String,
    pub status: CronStatus,
    pub last_triggered_at: Option<DateTime<Utc>>,
    pub next_trigger_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_session_id: Option<String>,
}

impl From<aura_cron::CronJob> for CronJob {
    fn from(v: aura_cron::CronJob) -> Self {
        Self {
            id: v.id,
            user_id: v.user_id,
            channel: v.channel.into(),
            schedule: v.schedule.into(),
            prompt: v.prompt,
            timezone: v.timezone,
            status: v.status.into(),
            last_triggered_at: v.last_triggered_at,
            next_trigger_at: v.next_trigger_at,
            created_at: v.created_at,
            updated_at: v.updated_at,
            origin_session_id: v.origin_session_id,
        }
    }
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
    /// `aura_gateway::server`). Rendered as "source" in the UI.
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

/// `GET /v1/traces` query params. All fields are optional; `None`
/// removes that constraint.
#[derive(Debug, Deserialize, Default, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TracesListQuery {
    /// Filter on the latest job's status (snake_case enum).
    #[serde(default)]
    pub status: Option<JobStatusKind>,
    /// Inclusive lower bound on `last_active`.
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    /// Exclusive upper bound on `last_active`.
    #[serde(default)]
    pub until: Option<DateTime<Utc>>,
    /// Case-insensitive substring on session id.
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub offset: Option<usize>,
}

/// One row of the trace browser list view. Mirrors
/// [`aura_agent::SessionSummary`] for the wire.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TraceSessionSummary {
    pub session_id: String,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    /// `None` when the session has no jobs (those rows are filtered
    /// out, but the type stays Option to keep the wire shape stable
    /// if the policy ever flips).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_job_status: Option<JobStatus>,
    pub job_count: usize,
    pub span_count: usize,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_creation_input_tokens: usize,
}

impl From<aura_agent::SessionSummary> for TraceSessionSummary {
    fn from(v: aura_agent::SessionSummary) -> Self {
        Self {
            session_id: v.session_id.to_string(),
            created_at: v.created_at,
            last_active: v.last_active,
            latest_job_status: v.latest_job_status.map(Into::into),
            job_count: v.job_count,
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
    pub cost_micro_usd: aura_model::MicroUsd,
    pub sessions_created: usize,
}

impl From<aura_agent::AnalyticsDayBucket> for AnalyticsDayBucket {
    fn from(v: aura_agent::AnalyticsDayBucket) -> Self {
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
    pub cost_micro_usd: aura_model::MicroUsd,
    pub call_count: usize,
}

impl From<aura_agent::AnalyticsModelBucket> for AnalyticsModelBucket {
    fn from(v: aura_agent::AnalyticsModelBucket) -> Self {
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
    pub total_cost_micro_usd: aura_model::MicroUsd,
    pub total_record_count: usize,
    pub daily: Vec<AnalyticsDayBucket>,
    pub by_model: Vec<AnalyticsModelBucket>,
}

// ── ToolDefinition ───────────────────────────────────────────────────

/// Mirror of [`aura_tools::ToolDefinition`].
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    #[schema(value_type = Object)]
    pub parameters_schema: serde_json::Value,
}

impl From<aura_tools::ToolDefinition> for ToolDefinition {
    fn from(v: aura_tools::ToolDefinition) -> Self {
        Self {
            name: v.name,
            description: v.description,
            parameters_schema: v.parameters_schema,
        }
    }
}
