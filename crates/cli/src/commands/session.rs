use serde_json::{Value, json};

use crate::cli::SessionCmd;
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: SessionCmd) -> Result<CommandOutput> {
    match cmd {
        SessionCmd::List => list(ctx).await,
        SessionCmd::Show { id } => show(ctx, &id).await,
        SessionCmd::History { id } => history(ctx, &id).await,
        SessionCmd::Kill { id, yes } => kill(ctx, &id, yes).await,
    }
}

fn sessions(ctx: &CommandContext) -> Result<&aura_agent::SessionManager> {
    ctx.session.as_deref().ok_or_else(|| {
        CliError::Manager("session manager is not available in this invocation".into())
    })
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
    let typed = aura_model::SessionId::from(id);
    let session = mgr
        .get(&typed)
        .await
        .map_err(|e| CliError::Manager(format!("get session: {e}")))?
        .ok_or_else(|| CliError::Manager(format!("session {id} not found")))?;
    let messages = mgr
        .load_active_session_messages(&typed)
        .await
        .map_err(|e| CliError::Manager(format!("load context: {e}")))?;

    let value = json!({
        "id": session.id.to_string(),
        "user": {
            "id": session.user.id,
            "name": session.user.name,
        },
        "channel": session.channel.to_string(),
        "created_at": session.created_at.to_rfc3339(),
        "last_active": session.last_active.to_rfc3339(),
        "messages": messages.len(),
        "active_skills": session.state.active_skills,
        "compression_count": session.state.compression_count,
    });

    let active_skills_human = if session.state.active_skills.is_empty() {
        "(none)".to_string()
    } else {
        session.state.active_skills.join(", ")
    };

    let human = format!(
        "id:             {}\nuser:           {}\nchannel:        {}\ncreated:        {}\nlast_active:    {}\nmessages:       {}\nactive_skills:  {}\ncompressions:   {}",
        session.id,
        session.user.id,
        session.channel,
        session.created_at.to_rfc3339(),
        session.last_active.to_rfc3339(),
        messages.len(),
        active_skills_human,
        session.state.compression_count,
    );

    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}

async fn history(ctx: &CommandContext, id: &str) -> Result<CommandOutput> {
    let mgr = sessions(ctx)?;
    let typed = aura_model::SessionId::from(id);
    let messages = mgr
        .history(&typed)
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

async fn kill(ctx: &CommandContext, id: &str, yes: bool) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && !yes {
        return Err(CliError::ConfirmationRequired(format!(
            "would delete session {id}; re-run with --yes to confirm"
        )));
    }

    let mgr = sessions(ctx)?;
    let typed = aura_model::SessionId::from(id);
    mgr.delete(&typed)
        .await
        .map_err(|e| CliError::Manager(format!("delete session: {e}")))?;

    Ok(CommandOutput {
        human: format!("deleted session {id}"),
        data: Some(json!({ "deleted": id })),
    })
}
