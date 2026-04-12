use aura_job::JobStatus;
use serde_json::{Value, json};

use crate::cli::{JobCmd, JobStatusArg};
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: JobCmd) -> Result<CommandOutput> {
    match cmd {
        JobCmd::List { status } => list(ctx, status.map(Into::into)).await,
        JobCmd::Show { id } => show(ctx, &id).await,
        JobCmd::Cancel { id, yes } => cancel(ctx, &id, yes).await,
    }
}

fn jobs(ctx: &CommandContext) -> Result<&aura_agent::JobManager> {
    ctx.job
        .as_deref()
        .ok_or_else(|| CliError::Manager("job manager is not available in this invocation".into()))
}

impl From<JobStatusArg> for JobStatus {
    fn from(v: JobStatusArg) -> Self {
        match v {
            JobStatusArg::Pending => JobStatus::Pending,
            JobStatusArg::InProgress => JobStatus::InProgress,
            JobStatusArg::Completed => JobStatus::Completed,
            JobStatusArg::Submitted => JobStatus::Submitted,
            JobStatusArg::Accepted => JobStatus::Accepted,
            JobStatusArg::Failed => JobStatus::Failed,
            JobStatusArg::Stuck => JobStatus::Stuck,
        }
    }
}

async fn list(ctx: &CommandContext, status: Option<JobStatus>) -> Result<CommandOutput> {
    let mgr = jobs(ctx)?;
    let jobs = mgr
        .list(status.clone())
        .await
        .map_err(|e| CliError::Manager(format!("list jobs: {e}")))?;

    if jobs.is_empty() {
        let label = status
            .as_ref()
            .map(|s| format!("no jobs with status {s}"))
            .unwrap_or_else(|| "no jobs".to_string());
        return Ok(CommandOutput {
            human: label,
            data: Some(json!({ "jobs": [] })),
        });
    }

    let rows: Vec<Value> = jobs
        .iter()
        .map(|j| {
            json!({
                "id": j.id,
                "session": j.session_id,
                "status": j.status.to_string(),
                "kind": serde_json::to_value(&j.kind).unwrap_or(Value::Null),
                "created_at": j.created_at.to_rfc3339(),
                "parent": j.parent_job_id,
            })
        })
        .collect();

    let mut human =
        String::from("id                                    session     status       created_at\n");
    for j in &jobs {
        human.push_str(&format!(
            "{:<38}  {:<10}  {:<11}  {}\n",
            j.id,
            j.session_id,
            j.status.to_string(),
            j.created_at.to_rfc3339(),
        ));
    }

    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({ "jobs": rows })),
    })
}

async fn show(ctx: &CommandContext, id: &str) -> Result<CommandOutput> {
    let mgr = jobs(ctx)?;
    let job = mgr
        .get(id)
        .await
        .map_err(|e| CliError::Manager(format!("get job: {e}")))?
        .ok_or_else(|| CliError::Manager(format!("job {id} not found")))?;

    let value = json!({
        "id": job.id,
        "session": job.session_id,
        "parent": job.parent_job_id,
        "status": job.status.to_string(),
        "kind": serde_json::to_value(&job.kind).unwrap_or(Value::Null),
        "created_at": job.created_at.to_rfc3339(),
        "started_at": job.started_at.map(|t| t.to_rfc3339()),
        "completed_at": job.completed_at.map(|t| t.to_rfc3339()),
        "trace_span": job.trace_span_id,
        "error": job.error,
        "output": job.output,
    });

    let human = format!(
        "id:           {}\nsession:      {}\nparent:       {}\nstatus:       {}\ncreated:      {}\nstarted:      {}\ncompleted:    {}\nerror:        {}",
        job.id,
        job.session_id,
        job.parent_job_id.as_deref().unwrap_or("(none)"),
        job.status,
        job.created_at.to_rfc3339(),
        job.started_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "(not started)".into()),
        job.completed_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "(not completed)".into()),
        job.error.as_deref().unwrap_or("(none)"),
    );

    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}

async fn cancel(ctx: &CommandContext, id: &str, yes: bool) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && !yes {
        return Err(CliError::ConfirmationRequired(format!(
            "would cancel job {id}; re-run with --yes to confirm"
        )));
    }

    let mgr = jobs(ctx)?;
    let job = mgr
        .cancel(id)
        .await
        .map_err(|e| CliError::Manager(format!("cancel job: {e}")))?;

    Ok(CommandOutput {
        human: format!("cancelled job {id} (status now {})", job.status),
        data: Some(json!({
            "cancelled": id,
            "status": job.status.to_string(),
        })),
    })
}
