//! LLM-facing tools for managing cron jobs. Require an `Arc<CronScheduler>`
//! to manipulate the global scheduler state.

use std::sync::Arc;

use crate::{CronError, CronJobPatch, CronSchedule, CronScheduler, NewCronJob};
use async_trait::async_trait;
use baybo_model::SessionId;
use baybo_tools::{
    Tool, ToolConcurrency, ToolContext, ToolError, ToolManifest, ToolOutput, ToolTriggerScope,
};
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
        Arc::new(CronUpdateTool::new(Arc::clone(&scheduler))),
        Arc::new(CronDeleteTool::new(Arc::clone(&scheduler))),
        Arc::new(CronPauseTool::new(Arc::clone(&scheduler))),
        Arc::new(CronResumeTool::new(Arc::clone(&scheduler))),
        Arc::new(CronListTool::new(scheduler)),
        // Holds no scheduler state: its only effect is to flip the fire's
        // `NotifySilence` handle, and it is visible only inside a recurring fire.
        Arc::new(CronReportNothingTool),
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
        channels: Vec::new(),
    };
    (tool, manifest)
}

// ---------------------------------------------------------------------------
// report_nothing (recurring cron fires only)
// ---------------------------------------------------------------------------

/// A recurring cron fire calls this to declare that its scheduled check found
/// nothing worth telling the user, so this run should notify no one. It is
/// visible only inside a recurring fire ([`ToolTriggerScope::CronConversation`]) and
/// takes effect only there: it flips the fire's `NotifySilence` handle, which
/// the agent loop reads to complete the fire without a notification (no chat
/// row, no push, no live pulse). Where the handle is absent — a user reply in a
/// cron conversation, or a one-shot fire — it is a no-op.
struct CronReportNothingTool;

#[async_trait]
impl Tool for CronReportNothingTool {
    fn name(&self) -> &str {
        "report_nothing"
    }

    fn description(&self) -> String {
        r#"For a recurring scheduled run ONLY: call this when the check you just performed found nothing the user needs to know about right now, so this run notifies no one — no message, no push. Use it for a job whose intent is to WATCH for a condition and speak up only when it holds (e.g. "check the build and tell me only if it broke", "let me know if that PR gets a review"). Do NOT use it to skip work you were asked to do: a job told to always produce something — a daily digest, a summary, a recurring reminder — must still produce it every run, even when the content is thin. When in doubt, report: a missed notification is worse than an extra one. Calling this suppresses this run's notification entirely, so produce no other reply."#
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn trigger_scope(&self) -> ToolTriggerScope {
        ToolTriggerScope::CronConversation
    }

    async fn execute(&self, _params: Value, ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        match &ctx.notify_silence {
            Some(silence) => {
                silence.request();
                Ok(ToolOutput::Text(
                    "Acknowledged — this scheduled run will not notify the user.".to_string(),
                ))
            }
            // No pending notification to suppress: a user reply inside a cron
            // conversation, or any turn that is not a recurring fire.
            None => Ok(ToolOutput::Text(
                "There is no scheduled notification to suppress on this turn.".to_string(),
            )),
        }
    }
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
        r#"Schedule a job whose `prompt` is run as a task on a timer. `title`, `timezone` and `prompt` are required; all times in and out are anchored to `timezone`. Supply exactly one of `schedule` (recurring cron expression, e.g. "0 9 * * *") or `at` (one-shot timestamp). A recurring fire opens its own conversation named after `title`; a one-shot fire reports its result back into THIS conversation and then stops, staying in the list as `executed`."#
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
                    "description": "Recurring cron expression evaluated in `timezone`. Supply exactly one non-empty value across `schedule` and `at`; use an empty string for the unused field when the strict schema materializes both."
                },
                "at": {
                    "type": "string",
                    "description": "One-shot timestamp; fires once, then the job stops and stays in the list as `executed`. Either RFC3339 with offset (e.g. \"2026-04-17T14:25:00Z\" or \"2026-04-17T22:25:00+08:00\") or a naive `YYYY-MM-DDTHH:MM:SS` interpreted in `timezone`. Supply exactly one non-empty value across `schedule` and `at`; use an empty string for the unused field when the strict schema materializes both."
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

        let schedule = match (set_field(p.schedule), set_field(p.at)) {
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
                // Inherited from the calling session, never a parameter —
                // exactly like `origin_session` above, and for the same
                // reason: a model may no more choose which board a job
                // files work on than it may choose which board a tool call
                // touches. A job scheduled from inside a board belongs to
                // that board; one scheduled from a chat belongs to none.
                project_id: ctx.session_trigger.project().cloned(),
            })
            .await
            .map_err(cron_tool_error)?;

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
// CronUpdate
// ---------------------------------------------------------------------------

struct CronUpdateTool {
    scheduler: Arc<CronScheduler>,
}

impl CronUpdateTool {
    fn new(scheduler: Arc<CronScheduler>) -> Self {
        Self { scheduler }
    }

    /// The zone a naive `at` in this call is read in: the one the call sets, or
    /// else the one the job already has. Either way the user means a wall-clock
    /// time in the zone their job lives in — a naive `at` read as UTC would move
    /// the reminder by the offset.
    async fn effective_timezone(
        &self,
        job_id: &str,
        patched: Option<&str>,
    ) -> Result<String, ToolError> {
        if let Some(timezone) = patched {
            return Ok(timezone.to_string());
        }
        self.scheduler
            .get_job(job_id)
            .await
            .map_err(cron_tool_error)?
            .map(|job| job.timezone)
            .ok_or_else(|| cron_tool_error(CronError::NotFound(job_id.to_string())))
    }
}

#[derive(Debug, Deserialize)]
struct UpdateParams {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    at: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
}

/// A field the caller left blank is a field it did not set — the model reaches
/// for `""` to mean "leave this alone" often enough that taking it literally
/// would rewrite the job with an empty cron expression, or leave a live job
/// firing an empty instruction. Every string field an edit can write goes
/// through here; a call that sets nothing else is then refused outright rather
/// than reported as an edit that landed.
fn set_field(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// A job a tool cannot find is either one that never existed or one the user
/// deleted — `CronList` shows neither, so the model cannot tell them apart from
/// the id alone. The error names both, and names the way out of the one that has
/// one: a job in the recycle bin has to come back before anything can be done to
/// it, and only the user can bring it back.
const NOT_FOUND_HINT: &str = "it is either unknown or in the recycle bin — a deleted job must be \
                              restored (ask the user to restore it) first";

/// A failed cron call is the caller's to fix when it named a field wrong or
/// asked for a time that has gone; anything else is ours. Shared by every cron
/// tool, so the same mistake does not read as a bad parameter from one and as a
/// system failure from another.
fn cron_tool_error(e: CronError) -> ToolError {
    match e {
        CronError::InvalidSchedule(_) | CronError::EmptyUpdate(_) | CronError::BlankPrompt => {
            ToolError::InvalidParams(e.to_string())
        }
        not_found @ CronError::NotFound(_) => {
            ToolError::Execution(format!("{not_found}: {NOT_FOUND_HINT}"))
        }
        other => ToolError::Execution(other.to_string()),
    }
}

#[async_trait]
impl Tool for CronUpdateTool {
    fn name(&self) -> &str {
        "CronUpdate"
    }

    fn description(&self) -> String {
        r#"Edit an existing cron job in place, by its ID: change its `prompt`, `title`, `schedule` / `at`, or `timezone`. This is how you change a job the user already has ("move the reminder to 8am", "make it remind me about the dentist instead") — always prefer it to CronDelete + CronCreate, which mints a new job and throws away the old one's history; an edited job keeps its ID, its past runs, and the conversations they opened. Pass the ID plus only the fields that change: anything you leave out keeps its current value, and setting nothing at all is an error. Changing `schedule` / `at` / `timezone` recomputes the next fire time FROM NOW — the fires the job missed are never made up — and a new `at` that has already passed is rejected. A PAUSED job stays paused: editing it does not start it again, so call CronResume when the user wants it running. A one-shot job that already fired CAN be re-armed by giving it a new `at` in the future. The updated job comes back with its new `next_trigger_at`, in its own timezone — tell the user when it will actually run next."#
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "ID of the cron job to edit. Use CronList to find it."
                },
                "title": {
                    "type": "string",
                    "description": "New short human name for the job. Omit it, or use an empty string under a strict schema, to keep the current one."
                },
                "prompt": {
                    "type": "string",
                    "description": "New instruction to execute when the job fires. Omit it, or use an empty string under a strict schema, to keep the current one. Same rules as CronCreate's `prompt`."
                },
                "schedule": {
                    "type": "string",
                    "description": "New recurring cron expression, evaluated in the job's timezone. Supply at most one non-empty value across `schedule` and `at`; both omitted or empty leaves the current schedule alone."
                },
                "at": {
                    "type": "string",
                    "description": "New one-shot timestamp: the job fires once at this time, then stops. Either RFC3339 with offset (e.g. \"2026-04-17T22:25:00+08:00\") or a naive `YYYY-MM-DDTHH:MM:SS`, read in the job's timezone (or in `timezone`, when this call also changes it). Must be in the future. Supply at most one non-empty value across `schedule` and `at`; an empty value is unchanged."
                },
                "timezone": {
                    "type": "string",
                    "description": "New IANA timezone (e.g. \"Asia/Shanghai\") for the job. Omit it, or use an empty string under a strict schema, to keep the current one. Changing it alone moves the job: the same cron expression names a different instant in a different zone."
                }
            },
            "required": ["id"]
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("title")
            .or_else(|| params.get("id"))
            .and_then(Value::as_str)
            .and_then(baybo_tools::progress::preview_arg)
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let p: UpdateParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        let timezone = set_field(p.timezone);
        let schedule = match (set_field(p.schedule), set_field(p.at)) {
            (Some(_), Some(_)) => {
                return Err(ToolError::InvalidParams(
                    "`schedule` and `at` are mutually exclusive".into(),
                ));
            }
            (Some(expr), None) => Some(CronSchedule::cron(expr)),
            (None, Some(at)) => {
                let tz = self.effective_timezone(&p.id, timezone.as_deref()).await?;
                Some(CronSchedule::at(parse_at_in_tz(&at, &tz)?))
            }
            (None, None) => None,
        };

        let job = self
            .scheduler
            .update_job(
                &p.id,
                CronJobPatch {
                    title: set_field(p.title),
                    prompt: set_field(p.prompt),
                    schedule,
                    timezone,
                    mcp_tool_grants: None,
                },
            )
            .await
            .map_err(cron_tool_error)?;

        Ok(ToolOutput::Json(json!({
            "id": job.id,
            "title": job.title,
            "prompt": job.prompt,
            "schedule": job.schedule.display(),
            "timezone": job.timezone,
            "status": job.status.as_str(),
            "next_trigger_at": job.format_time_opt(job.next_trigger_at),
        })))
    }
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
         recycle bin. To stop a job only temporarily, use CronPause instead. To \
         change what a job does or when it runs, use CronUpdate: deleting it and \
         creating a new one throws away its past runs and the conversations they \
         opened."
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
         give it a new time with CronUpdate, which keeps the job and its history."
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
            .map_err(cron_tool_error)?;

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
        jobs.sort_by_key(|j| j.created_at);

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
    use baybo_model::{CronStatus, McpToolGrant, McpTransportIdentity, TriggerSource};
    use tokio::sync::mpsc;

    fn ctx_with_trigger(session_id: &str, trigger: TriggerSource) -> ToolContext {
        ToolContext {
            session_id: session_id.into(),
            session_trigger: trigger,
            ..ToolContext::for_test()
        }
    }

    #[test]
    fn report_nothing_is_visible_only_to_recurring_fires() {
        assert_eq!(
            CronReportNothingTool.trigger_scope(),
            ToolTriggerScope::CronConversation
        );
    }

    #[tokio::test]
    async fn report_nothing_requests_silence_when_a_handle_is_present() {
        let silence = baybo_tools::NotifySilence::new();
        let ctx = ToolContext {
            notify_silence: Some(silence.clone()),
            ..ToolContext::for_test()
        };
        let out = CronReportNothingTool
            .execute(json!({}), &ctx)
            .await
            .expect("executes");
        assert!(matches!(out, ToolOutput::Text(_)));
        assert!(
            silence.requested(),
            "a recurring fire's notification must be suppressed"
        );
    }

    #[tokio::test]
    async fn report_nothing_is_a_no_op_without_a_handle() {
        // A user reply in a cron conversation, or any non-fire turn: the handle
        // is absent, which is the guard — the call is a harmless no-op.
        let ctx = ToolContext::for_test();
        let out = CronReportNothingTool
            .execute(json!({}), &ctx)
            .await
            .expect("executes");
        assert!(matches!(out, ToolOutput::Text(_)));
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
                project_id: None,
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

    async fn job_in_tz(scheduler: &CronScheduler, timezone: &str) -> String {
        scheduler
            .create_job(NewCronJob {
                user_id: "u1".to_string(),
                channel: baybo_model::ChannelType::tui(),
                title: "test job".to_string(),
                schedule: CronSchedule::cron("0 9 * * *"),
                prompt: "news".to_string(),
                timezone: timezone.to_string(),
                origin_session_id: None,
                project_id: None,
            })
            .await
            .expect("job creates")
            .id
    }

    async fn update(scheduler: &Arc<CronScheduler>, params: Value) -> baybo_tools::Result<Value> {
        CronUpdateTool::new(Arc::clone(scheduler))
            .execute(params, &ToolContext::for_test())
            .await
            .map(json_output)
    }

    /// The tool hands the model back the job it actually wrote — including the
    /// new `next_trigger_at`, so the model can tell the user when it will run
    /// rather than guessing.
    #[tokio::test]
    async fn an_edit_returns_the_job_as_it_now_stands() {
        let (scheduler, _rx) = test_scheduler();
        let id = recurring_job(&scheduler).await;

        let updated = update(
            &scheduler,
            json!({ "id": id, "prompt": "summarise the standup" }),
        )
        .await
        .expect("update runs");

        assert_eq!(updated["id"], id, "the edit re-minted the job");
        assert_eq!(updated["prompt"], "summarise the standup");
        assert_eq!(updated["title"], "test job", "an unset field was rewritten");
        assert_eq!(updated["schedule"], "0 9 * * *");
        assert_eq!(updated["status"], "enabled");
        assert!(updated["next_trigger_at"].is_string(), "{updated}");

        let job = scheduler.get_job(&id).await.unwrap().unwrap();
        assert_eq!(job.prompt, "summarise the standup");
    }

    /// The model reaches for `""` to mean "leave this alone". Taken literally on
    /// `prompt`, that would leave a live, armed job firing an empty instruction
    /// forever — the one way an edit can break a job that was working. Every
    /// string field reads a blank as unset, so an edit that sets only blanks
    /// sets nothing, and says so.
    #[tokio::test]
    async fn a_blank_field_is_a_field_the_edit_did_not_set() {
        let (scheduler, _rx) = test_scheduler();
        let id = recurring_job(&scheduler).await;

        let updated = update(
            &scheduler,
            json!({ "id": id, "title": "Standup", "prompt": "   " }),
        )
        .await
        .expect("update runs");
        assert_eq!(updated["title"], "Standup");
        assert_eq!(updated["prompt"], "news", "a blank prompt blanked the job");

        let err = update(&scheduler, json!({ "id": id, "prompt": "" }))
            .await
            .expect_err("a blank-only edit sets nothing");
        assert!(matches!(err, ToolError::InvalidParams(_)), "{err:?}");

        let job = scheduler.get_job(&id).await.unwrap().unwrap();
        assert_eq!(job.prompt, "news");
        assert_eq!(job.status, CronStatus::Enabled);
    }

    #[test]
    fn schedule_pair_describes_its_strict_schema_filler() {
        let (scheduler, _rx) = test_scheduler();
        for schema in [
            CronCreateTool::new(Arc::clone(&scheduler)).parameters_schema(),
            CronUpdateTool::new(scheduler).parameters_schema(),
        ] {
            for field in ["schedule", "at"] {
                assert!(
                    schema["properties"][field]["description"]
                        .as_str()
                        .is_some_and(|description| description.contains("empty")),
                    "{field}: {schema}"
                );
            }
        }
    }

    #[tokio::test]
    async fn cron_tools_cannot_create_or_replace_mcp_grants() {
        let (scheduler, _rx) = test_scheduler();
        let create = CronCreateTool::new(Arc::clone(&scheduler));
        let update = CronUpdateTool::new(Arc::clone(&scheduler));
        assert!(
            create.parameters_schema()["properties"]
                .get("mcp_tool_grants")
                .is_none()
        );
        assert!(
            update.parameters_schema()["properties"]
                .get("mcp_tool_grants")
                .is_none()
        );

        let attempted = McpToolGrant::new(
            "server/attempted",
            McpTransportIdentity::from_sha256([7; 32]),
        );
        let created = json_output(
            create
                .execute(
                    json!({
                        "title": "MCP job",
                        "timezone": "UTC",
                        "prompt": "use a server",
                        "schedule": "0 9 * * *",
                        "mcp_tool_grants": [serde_json::to_value(&attempted).expect("grant")],
                    }),
                    &ToolContext::for_test(),
                )
                .await
                .expect("create runs"),
        );
        let id = created["id"].as_str().expect("job id");
        assert!(
            scheduler
                .get_job(id)
                .await
                .unwrap()
                .unwrap()
                .mcp_tool_grants
                .is_empty()
        );

        let existing = McpToolGrant::new(
            "server/existing",
            McpTransportIdentity::from_sha256([8; 32]),
        );
        scheduler
            .update_job(
                id,
                CronJobPatch {
                    mcp_tool_grants: Some(vec![existing.clone()]),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        update
            .execute(
                json!({
                    "id": id,
                    "title": "Renamed MCP job",
                    "mcp_tool_grants": [serde_json::to_value(&attempted).expect("grant")],
                }),
                &ToolContext::for_test(),
            )
            .await
            .expect("update runs");
        assert_eq!(
            scheduler
                .get_job(id)
                .await
                .unwrap()
                .unwrap()
                .mcp_tool_grants,
            vec![existing]
        );
    }

    /// Editing nothing is always a caller bug, and the model must see it as one
    /// — not as a job that quietly did not change.
    #[tokio::test]
    async fn an_edit_that_sets_no_field_is_an_invalid_call() {
        let (scheduler, _rx) = test_scheduler();
        let id = recurring_job(&scheduler).await;

        let err = update(&scheduler, json!({ "id": id })).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_schedule_and_an_at_cannot_both_be_set() {
        let (scheduler, _rx) = test_scheduler();
        let id = recurring_job(&scheduler).await;

        let err = update(
            &scheduler,
            json!({ "id": id, "schedule": "0 9 * * *", "at": "2099-04-17T14:25:00Z" }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ToolError::InvalidParams(_)), "{err:?}");
    }

    /// A naive `at` means a wall-clock time in the zone the job lives in. Read as
    /// UTC it would move the reminder by the offset — eight hours, for a job the
    /// user scheduled in Shanghai.
    #[tokio::test]
    async fn a_naive_at_is_read_in_the_jobs_own_timezone() {
        let (scheduler, _rx) = test_scheduler();
        let id = job_in_tz(&scheduler, "Asia/Shanghai").await;

        let updated = update(&scheduler, json!({ "id": id, "at": "2099-04-17T22:25:00" }))
            .await
            .expect("update runs");

        assert_eq!(updated["timezone"], "Asia/Shanghai");
        assert_eq!(
            updated["next_trigger_at"], "2099-04-17T22:25:00+08:00",
            "the fire time is rendered back in the job's own zone",
        );
        let job = scheduler.get_job(&id).await.unwrap().unwrap();
        assert_eq!(
            job.next_trigger_at.map(|t| t.to_rfc3339()),
            Some("2099-04-17T14:25:00+00:00".to_string()),
        );
    }

    /// Editing a paused job does not start it again: the tool reports it still
    /// paused, with no fire time, so the model cannot tell the user otherwise.
    #[tokio::test]
    async fn an_edit_leaves_a_paused_job_paused() {
        let (scheduler, _rx) = test_scheduler();
        let id = recurring_job(&scheduler).await;
        scheduler.disable_job(&id).await.unwrap();

        let updated = update(
            &scheduler,
            json!({ "id": id, "schedule": "*/5 * * * *", "prompt": "new prompt" }),
        )
        .await
        .expect("update runs");

        assert_eq!(updated["status"], "disabled");
        assert!(
            updated["next_trigger_at"].is_null(),
            "a paused job was re-armed by an edit: {updated}",
        );
        assert_eq!(updated["schedule"], "*/5 * * * *");
        assert_eq!(updated["prompt"], "new prompt");
    }

    /// An `at` that has already gone has no fire left in it. The model gets an
    /// invalid-params error with the current time in it, so its retry can land.
    #[tokio::test]
    async fn an_at_in_the_past_is_refused() {
        let (scheduler, _rx) = test_scheduler();
        let id = recurring_job(&scheduler).await;

        let err = update(
            &scheduler,
            json!({ "id": id, "at": "2020-04-17T14:25:00Z" }),
        )
        .await
        .unwrap_err();
        match err {
            ToolError::InvalidParams(msg) => assert!(msg.contains("now is"), "{msg}"),
            other => panic!("expected invalid params, got {other:?}"),
        }
    }

    /// A job in the recycle bin is not editable — the model is told to restore
    /// it, not quietly handed a job that can never fire.
    #[tokio::test]
    async fn a_binned_job_cannot_be_edited_through_the_tool() {
        let (scheduler, _rx) = test_scheduler();
        let id = recurring_job(&scheduler).await;
        scheduler.delete_job(&id).await.unwrap();

        let err = update(&scheduler, json!({ "id": id, "prompt": "new prompt" }))
            .await
            .unwrap_err();
        let ToolError::Execution(msg) = &err else {
            panic!("expected an execution error, got {err:?}");
        };
        assert!(
            msg.contains("restore"),
            "the model is left to guess why its edit failed, and cannot act on it: {msg}",
        );
        assert_eq!(
            scheduler.get_job(&id).await.unwrap().unwrap().prompt,
            "news",
            "a binned job was edited",
        );
        assert!(
            scheduler.get_job(&id).await.unwrap().unwrap().is_deleted(),
            "a refused edit brought a binned job back",
        );
    }

    /// The `at` path resolves the job's timezone before it edits anything, so it
    /// has a second way to miss the job — and it must miss it the same way, with
    /// the same guidance.
    #[tokio::test]
    async fn an_unknown_id_is_refused_on_both_paths_into_the_tool() {
        let (scheduler, _rx) = test_scheduler();

        for params in [
            json!({ "id": "ghost", "prompt": "new prompt" }),
            json!({ "id": "ghost", "at": "2099-04-17T22:25:00" }),
        ] {
            let err = update(&scheduler, params).await.unwrap_err();
            let ToolError::Execution(msg) = &err else {
                panic!("expected an execution error, got {err:?}");
            };
            assert!(msg.contains("ghost"), "{msg}");
            assert!(msg.contains("restore"), "{msg}");
        }
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
                project_id: None,
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
                project_id: None,
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
                project_id: None,
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
