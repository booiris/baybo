use std::path::PathBuf;
use std::sync::Arc;

use aura_model::SessionId;
use aura_storage::TraceStore;
use serde_json::json;
use tokio::fs;

use crate::cli::TraceCmd;
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: TraceCmd) -> Result<CommandOutput> {
    match cmd {
        TraceCmd::List { session, limit } => list(ctx, session.as_deref(), limit).await,
        TraceCmd::Show { id } => show(ctx, &id).await,
        TraceCmd::Export { id, out, yes } => export(ctx, &id, out.as_deref(), yes).await,
    }
}

fn store(ctx: &CommandContext) -> Result<&Arc<dyn TraceStore>> {
    ctx.trace
        .as_ref()
        .ok_or_else(|| CliError::Manager("trace store is not available in this invocation".into()))
}

fn jobs(ctx: &CommandContext) -> Result<&aura_agent::JobLifecycle> {
    ctx.job
        .as_deref()
        .ok_or_else(|| CliError::Manager("job manager is not available in this invocation".into()))
}

/// Pull the (job_count, step_count, span_count) tuple for one session
/// — handy for both list and show summaries.
async fn session_summary_counts(
    ctx: &CommandContext,
    session_id: &SessionId,
) -> Result<(usize, usize, usize)> {
    let st = store(ctx)?;
    let job_mgr = jobs(ctx)?;
    let session_jobs: Vec<_> = job_mgr
        .list(None)
        .await
        .map_err(|e: aura_job::JobError| CliError::Manager(format!("list jobs: {e}")))?
        .into_iter()
        .filter(|j| &j.session_id == session_id)
        .collect();
    let mut step_count = 0;
    let mut span_count = 0;
    for j in &session_jobs {
        let steps = st
            .list_steps_by_job(&j.id)
            .await
            .map_err(|e: aura_trace::TraceError| CliError::Manager(format!("list steps: {e}")))?;
        for step in &steps {
            let spans =
                st.list_spans_by_step(&step.id)
                    .await
                    .map_err(|e: aura_trace::TraceError| {
                        CliError::Manager(format!("list spans: {e}"))
                    })?;
            span_count += spans.len();
        }
        step_count += steps.len();
    }
    Ok((session_jobs.len(), step_count, span_count))
}

async fn list(ctx: &CommandContext, session: Option<&str>, limit: usize) -> Result<CommandOutput> {
    let job_mgr = jobs(ctx)?;
    let mut sessions: Vec<SessionId> = match session {
        Some(s) => vec![SessionId::from(s)],
        None => {
            let mut seen = std::collections::BTreeSet::new();
            for j in job_mgr
                .list(None)
                .await
                .map_err(|e: aura_job::JobError| CliError::Manager(format!("list jobs: {e}")))?
            {
                seen.insert(j.session_id);
            }
            seen.into_iter().collect()
        }
    };
    sessions.truncate(limit);

    if sessions.is_empty() {
        return Ok(CommandOutput {
            human: "no session traces recorded".into(),
            data: Some(json!({ "traces": [] })),
        });
    }

    let mut rows = Vec::with_capacity(sessions.len());
    let mut human = String::from("session                                 jobs   steps  spans\n");
    for sid in &sessions {
        let (jobs, steps, spans) = session_summary_counts(ctx, sid).await?;
        rows.push(json!({
            "session": sid.to_string(),
            "jobs": jobs,
            "steps": steps,
            "spans": spans,
        }));
        human.push_str(&format!(
            "{:<38}  {:<5}  {:<5}  {}\n",
            sid.to_string(),
            jobs,
            steps,
            spans,
        ));
    }

    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({ "traces": rows })),
    })
}

async fn show(ctx: &CommandContext, id: &str) -> Result<CommandOutput> {
    let session_id = SessionId::from(id);
    let (jobs_n, steps_n, spans_n) = session_summary_counts(ctx, &session_id).await?;
    if jobs_n == 0 {
        return Err(CliError::Manager(format!(
            "no trace recorded for session {id}"
        )));
    }

    let value = json!({
        "session": id,
        "jobs": jobs_n,
        "steps": steps_n,
        "spans": spans_n,
    });

    let human = format!(
        "session:      {}\njobs:         {}\nsteps:        {}\nspans:        {}",
        id, jobs_n, steps_n, spans_n,
    );

    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}

async fn export(
    ctx: &CommandContext,
    id: &str,
    out: Option<&str>,
    yes: bool,
) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && out.is_some() && !yes {
        return Err(CliError::ConfirmationRequired(format!(
            "would write trace for session {id} to {}; re-run with --yes to confirm",
            out.unwrap_or("(stdout)")
        )));
    }
    let st = store(ctx)?;
    let job_mgr = jobs(ctx)?;
    let session_id = SessionId::from(id);
    let session_jobs: Vec<_> = job_mgr
        .list(None)
        .await
        .map_err(|e: aura_job::JobError| CliError::Manager(format!("list jobs: {e}")))?
        .into_iter()
        .filter(|j| j.session_id == session_id)
        .collect();
    if session_jobs.is_empty() {
        return Err(CliError::Manager(format!(
            "no trace recorded for session {id}"
        )));
    }

    let mut tree = Vec::with_capacity(session_jobs.len());
    for job in session_jobs {
        let steps = st
            .list_steps_by_job(&job.id)
            .await
            .map_err(|e: aura_trace::TraceError| CliError::Manager(format!("list steps: {e}")))?;
        let mut step_blocks = Vec::with_capacity(steps.len());
        for step in steps {
            let spans =
                st.list_spans_by_step(&step.id)
                    .await
                    .map_err(|e: aura_trace::TraceError| {
                        CliError::Manager(format!("list spans: {e}"))
                    })?;
            step_blocks.push(json!({ "step": step, "spans": spans }));
        }
        tree.push(json!({ "job": job, "steps": step_blocks }));
    }
    let exported = json!({ "session": id, "jobs": tree });
    let json_text = serde_json::to_string_pretty(&exported)
        .map_err(|e| CliError::Manager(format!("serialize trace: {e}")))?;

    if let Some(path_str) = out {
        let path = PathBuf::from(path_str);
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| CliError::Manager(format!("create parent {parent:?}: {e}")))?;
        }
        fs::write(&path, json_text.as_bytes())
            .await
            .map_err(|e| CliError::Manager(format!("write {path:?}: {e}")))?;
        return Ok(CommandOutput {
            human: format!("wrote trace for session {id} to {path_str}"),
            data: Some(json!({
                "session": id,
                "path": path_str,
                "bytes": json_text.len(),
            })),
        });
    }

    Ok(CommandOutput {
        human: json_text.clone(),
        data: Some(exported),
    })
}
