use aura_skills::{SkillIssueKind, SkillValidation};
use serde_json::{Value, json};

use crate::cli::SkillsCmd;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub fn handle(ctx: &CommandContext, cmd: SkillsCmd) -> Result<CommandOutput> {
    match cmd {
        SkillsCmd::List => list(ctx),
        SkillsCmd::Info { name } => info(ctx, &name),
        SkillsCmd::Search { query } => search(ctx, query.as_deref().unwrap_or("")),
        SkillsCmd::Check { name } => check(ctx, name.as_deref()),
    }
}

fn list(ctx: &CommandContext) -> Result<CommandOutput> {
    let mut names = ctx.skills.list();
    names.sort();

    let human = if names.is_empty() {
        "(no skills registered)".to_string()
    } else {
        let mut buf = String::from("NAME\n");
        for n in &names {
            buf.push_str(n);
            buf.push('\n');
        }
        buf.trim_end().to_string()
    };
    Ok(CommandOutput::structured(
        human,
        &json!({ "skills": names }),
    ))
}

fn info(ctx: &CommandContext, name: &str) -> Result<CommandOutput> {
    let skill = ctx
        .skills
        .get(name)
        .ok_or_else(|| CliError::UnknownCommand(format!("skill: {name}")))?;
    let value = serde_json::to_value(skill)?;
    let human = serde_json::to_string_pretty(&value)?;
    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}

fn search(ctx: &CommandContext, query: &str) -> Result<CommandOutput> {
    let hits = ctx.skills.search(query);

    let rows: Vec<Value> = hits
        .iter()
        .map(|s| {
            json!({
                "name": s.name,
                "version": s.version,
                "description": s.description,
            })
        })
        .collect();

    let human = if hits.is_empty() {
        if query.is_empty() {
            "(no skills registered)".to_string()
        } else {
            format!("no skills match '{query}'")
        }
    } else {
        let mut buf = format!("{} match(es)\n", hits.len());
        for s in &hits {
            buf.push_str(&format!("  {:<24}  {}\n", s.name, s.description));
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

fn check(ctx: &CommandContext, name: Option<&str>) -> Result<CommandOutput> {
    let reports: Vec<SkillValidation> = match name {
        Some(n) => {
            let v = ctx
                .skills
                .validate(n)
                .ok_or_else(|| CliError::UnknownCommand(format!("skill: {n}")))?;
            vec![v]
        }
        None => ctx.skills.validate_all(),
    };

    let ok_count = reports.iter().filter(|r| r.ok).count();
    let fail_count = reports.len() - ok_count;

    let rows: Vec<Value> = reports.iter().map(report_to_json).collect();

    let mut human = if reports.is_empty() {
        "(no skills to check)".to_string()
    } else {
        format!(
            "{} skill(s): {} ok, {} failing\n",
            reports.len(),
            ok_count,
            fail_count
        )
    };
    for r in &reports {
        let status = if r.ok { "ok" } else { "FAIL" };
        human.push_str(&format!("  [{status}] {}\n", r.name));
        for issue in &r.issues {
            human.push_str(&format!(
                "      - {}: {}\n",
                kind_label(issue.kind),
                issue.detail
            ));
        }
    }

    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({
            "total": reports.len(),
            "ok": ok_count,
            "failing": fail_count,
            "reports": rows,
        })),
    })
}

fn report_to_json(r: &SkillValidation) -> Value {
    let issues: Vec<Value> = r
        .issues
        .iter()
        .map(|i| {
            json!({
                "kind": kind_label(i.kind),
                "detail": i.detail,
            })
        })
        .collect();
    json!({
        "name": r.name,
        "ok": r.ok,
        "issues": issues,
        "notes": r.notes,
    })
}

fn kind_label(kind: SkillIssueKind) -> &'static str {
    match kind {
        SkillIssueKind::EmptyName => "empty_name",
        SkillIssueKind::EmptyVersion => "empty_version",
        SkillIssueKind::EmptyPrompt => "empty_prompt",
        SkillIssueKind::MissingBinary => "missing_binary",
        SkillIssueKind::MissingEnvVar => "missing_env_var",
    }
}
