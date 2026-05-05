//! LLM-facing tools for managing cron jobs. Require an `Arc<CronScheduler>`
//! to manipulate the global scheduler state.

use std::sync::Arc;

use async_trait::async_trait;
use aura_tools::{Tool, ToolContext, ToolError, ToolManifest, ToolOutput};
use chrono::{DateTime, LocalResult, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{CronSchedule, CronScheduler};

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
        Arc::new(CronListTool::new(scheduler)),
    ];

    tools.into_iter().map(with_manifest).collect()
}

fn with_manifest(tool: Arc<dyn Tool>) -> (Arc<dyn Tool>, ToolManifest) {
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        trust_level: aura_model::TrustLevel::Trusted,
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
    prompt: String,
    timezone: String,
}

#[async_trait]
impl Tool for CronCreateTool {
    fn name(&self) -> &str {
        "CronCreate"
    }

    fn description(&self) -> &str {
        "Schedule a cron job. `timezone` and `prompt` are required — \
         every fire sends `prompt` through the LLM, and every time in \
         inputs and outputs is anchored to `timezone`. Exactly one of \
         `schedule` (recurring cron expression, e.g. \"0 9 * * *\") or \
         `at` (one-shot timestamp) is required — `at` jobs fire once \
         then auto-delete."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "timezone": {
                    "type": "string",
                    "description": "IANA timezone (e.g. \"Asia/Shanghai\", \"UTC\") used to evaluate `schedule`, interpret naive `at`, and render `next_trigger_at` in the output."
                },
                "prompt": {
                    "type": "string",
                    "description": "Prompt text sent through the LLM on each trigger."
                },
                "schedule": {
                    "type": "string",
                    "description": "Recurring cron expression evaluated in `timezone`. Supply exactly one of `schedule` or `at`."
                },
                "at": {
                    "type": "string",
                    "description": "One-shot timestamp; fires once then auto-deletes. Either RFC3339 with offset (e.g. \"2026-04-17T14:25:00Z\" or \"2026-04-17T22:25:00+08:00\") or a naive `YYYY-MM-DDTHH:MM:SS` interpreted in `timezone`. Supply exactly one of `schedule` or `at`."
                }
            },
            "required": ["timezone", "prompt"]
        })
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let p: CreateParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        let timezone = p.timezone;

        let schedule = match (p.schedule, p.at) {
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
            .create_job(
                &ctx.user.id,
                ctx.user.channel.clone(),
                schedule,
                p.prompt,
                timezone,
                Some(ctx.session_id.clone()),
            )
            .await
            .map_err(|e| ToolError::Execution(format!("{e}")))?;

        Ok(ToolOutput::Json(json!({
            "id": job.id,
            "schedule": job.schedule.display(),
            "timezone": job.timezone,
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

#[derive(Debug, Deserialize)]
struct DeleteParams {
    id: String,
}

#[async_trait]
impl Tool for CronDeleteTool {
    fn name(&self) -> &str {
        "CronDelete"
    }

    fn description(&self) -> &str {
        "Cancel and remove a scheduled cron job by its ID."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Cron job ID to delete" }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let p: DeleteParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;

        self.scheduler
            .delete_job(&p.id)
            .await
            .map_err(|e| ToolError::Execution(format!("{e}")))?;

        Ok(ToolOutput::Json(json!({ "deleted": p.id })))
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

    fn description(&self) -> &str {
        "List all scheduled cron jobs. Trigger times are rendered in \
         each job's own `timezone`."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
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
