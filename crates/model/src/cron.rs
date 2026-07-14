use crate::{ChannelType, SessionId};
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

/// Lifecycle state of a cron job row.
///
/// `Executed` is reserved for one-shot (`At`) jobs after their single
/// fire — the row is kept (not deleted) so callers and the web UI can
/// see "this fired" history. Recurring (`Cron`) jobs cycle between
/// `Enabled` and `Disabled` and never enter `Executed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CronStatus {
    Enabled,
    Disabled,
    Executed,
}

/// Storage backwards-compat default for the `timezone` field on
/// `CronJob`. Rows persisted before this field existed deserialize
/// with `"UTC"`, preserving their original behavior. Inputs no longer
/// fall back to this — every entry point requires an explicit
/// `timezone`.
fn default_timezone() -> String {
    "UTC".to_string()
}

impl CronStatus {
    /// Stable wire string. Mirrors `serde(rename_all = "snake_case")` so
    /// the storage row, CLI labels, and tool output stay in lockstep
    /// without three independent match ladders that can drift.
    pub fn as_str(&self) -> &'static str {
        match self {
            CronStatus::Enabled => "enabled",
            CronStatus::Disabled => "disabled",
            CronStatus::Executed => "executed",
        }
    }
}

/// When a cron job fires.
///
/// The variant alone determines recurrence: `Cron` expressions always repeat,
/// `At` timestamps always fire exactly once and then the job self-evicts.
/// There is no separate "run mode" — a recurring single timestamp or a
/// one-shot cron expression has no sensible meaning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CronSchedule {
    /// Standard cron expression (5/6/7-field), fires on every matching tick.
    Cron { expr: String },
    /// Absolute UTC instant, fires exactly once, then the job is deleted.
    At { time: DateTime<Utc> },
}

impl CronSchedule {
    pub fn cron(expr: impl Into<String>) -> Self {
        Self::Cron { expr: expr.into() }
    }

    pub fn at(time: DateTime<Utc>) -> Self {
        Self::At { time }
    }

    /// True when the schedule fires at most once (i.e. `At`).
    pub fn is_one_shot(&self) -> bool {
        matches!(self, Self::At { .. })
    }

    /// Human-friendly single-line rendering, used by CLI/tool output where
    /// the full typed variant is not meaningful to the reader.
    pub fn display(&self) -> String {
        match self {
            Self::Cron { expr } => expr.clone(),
            Self::At { time } => time.to_rfc3339(),
        }
    }
}

// ── CronJob ──────────────────────────────────────────────────────────

/// A persistent cron job definition.
///
/// Bound to `user_id + channel` (not `session_id`) so it survives
/// session expiration. Session is resolved dynamically at trigger time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub user_id: String,
    pub channel: ChannelType,
    /// Short human name for the job, written by the model at creation
    /// (`CronCreate` requires it). Names the fire's conversation
    /// (`{title} · {M/d}`), heads a one-shot's notification, and labels the
    /// job in `baybo cron list` / the admin cron page. Rows persisted before
    /// this field existed deserialize empty; display sites fall back to a
    /// prompt truncation.
    #[serde(default)]
    pub title: String,
    /// When this job fires. See [`CronSchedule`] for the two variants.
    pub schedule: CronSchedule,
    /// Prompt fed through the agent loop on every fire.
    pub prompt: String,
    /// IANA timezone (e.g. `"Asia/Shanghai"`, `"UTC"`) the cron expression
    /// is evaluated in. Has no effect for `At` schedules — those carry an
    /// absolute UTC instant. Old rows without this field deserialize as
    /// `"UTC"` to preserve their existing fire semantics.
    #[serde(default = "default_timezone")]
    pub timezone: String,
    pub status: CronStatus,
    pub last_triggered_at: Option<DateTime<Utc>>,
    /// Pre-computed next fire time for efficient DB queries. `None` means
    /// the job is disabled or its one-shot time has passed.
    pub next_trigger_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Session where this cron job was created (for traceability).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_session_id: Option<SessionId>,
    /// When the user moved this job to the recycle bin. `None` = live.
    ///
    /// Orthogonal to [`Self::status`]: a deleted one-shot that already fired
    /// keeps `Executed`, and restoring it restores exactly the status it had.
    /// A deleted job never fires and is hidden from every listing, but stays
    /// resolvable by id — its execution rows and the conversations they opened
    /// still name a real job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<DateTime<Utc>>,
}

impl CronJob {
    pub fn is_enabled(&self) -> bool {
        self.status == CronStatus::Enabled
    }

    /// True while the job sits in the recycle bin.
    pub fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }

    pub fn is_one_shot(&self) -> bool {
        self.schedule.is_one_shot()
    }

    /// Format a UTC instant as RFC3339 in this job's `timezone`.
    /// Falls back to UTC (with a warn log) if the stored zone string
    /// is unparseable — display must never blow up on a single bad
    /// row, since the row was already accepted at creation time.
    pub fn format_time(&self, dt: DateTime<Utc>) -> String {
        match self.timezone.parse::<Tz>() {
            Ok(tz) => dt.with_timezone(&tz).to_rfc3339(),
            Err(e) => {
                tracing::warn!(
                    job_id = %self.id,
                    timezone = %self.timezone,
                    error = %e,
                    "stored cron job has unparseable timezone; formatting as UTC",
                );
                dt.to_rfc3339()
            }
        }
    }

    /// Convenience for optional timestamps.
    pub fn format_time_opt(&self, dt: Option<DateTime<Utc>>) -> Option<String> {
        dt.map(|t| self.format_time(t))
    }

    /// Display name for this job: its [`Self::title`], falling back to a
    /// truncated prompt for rows created before the field existed.
    pub fn display_title(&self) -> String {
        display_title(&self.title, &self.prompt)
    }
}

/// Longest prompt prefix used as a stand-in title for a legacy (title-less)
/// cron row.
const LEGACY_TITLE_MAX_CHARS: usize = 40;

/// A job's display name: `title` when set, else a truncated `prompt` (legacy
/// rows predate the title field). Shared by every surface that names a job so
/// the fallback can't drift between the CLI, the admin page, and the fire's
/// conversation title.
pub fn display_title(title: &str, prompt: &str) -> String {
    let title = title.trim();
    if !title.is_empty() {
        return title.to_string();
    }
    let prompt = prompt.trim();
    let truncated: String = prompt.chars().take(LEGACY_TITLE_MAX_CHARS).collect();
    if prompt.chars().count() > LEGACY_TITLE_MAX_CHARS {
        format!("{truncated}…")
    } else {
        truncated
    }
}

// ── ExecutionStatus ──────────────────────────────────────────────────

/// Execution lifecycle status for crash recovery and idempotency.
///
/// `Pending` → execution recorded but trigger not yet dispatched.
/// `Dispatched` → trigger successfully sent to the actor.
///
/// On restart, `Pending` executions are re-dispatched (they crashed
/// between record and send). `Dispatched` executions are left to the
/// Job system's `Stuck` recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Pending,
    Dispatched,
}

// ── ExecutionOutcome ─────────────────────────────────────────────────

/// How a fire's turn ended, recorded on the execution when the fire reaches a
/// terminal job state. Drives the notification a one-shot delivers into its
/// origin conversation: every outcome notifies (a scheduled reminder that
/// silently evaporates is this feature's worst failure), only the framing
/// differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    /// The turn completed with a reply — the notification carries it.
    Success,
    /// The turn completed but produced no text (tool-only or empty reply);
    /// the notification carries a fallback line instead.
    Blank,
    /// The turn failed or was cancelled; the notification says so.
    Failed,
}

impl ExecutionOutcome {
    /// Stable wire/db spelling, matching `serde(rename_all = "snake_case")`.
    pub fn as_str(&self) -> &'static str {
        match self {
            ExecutionOutcome::Success => "success",
            ExecutionOutcome::Blank => "blank",
            ExecutionOutcome::Failed => "failed",
        }
    }
}

// ── CronExecution ────────────────────────────────────────────────────

/// An immutable record of a single cron job execution — and, for one-shot
/// jobs, the durable ledger of its result's delivery into the origin
/// conversation. Preserved even after one-shot jobs are evicted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronExecution {
    pub id: String,
    pub job_id: String,
    pub user_id: String,
    pub channel: ChannelType,
    /// The job's title at fire time — snapshotted like `prompt`, so a fire's
    /// notification and conversation stay correctly named even after the job
    /// row is edited or deleted. Empty for rows predating the field (and for
    /// title-less legacy jobs); [`Self::display_title`] falls back to the
    /// prompt.
    #[serde(default)]
    pub title: String,
    /// The job's IANA timezone at fire time. Dates the conversation a recurring
    /// fire opens (`{title} · {M/d}`) in the zone the user scheduled it in.
    /// Empty for rows predating the field; consumers fall back to UTC.
    #[serde(default)]
    pub timezone: String,
    pub schedule: CronSchedule,
    pub prompt: String,
    /// The schedule slot that was due (i.e. the `next_trigger_at` value from the job).
    pub scheduled_fire_time: DateTime<Utc>,
    pub triggered_at: DateTime<Utc>,
    pub status: ExecutionStatus,
    /// The session that originally registered the cron job (if any).
    /// Carried through to the dispatched `CronTriggerEvent` so the
    /// router can set `Lineage` / `TriggerSource` on the resulting
    /// turn — the symmetric counterpart to `create_spawned_session`'s
    /// lineage plumbing for subagents and maintenance.
    #[serde(default)]
    pub origin_session_id: Option<SessionId>,

    // ── Delivery ledger (one-shot result → origin conversation) ──
    /// The isolated session this fire ran in. Stamped when the fire's turn
    /// reaches a terminal state; the notification's content is read from this
    /// session's reply row.
    #[serde(default)]
    pub fire_session_id: Option<SessionId>,
    /// When the fire's turn reached a terminal job state.
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    /// How the turn ended. `None` until the fire completes.
    #[serde(default)]
    pub outcome: Option<ExecutionOutcome>,
    /// `session_messages.ordinal` of the fire's reply row in
    /// `fire_session_id`, taken off the `Completed` lifecycle edge — the
    /// notification reads exactly that row (no read-after-write poll).
    #[serde(default)]
    pub reply_ordinal: Option<i64>,
    /// When this execution's delivery was **resolved**: either the result was
    /// appended to the origin conversation, or it was terminally dropped
    /// (no usable origin — see the router's fallbacks). Both are resolutions,
    /// so the boot re-drive scan (`completed_at` set, `notified_at` unset)
    /// converges instead of re-attempting a hopeless delivery on every boot.
    #[serde(default)]
    pub notified_at: Option<DateTime<Utc>>,
}

impl CronExecution {
    /// A freshly-recorded execution for `job`'s due slot, `Pending` (not yet
    /// dispatched) and with an empty delivery ledger — the ledger fields are
    /// stamped later, as the fire completes and its result is delivered.
    pub fn pending(
        job: &CronJob,
        scheduled_fire_time: DateTime<Utc>,
        triggered_at: DateTime<Utc>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            job_id: job.id.clone(),
            user_id: job.user_id.clone(),
            channel: job.channel.clone(),
            title: job.title.clone(),
            timezone: job.timezone.clone(),
            schedule: job.schedule.clone(),
            prompt: job.prompt.clone(),
            scheduled_fire_time,
            triggered_at,
            status: ExecutionStatus::Pending,
            origin_session_id: job.origin_session_id.clone(),
            fire_session_id: None,
            completed_at: None,
            outcome: None,
            reply_ordinal: None,
            notified_at: None,
        }
    }

    /// True once the fire's turn reached a terminal state but its result has
    /// not yet been delivered to (or terminally dropped by) the origin
    /// conversation — the boot re-drive's scan predicate.
    pub fn awaits_delivery(&self) -> bool {
        self.completed_at.is_some() && self.notified_at.is_none()
    }

    /// True when this execution ran a one-shot job, i.e. its result belongs in
    /// the origin conversation rather than in its own.
    pub fn is_one_shot(&self) -> bool {
        self.schedule.is_one_shot()
    }

    /// Display name for the fire: the snapshotted [`Self::title`], falling
    /// back to a truncated prompt for legacy rows.
    pub fn display_title(&self) -> String {
        display_title(&self.title, &self.prompt)
    }
}

// ── PendingCronResult ────────────────────────────────────────────────

/// A finished one-shot fire, handed to the **origin** conversation's actor
/// (`AgentMessage::CronResultReady`) for zero-inference delivery: the actor
/// appends the framed result as an assistant row, dispatches it, and stamps
/// the execution's `notified_at`.
///
/// Built by the cron waiter from the fire's terminal lifecycle event, and
/// rebuilt verbatim by the boot re-drive from the persisted [`CronExecution`]
/// — so both paths deliver identical content. `execution_id` is the
/// source of the transcript row's durable idempotency key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingCronResult {
    pub execution_id: String,
    pub cron_job_id: String,
    /// The job's display title, used in the notification header.
    pub job_title: String,
    /// Session the fire ran in; its reply row at `reply_ordinal` carries the
    /// content to deliver.
    pub fire_session_id: SessionId,
    pub reply_ordinal: Option<i64>,
    pub outcome: ExecutionOutcome,
    /// Why the fire failed, when `outcome` is [`ExecutionOutcome::Failed`].
    pub failure_reason: Option<String>,
    pub completed_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job_with_tz(tz: &str) -> CronJob {
        CronJob {
            id: "cj-fmt".to_string(),
            user_id: "u-1".to_string(),
            channel: ChannelType::tui(),
            title: "fmt job".to_string(),
            schedule: CronSchedule::cron("0 9 * * *"),
            prompt: "fmt".to_string(),
            timezone: tz.to_string(),
            status: CronStatus::Enabled,
            last_triggered_at: None,
            next_trigger_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            origin_session_id: None,
            deleted_at: None,
        }
    }

    #[test]
    fn format_time_renders_with_offset() {
        let dt: DateTime<Utc> = "2026-05-05T09:30:00Z".parse().unwrap();
        assert_eq!(
            job_with_tz("Asia/Shanghai").format_time(dt),
            "2026-05-05T17:30:00+08:00"
        );
    }

    #[test]
    fn format_time_falls_back_to_utc_on_bad_zone() {
        let dt: DateTime<Utc> = "2026-05-05T09:30:00Z".parse().unwrap();
        assert_eq!(
            job_with_tz("Mars/Olympus").format_time(dt),
            "2026-05-05T09:30:00+00:00"
        );
    }

    #[test]
    fn format_time_opt_passes_through_none() {
        assert!(job_with_tz("UTC").format_time_opt(None).is_none());
    }

    #[test]
    fn serde_round_trip_cron() {
        let job = CronJob {
            id: "cj-1".to_string(),
            user_id: "u-1".to_string(),
            channel: ChannelType::tui(),
            title: "morning news".to_string(),
            schedule: CronSchedule::cron("0 9 * * *"),
            prompt: "push news".to_string(),
            timezone: "Asia/Shanghai".to_string(),
            status: CronStatus::Enabled,
            last_triggered_at: None,
            next_trigger_at: Some(Utc::now()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            origin_session_id: Some(SessionId::from("sess-1")),
            deleted_at: None,
        };
        let json = serde_json::to_string(&job).unwrap();
        let restored: CronJob = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.id, "cj-1");
        assert_eq!(restored.status, CronStatus::Enabled);
        assert_eq!(restored.timezone, "Asia/Shanghai");
        assert!(!restored.is_one_shot());
        assert!(restored.is_enabled());
        assert!(!restored.is_deleted());
        assert_eq!(
            restored.origin_session_id.as_ref().map(|s| s.as_str()),
            Some("sess-1"),
        );
    }

    #[test]
    fn serde_round_trip_at() {
        let fire_at = Utc::now();
        let job = CronJob {
            id: "cj-at".to_string(),
            user_id: "u-1".to_string(),
            channel: ChannelType::tui(),
            title: "one shot".to_string(),
            schedule: CronSchedule::at(fire_at),
            prompt: "one shot".to_string(),
            timezone: "UTC".to_string(),
            status: CronStatus::Enabled,
            last_triggered_at: None,
            next_trigger_at: Some(fire_at),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            origin_session_id: None,
            deleted_at: None,
        };
        let json = serde_json::to_string(&job).unwrap();
        let restored: CronJob = serde_json::from_str(&json).unwrap();
        assert!(restored.is_one_shot());
        match restored.schedule {
            CronSchedule::At { time } => assert_eq!(time, fire_at),
            other => panic!("expected At, got {other:?}"),
        }
    }

    #[test]
    fn legacy_row_without_timezone_defaults_to_utc() {
        // Older rows persisted before the field existed must round-trip
        // with `UTC` so their fire semantics don't silently change.
        let json = r#"{
            "id":"cj-old","user_id":"u-1","channel":"tui",
            "schedule":{"kind":"cron","expr":"0 9 * * *"},
            "prompt":"x",
            "status":"enabled","next_trigger_at":null,
            "created_at":"2025-01-01T00:00:00Z","updated_at":"2025-01-01T00:00:00Z"
        }"#;
        let restored: CronJob = serde_json::from_str(json).unwrap();
        assert_eq!(restored.timezone, "UTC");
        assert!(
            !restored.is_deleted(),
            "a stored row is live unless stamped"
        );
    }

    #[test]
    fn deleted_at_round_trips_and_is_orthogonal_to_status() {
        let deleted_at: DateTime<Utc> = "2026-07-01T08:00:00Z".parse().unwrap();
        let mut job = job_with_tz("UTC");
        job.status = CronStatus::Executed;
        job.deleted_at = Some(deleted_at);

        let restored: CronJob =
            serde_json::from_str(&serde_json::to_string(&job).unwrap()).unwrap();
        assert!(restored.is_deleted());
        assert_eq!(restored.deleted_at, Some(deleted_at));
        assert_eq!(
            restored.status,
            CronStatus::Executed,
            "a fired one-shot keeps its status in the recycle bin"
        );
    }

    #[test]
    fn status_serde() {
        assert_eq!(
            serde_json::to_string(&CronStatus::Enabled).unwrap(),
            "\"enabled\""
        );
        assert_eq!(
            serde_json::to_string(&CronStatus::Disabled).unwrap(),
            "\"disabled\""
        );
        assert_eq!(
            serde_json::to_string(&CronStatus::Executed).unwrap(),
            "\"executed\""
        );
    }

    #[test]
    fn schedule_display() {
        assert_eq!(CronSchedule::cron("0 9 * * *").display(), "0 9 * * *");
        let ts: DateTime<Utc> = "2026-04-17T14:25:00Z".parse().unwrap();
        assert_eq!(CronSchedule::at(ts).display(), "2026-04-17T14:25:00+00:00");
    }

    #[test]
    fn execution_serde_round_trip() {
        let mut exec = CronExecution::pending(&job_with_tz("UTC"), Utc::now(), Utc::now());
        exec.origin_session_id = Some("sess-cron".into());
        exec.fire_session_id = Some("cron-abc".into());
        exec.completed_at = Some(Utc::now());
        exec.outcome = Some(ExecutionOutcome::Success);
        exec.reply_ordinal = Some(7);

        let json = serde_json::to_string(&exec).unwrap();
        let restored: CronExecution = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.job_id, "cj-fmt");
        assert_eq!(restored.status, ExecutionStatus::Pending);
        assert_eq!(restored.outcome, Some(ExecutionOutcome::Success));
        assert_eq!(restored.reply_ordinal, Some(7));
        assert_eq!(
            restored.fire_session_id.as_ref().map(|s| s.as_str()),
            Some("cron-abc")
        );
        // Completed but never notified — the boot re-drive's scan predicate.
        assert!(restored.awaits_delivery());
    }

    #[test]
    fn legacy_execution_row_deserializes_with_empty_delivery_ledger() {
        // Rows persisted before the ledger existed must load, and must not
        // look like they are awaiting delivery (no `completed_at`).
        let json = r#"{
            "id":"ce-old","job_id":"cj-old","user_id":"u-1","channel":"tui",
            "schedule":{"kind":"at","time":"2025-01-01T00:00:00Z"},
            "prompt":"x",
            "scheduled_fire_time":"2025-01-01T00:00:00Z",
            "triggered_at":"2025-01-01T00:00:00Z",
            "status":"dispatched"
        }"#;
        let restored: CronExecution = serde_json::from_str(json).unwrap();
        assert!(restored.fire_session_id.is_none());
        assert!(restored.outcome.is_none());
        assert!(restored.notified_at.is_none());
        assert!(!restored.awaits_delivery());
    }

    #[test]
    fn execution_status_serde() {
        assert_eq!(
            serde_json::to_string(&ExecutionStatus::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutionStatus::Dispatched).unwrap(),
            "\"dispatched\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutionOutcome::Blank).unwrap(),
            "\"blank\""
        );
    }

    #[test]
    fn display_title_falls_back_to_truncated_prompt() {
        let mut job = job_with_tz("UTC");
        assert_eq!(job.display_title(), "fmt job");

        job.title = String::new();
        job.prompt = "short prompt".to_string();
        assert_eq!(job.display_title(), "short prompt");

        job.prompt = "x".repeat(60);
        let fallback = job.display_title();
        assert!(fallback.ends_with('…'), "{fallback}");
        assert_eq!(fallback.chars().count(), LEGACY_TITLE_MAX_CHARS + 1);
    }
}
