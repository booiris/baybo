use baybo_cron::CronJob;
use serde_json::{Value, json};

use crate::cli::CronCmd;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: CronCmd) -> Result<CommandOutput> {
    match cmd {
        CronCmd::List => list(ctx).await,
        CronCmd::Show { id } => show(ctx, &id).await,
    }
}

fn cron(ctx: &CommandContext) -> Result<&baybo_agent::CronScheduler> {
    ctx.cron.as_deref().ok_or_else(|| {
        CliError::Manager("cron scheduler is not available in this invocation".into())
    })
}

fn job_summary(j: &CronJob) -> Value {
    json!({
        "id": j.id,
        "title": j.display_title(),
        "user": j.user_id,
        "channel": format!("{:?}", j.channel),
        "schedule": j.schedule.display(),
        "one_shot": j.is_one_shot(),
        "status": j.status.as_str(),
        "timezone": j.timezone,
        "next_trigger_at": j.format_time_opt(j.next_trigger_at),
        "last_triggered_at": j.format_time_opt(j.last_triggered_at),
        "builtin": j.builtin,
    })
}

async fn list(ctx: &CommandContext) -> Result<CommandOutput> {
    let mgr = cron(ctx)?;
    let mut jobs = mgr
        .list_all_jobs()
        .await
        .map_err(|e| CliError::Manager(format!("list cron jobs: {e}")))?;
    jobs.sort_by_key(|j| j.created_at);

    if jobs.is_empty() {
        return Ok(CommandOutput {
            human: "no cron jobs scheduled".into(),
            data: Some(json!({ "jobs": [] })),
        });
    }

    let rows: Vec<Value> = jobs.iter().map(job_summary).collect();
    let mut human = String::from(
        "id                                    title                 user       status    schedule              next_trigger\n",
    );
    for j in &jobs {
        human.push_str(&format!(
            "{:<38}  {:<20}  {:<9}  {:<8}  {:<20}  {}\n",
            j.id,
            j.display_title(),
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

async fn show(ctx: &CommandContext, id: &str) -> Result<CommandOutput> {
    let mgr = cron(ctx)?;
    let job = mgr
        .get_job(id)
        .await
        .map_err(|e| CliError::Manager(format!("get cron job: {e}")))?
        .ok_or_else(|| CliError::Manager(format!("cron job {id} not found")))?;

    let mut value = job_summary(&job);
    if let Value::Object(ref mut map) = value {
        map.insert("prompt".into(), Value::String(job.prompt.clone()));
        map.insert(
            "origin_session_id".into(),
            job.origin_session_id
                .as_ref()
                .map(|s| Value::String(s.as_str().to_owned()))
                .unwrap_or(Value::Null),
        );
        map.insert(
            "created_at".into(),
            Value::String(job.created_at.to_rfc3339()),
        );
        map.insert(
            "updated_at".into(),
            Value::String(job.updated_at.to_rfc3339()),
        );
    }

    let human = format!(
        "id:                {}\ntitle:             {}\nowned_by:          {}\nuser:              {}\nchannel:           {:?}\nstatus:            {}\nschedule:          {}\ntimezone:          {}\none_shot:          {}\nnext_trigger:      {}\nlast_triggered:    {}\norigin_session:    {}\ncreated:           {}\nupdated:           {}\nprompt:            {}",
        job.id,
        job.display_title(),
        if job.builtin {
            "baybo (built in: pausable and reschedulable, but its name, instruction \
             and existence are the runtime's)"
        } else {
            "you"
        },
        job.user_id,
        job.channel,
        job.status.as_str(),
        job.schedule.display(),
        job.timezone,
        job.is_one_shot(),
        job.format_time_opt(job.next_trigger_at)
            .unwrap_or_else(|| "(disabled)".into()),
        job.format_time_opt(job.last_triggered_at)
            .unwrap_or_else(|| "(never)".into()),
        job.origin_session_id
            .as_ref()
            .map(|s| s.as_str())
            .unwrap_or("(none)"),
        job.created_at.to_rfc3339(),
        job.updated_at.to_rfc3339(),
        job.prompt,
    );

    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}
