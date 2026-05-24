use std::path::PathBuf;

use aura_model::SessionId;
use aura_query::QueryApi;
use aura_session::StoredMessage;
use serde_json::{Value, json};
use tokio::fs;

use crate::cli::SessionCmd;
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: SessionCmd) -> Result<CommandOutput> {
    match cmd {
        SessionCmd::List => list(ctx).await,
        SessionCmd::Show { id } => show(ctx, &id).await,
        SessionCmd::History {
            id,
            include_superseded,
            superseded_only,
        } => history(ctx, &id, include_superseded, superseded_only).await,
        SessionCmd::Export { id, out, yes } => export(ctx, &id, out.as_deref(), yes).await,
    }
}

fn sessions(ctx: &CommandContext) -> Result<&aura_agent::SessionManager> {
    ctx.session.as_deref().ok_or_else(|| {
        CliError::Manager("session manager is not available in this invocation".into())
    })
}

/// Trace replay is optional on `show` — argv-light boots that lack
/// `QueryApi` still surface metadata + message count gracefully.
async fn try_replay_counts(
    ctx: &CommandContext,
    session_id: &SessionId,
) -> Option<(usize, usize, usize)> {
    let api: &QueryApi = ctx.query_api.as_deref()?;
    let replay = api.replay(session_id, None).await.ok()?;
    let mut steps = 0;
    let mut spans = 0;
    for j in &replay.jobs {
        steps += j.steps.len();
        spans += j.steps.iter().map(|s| s.spans.len()).sum::<usize>();
    }
    Some((replay.jobs.len(), steps, spans))
}

async fn list(ctx: &CommandContext) -> Result<CommandOutput> {
    let mgr = sessions(ctx)?;
    let sessions = mgr
        .list()
        .await
        .map_err(|e| CliError::Manager(format!("list sessions: {e}")))?;

    if sessions.is_empty() {
        return Ok(CommandOutput {
            human: "no sessions".to_string(),
            data: Some(json!({ "sessions": [] })),
        });
    }

    let rows: Vec<Value> = sessions
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "user": s.user.id,
                "channel": s.channel.to_string(),
                "last_active": s.last_active.to_rfc3339(),
            })
        })
        .collect();

    let mut human = String::from("id                                    channel   last_active\n");
    for s in &sessions {
        human.push_str(&format!(
            "{:<38}  {:<8}  {}\n",
            s.id,
            s.channel.to_string(),
            s.last_active.to_rfc3339(),
        ));
    }

    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({ "sessions": rows })),
    })
}

async fn show(ctx: &CommandContext, id: &str) -> Result<CommandOutput> {
    let mgr = sessions(ctx)?;
    let typed = SessionId::from(id);
    let session = mgr
        .get(&typed)
        .await
        .map_err(|e| CliError::Manager(format!("get session: {e}")))?
        .ok_or_else(|| CliError::Manager(format!("session {id} not found")))?;
    let messages = mgr
        .load_active_session_messages(&typed)
        .await
        .map_err(|e| CliError::Manager(format!("load context: {e}")))?;
    let trace_counts = try_replay_counts(ctx, &typed).await;

    let mut value = json!({
        "id": session.id.to_string(),
        "user": {
            "id": session.user.id,
            "name": session.user.name,
        },
        "channel": session.channel.to_string(),
        "created_at": session.created_at.to_rfc3339(),
        "last_active": session.last_active.to_rfc3339(),
        "messages": messages.len(),
        "called_skills": aura_context::scan_skill_calls(&messages),
        "compression_count": session.state.compression_count,
    });
    if let Some((jobs, steps, spans)) = trace_counts
        && let Value::Object(ref mut map) = value
    {
        map.insert(
            "trace".into(),
            json!({ "jobs": jobs, "steps": steps, "spans": spans }),
        );
    }

    let called_skills = aura_context::scan_skill_calls(&messages);
    let called_skills_human = if called_skills.is_empty() {
        "(none)".to_string()
    } else {
        called_skills.join(", ")
    };
    let trace_human = match trace_counts {
        Some((j, s, p)) => format!("\ntrace:          {j} jobs · {s} steps · {p} spans"),
        None => String::new(),
    };

    let human = format!(
        "id:             {}\nuser:           {}\nchannel:        {}\ncreated:        {}\nlast_active:    {}\nmessages:       {}\ncalled_skills:  {}\ncompressions:   {}{}",
        session.id,
        session.user.id,
        session.channel,
        session.created_at.to_rfc3339(),
        session.last_active.to_rfc3339(),
        messages.len(),
        called_skills_human,
        session.state.compression_count,
        trace_human,
    );

    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}

async fn history(
    ctx: &CommandContext,
    id: &str,
    include_superseded: bool,
    superseded_only: bool,
) -> Result<CommandOutput> {
    let mgr = sessions(ctx)?;
    let typed = SessionId::from(id);

    if !(include_superseded || superseded_only) {
        return active_history(mgr, id, &typed).await;
    }
    full_history(mgr, id, &typed, superseded_only).await
}

async fn active_history(
    mgr: &aura_agent::SessionManager,
    id: &str,
    typed: &SessionId,
) -> Result<CommandOutput> {
    let messages = mgr
        .history(typed)
        .await
        .map_err(|e| CliError::Manager(format!("session history: {e}")))?;

    let count = messages.len();
    let value = serde_json::to_value(&messages)
        .map_err(|e| CliError::Serialization(format!("serialize messages: {e}")))?;

    let human = if count == 0 {
        format!("session {id}: no messages")
    } else {
        let mut buf = format!("session {id}: {count} messages\n");
        for (i, msg) in messages.iter().enumerate() {
            buf.push_str(&format!(
                "  [{i}] {:?} · {} blocks\n",
                msg.role,
                msg.content.len()
            ));
        }
        buf.trim_end().to_string()
    };

    Ok(CommandOutput {
        human,
        data: Some(json!({ "session": id, "messages": value })),
    })
}

/// Walk the full message log including compaction-superseded rows.
/// Each row carries `superseded_by` — `None` means active, `Some(n)`
/// means a later compaction at ordinal `n` replaced this row.
async fn full_history(
    mgr: &aura_agent::SessionManager,
    id: &str,
    typed: &SessionId,
    superseded_only: bool,
) -> Result<CommandOutput> {
    let all = mgr
        .load_session_messages_with_supersede(typed)
        .await
        .map_err(|e| CliError::Manager(format!("session history: {e}")))?;
    let filtered: Vec<&StoredMessage> = if superseded_only {
        all.iter().filter(|m| m.superseded_by.is_some()).collect()
    } else {
        all.iter().collect()
    };

    if filtered.is_empty() {
        let human = if superseded_only {
            format!("session {id}: no superseded messages")
        } else {
            format!("session {id}: no messages")
        };
        return Ok(CommandOutput {
            human,
            data: Some(json!({ "session": id, "messages": [] })),
        });
    }

    let active_count = all.iter().filter(|m| m.superseded_by.is_none()).count();
    let total = all.len();
    let superseded_count = total - active_count;

    let header = if superseded_only {
        let plural = if filtered.len() == 1 { "" } else { "s" };
        format!(
            "session {id}: {} superseded message{plural}",
            filtered.len()
        )
    } else {
        format!(
            "session {id}: {total} messages ({active_count} active, {superseded_count} superseded)"
        )
    };

    let mut buf = format!("{header}\n");
    for m in &filtered {
        let marker = match m.superseded_by {
            Some(n) => format!("[→ #{n}]"),
            None => "[active]".into(),
        };
        buf.push_str(&format!(
            "  [#{}] {:?} · {} blocks · {marker}\n",
            m.ordinal,
            m.message.role,
            m.message.content.len()
        ));
    }

    let rows: Vec<Value> = filtered
        .iter()
        .map(|m| {
            json!({
                "ordinal": m.ordinal,
                "superseded_by": m.superseded_by,
                "message": m.message,
            })
        })
        .collect();

    Ok(CommandOutput {
        human: buf.trim_end().to_string(),
        data: Some(json!({ "session": id, "messages": rows })),
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

    let api = ctx
        .query_api
        .as_deref()
        .ok_or_else(|| CliError::Manager("query api is not available in this invocation".into()))?;
    let session_id = SessionId::from(id);
    let replay = api
        .replay(&session_id, None)
        .await
        .map_err(|e| CliError::Manager(format!("replay session: {e}")))?;
    if replay.jobs.is_empty() {
        return Err(CliError::Manager(format!(
            "no trace recorded for session {id}"
        )));
    }

    let tree: Vec<Value> = replay
        .jobs
        .iter()
        .map(|rj| {
            let steps: Vec<Value> = rj
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
