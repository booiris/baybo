use std::path::Path;

use aura_security::LeakAction;
use serde_json::{Value, json};

use crate::cli::{LeaksCmd, SecurityCmd};
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: SecurityCmd) -> Result<CommandOutput> {
    match cmd {
        SecurityCmd::Audit => audit(ctx),
        SecurityCmd::Leaks { cmd } => match cmd {
            LeaksCmd::Check { path } => leaks_check(ctx, &path),
        },
    }
}

fn audit(ctx: &CommandContext) -> Result<CommandOutput> {
    let gateway = ctx.security.as_deref().ok_or_else(|| {
        CliError::Manager("security gateway is not available in this invocation".into())
    })?;
    let report = gateway.audit();

    let rules: Vec<Value> = report
        .rules
        .iter()
        .map(|r| {
            json!({
                "name": r.name,
                "action": action_label(&r.action),
            })
        })
        .collect();

    let mut human = format!(
        "leak rules: {} total ({} block, {} replace)\n",
        report.total_rules, report.block_rules, report.replace_rules
    );
    for r in &report.rules {
        human.push_str(&format!("  {:<30}  {}\n", r.name, action_label(&r.action)));
    }
    human.push_str(&format!(
        "\nsecret vault:\n  master_key_configured: {}\n",
        report.secret_vault.master_key_configured
    ));

    let data = json!({
        "leak_rules": {
            "total": report.total_rules,
            "block": report.block_rules,
            "replace": report.replace_rules,
            "rules": rules,
        },
        "secret_vault": {
            "master_key_configured": report.secret_vault.master_key_configured,
        },
    });

    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(data),
    })
}

fn leaks_check(ctx: &CommandContext, path: &str) -> Result<CommandOutput> {
    let detector = ctx.leak_detector.as_deref().ok_or_else(|| {
        CliError::Manager("leak detector is not available in this invocation".into())
    })?;

    let result = detector
        .check_file(Path::new(path))
        .map_err(|e| CliError::Io(format!("read {path}: {e}")))?;

    let hits: Vec<Value> = result
        .replacements
        .iter()
        .map(|r| json!({ "rule": r.rule_name, "placeholder": r.placeholder }))
        .collect();

    let data = json!({
        "path": path,
        "blocked": result.blocked,
        "block_reason": result.block_reason,
        "hit_count": hits.len(),
        "hits": hits,
    });

    let human = if result.blocked {
        format!(
            "BLOCKED  {path}\n  reason: {}",
            result.block_reason.as_deref().unwrap_or("(no reason)")
        )
    } else if hits.is_empty() {
        format!("clean    {path}  (no matches)")
    } else {
        let mut out = format!("hits     {path}  ({} match(es))\n", hits.len());
        for r in &result.replacements {
            out.push_str(&format!("  {}\n", r.rule_name));
        }
        out.trim_end().to_string()
    };

    Ok(CommandOutput {
        human,
        data: Some(data),
    })
}

fn action_label(action: &LeakAction) -> &'static str {
    match action {
        LeakAction::Block => "block",
        LeakAction::Replace => "replace",
    }
}
