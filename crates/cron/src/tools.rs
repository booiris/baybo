//! LLM-facing tools for managing cron jobs. Require an `Arc<CronScheduler>`
//! to manipulate the global scheduler state.

use std::sync::Arc;

use crate::{CronSchedule, CronScheduler, NewCronJob};
use async_trait::async_trait;
use baybo_model::SessionId;
use baybo_tools::{Tool, ToolConcurrency, ToolContext, ToolError, ToolManifest, ToolOutput};
use chrono::{DateTime, LocalResult, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{Value, json};

/// Parse an `at` parameter accepting either RFC3339 with offset
/// (`2026-04-17T14:25:00Z` or `2026-04-17T22:25:00+08:00`) or a naive
/// ISO-8601 datetime (`2026-04-17T14:25:00`, optionally space-separated)
/// interpreted in `tz_name`. Naive ambiguous times (DST fall-back) take
/// the earlier offset; nonexistent times (DST spring-forward gap) are
/// rejected.
fn parse_at_in_tz(at: &str, tz_name: &str) -> Result<DateTime<Utc>, ToolError> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(at) {
        return Ok(dt.with_timezone(&Utc));
    }
    let tz = parse_tz(tz_name)?;
    let naive = NaiveDateTime::parse_from_str(at, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M:%S"))
        .map_err(|e| {
            ToolError::InvalidParams(format!(
                "`at` must be RFC3339 with offset or naive `YYYY-MM-DDTHH:MM:SS`: {e}"
            ))
        })?;
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(dt) => Ok(dt.with_timezone(&Utc)),
        LocalResult::Ambiguous(earliest, _latest) => Ok(earliest.with_timezone(&Utc)),
        LocalResult::None => Err(ToolError::InvalidParams(format!(
            "`at` time {at} does not exist in timezone {tz_name} (DST gap)"
        ))),
    }
}

fn parse_tz(name: &str) -> Result<Tz, ToolError> {
    name.parse::<Tz>()
        .map_err(|e| ToolError::InvalidParams(format!("invalid timezone {name}: {e}")))
}

/// Build every cron tool with its manifest, ready to be registered with a
/// `ToolRegistry`. Always `Trusted` with no capabilities — they operate on
/// agent-internal scheduler state, not on the host filesystem or network.
pub fn agent_tools(scheduler: Arc<CronScheduler>) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(CronCreateTool::new(Arc::clone(&scheduler))),
        Arc::new(CronDeleteTool::new(Arc::clone(&scheduler))),
        Arc::new(CronPauseTool::new(Arc::clone(&scheduler))),
        Arc::new(CronResumeTool::new(Arc::clone(&scheduler))),
        Arc::new(CronListTool::new(scheduler)),
    ];

    tools.into_iter().map(with_manifest).collect()
}

fn with_manifest(tool: Arc<dyn Tool>) -> (Arc<dyn Tool>, ToolManifest) {
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description(),
        trust_level: baybo_model::TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: vec![],
    };
    (tool, manifest)
}

// ---------------------------------------------------------------------------
// CronCreate
// ---------------------------------------------------------------------------

struct CronCreateTool {
    scheduler: Arc<CronScheduler>,
}

impl CronCreateTool {
    fn new(scheduler: Arc<CronScheduler>) -> Self {
        Self { scheduler }
    }
}

#[derive(Debug, Deserialize)]
struct CreateParams {
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    at: Option<String>,
    title: String,
    prompt: String,
    timezone: String,
}

#[async_trait]
impl Tool for CronCreateTool {
    fn name(&self) -> &str {
        "CronCreate"
    }

    fn description(&self) -> String {
        r#"Schedule a job whose `prompt` is run as a task on a timer. `title`, `timezone` and `prompt` are required: on every fire the agent executes `prompt` in a fresh session as an instruction to carry out — NOT as a message from the user — and all times in inputs and outputs are anchored to `timezone`. Supply exactly one of `schedule` (recurring cron expression, e.g. "0 9 * * *") or `at` (one-shot timestamp); an `at` job fires once and then stops, staying in the list as `executed`. Write `prompt` as a self-contained task instruction (see its description) so the fire does the right thing. A recurring fire opens its own conversation named after `title`; a one-shot fire reports its result back into THIS conversation."#
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "title": {
                    "type": "string",
                    "description": "Short human name for the job (a few words, in the user's language), e.g. \"Standup reminder\". Names the conversation a recurring fire opens and heads the result a one-shot reports back. Describe the task, not the schedule."
                },
                "timezone": {
                    "type": "string",
                    "description": "IANA timezone (e.g. \"Asia/Shanghai\", \"UTC\") used to evaluate `schedule`, interpret naive `at`, and render `next_trigger_at` in the output."
                },
                "prompt": {
                    "type": "string",
                    "description": r#"The instruction to execute when the job fires. Each fire runs in a fresh session with NO memory of this conversation, and the text is handed to the agent as a task to perform — NOT as a message from the user. Write it as a self-contained, imperative instruction describing what to DO, and inline every detail the task needs (names, topics, recipients, output format), since nothing from the current chat carries over. When the user wants something said or sent, phrase it as an action rather than the bare words: e.g. for "say hi to me in a minute" use "Send the user a greeting: hi" — NOT "hi" on its own, which the fire would misread as the user greeting the agent."#
                },
                "schedule": {
                    "type": "string",
                    "description": "Recurring cron expression evaluated in `timezone`. Supply exactly one of `schedule` or `at`."
                },
                "at": {
                    "type": "string",
                    "description": "One-shot timestamp; fires once, then the job stops and stays in the list as `executed`. Either RFC3339 with offset (e.g. \"2026-04-17T14:25:00Z\" or \"2026-04-17T22:25:00+08:00\") or a naive `YYYY-MM-DDTHH:MM:SS` interpreted in `timezone`. Supply exactly one of `schedule` or `at`."
                }
            },
            "required": ["title", "timezone", "prompt"]
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("title")
            .or_else(|| params.get("prompt"))
            .and_then(Value::as_str)
            .and_then(baybo_tools::progress::preview_arg)
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p: CreateParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        let timezone = p.timezone;

        let schedule = p.schedule.filter(|s| !s.trim().is_empty());
        let at = p.at.filter(|s| !s.trim().is_empty());

        let schedule = match (schedule, at) {
            (Some(_), Some(_)) => {
                return Err(ToolError::InvalidParams(
                    "`schedule` and `at` are mutually exclusive".into(),
                ));
            }
            (Some(expr), None) => CronSchedule::cron(expr),
            (None, Some(at)) => CronSchedule::at(parse_at_in_tz(&at, &timezone)?),
            (None, None) => {
                return Err(ToolError::InvalidParams(
                    "either `schedule` or `at` is required".into(),
                ));
            }
        };

        let job = self
            .scheduler
            .create_job(NewCronJob {
                user_id: ctx.user.id.clone(),
                channel: ctx.user.channel.clone(),
                title: p.title,
                schedule,
                prompt: p.prompt,
                timezone,
                origin_session_id: Some(origin_session(ctx)),
            })
            .await
            .map_err(|e| ToolError::Execution(format!("{e}")))?;

        Ok(ToolOutput::Json(json!({
            "id": job.id,
            "title": job.title,
            "schedule": job.schedule.display(),
            "timezone": job.timezone,
            "next_trigger_at": job.format_time_opt(job.next_trigger_at),
        })))
    }
}

/// The conversation a job created from this call belongs to — where a one-shot
/// fire will report its result.
///
/// Almost always the calling session: it is the conversation the request came
/// from, so it is where the answer belongs. The one exception is a job created
/// from inside a **one-shot fire's session**, which is a private workspace the
/// user cannot see or open — a result delivered there would reach nobody. Such
/// a job inherits the fire's own origin instead, collapsing the chain onto the
/// real conversation. (A fire with no origin of its own yields its own id,
/// which the delivery path then recognises as unusable and drops.)
///
/// A **recurring** fire's session is NOT an exception: it is a listed,
/// replyable conversation, so a job scheduled there — by the fire itself, or by
/// a user replying in it — reports back there, like any other conversation.
fn origin_session(ctx: &ToolContext) -> SessionId {
    if ctx.session_trigger.is_cron_conversation() {
        return ctx.session_id.clone();
    }
    ctx.session_trigger
        .cron_origin_session_id()
        .cloned()
        .unwrap_or_else(|| ctx.session_id.clone())
}

// ---------------------------------------------------------------------------
// CronDelete
// ---------------------------------------------------------------------------

struct CronDeleteTool {
    scheduler: Arc<CronScheduler>,
}

impl CronDeleteTool {
    fn new(scheduler: Arc<CronScheduler>) -> Self {
        Self { scheduler }
    }
}

/// The parameters of every tool that acts on one existing job.
#[derive(Debug, Deserialize)]
struct JobIdParams {
    id: String,
}

impl JobIdParams {
    fn parse(params: Value) -> Result<Self, ToolError> {
        serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))
    }
}

/// Schema shared by the single-job tools: one required `id`, described in the
/// terms of the acting verb.
fn job_id_schema(description: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string", "description": description }
        },
        "required": ["id"]
    })
}

fn job_id_progress_label(params: &Value) -> Option<String> {
    params
        .get("id")
        .and_then(Value::as_str)
        .and_then(baybo_tools::progress::preview_arg)
}

#[async_trait]
impl Tool for CronDeleteTool {
    fn name(&self) -> &str {
        "CronDelete"
    }

    fn description(&self) -> String {
        "Cancel a scheduled cron job by its ID. It stops firing at once and \
         leaves the list, but is recoverable — the user can restore it from the \
         recycle bin. To stop a job only temporarily, use CronPause instead."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        job_id_schema("Cron job ID to delete")
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        job_id_progress_label(params)
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p = JobIdParams::parse(params)?;

        self.scheduler
            .delete_job(&p.id)
            .await
            .map_err(|e| ToolError::Execution(format!("{e}")))?;

        Ok(ToolOutput::Json(json!({ "deleted": p.id })))
    }
}

// ---------------------------------------------------------------------------
// CronPause
// ---------------------------------------------------------------------------

struct CronPauseTool {
    scheduler: Arc<CronScheduler>,
}

impl CronPauseTool {
    fn new(scheduler: Arc<CronScheduler>) -> Self {
        Self { scheduler }
    }
}

#[async_trait]
impl Tool for CronPauseTool {
    fn name(&self) -> &str {
        "CronPause"
    }

    fn description(&self) -> String {
        "Pause a scheduled cron job by its ID: it keeps its place in the list \
         but stops firing until CronResume. Use this when the user wants a job \
         to stop for now; use CronDelete when they want it gone."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        job_id_schema("Cron job ID to pause")
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        job_id_progress_label(params)
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p = JobIdParams::parse(params)?;

        self.scheduler
            .disable_job(&p.id)
            .await
            .map_err(|e| ToolError::Execution(format!("{e}")))?;

        Ok(ToolOutput::Json(json!({ "paused": p.id })))
    }
}

// ---------------------------------------------------------------------------
// CronResume
// ---------------------------------------------------------------------------

struct CronResumeTool {
    scheduler: Arc<CronScheduler>,
}

impl CronResumeTool {
    fn new(scheduler: Arc<CronScheduler>) -> Self {
        Self { scheduler }
    }
}

#[async_trait]
impl Tool for CronResumeTool {
    fn name(&self) -> &str {
        "CronResume"
    }

    fn description(&self) -> String {
        "Resume a paused cron job by its ID. Its next fire is computed from now \
         — the slots it missed while paused are not made up. Fails for a one-shot \
         job whose time has already passed: there is nothing left to fire, so \
         schedule a new one with CronCreate."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        job_id_schema("Cron job ID to resume")
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        job_id_progress_label(params)
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p = JobIdParams::parse(params)?;

        self.scheduler
            .enable_job(&p.id)
            .await
            .map_err(|e| ToolError::Execution(format!("{e}")))?;

        let next_trigger_at = self
            .scheduler
            .get_job(&p.id)
            .await
            .ok()
            .flatten()
            .and_then(|j| j.format_time_opt(j.next_trigger_at));

        Ok(ToolOutput::Json(json!({
            "resumed": p.id,
            "next_trigger_at": next_trigger_at,
        })))
    }
}

// ---------------------------------------------------------------------------
// CronList
// ---------------------------------------------------------------------------

struct CronListTool {
    scheduler: Arc<CronScheduler>,
}

impl CronListTool {
    fn new(scheduler: Arc<CronScheduler>) -> Self {
        Self { scheduler }
    }
}

#[async_trait]
impl Tool for CronListTool {
    fn name(&self) -> &str {
        "CronList"
    }

    /// Read-only listing — safe to run concurrently.
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn description(&self) -> String {
        "List all scheduled cron jobs. Trigger times are rendered in \
         each job's own `timezone`."
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let mut jobs = self
            .scheduler
            .list_all_jobs()
            .await
            .map_err(|e| ToolError::Execution(format!("{e}")))?;
        jobs.sort_by(|a, b| a.created_at.cmp(&b.created_at));

        let rows: Vec<Value> = jobs
            .iter()
            .map(|j| {
                json!({
                    "id": j.id,
                    "title": j.display_title(),
                    "schedule": j.schedule.display(),
                    "status": j.status.as_str(),
                    "timezone": j.timezone,
                    "next_trigger_at": j.format_time_opt(j.next_trigger_at),
                    "last_triggered_at": j.format_time_opt(j.last_triggered_at),
                })
            })
            .collect();

        Ok(ToolOutput::Json(json!({ "jobs": rows })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shutdown::NeverShutdown;
    use crate::test_support::InMemoryCronStore;
    use baybo_model::{CronStatus, TriggerSource};
    use tokio::sync::mpsc;

    fn ctx_with_trigger(session_id: &str, trigger: TriggerSource) -> ToolContext {
        ToolContext {
            session_id: session_id.into(),
            session_trigger: trigger,
            ..ToolContext::for_test()
        }
    }

    /// A scheduler over the in-memory store, plus the trigger receiver (held so
    /// the channel stays open).
    fn test_scheduler() -> (Arc<CronScheduler>, mpsc::Receiver<crate::CronTriggerEvent>) {
        let (tx, rx) = mpsc::channel(8);
        let scheduler = CronScheduler::new(
            Arc::new(InMemoryCronStore::new()),
            tx,
            Arc::new(NeverShutdown),
        );
        (Arc::new(scheduler), rx)
    }

    async fn recurring_job(scheduler: &CronScheduler) -> String {
        scheduler
            .create_job(NewCronJob {
                user_id: "u1".to_string(),
                channel: baybo_model::ChannelType::tui(),
                title: "test job".to_string(),
                schedule: CronSchedule::cron("0 9 * * *"),
                prompt: "news".to_string(),
                timezone: "UTC".to_string(),
                origin_session_id: None,
            })
            .await
            .expect("job creates")
            .id
    }

    fn json_output(output: ToolOutput) -> Value {
        match output {
            ToolOutput::Json(v) => v,
            other => panic!("expected json output, got {other:?}"),
        }
    }

    /// Pause takes a job out of the firing schedule without removing it; resume
    /// puts it back with a fresh future slot.
    #[tokio::test]
    async fn pause_and_resume_round_trip_through_the_tools() {
        let (scheduler, _rx) = test_scheduler();
        let id = recurring_job(&scheduler).await;
        let ctx = ToolContext::for_test();

        let paused = json_output(
            CronPauseTool::new(Arc::clone(&scheduler))
                .execute(json!({ "id": id }), &ctx)
                .await
                .expect("pause runs"),
        );
        assert_eq!(paused["paused"], id);
        let job = scheduler.get_job(&id).await.unwrap().unwrap();
        assert_eq!(job.status, CronStatus::Disabled);
        assert!(job.next_trigger_at.is_none());

        let resumed = json_output(
            CronResumeTool::new(Arc::clone(&scheduler))
                .execute(json!({ "id": id }), &ctx)
                .await
                .expect("resume runs"),
        );
        assert_eq!(resumed["resumed"], id);
        assert!(
            resumed["next_trigger_at"].is_string(),
            "resume reports when the job fires next: {resumed}"
        );
        let job = scheduler.get_job(&id).await.unwrap().unwrap();
        assert_eq!(job.status, CronStatus::Enabled);
        assert!(job.next_trigger_at.is_some_and(|t| t > Utc::now()));
    }

    /// A paused job is still listed — that is what separates it from a deleted
    /// one, which the model must not see (nor try to act on).
    #[tokio::test]
    async fn list_keeps_paused_jobs_and_drops_deleted_ones() {
        let (scheduler, _rx) = test_scheduler();
        let paused = recurring_job(&scheduler).await;
        let deleted = recurring_job(&scheduler).await;
        let ctx = ToolContext::for_test();

        CronPauseTool::new(Arc::clone(&scheduler))
            .execute(json!({ "id": paused }), &ctx)
            .await
            .expect("pause runs");
        CronDeleteTool::new(Arc::clone(&scheduler))
            .execute(json!({ "id": deleted }), &ctx)
            .await
            .expect("delete runs");

        let listed = json_output(
            CronListTool::new(Arc::clone(&scheduler))
                .execute(json!({}), &ctx)
                .await
                .expect("list runs"),
        );
        let jobs = listed["jobs"].as_array().expect("jobs array").clone();
        assert_eq!(jobs.len(), 1, "{listed}");
        assert_eq!(jobs[0]["id"], paused);
        assert_eq!(jobs[0]["status"], "disabled");
    }

    /// A job created from a normal conversation belongs to that conversation.
    #[test]
    fn origin_is_the_calling_session() {
        let ctx = ctx_with_trigger("sess-user", TriggerSource::User);
        assert_eq!(origin_session(&ctx).as_str(), "sess-user");
    }

    /// A **one-shot** fire's session is a private workspace nobody can open, so
    /// a job scheduled from inside one must not anchor there — the result would
    /// reach nobody. It inherits the fire's own origin instead, collapsing the
    /// chain onto the real conversation.
    #[test]
    fn origin_inherits_through_a_one_shot_fire_workspace() {
        let ctx = ctx_with_trigger(
            "cron-fire-1",
            TriggerSource::Cron {
                cron_job_id: "cj-1".into(),
                origin_session_id: Some("sess-user".into()),
                conversation: false,
                job_title: None,
            },
        );
        assert_eq!(origin_session(&ctx).as_str(), "sess-user");
    }

    /// A **recurring** fire's session IS a conversation — listed, replyable —
    /// so a job scheduled there belongs there, whether the fire scheduled it or
    /// a user replying in it did ("remind me in an hour"). Inheriting the
    /// recurring job's own origin would report the result into a different
    /// conversation than the one the user asked in — or, when that job has no
    /// origin (every job created via the admin API), drop it entirely.
    #[test]
    fn origin_is_the_recurring_fire_conversation_the_user_is_in() {
        let ctx = ctx_with_trigger(
            "cron-daily-news",
            TriggerSource::Cron {
                cron_job_id: "cj-news".into(),
                origin_session_id: Some("sess-somewhere-else".into()),
                conversation: true,
                job_title: Some("Daily news".into()),
            },
        );
        assert_eq!(origin_session(&ctx).as_str(), "cron-daily-news");
    }

    /// A one-shot fire with no origin of its own (a legacy job) has nothing to
    /// inherit; its own id is recorded, and the delivery path recognises that
    /// as unusable and drops rather than notifying into the void.
    #[test]
    fn origin_falls_back_to_the_fire_session_when_it_has_none() {
        let ctx = ctx_with_trigger(
            "cron-fire-2",
            TriggerSource::Cron {
                cron_job_id: "cj-legacy".into(),
                origin_session_id: None,
                conversation: false,
                job_title: None,
            },
        );
        assert_eq!(origin_session(&ctx).as_str(), "cron-fire-2");
    }

    #[test]
    fn parse_at_accepts_rfc3339_z() {
        let dt = parse_at_in_tz("2026-04-17T14:25:00Z", "Asia/Shanghai").unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-04-17T14:25:00+00:00");
    }

    #[test]
    fn parse_at_accepts_rfc3339_with_offset() {
        let dt = parse_at_in_tz("2026-04-17T22:25:00+08:00", "UTC").unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-04-17T14:25:00+00:00");
    }

    #[test]
    fn parse_at_naive_uses_supplied_timezone() {
        let dt = parse_at_in_tz("2026-04-17T22:25:00", "Asia/Shanghai").unwrap();
        // 22:25 +08:00 == 14:25 UTC.
        assert_eq!(dt.to_rfc3339(), "2026-04-17T14:25:00+00:00");
    }

    #[test]
    fn parse_at_naive_space_separator() {
        let dt = parse_at_in_tz("2026-04-17 22:25:00", "Asia/Shanghai").unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-04-17T14:25:00+00:00");
    }

    #[test]
    fn parse_at_rejects_garbage() {
        let err = parse_at_in_tz("not a time", "UTC").unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[test]
    fn parse_at_rejects_bad_timezone_for_naive() {
        let err = parse_at_in_tz("2026-04-17T14:25:00", "Mars/Olympus").unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)));
    }

    #[test]
    fn parse_at_rfc3339_offset_overrides_timezone_param() {
        // Explicit offset wins; the `timezone` param is irrelevant for
        // RFC3339-with-offset, but a bogus tz must not block parsing.
        let dt = parse_at_in_tz("2026-04-17T14:25:00Z", "Mars/Olympus").unwrap();
        assert_eq!(dt.to_rfc3339(), "2026-04-17T14:25:00+00:00");
    }
}
