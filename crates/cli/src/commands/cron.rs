use aura_cron::{CronJob, CronRunMode, CronStatus};
use aura_model::ChannelType;
use serde_json::{Value, json};

use crate::cli::CronCmd;
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: CronCmd) -> Result<CommandOutput> {
    match cmd {
        CronCmd::Add {
            user,
            channel,
            schedule,
            prompt,
            one_shot,
            yes,
        } => add(ctx, &user, &channel, &schedule, &prompt, one_shot, yes).await,
        CronCmd::List => list(ctx).await,
        CronCmd::Show { id } => show(ctx, &id).await,
        CronCmd::Rm { id, yes } => rm(ctx, &id, yes).await,
        CronCmd::Enable { id } => enable(ctx, &id).await,
        CronCmd::Disable { id } => disable(ctx, &id).await,
        CronCmd::Run { id, yes } => run_now(ctx, &id, yes).await,
        CronCmd::Runs { id } => runs(ctx, &id).await,
    }
}

fn parse_channel(raw: &str) -> Result<ChannelType> {
    match raw.to_ascii_lowercase().as_str() {
        "tui" | "cli" => Ok(ChannelType::Tui),
        "telegram" => Ok(ChannelType::Telegram),
        "discord" => Ok(ChannelType::Discord),
        "http" => Ok(ChannelType::Http),
        other => Err(CliError::Manager(format!(
            "unknown channel '{other}'; expected one of tui, telegram, discord, http"
        ))),
    }
}

#[allow(clippy::too_many_arguments)]
async fn add(
    ctx: &CommandContext,
    user: &str,
    channel: &str,
    schedule: &str,
    prompt: &str,
    one_shot: bool,
    yes: bool,
) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && !yes {
        return Err(CliError::ConfirmationRequired(format!(
            "would schedule cron job for user {user} on {channel} \
             ({schedule:?}); re-run with --yes to confirm"
        )));
    }
    let channel_ty = parse_channel(channel)?;
    let run_mode = if one_shot {
        CronRunMode::OneShot
    } else {
        CronRunMode::Recurring
    };

    let mgr = cron(ctx)?;
    let job = mgr
        .create_job(user, channel_ty, schedule, prompt, run_mode)
        .await
        .map_err(|e| CliError::Manager(format!("create cron job: {e}")))?;

    let human = format!(
        "scheduled cron job {id} for user {user} on {ch} ({sched})\nnext trigger: {next}",
        id = job.id,
        user = job.user_id,
        ch = job.channel,
        sched = job.schedule,
        next = job
            .next_trigger_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "(disabled)".into()),
    );
    Ok(CommandOutput {
        human,
        data: Some(json!({
            "id": job.id,
            "user": job.user_id,
            "channel": format!("{}", job.channel),
            "schedule": job.schedule,
            "prompt": job.prompt,
            "run_mode": run_mode_label(&job.run_mode),
            "status": status_label(&job.status),
            "next_trigger_at": job.next_trigger_at.map(|t| t.to_rfc3339()),
            "created_at": job.created_at.to_rfc3339(),
        })),
    })
}

fn cron(ctx: &CommandContext) -> Result<&aura_agent::CronScheduler> {
    ctx.cron.as_deref().ok_or_else(|| {
        CliError::Manager("cron scheduler is not available in this invocation".into())
    })
}

fn run_mode_label(m: &CronRunMode) -> &'static str {
    match m {
        CronRunMode::Recurring => "recurring",
        CronRunMode::OneShot => "one_shot",
    }
}

fn status_label(s: &CronStatus) -> &'static str {
    match s {
        CronStatus::Enabled => "enabled",
        CronStatus::Disabled => "disabled",
    }
}

fn job_summary(j: &CronJob) -> Value {
    json!({
        "id": j.id,
        "user": j.user_id,
        "channel": format!("{:?}", j.channel),
        "schedule": j.schedule,
        "status": status_label(&j.status),
        "next_trigger_at": j.next_trigger_at.map(|t| t.to_rfc3339()),
        "last_triggered_at": j.last_triggered_at.map(|t| t.to_rfc3339()),
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
            status_label(&j.status),
            j.schedule,
            j.next_trigger_at
                .map(|t| t.to_rfc3339())
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

    let value = json!({
        "id": job.id,
        "user": job.user_id,
        "channel": format!("{:?}", job.channel),
        "schedule": job.schedule,
        "prompt": job.prompt,
        "status": status_label(&job.status),
        "run_mode": format!("{:?}", job.run_mode),
        "last_triggered_at": job.last_triggered_at.map(|t| t.to_rfc3339()),
        "next_trigger_at": job.next_trigger_at.map(|t| t.to_rfc3339()),
        "created_at": job.created_at.to_rfc3339(),
        "updated_at": job.updated_at.to_rfc3339(),
    });

    let human = format!(
        "id:           {}\nuser:         {}\nstatus:       {}\nschedule:     {}\nprompt:       {}\nlast run:     {}\nnext run:     {}",
        job.id,
        job.user_id,
        status_label(&job.status),
        job.schedule,
        job.prompt,
        job.last_triggered_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "(never)".into()),
        job.next_trigger_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "(disabled)".into()),
    );

    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}

async fn rm(ctx: &CommandContext, id: &str, yes: bool) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && !yes {
        return Err(CliError::ConfirmationRequired(format!(
            "would delete cron job {id}; re-run with --yes to confirm"
        )));
    }
    let mgr = cron(ctx)?;
    mgr.delete_job(id)
        .await
        .map_err(|e| CliError::Manager(format!("delete cron job: {e}")))?;
    Ok(CommandOutput {
        human: format!("deleted cron job {id}"),
        data: Some(json!({ "deleted": id })),
    })
}

async fn enable(ctx: &CommandContext, id: &str) -> Result<CommandOutput> {
    let mgr = cron(ctx)?;
    mgr.enable_job(id)
        .await
        .map_err(|e| CliError::Manager(format!("enable cron job: {e}")))?;
    Ok(CommandOutput {
        human: format!("enabled cron job {id}"),
        data: Some(json!({ "enabled": id })),
    })
}

async fn disable(ctx: &CommandContext, id: &str) -> Result<CommandOutput> {
    let mgr = cron(ctx)?;
    mgr.disable_job(id)
        .await
        .map_err(|e| CliError::Manager(format!("disable cron job: {e}")))?;
    Ok(CommandOutput {
        human: format!("disabled cron job {id}"),
        data: Some(json!({ "disabled": id })),
    })
}

async fn run_now(ctx: &CommandContext, id: &str, yes: bool) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && !yes {
        return Err(CliError::ConfirmationRequired(format!(
            "would fire cron job {id} now; re-run with --yes to confirm"
        )));
    }
    let mgr = cron(ctx)?;
    let exec = mgr
        .trigger_now(id)
        .await
        .map_err(|e| CliError::Manager(format!("trigger cron job: {e}")))?;
    Ok(CommandOutput {
        human: format!(
            "dispatched cron job {id} (execution {} at {})",
            exec.id,
            exec.triggered_at.to_rfc3339()
        ),
        data: Some(json!({
            "job": id,
            "execution": exec.id,
            "triggered_at": exec.triggered_at.to_rfc3339(),
        })),
    })
}

async fn runs(ctx: &CommandContext, id: &str) -> Result<CommandOutput> {
    let mgr = cron(ctx)?;
    let mut execs = mgr
        .list_executions(id)
        .await
        .map_err(|e| CliError::Manager(format!("list cron executions: {e}")))?;
    execs.sort_by(|a, b| b.triggered_at.cmp(&a.triggered_at));

    if execs.is_empty() {
        return Ok(CommandOutput {
            human: format!("no executions recorded for cron job {id}"),
            data: Some(json!({ "runs": [] })),
        });
    }

    let rows: Vec<Value> = execs
        .iter()
        .map(|e| {
            json!({
                "id": e.id,
                "job": e.job_id,
                "triggered_at": e.triggered_at.to_rfc3339(),
                "scheduled_fire_time": e.scheduled_fire_time.to_rfc3339(),
                "status": format!("{:?}", e.status).to_lowercase(),
            })
        })
        .collect();

    let mut human =
        String::from("execution                              triggered_at              status\n");
    for e in &execs {
        human.push_str(&format!(
            "{:<38}  {:<24}  {}\n",
            e.id,
            e.triggered_at.to_rfc3339(),
            format!("{:?}", e.status).to_lowercase(),
        ));
    }

    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({ "runs": rows })),
    })
}
