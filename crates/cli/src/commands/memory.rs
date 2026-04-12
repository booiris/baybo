use aura_model::{MemoryCategory, MemoryEntry};
use serde_json::{Value, json};

use crate::cli::MemoryCmd;
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: MemoryCmd) -> Result<CommandOutput> {
    match cmd {
        MemoryCmd::List { user, limit } => list(ctx, user.as_deref(), limit).await,
        MemoryCmd::Search { query, user, limit } => {
            search(ctx, user.as_deref(), &query, limit).await
        }
        MemoryCmd::Show { id } => show(ctx, &id).await,
        MemoryCmd::Promote { id, to, yes } => promote(ctx, &id, to, yes).await,
        MemoryCmd::Clear { session, yes } => clear(ctx, &session, yes).await,
    }
}

fn manager(ctx: &CommandContext) -> Result<&aura_agent::MemoryManager> {
    ctx.memory.as_deref().ok_or_else(|| {
        CliError::Manager("memory manager is not available in this invocation".into())
    })
}

fn category_label(c: &MemoryCategory) -> &'static str {
    match c {
        MemoryCategory::UserPreference => "preference",
        MemoryCategory::KeyFact => "fact",
    }
}

fn entry_summary(e: &MemoryEntry) -> Value {
    json!({
        "id": e.id,
        "user": e.user_id,
        "category": category_label(&e.category),
        "importance": e.importance,
        "content": e.content,
        "source_session_id": e.source_session_id,
        "created_at": e.created_at.to_rfc3339(),
        "last_accessed": e.last_accessed.to_rfc3339(),
    })
}

fn truncate_preview(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

async fn list(ctx: &CommandContext, user: Option<&str>, limit: usize) -> Result<CommandOutput> {
    let mgr = manager(ctx)?;
    let mut entries = mgr
        .list(user)
        .await
        .map_err(|e| CliError::Manager(format!("list memories: {e}")))?;
    entries.sort_by(|a, b| {
        b.importance
            .partial_cmp(&a.importance)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.last_accessed.cmp(&a.last_accessed))
    });
    entries.truncate(limit);

    if entries.is_empty() {
        return Ok(CommandOutput {
            human: match user {
                Some(u) => format!("no memories recorded for user {u}"),
                None => "no memories recorded".into(),
            },
            data: Some(json!({ "entries": [] })),
        });
    }

    let rows: Vec<Value> = entries.iter().map(entry_summary).collect();
    let mut human = String::from(
        "id                                    user       cat         imp    content\n",
    );
    for e in &entries {
        human.push_str(&format!(
            "{:<38}  {:<9}  {:<10}  {:<5.2}  {}\n",
            e.id,
            e.user_id,
            category_label(&e.category),
            e.importance,
            truncate_preview(&e.content, 60),
        ));
    }

    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({ "entries": rows })),
    })
}

async fn search(
    ctx: &CommandContext,
    user: Option<&str>,
    query: &str,
    limit: usize,
) -> Result<CommandOutput> {
    let mgr = manager(ctx)?;
    let entries = mgr
        .search(user, query, limit)
        .await
        .map_err(|e| CliError::Manager(format!("search memories: {e}")))?;

    if entries.is_empty() {
        return Ok(CommandOutput {
            human: format!("no memories matched query {query:?}"),
            data: Some(json!({ "entries": [], "query": query })),
        });
    }

    let rows: Vec<Value> = entries.iter().map(entry_summary).collect();
    let mut human = format!("{} match(es) for {query:?}:\n", entries.len());
    for e in &entries {
        human.push_str(&format!(
            "  [{}] ({:.2}) {}\n",
            e.id,
            e.importance,
            truncate_preview(&e.content, 80),
        ));
    }

    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({ "entries": rows, "query": query })),
    })
}

async fn show(ctx: &CommandContext, id: &str) -> Result<CommandOutput> {
    let mgr = manager(ctx)?;
    let entry = mgr
        .get(id)
        .await
        .map_err(|e| CliError::Manager(format!("get memory: {e}")))?
        .ok_or_else(|| CliError::Manager(format!("memory entry {id} not found")))?;

    let value = entry_summary(&entry);
    let human = format!(
        "id:           {}\nuser:         {}\ncategory:     {}\nimportance:   {:.2}\nsession:      {}\ncreated_at:   {}\nlast_access:  {}\ncontent:      {}",
        entry.id,
        entry.user_id,
        category_label(&entry.category),
        entry.importance,
        entry.source_session_id.as_deref().unwrap_or("(none)"),
        entry.created_at.to_rfc3339(),
        entry.last_accessed.to_rfc3339(),
        entry.content,
    );

    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}

async fn promote(ctx: &CommandContext, id: &str, to: f32, yes: bool) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && !yes {
        return Err(CliError::ConfirmationRequired(format!(
            "would set memory {id} importance to {to:.2}; re-run with --yes to confirm"
        )));
    }
    let mgr = manager(ctx)?;
    let updated = mgr
        .set_importance(id, to)
        .await
        .map_err(|e| CliError::Manager(format!("promote memory: {e}")))?;
    Ok(CommandOutput {
        human: format!("set memory {id} importance to {:.2}", updated.importance),
        data: Some(json!({
            "id": updated.id,
            "importance": updated.importance,
        })),
    })
}

async fn clear(ctx: &CommandContext, session_id: &str, yes: bool) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && !yes {
        return Err(CliError::ConfirmationRequired(format!(
            "would clear every memory recorded from session {session_id}; \
             re-run with --yes to confirm"
        )));
    }
    let mgr = manager(ctx)?;
    let removed = mgr
        .delete_for_session(session_id)
        .await
        .map_err(|e| CliError::Manager(format!("clear memories: {e}")))?;
    Ok(CommandOutput {
        human: format!(
            "cleared {removed} memor{} from session {session_id}",
            if removed == 1 { "y" } else { "ies" }
        ),
        data: Some(json!({
            "session": session_id,
            "cleared": removed,
        })),
    })
}
