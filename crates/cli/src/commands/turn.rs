use baybo_model::TurnId;
use baybo_turn::{CancelReason, TurnStatusKind};
use serde_json::{Value, json};

use crate::cli::{TurnCmd, TurnStatusArg};
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: TurnCmd) -> Result<CommandOutput> {
    match cmd {
        TurnCmd::List { status } => list(ctx, status.map(Into::into)).await,
        TurnCmd::Show { id } => show(ctx, &id).await,
        TurnCmd::Cancel { id, yes } => cancel(ctx, &id, yes).await,
    }
}

fn turns(ctx: &CommandContext) -> Result<&baybo_turn::TurnLifecycle> {
    ctx.turn
        .as_deref()
        .ok_or_else(|| CliError::Manager("turn manager is not available in this invocation".into()))
}

impl From<TurnStatusArg> for TurnStatusKind {
    fn from(v: TurnStatusArg) -> Self {
        match v {
            TurnStatusArg::Pending => TurnStatusKind::Pending,
            TurnStatusArg::InProgress => TurnStatusKind::InProgress,
            TurnStatusArg::Stuck => TurnStatusKind::Stuck,
            TurnStatusArg::Cancelled => TurnStatusKind::Cancelled,
            TurnStatusArg::Failed => TurnStatusKind::Failed,
            TurnStatusArg::Completed => TurnStatusKind::Completed,
        }
    }
}

fn parse_id(id: &str) -> Result<TurnId> {
    id.parse()
        .map_err(|e| CliError::Manager(format!("invalid turn id '{id}': {e}")))
}

async fn list(ctx: &CommandContext, status: Option<TurnStatusKind>) -> Result<CommandOutput> {
    let mgr = turns(ctx)?;
    let turns = mgr
        .list(status)
        .await
        .map_err(|e: baybo_turn::TurnError| CliError::Manager(format!("list turns: {e}")))?;

    if turns.is_empty() {
        let label = status
            .map(|s| format!("no turns with status {s}"))
            .unwrap_or_else(|| "no turns".to_string());
        return Ok(CommandOutput {
            human: label,
            data: Some(json!({ "turns": [] })),
        });
    }

    let rows: Vec<Value> = turns
        .iter()
        .map(|j| {
            json!({
                "id": j.id.to_string(),
                "session": j.session_id.to_string(),
                "status": j.status.kind().to_string(),
                "input_kind": serde_json::to_value(j.input_kind()).unwrap_or(Value::Null),
                "origin": serde_json::to_value(j.origin).unwrap_or(Value::Null),
                "created_at": j.created_at.to_rfc3339(),
                "parent": j.parent_turn_id.map(|p| p.to_string()),
            })
        })
        .collect();

    let mut human =
        String::from("id                          session     status       created_at\n");
    for j in &turns {
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
        data: Some(json!({ "turns": rows })),
    })
}

async fn show(ctx: &CommandContext, id: &str) -> Result<CommandOutput> {
    let mgr = turns(ctx)?;
    let turn_id = parse_id(id)?;
    let turn = mgr
        .get(&turn_id)
        .await
        .map_err(|e: baybo_turn::TurnError| CliError::Manager(format!("get turn: {e}")))?
        .ok_or_else(|| CliError::Manager(format!("turn {id} not found")))?;

    let value = json!({
        "id": turn.id.to_string(),
        "session": turn.session_id.to_string(),
        "parent": turn.parent_turn_id.map(|p| p.to_string()),
        "status": turn.status.kind().to_string(),
        "status_detail": serde_json::to_value(&turn.status).unwrap_or(Value::Null),
        "input_kind": serde_json::to_value(turn.input_kind()).unwrap_or(Value::Null),
        "origin": serde_json::to_value(turn.origin).unwrap_or(Value::Null),
        "created_at": turn.created_at.to_rfc3339(),
        "started_at": turn.started_at.map(|t| t.to_rfc3339()),
        "ended_at": turn.ended_at.map(|t| t.to_rfc3339()),
        "emitted_span_ids": turn.emitted_span_ids.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "final_result": turn.final_result.as_ref().map(|o| serde_json::to_value(o).unwrap_or(Value::Null)),
    });

    let human = format!(
        "id:           {}\nsession:      {}\nparent:       {}\nstatus:       {}\ninput kind:   {:?}\norigin:       {:?}\ncreated:      {}\nstarted:      {}\nended:        {}",
        turn.id,
        turn.session_id,
        turn.parent_turn_id
            .map(|p| p.to_string())
            .unwrap_or_else(|| "(none)".into()),
        turn.status.kind(),
        turn.input_kind(),
        turn.origin,
        turn.created_at.to_rfc3339(),
        turn.started_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "(not started)".into()),
        turn.ended_at
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
            "would cancel turn {id}; re-run with --yes to confirm"
        )));
    }

    let mgr = turns(ctx)?;
    let turn_id = parse_id(id)?;
    mgr.cancel(&turn_id, CancelReason::OperatorCancel, vec![])
        .await
        .map_err(|e: baybo_turn::TurnError| CliError::Manager(format!("cancel turn: {e}")))?;
    let turn = mgr
        .get(&turn_id)
        .await
        .map_err(|e: baybo_turn::TurnError| CliError::Manager(format!("reload turn: {e}")))?
        .ok_or_else(|| CliError::Manager(format!("turn {id} not found after cancel")))?;

    Ok(CommandOutput {
        human: format!("cancelled turn {id} (status now {})", turn.status.kind()),
        data: Some(json!({
            "cancelled": id,
            "status": turn.status.kind().to_string(),
        })),
    })
}
