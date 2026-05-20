//! `aura agents` — typed-subagent observability.
//!
//! Three reads:
//!
//! - `list` — registered profiles + default tier.
//! - `info <name>` — full profile metadata including system prompt.
//! - `in-flight` — live fan-out limiter snapshot (per-root counts).
//!
//! Argv contexts that didn't wire the runtime get `(no live runtime)`
//! for `in-flight`; the catalogue subcommands work either way since
//! the registry is always populated (defaults to empty).

use aura_subagent::{SubagentProfile, SubagentProfileSummary};
use serde_json::{Value, json};

use crate::cli::AgentsCmd;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

fn summary_row(p: &SubagentProfileSummary) -> Value {
    json!({
        "name": p.name,
        "default_tier": p.default_tier.map(|t| t.as_str()),
        "description": p.description,
    })
}

pub async fn handle(ctx: &CommandContext, cmd: AgentsCmd) -> Result<CommandOutput> {
    match cmd {
        AgentsCmd::List => list(ctx),
        AgentsCmd::Info { name } => info(ctx, &name),
        AgentsCmd::Search { query } => search(ctx, query.as_deref().unwrap_or("")),
        AgentsCmd::InFlight => in_flight(ctx),
    }
}

fn list(ctx: &CommandContext) -> Result<CommandOutput> {
    let profiles = ctx.subagent_profiles.all_summaries_sorted();
    let rows: Vec<Value> = profiles.iter().map(summary_row).collect();
    let human = if profiles.is_empty() {
        "(no subagent profiles registered)".to_string()
    } else {
        let mut buf = String::from("NAME                      TIER       DESCRIPTION\n");
        for p in &profiles {
            let tier = p.default_tier.map(|t| t.as_str()).unwrap_or("-");
            let first_desc_line = p.description.lines().next().unwrap_or("");
            buf.push_str(&format!(
                "{:<25} {:<10} {}\n",
                p.name, tier, first_desc_line
            ));
        }
        buf.trim_end().to_string()
    };
    Ok(CommandOutput::structured(
        human,
        &json!({ "profiles": rows }),
    ))
}

fn info(ctx: &CommandContext, name: &str) -> Result<CommandOutput> {
    let profile: SubagentProfile = ctx
        .subagent_profiles
        .get(name)
        .ok_or_else(|| CliError::UnknownCommand(format!("subagent profile: {name}")))?;
    let value = serde_json::to_value(&profile)?;
    let human = serde_json::to_string_pretty(&value)?;
    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}

fn search(ctx: &CommandContext, query: &str) -> Result<CommandOutput> {
    let hits = ctx.subagent_profiles.search(query);
    let rows: Vec<Value> = hits.iter().map(summary_row).collect();

    let human = if hits.is_empty() {
        if query.trim().is_empty() {
            "(no subagent profiles registered)".to_string()
        } else {
            format!("no subagent profiles match '{query}'")
        }
    } else {
        let mut buf = format!("{} match(es)\n", hits.len());
        for p in &hits {
            let first = p.description.lines().next().unwrap_or("");
            buf.push_str(&format!("  {:<24}  {}\n", p.name, first));
        }
        buf.trim_end().to_string()
    };

    Ok(CommandOutput {
        human,
        data: Some(json!({
            "query": query,
            "hit_count": hits.len(),
            "hits": rows,
        })),
    })
}

fn in_flight(ctx: &CommandContext) -> Result<CommandOutput> {
    let Some(limiter) = ctx.subagent_dispatch_limiter.as_ref() else {
        return Ok(CommandOutput::structured(
            "(no live runtime — `aura agents in-flight` requires a running gateway / TUI)"
                .to_string(),
            &json!({ "live": false, "in_flight": [] }),
        ));
    };
    let mut snapshot = limiter.snapshot();
    snapshot.sort_by(|a, b| a.0.as_ref().cmp(b.0.as_ref()));
    let rows: Vec<Value> = snapshot
        .iter()
        .map(|(root, count)| {
            json!({
                "root_session_id": root.as_ref(),
                "in_flight": *count,
            })
        })
        .collect();
    let total: u32 = snapshot.iter().map(|(_, c)| *c).sum();
    let human = if snapshot.is_empty() {
        "(no subagents in flight)".to_string()
    } else {
        let mut buf = format!(
            "{} in-flight subagent(s) across {} root session(s)\n",
            total,
            snapshot.len()
        );
        buf.push_str("ROOT_SESSION_ID                            COUNT\n");
        for (root, count) in &snapshot {
            buf.push_str(&format!("{:<42} {}\n", root.as_ref(), count));
        }
        buf.trim_end().to_string()
    };
    Ok(CommandOutput {
        human,
        data: Some(json!({
            "live": true,
            "total": total,
            "in_flight": rows,
        })),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::OutputFormat;
    use aura_config::AuraConfig;
    use aura_subagent::SubagentRegistry;
    use aura_tools::{FanOutLimiter, SubagentDispatchLimiter};
    use std::sync::Arc;

    fn ctx_with(
        registry: Arc<SubagentRegistry>,
        limiter: Option<Arc<dyn SubagentDispatchLimiter>>,
    ) -> CommandContext {
        let mut builder = crate::context::ContextBuilder::new(Arc::new(AuraConfig::default()))
            .subagent_profiles(registry);
        if let Some(l) = limiter {
            builder = builder.subagent_dispatch_limiter(l);
        }
        builder.build()
    }

    #[test]
    fn list_empty_registry_returns_friendly_human() {
        let ctx = ctx_with(Arc::new(SubagentRegistry::new()), None);
        let out = list(&ctx).unwrap();
        assert!(out.human.contains("no subagent profiles registered"));
    }

    #[test]
    fn list_returns_every_builtin_profile_with_tier() {
        let reg = SubagentRegistry::new();
        reg.register_builtins();
        let ctx = ctx_with(Arc::new(reg), None);
        let out = list(&ctx).unwrap();
        for expected in ["general-purpose", "explorer", "planner", "reviewer"] {
            assert!(
                out.human.contains(expected),
                "list missing built-in `{expected}`:\n{}",
                out.human
            );
        }
        // Per-tier defaults surface in the human row.
        assert!(out.human.contains("fast"));
        assert!(out.human.contains("deep"));
    }

    #[test]
    fn info_unknown_name_returns_unknown_command_error() {
        let ctx = ctx_with(Arc::new(SubagentRegistry::new()), None);
        let err = info(&ctx, "ghost").unwrap_err();
        match err {
            CliError::UnknownCommand(msg) => assert!(msg.contains("ghost")),
            other => panic!("expected UnknownCommand, got {other:?}"),
        }
    }

    #[test]
    fn info_known_profile_renders_full_metadata() {
        let reg = SubagentRegistry::new();
        reg.register_builtins();
        let ctx = ctx_with(Arc::new(reg), None);
        let out = info(&ctx, "explorer").unwrap();
        assert!(out.human.contains("\"name\""));
        assert!(out.human.contains("\"explorer\""));
        assert!(out.human.contains("\"system_prompt\""));
    }

    #[test]
    fn search_filters_by_description_substring() {
        let reg = SubagentRegistry::new();
        reg.register_builtins();
        let ctx = ctx_with(Arc::new(reg), None);
        // "review" is in reviewer's description; should hit only it.
        let out = search(&ctx, "review").unwrap();
        assert!(out.human.contains("reviewer"));
        assert!(!out.human.contains("explorer"));
    }

    #[test]
    fn in_flight_without_limiter_reports_no_runtime() {
        let ctx = ctx_with(Arc::new(SubagentRegistry::new()), None);
        let out = in_flight(&ctx).unwrap();
        assert!(out.human.contains("no live runtime"));
        let data = out.data.unwrap();
        assert_eq!(data["live"], serde_json::Value::Bool(false));
    }

    #[test]
    fn in_flight_with_populated_limiter_reports_counts() {
        let limiter = Arc::new(FanOutLimiter::new());
        let root_a = aura_model::SessionId::from("root-a");
        let root_b = aura_model::SessionId::from("root-b");
        let _ = limiter.try_reserve(&root_a, 4);
        let _ = limiter.try_reserve(&root_a, 4);
        let _ = limiter.try_reserve(&root_b, 4);
        let ctx = ctx_with(
            Arc::new(SubagentRegistry::new()),
            Some(limiter.clone() as Arc<dyn SubagentDispatchLimiter>),
        );
        let out = in_flight(&ctx).unwrap();
        assert!(out.human.contains("3 in-flight subagent(s) across 2 root session(s)"));
        assert!(out.human.contains("root-a"));
        assert!(out.human.contains("root-b"));
        // Format invariant — `OutputFormat::Human` is exercised when
        // the CLI prints this; the helper still returns a non-empty
        // human field regardless of the eventual format pick.
        let _ = OutputFormat::Human;
    }
}

