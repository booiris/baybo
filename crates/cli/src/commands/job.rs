use aura_job::{CancelReason, JobStatusKind};
use aura_model::JobId;
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

fn jobs(ctx: &CommandContext) -> Result<&aura_job::JobLifecycle> {
    ctx.job
        .as_deref()
        .ok_or_else(|| CliError::Manager("job manager is not available in this invocation".into()))
}

impl From<JobStatusArg> for JobStatusKind {
    fn from(v: JobStatusArg) -> Self {
        match v {
            JobStatusArg::Pending => JobStatusKind::Pending,
            JobStatusArg::InProgress => JobStatusKind::InProgress,
            JobStatusArg::Stuck => JobStatusKind::Stuck,
            JobStatusArg::Cancelled => JobStatusKind::Cancelled,
            JobStatusArg::Failed => JobStatusKind::Failed,
            JobStatusArg::Completed => JobStatusKind::Completed,
        }
    }
}

fn parse_id(id: &str) -> Result<JobId> {
    id.parse()
        .map_err(|e| CliError::Manager(format!("invalid job id '{id}': {e}")))
}

async fn list(ctx: &CommandContext, status: Option<JobStatusKind>) -> Result<CommandOutput> {
    let mgr = jobs(ctx)?;
    let jobs = mgr
        .list(status)
        .await
        .map_err(|e: aura_job::JobError| CliError::Manager(format!("list jobs: {e}")))?;

    if jobs.is_empty() {
        let label = status
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
                "id": j.id.to_string(),
                "session": j.session_id.to_string(),
                "status": j.status.kind().to_string(),
                "kind": serde_json::to_value(j.kind).unwrap_or(Value::Null),
                "created_at": j.created_at.to_rfc3339(),
                "parent": j.parent_job_id.map(|p| p.to_string()),
            })
        })
        .collect();

    let mut human =
        String::from("id                          session     status       created_at\n");
    for j in &jobs {
        human.push_str(&format!(
            "{:<26}  {:<10}  {:<11}  {}\n",
            j.id.to_string(),
            j.session_id.to_string(),
            j.status.kind().to_string(),
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
    let job_id = parse_id(id)?;
    let job = mgr
        .get(&job_id)
        .await
        .map_err(|e: aura_job::JobError| CliError::Manager(format!("get job: {e}")))?
        .ok_or_else(|| CliError::Manager(format!("job {id} not found")))?;

    let value = json!({
        "id": job.id.to_string(),
        "session": job.session_id.to_string(),
        "parent": job.parent_job_id.map(|p| p.to_string()),
        "status": job.status.kind().to_string(),
        "status_detail": serde_json::to_value(&job.status).unwrap_or(Value::Null),
        "kind": serde_json::to_value(job.kind).unwrap_or(Value::Null),
        "effective_soul_version": job.effective_soul_version,
        "created_at": job.created_at.to_rfc3339(),
        "started_at": job.started_at.map(|t| t.to_rfc3339()),
        "ended_at": job.ended_at.map(|t| t.to_rfc3339()),
        "emitted_span_ids": job.emitted_span_ids.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "final_result": job.final_result.as_ref().map(|o| serde_json::to_value(o).unwrap_or(Value::Null)),
    });

    let human = format!(
        "id:           {}\nsession:      {}\nparent:       {}\nstatus:       {}\nkind:         {:?}\ncreated:      {}\nstarted:      {}\nended:        {}",
        job.id,
        job.session_id,
        job.parent_job_id
            .map(|p| p.to_string())
            .unwrap_or_else(|| "(none)".into()),
        job.status.kind(),
        job.kind,
        job.created_at.to_rfc3339(),
        job.started_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "(not started)".into()),
        job.ended_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "(not ended)".into()),
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
    let job_id = parse_id(id)?;
    mgr.cancel(&job_id, CancelReason::OperatorCancel, vec![])
        .await
        .map_err(|e: aura_job::JobError| CliError::Manager(format!("cancel job: {e}")))?;
    let job = mgr
        .get(&job_id)
        .await
        .map_err(|e: aura_job::JobError| CliError::Manager(format!("reload job: {e}")))?
        .ok_or_else(|| CliError::Manager(format!("job {id} not found after cancel")))?;

    Ok(CommandOutput {
        human: format!("cancelled job {id} (status now {})", job.status.kind()),
        data: Some(json!({
            "cancelled": id,
            "status": job.status.kind().to_string(),
        })),
    })
}
