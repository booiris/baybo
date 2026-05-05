use aura_cron::CronJob;
use serde_json::{Value, json};

use crate::cli::CronCmd;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: CronCmd) -> Result<CommandOutput> {
    match cmd {
        CronCmd::List => list(ctx).await,
    }
}

fn cron(ctx: &CommandContext) -> Result<&aura_agent::CronScheduler> {
    ctx.cron.as_deref().ok_or_else(|| {
        CliError::Manager("cron scheduler is not available in this invocation".into())
    })
}

fn job_summary(j: &CronJob) -> Value {
    json!({
        "id": j.id,
        "user": j.user_id,
        "channel": format!("{:?}", j.channel),
        "schedule": j.schedule.display(),
        "one_shot": j.is_one_shot(),
        "status": j.status.as_str(),
        "timezone": j.timezone,
        "next_trigger_at": j.format_time_opt(j.next_trigger_at),
        "last_triggered_at": j.format_time_opt(j.last_triggered_at),
    })
}

async fn list(ctx: &CommandContext) -> Result<CommandOutput> {
    let mgr = cron(ctx)?;
    let mut jobs = mgr
        .list_all_jobs()
        .await
        .map_err(|e| CliError::Manager(format!("list cron jobs: {e}")))?;
    jobs.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    if jobs.is_empty() {
        return Ok(CommandOutput {
            human: "no cron jobs scheduled".into(),
            data: Some(json!({ "jobs": [] })),
        });
    }

    let rows: Vec<Value> = jobs.iter().map(job_summary).collect();
    let mut human = String::from(
        "id                                    user       status    schedule              next_trigger\n",
    );
    for j in &jobs {
        human.push_str(&format!(
            "{:<38}  {:<9}  {:<8}  {:<20}  {}\n",
            j.id,
            j.user_id,
            j.status.as_str(),
            j.schedule.display(),
            j.format_time_opt(j.next_trigger_at)
                .unwrap_or_else(|| "(disabled)".into()),
        ));
    }

    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({ "jobs": rows })),
    })
}
