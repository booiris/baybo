use std::path::PathBuf;
use std::sync::Arc;

use aura_model::{ContentBlock, Role};
use aura_storage::TraceStore;
use aura_trace::{SessionTrace, TraceFilter, snapshot::find_nearest_snapshot};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tokio::fs;

use crate::cli::TraceCmd;
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: TraceCmd) -> Result<CommandOutput> {
    match cmd {
        TraceCmd::List { session, limit } => list(ctx, session.as_deref(), limit).await,
        TraceCmd::Show { id } => show(ctx, &id).await,
        TraceCmd::Snapshot { id, node, full } => snapshot(ctx, &id, node.as_deref(), full).await,
        TraceCmd::Export { id, out, yes } => export(ctx, &id, out.as_deref(), yes).await,
    }
}

fn store(ctx: &CommandContext) -> Result<&Arc<dyn TraceStore>> {
    ctx.trace
        .as_ref()
        .ok_or_else(|| CliError::Manager("trace store is not available in this invocation".into()))
}

fn latest_started_at(trace: &SessionTrace) -> Option<DateTime<Utc>> {
    trace.nodes.values().map(|n| n.span.started_at).max()
}

fn trace_summary(trace: &SessionTrace) -> Value {
    json!({
        "session": trace.session_id,
        "nodes": trace.nodes.len(),
        "active_leaf": trace.active_leaf,
        "latest_started_at": latest_started_at(trace).map(|t| t.to_rfc3339()),
    })
}

async fn list(ctx: &CommandContext, session: Option<&str>, limit: usize) -> Result<CommandOutput> {
    let st = store(ctx)?;
    let filter = TraceFilter {
        session_id: session.map(str::to_string),
        from: None,
        to: None,
    };
    let mut traces = st
        .query_traces(filter)
        .await
        .map_err(|e| CliError::Manager(format!("list traces: {e}")))?;
    traces.sort_by_key(|t| std::cmp::Reverse(latest_started_at(t)));
    traces.truncate(limit);

    if traces.is_empty() {
        return Ok(CommandOutput {
            human: match session {
                Some(s) => format!("no trace recorded for session {s}"),
                None => "no session traces recorded".into(),
            },
            data: Some(json!({ "traces": [] })),
        });
    }

    let rows: Vec<Value> = traces.iter().map(trace_summary).collect();
    let mut human =
        String::from("session                               nodes  latest_started_at\n");
    for t in &traces {
        human.push_str(&format!(
            "{:<37}  {:<5}  {}\n",
            t.session_id,
            t.nodes.len(),
            latest_started_at(t)
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| "(empty)".into()),
        ));
    }

    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({ "traces": rows })),
    })
}

async fn show(ctx: &CommandContext, id: &str) -> Result<CommandOutput> {
    let st = store(ctx)?;
    let trace = st
        .load_trace(id)
        .await
        .map_err(|e| CliError::Manager(format!("load trace: {e}")))?
        .ok_or_else(|| CliError::Manager(format!("no trace recorded for session {id}")))?;

    let kinds: std::collections::BTreeMap<String, usize> =
        trace
            .nodes
            .values()
            .fold(std::collections::BTreeMap::new(), |mut acc, n| {
                let key = format!("{:?}", n.span.kind);
                *acc.entry(key).or_insert(0) += 1;
                acc
            });

    let kinds_json: Value = Value::Object(
        kinds
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect::<serde_json::Map<_, _>>(),
    );

    let value = json!({
        "session": trace.session_id,
        "root": trace.root,
        "active_leaf": trace.active_leaf,
        "nodes": trace.nodes.len(),
        "latest_started_at": latest_started_at(&trace).map(|t| t.to_rfc3339()),
        "span_kinds": kinds_json,
    });

    let mut human = format!(
        "session:      {}\nroot:         {}\nactive_leaf:  {}\nnodes:        {}\nlatest:       {}",
        trace.session_id,
        trace.root,
        trace.active_leaf,
        trace.nodes.len(),
        latest_started_at(&trace)
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| "(empty)".into()),
    );
    if !kinds.is_empty() {
        human.push_str("\nspan kinds:");
        for (k, v) in &kinds {
            human.push_str(&format!("\n  {k:<40}  {v}"));
        }
    }

    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}

async fn snapshot(
    ctx: &CommandContext,
    id: &str,
    node: Option<&str>,
    full: bool,
) -> Result<CommandOutput> {
    let st = store(ctx)?;
    let trace = st
        .load_trace(id)
        .await
        .map_err(|e| CliError::Manager(format!("load trace: {e}")))?
        .ok_or_else(|| CliError::Manager(format!("no trace recorded for session {id}")))?;

    let start_node = node.unwrap_or(&trace.active_leaf).to_string();
    if !trace.nodes.contains_key(&start_node) {
        return Err(CliError::Manager(format!(
            "trace node {start_node} is not part of session {id}"
        )));
    }
    let snap = find_nearest_snapshot(&trace, &start_node)
        .map_err(|e| CliError::Manager(format!("find snapshot: {e}")))?;

    let messages_preview: Vec<Value> = snap
        .messages
        .iter()
        .map(|m| {
            json!({
                "role": role_label(&m.role),
                "blocks": m.content.len(),
                "text_preview": first_text_preview(&m.content, 80),
            })
        })
        .collect();

    let mut payload = json!({
        "session": trace.session_id,
        "start_node": start_node,
        "token_count": snap.token_count,
        "message_count": snap.messages.len(),
        "messages": messages_preview,
    });
    if full {
        payload["messages_full"] = serde_json::to_value(&snap.messages).unwrap_or(Value::Null);
    }

    let human = format!(
        "session:      {}\nstart node:   {}\ntokens:       {}\nmessages:     {}",
        trace.session_id,
        start_node,
        snap.token_count,
        snap.messages.len(),
    );
    Ok(CommandOutput {
        human,
        data: Some(payload),
    })
}

fn role_label(r: &Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn first_text_preview(blocks: &[ContentBlock], max: usize) -> Option<String> {
    blocks.iter().find_map(|b| match b {
        ContentBlock::Text(t) => Some(if t.chars().count() > max {
            let mut s: String = t.chars().take(max).collect();
            s.push('…');
            s
        } else {
            t.clone()
        }),
        _ => None,
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
    let trace = st
        .load_trace(id)
        .await
        .map_err(|e| CliError::Manager(format!("load trace: {e}")))?
        .ok_or_else(|| CliError::Manager(format!("no trace recorded for session {id}")))?;

    let json_text = serde_json::to_string_pretty(&trace)
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
                "session": trace.session_id,
                "path": path_str,
                "bytes": json_text.len(),
            })),
        });
    }

    Ok(CommandOutput {
        human: json_text.clone(),
        data: Some(serde_json::to_value(&trace).unwrap_or(Value::Null)),
    })
}
