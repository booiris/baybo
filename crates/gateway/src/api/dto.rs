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
//! Channel-side routes reuse [`ListResponse`] too; the generic
//! `ToSchema` impl only applies when `T: ToSchema`, so a channel route
//! using `ListResponse<Session>` still compiles even though `Session`
//! stays outside the OpenAPI surface.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Envelope for list endpoints. Lets us add `next_cursor`, `total`,
/// etc. later without breaking clients that parse `items`.
#[derive(Debug, Serialize, ToSchema)]
pub struct ListResponse<T> {
    pub items: Vec<T>,
}

impl<T> ListResponse<T> {
    pub fn new(items: Vec<T>) -> Self {
        Self { items }
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
    #[serde(default)]
    pub origin_session_id: Option<String>,
}

// ── MemoryEntry ──────────────────────────────────────────────────────

/// Mirror of [`aura_model::MemoryCategory`]. Serde uses an adjacently
/// tagged shape where unit variants collapse to `{"type":"KeyFact"}`;
/// utoipa's derive can't express that, so the schema is hand-written.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum MemoryCategory {
    UserPreference,
    KeyFact,
}

impl utoipa::PartialSchema for MemoryCategory {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::Schema> {
        use utoipa::openapi::{ObjectBuilder, Type, schema::SchemaType};
        ObjectBuilder::new()
            .property(
                "type",
                ObjectBuilder::new()
                    .schema_type(SchemaType::Type(Type::String))
                    .enum_values(Some(vec!["UserPreference", "KeyFact"])),
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
            aura_model::MemoryCategory::UserPreference => Self::UserPreference,
            aura_model::MemoryCategory::KeyFact => Self::KeyFact,
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

/// Mirror of [`aura_job::JobStatus`].
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    InProgress,
    Completed,
    Submitted,
    Accepted,
    Failed,
    Stuck,
    Cancelled,
    Abandoned,
}

impl From<aura_job::JobStatus> for JobStatus {
    fn from(v: aura_job::JobStatus) -> Self {
        match v {
            aura_job::JobStatus::Pending => Self::Pending,
            aura_job::JobStatus::InProgress => Self::InProgress,
            aura_job::JobStatus::Completed => Self::Completed,
            aura_job::JobStatus::Submitted => Self::Submitted,
            aura_job::JobStatus::Accepted => Self::Accepted,
            aura_job::JobStatus::Failed => Self::Failed,
            aura_job::JobStatus::Stuck => Self::Stuck,
            aura_job::JobStatus::Cancelled => Self::Cancelled,
            aura_job::JobStatus::Abandoned => Self::Abandoned,
        }
    }
}

/// Mirror of [`aura_job::OperationKind`].
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationKind {
    LlmCall {
        model: String,
    },
    ToolExecution {
        tool_name: String,
    },
    SkillExecution {
        skill_name: String,
    },
    CronExecution {
        cron_job_id: String,
    },
    ContextCompression {
        strategy: String,
    },
    MemoryOperation {
        operation: String,
    },
    UserMessageHandling {
        session_id: String,
    },
    SubAgentSpawn {
        child_session_id: String,
        child_job_id: String,
    },
    Acceptance {
        from: JobStatus,
        to: JobStatus,
    },
}

impl From<aura_job::OperationKind> for OperationKind {
    fn from(v: aura_job::OperationKind) -> Self {
        match v {
            aura_job::OperationKind::LlmCall { model } => Self::LlmCall { model },
            aura_job::OperationKind::ToolExecution { tool_name } => {
                Self::ToolExecution { tool_name }
            }
            aura_job::OperationKind::SkillExecution { skill_name } => {
                Self::SkillExecution { skill_name }
            }
            aura_job::OperationKind::CronExecution { cron_job_id } => {
                Self::CronExecution { cron_job_id }
            }
            aura_job::OperationKind::ContextCompression { strategy } => {
                Self::ContextCompression { strategy }
            }
            aura_job::OperationKind::MemoryOperation { operation } => {
                Self::MemoryOperation { operation }
            }
            aura_job::OperationKind::UserMessageHandling { session_id } => {
                Self::UserMessageHandling { session_id }
            }
            aura_job::OperationKind::SubAgentSpawn {
                child_session_id,
                child_job_id,
            } => Self::SubAgentSpawn {
                child_session_id,
                child_job_id,
            },
            aura_job::OperationKind::Acceptance { from, to } => Self::Acceptance {
                from: from.into(),
                to: to.into(),
            },
        }
    }
}

/// Mirror of [`aura_job::Job`].
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Job {
    pub id: String,
    pub session_id: String,
    pub parent_job_id: Option<String>,
    pub kind: OperationKind,
    pub status: JobStatus,
    #[schema(value_type = Option<Object>)]
    pub input: Option<serde_json::Value>,
    #[schema(value_type = Option<Object>)]
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
    pub trace_span_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    /// Acceptance policy discriminator: `"auto"`, `"auto_submit"`,
    /// or `"manual"`. Per-policy detail (the chosen `Acceptor`) is
    /// not surfaced in v1 to keep the public schema small.
    pub acceptance: String,
    /// Recovery policy discriminator: `"auto_resume"`, `"manual"`,
    /// `"abandon"`. Same reasoning as `acceptance` for the lack of
    /// nested detail.
    pub recovery: String,
    pub submitted_at: Option<DateTime<Utc>>,
    pub accepted_at: Option<DateTime<Utc>>,
    pub recovery_attempts: u32,
}

impl From<aura_job::Job> for Job {
    fn from(v: aura_job::Job) -> Self {
        let acceptance = match &v.acceptance {
            aura_job::AcceptancePolicy::Auto => "auto",
            aura_job::AcceptancePolicy::AutoSubmit { .. } => "auto_submit",
            aura_job::AcceptancePolicy::Manual { .. } => "manual",
        };
        let recovery = match &v.recovery {
            aura_job::RecoveryPolicy::AutoResume { .. } => "auto_resume",
            aura_job::RecoveryPolicy::Manual => "manual",
            aura_job::RecoveryPolicy::Abandon => "abandon",
        };
        Self {
            id: v.id,
            session_id: v.session_id,
            parent_job_id: v.parent_job_id,
            kind: v.kind.into(),
            status: v.status.into(),
            input: v.input,
            output: v.output,
            error: v.error,
            trace_span_id: v.trace_span_id,
            created_at: v.created_at,
            started_at: v.started_at,
            completed_at: v.completed_at,
            acceptance: acceptance.to_owned(),
            recovery: recovery.to_owned(),
            submitted_at: v.submitted_at,
            accepted_at: v.accepted_at,
            recovery_attempts: v.recovery_attempts,
        }
    }
}

// ── Session ──────────────────────────────────────────────────────────

/// Mirror of [`aura_model::SessionTrigger`]. Discriminator-only —
/// per-variant payload (cron job id, system kind extras) lives on
/// [`SessionDetail`] adjacent fields when relevant.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionTrigger {
    User,
    Cron,
    System,
    Parent,
}

impl From<&aura_model::SessionTrigger> for SessionTrigger {
    fn from(v: &aura_model::SessionTrigger) -> Self {
        match v {
            aura_model::SessionTrigger::User => Self::User,
            aura_model::SessionTrigger::Cron { .. } => Self::Cron,
            aura_model::SessionTrigger::System { .. } => Self::System,
            aura_model::SessionTrigger::Parent { .. } => Self::Parent,
        }
    }
}

/// Mirror of [`aura_model::ParentLinkKind`].
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ParentLinkKind {
    SubAgent,
    Fork,
    CronChain,
    SystemContinuation,
}

impl From<aura_model::ParentLinkKind> for ParentLinkKind {
    fn from(v: aura_model::ParentLinkKind) -> Self {
        match v {
            aura_model::ParentLinkKind::SubAgent => Self::SubAgent,
            aura_model::ParentLinkKind::Fork => Self::Fork,
            aura_model::ParentLinkKind::CronChain => Self::CronChain,
            aura_model::ParentLinkKind::SystemContinuation => Self::SystemContinuation,
        }
    }
}

/// Cross-session reference; mirror of [`aura_model::SessionParentLink`].
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SessionParentLink {
    pub session_id: String,
    pub kind: ParentLinkKind,
    pub at_job_id: String,
    pub at_span_id: Option<String>,
}

impl From<&aura_model::SessionParentLink> for SessionParentLink {
    fn from(v: &aura_model::SessionParentLink) -> Self {
        Self {
            session_id: v.session_id.clone(),
            kind: v.kind.into(),
            at_job_id: v.at_job_id.clone(),
            at_span_id: v.at_span_id.clone(),
        }
    }
}

/// List-form `Session`. Excludes the message transcript so list
/// responses stay small.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SessionSummary {
    pub id: String,
    pub user_id: String,
    pub channel: ChannelType,
    pub trigger: SessionTrigger,
    pub parent_link: Option<SessionParentLink>,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub message_count: usize,
}

impl From<&aura_model::Session> for SessionSummary {
    fn from(v: &aura_model::Session) -> Self {
        Self {
            id: v.id.clone(),
            user_id: v.user.id.clone(),
            channel: v.channel.clone().into(),
            trigger: (&v.trigger).into(),
            parent_link: v.parent_link.as_ref().map(Into::into),
            created_at: v.created_at,
            last_active: v.last_active,
            message_count: v.messages.len(),
        }
    }
}

/// Detail-form `Session`. Metadata-only on the admin surface — the
/// message transcript is intentionally omitted so a leaked admin
/// token can't pull chat content. Use the trace export endpoint
/// (`GET /v1/traces/{session_id}`) to pull the full call chain
/// instead.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub summary: SessionSummary,
    /// `SessionState.active_skills` snapshot.
    pub active_skills: Vec<String>,
    /// Number of context-compression passes performed in the session.
    pub compression_count: u32,
}

impl From<aura_model::Session> for SessionDetail {
    fn from(v: aura_model::Session) -> Self {
        let summary = SessionSummary::from(&v);
        Self {
            summary,
            active_skills: v.state.active_skills.clone(),
            compression_count: v.state.compression_count,
        }
    }
}

/// Body for `POST /v1/sessions/{id}/fork`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ForkSessionRequest {
    pub at_job_id: String,
}

// ── CronJob ──────────────────────────────────────────────────────────

/// Mirror of [`aura_cron::CronStatus`].
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CronStatus {
    Enabled,
    Disabled,
}

impl From<aura_cron::CronStatus> for CronStatus {
    fn from(v: aura_cron::CronStatus) -> Self {
        match v {
            aura_cron::CronStatus::Enabled => Self::Enabled,
            aura_cron::CronStatus::Disabled => Self::Disabled,
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

/// Mirror of [`aura_model::HostPattern`].
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum HostPattern {
    Exact(String),
    Wildcard(String),
}

impl From<aura_model::HostPattern> for HostPattern {
    fn from(v: aura_model::HostPattern) -> Self {
        match v {
            aura_model::HostPattern::Exact(h) => Self::Exact(h),
            aura_model::HostPattern::Wildcard(h) => Self::Wildcard(h),
        }
    }
}

/// Mirror of [`aura_model::ApprovedResource`]. Paths serialize as
/// strings on the wire.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovedResource {
    ReadFile {
        #[schema(value_type = String)]
        path: PathBuf,
    },
    WriteFile {
        #[schema(value_type = String)]
        path: PathBuf,
    },
    Http {
        host: HostPattern,
    },
    ExecCommand {
        command: String,
    },
}

impl From<aura_model::ApprovedResource> for ApprovedResource {
    fn from(v: aura_model::ApprovedResource) -> Self {
        match v {
            aura_model::ApprovedResource::ReadFile { path } => Self::ReadFile { path },
            aura_model::ApprovedResource::WriteFile { path } => Self::WriteFile { path },
            aura_model::ApprovedResource::Http { host } => Self::Http { host: host.into() },
            aura_model::ApprovedResource::ExecCommand { command } => Self::ExecCommand { command },
        }
    }
}

/// Mirror of [`aura_cron::TriggerAction`].
#[derive(Debug, Clone, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerAction {
    Prompt {
        prompt: String,
    },
    ToolCall {
        tool_name: String,
        #[schema(value_type = Object)]
        params: serde_json::Value,
        approved_resources: Vec<ApprovedResource>,
    },
}

impl From<aura_cron::TriggerAction> for TriggerAction {
    fn from(v: aura_cron::TriggerAction) -> Self {
        match v {
            aura_cron::TriggerAction::Prompt { prompt } => Self::Prompt { prompt },
            aura_cron::TriggerAction::ToolCall {
                tool_name,
                params,
                approved_resources,
            } => Self::ToolCall {
                tool_name,
                params,
                approved_resources: approved_resources.into_iter().map(Into::into).collect(),
            },
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
    pub action: TriggerAction,
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
            action: v.action.into(),
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
