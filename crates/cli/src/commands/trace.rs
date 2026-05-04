use std::path::PathBuf;

use aura_agent::{JobFilter, QueryApi};
use aura_model::SessionId;
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

fn jobs(ctx: &CommandContext) -> Result<&aura_agent::JobLifecycle> {
    ctx.job
        .as_deref()
        .ok_or_else(|| CliError::Manager("job manager is not available in this invocation".into()))
}

/// Cached `QueryApi` from the context. Built once at context-build
/// time, shared across every trace command in the same invocation.
fn query_api(ctx: &CommandContext) -> Result<&QueryApi> {
    ctx.query_api
        .as_deref()
        .ok_or_else(|| CliError::Manager("query api is not available in this invocation".into()))
}

/// Pull the (job_count, step_count, span_count) tuple for one session
/// — handy for both list and show summaries. Backed by `QueryApi::replay`
/// so the count walk reuses the same fork-aware view as `aura trace export`.
async fn session_summary_counts(
    ctx: &CommandContext,
    session_id: &SessionId,
) -> Result<(usize, usize, usize)> {
    let api = query_api(ctx)?;
    let replay = api
        .replay(session_id, None)
        .await
        .map_err(|e| CliError::Manager(format!("replay session: {e}")))?;
    let mut step_count = 0;
    let mut span_count = 0;
    for j in &replay.jobs {
        step_count += j.steps.len();
        span_count += j.steps.iter().map(|s| s.spans.len()).sum::<usize>();
    }
    Ok((replay.jobs.len(), step_count, span_count))
}

async fn list(ctx: &CommandContext, session: Option<&str>, limit: usize) -> Result<CommandOutput> {
    // `list_jobs` (via QueryApi) is per-session — to enumerate every
    // session that has trace data we still need the raw job-store
    // listing. For a single supplied session id we fall back to the
    // QueryApi path for free.
    let job_mgr = jobs(ctx)?;
    let mut sessions: Vec<SessionId> = match session {
        Some(s) => {
            let sid = SessionId::from(s);
            // Sanity check via QueryApi so the "no trace" message
            // mirrors `show` / `export`.
            let api = query_api(ctx)?;
            let summaries = api
                .list_jobs(&sid, JobFilter::default())
                .await
                .map_err(|e| CliError::Manager(format!("list jobs: {e}")))?;
            if summaries.is_empty() {
                Vec::new()
            } else {
                vec![sid]
            }
        }
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
    let session_id = SessionId::from(id);
    let api = query_api(ctx)?;
    let replay = api
        .replay(&session_id, None)
        .await
        .map_err(|e| CliError::Manager(format!("replay session: {e}")))?;
    if replay.jobs.is_empty() {
        return Err(CliError::Manager(format!(
            "no trace recorded for session {id}"
        )));
    }

    let tree: Vec<serde_json::Value> = replay
        .jobs
        .iter()
        .map(|rj| {
            let steps: Vec<serde_json::Value> = rj
                .steps
                .iter()
                .map(|rs| json!({ "step": rs.step, "spans": rs.spans }))
                .collect();
            json!({ "job": rj.job, "steps": steps })
        })
        .collect();
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
