use baybo_skills::{SkillDefinition, SkillIssueKind, SkillValidation};
use baybo_skills_assessor::{AssessError, AssessedSkill, RiskLevel, SkillAssessor};
use serde_json::{Value, json};

use crate::cli::SkillsCmd;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: SkillsCmd) -> Result<CommandOutput> {
    match cmd {
        SkillsCmd::List => list(ctx),
        SkillsCmd::Info { name } => info(ctx, &name),
        SkillsCmd::Search { query } => search(ctx, query.as_deref().unwrap_or("")),
        SkillsCmd::Check { name } => check(ctx, name.as_deref()).await,
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

/// Outcome of the LLM risk check for a single skill.
enum RiskSummary {
    /// The skill has no on-disk source (e.g., fixture), so nothing to hash.
    NoSource,
    /// The assessor was not wired up (CLI invoked without an LLM client).
    NotConfigured,
    /// Risk check succeeded. May be primary-scope with a full-scope
    /// assessment pending in the background.
    Verdict(AssessedSkill),
    /// Risk check failed (LLM error, unparseable reply, I/O, …). The
    /// skill is still listed — we surface the error rather than blocking
    /// on a failing assessor.
    Error(String),
}

async fn check(ctx: &CommandContext, name: Option<&str>) -> Result<CommandOutput> {
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

    // Run the risk check for each report, reusing the skill definition
    // from the registry. Errors from the assessor are captured per-skill
    // so one failure doesn't tank the whole report.
    let mut risk_summaries: Vec<RiskSummary> = Vec::with_capacity(reports.len());
    for r in &reports {
        let summary = match ctx.skills.get(&r.name) {
            Some(skill) => assess_one(ctx.skill_assessor.as_deref(), &skill).await,
            None => RiskSummary::Error(format!("skill '{}' is no longer registered", r.name)),
        };
        risk_summaries.push(summary);
    }

    let ok_count = reports
        .iter()
        .zip(risk_summaries.iter())
        .filter(|(r, s)| r.ok && !risk_summary_is_blocking(s))
        .count();
    let fail_count = reports.len() - ok_count;

    let rows: Vec<Value> = reports
        .iter()
        .zip(risk_summaries.iter())
        .map(|(r, s)| report_to_json(r, s))
        .collect();

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
    for (r, s) in reports.iter().zip(risk_summaries.iter()) {
        let status = if r.ok && !risk_summary_is_blocking(s) {
            "ok"
        } else {
            "FAIL"
        };
        human.push_str(&format!("  [{status}] {}\n", r.name));
        for issue in &r.issues {
            human.push_str(&format!(
                "      - {}: {}\n",
                kind_label(issue.kind),
                issue.detail
            ));
        }
        push_risk_line(&mut human, s);
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

async fn assess_one(assessor: Option<&SkillAssessor>, skill: &SkillDefinition) -> RiskSummary {
    let Some(assessor) = assessor else {
        return RiskSummary::NotConfigured;
    };
    if skill.source_path.is_none() {
        return RiskSummary::NoSource;
    }
    match assessor.check(skill).await {
        Ok(verdict) => RiskSummary::Verdict(verdict),
        Err(AssessError::NoSourcePath) => RiskSummary::NoSource,
        Err(err) => RiskSummary::Error(err.to_string()),
    }
}

fn risk_summary_is_blocking(s: &RiskSummary) -> bool {
    matches!(
        s,
        RiskSummary::Verdict(a) if a.verdict.level == RiskLevel::Dangerous
    )
}

fn push_risk_line(buf: &mut String, s: &RiskSummary) {
    match s {
        RiskSummary::Verdict(a) => {
            let pending = if a.background_pending {
                " (full-scope assessment in progress)"
            } else {
                ""
            };
            buf.push_str(&format!(
                "      - risk[{scope}]: {level} — {rationale}{pending}\n",
                scope = a.scope.as_str(),
                level = a.verdict.level.as_str(),
                rationale = a.verdict.rationale,
            ));
        }
        RiskSummary::Error(msg) => {
            buf.push_str(&format!("      - risk: unknown — {msg}\n"));
        }
        RiskSummary::NoSource | RiskSummary::NotConfigured => {}
    }
}

fn report_to_json(r: &SkillValidation, risk: &RiskSummary) -> Value {
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
    let risk_json = match risk {
        RiskSummary::Verdict(a) => json!({
            "status": "assessed",
            "scope": a.scope.as_str(),
            "background_pending": a.background_pending,
            "level": a.verdict.level.as_str(),
            "rationale": a.verdict.rationale,
            "model": a.verdict.model,
            "content_hash": a.verdict.content_hash,
            "assessed_at": a.verdict.assessed_at,
        }),
        RiskSummary::Error(msg) => json!({ "status": "error", "detail": msg }),
        RiskSummary::NoSource => json!({ "status": "no_source" }),
        RiskSummary::NotConfigured => json!({ "status": "not_configured" }),
    };
    json!({
        "name": r.name,
        "ok": r.ok && !risk_summary_is_blocking(risk),
        "issues": issues,
        "notes": r.notes,
        "risk": risk_json,
    })
}

fn kind_label(kind: SkillIssueKind) -> &'static str {
    match kind {
        SkillIssueKind::EmptyName => "empty_name",
        SkillIssueKind::InvalidName => "invalid_name",
        SkillIssueKind::EmptyVersion => "empty_version",
        SkillIssueKind::InvalidVersion => "invalid_version",
        SkillIssueKind::EmptyPrompt => "empty_prompt",
        SkillIssueKind::MissingBinary => "missing_binary",
        SkillIssueKind::MissingEnvVar => "missing_env_var",
    }
}
