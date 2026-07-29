use std::str::FromStr;

use baybo_cost::{CostSummary, TimeRange};
use baybo_model::{SessionId, TurnId};
use baybo_query::{CostScope, QueryApi};
use chrono::{Duration, Utc};
use serde_json::{Value, json};

use crate::cli::CostCmd;
use crate::commands::parse_date_arg;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

const DATE_FMT: &str = "%Y-%m-%d";

pub async fn handle(ctx: &CommandContext, cmd: CostCmd) -> Result<CommandOutput> {
    match cmd {
        CostCmd::Show {
            user,
            session,
            turn,
            since,
            until,
        } => show(ctx, user, session, turn, since.as_deref(), until.as_deref()).await,
    }
}

fn query_api(ctx: &CommandContext) -> Result<&QueryApi> {
    ctx.query_api
        .as_deref()
        .ok_or_else(|| CliError::Manager("query api is not available in this invocation".into()))
}

async fn show(
    ctx: &CommandContext,
    user: Option<String>,
    session: Option<String>,
    turn: Option<String>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<CommandOutput> {
    let api = query_api(ctx)?;
    let range = resolve_range(since, until)?;
    let (scope_label, scope_value, summary) = match (user, session, turn) {
        (Some(uid), _, _) => {
            let label = format!("user={uid}");
            let value = json!({ "user": uid.clone(), "from": range.from.to_rfc3339(), "to": range.to.to_rfc3339() });
            let summary = api
                .cost_summary(CostScope::User {
                    user_id: uid,
                    range,
                })
                .await
                .map_err(|e| CliError::Manager(format!("cost summary (user): {e}")))?;
            (label, value, summary)
        }
        (None, Some(sid), _) => {
            let label = format!("session={sid}");
            let value = json!({ "session": sid.clone() });
            let summary = api
                .cost_summary(CostScope::Session(SessionId::from(sid.as_str())))
                .await
                .map_err(|e| CliError::Manager(format!("cost summary (session): {e}")))?;
            (label, value, summary)
        }
        (None, None, Some(jid)) => {
            let label = format!("turn={jid}");
            let value = json!({ "turn": jid.clone() });
            let parsed = TurnId::from_str(&jid).map_err(|e| {
                CliError::Parse(format!("invalid --turn {jid:?}: expected ULID ({e})"))
            })?;
            let summary = api
                .cost_summary(CostScope::Turn(parsed))
                .await
                .map_err(|e| CliError::Manager(format!("cost summary (turn): {e}")))?;
            (label, value, summary)
        }
        (None, None, None) => {
            let label = format!(
                "range={} → {}",
                range.from.format(DATE_FMT),
                range.to.format(DATE_FMT)
            );
            let value = json!({ "from": range.from.to_rfc3339(), "to": range.to.to_rfc3339() });
            let summary = api
                .cost_summary(CostScope::TimeRange(range))
                .await
                .map_err(|e| CliError::Manager(format!("cost summary (range): {e}")))?;
            (label, value, summary)
        }
    };

    let human = render_human(&scope_label, &summary);
    Ok(CommandOutput {
        human,
        data: Some(json!({
            "scope": scope_value,
            "summary": summary_to_json(&summary),
        })),
    })
}

fn resolve_range(since: Option<&str>, until: Option<&str>) -> Result<TimeRange> {
    let today = Utc::now().date_naive();
    let from_date = match since {
        Some(s) => parse_date_arg(s, "--since")?,
        None => today,
    };
    let until_date = match until {
        Some(s) => parse_date_arg(s, "--until")?,
        None => today + Duration::days(1),
    };
    if until_date <= from_date {
        return Err(CliError::Parse(format!(
            "--until ({}) must be strictly greater than --since ({})",
            until_date.format(DATE_FMT),
            from_date.format(DATE_FMT)
        )));
    }
    let from = from_date
        .and_hms_opt(0, 0, 0)
        .expect("midnight is valid")
        .and_utc();
    let to = until_date
        .and_hms_opt(0, 0, 0)
        .expect("midnight is valid")
        .and_utc();
    Ok(TimeRange { from, to })
}

fn render_human(scope_label: &str, s: &CostSummary) -> String {
    format!(
        "scope:            {scope_label}\ntotal cost:       ${:.4}\ncalls:            {}\ninput tokens:     {}\ncached input:     {}\ncache writes:     {}\noutput tokens:    {}",
        s.total_cost_usd.as_usd_decimal(),
        s.record_count,
        s.total_input_tokens,
        s.total_cached_input_tokens,
        s.total_cache_creation_input_tokens,
        s.total_output_tokens,
    )
}

fn summary_to_json(s: &CostSummary) -> Value {
    json!({
        "cost_usd": format!("{:.6}", s.total_cost_usd.as_usd_decimal()),
        "cost_micro_usd": s.total_cost_usd.into_micros(),
        "calls": s.record_count,
        "input_tokens": s.total_input_tokens,
        "cached_input_tokens": s.total_cached_input_tokens,
        "cache_creation_input_tokens": s.total_cache_creation_input_tokens,
        "output_tokens": s.total_output_tokens,
    })
}
