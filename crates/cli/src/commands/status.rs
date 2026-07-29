use baybo_cost::TimeRange;
use baybo_query::CostScope;
use baybo_turn::TurnStatusKind;
use chrono::{Duration, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

const RECENT_FAILURE_WINDOW: Duration = Duration::hours(24);

pub async fn handle(ctx: &CommandContext, live: bool) -> Result<CommandOutput> {
    let skills_count = ctx.skills.list().len();
    let tools_count = ctx.tools.tool_definitions().len();
    let channels_count = ctx.channels.len();
    let (provider, model) = match ctx.llm.as_ref() {
        Some(c) => (c.model_info().provider.clone(), c.model_info().id.clone()),
        None => ("(not configured)".into(), "(not configured)".into()),
    };

    let mut value = json!({
        "skills": skills_count,
        "tools": tools_count,
        "channels": channels_count,
        "llm": { "provider": provider, "model": model },
        "config_path": ctx.config_path.as_ref().map(|p| p.display().to_string()),
    });

    let mut human = format!(
        "skills:   {skills_count}\ntools:    {tools_count}\nchannels: {channels_count}\nllm:      {provider} / {model}\nconfig:   {}",
        ctx.config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(default)".to_string()),
    );

    if live {
        let snapshot = live_snapshot(ctx).await?;
        snapshot.append_human(&mut human);
        if let Value::Object(ref mut map) = value {
            map.insert(
                "live".into(),
                serde_json::to_value(&snapshot)
                    .map_err(|e| CliError::Serialization(format!("live snapshot: {e}")))?,
            );
        }
    }

    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}

/// Three independent runtime counters: in-flight turns, recently-failed
/// turns (24h), today's spend. Each section degrades gracefully when
/// its manager is absent (argv-light commands don't wire the full
/// graph) by carrying `None`.
#[derive(Debug, Serialize)]
struct LiveSnapshot {
    in_flight_turns: Option<usize>,
    failed_turns_24h: Option<usize>,
    cost_today: Option<LiveCostSummary>,
}

#[derive(Debug, Serialize)]
struct LiveCostSummary {
    /// `MicroUsd::as_usd_decimal` formatted to four places. Kept as a
    /// string so JSON consumers don't accidentally re-introduce float
    /// drift across `baybo status --live --json` aggregations.
    cost_usd: String,
    calls: usize,
    input_tokens: usize,
    output_tokens: usize,
}

impl LiveSnapshot {
    fn append_human(&self, out: &mut String) {
        out.push_str("\n\nlive:");
        out.push_str(&format!(
            "\n  in-flight turns:   {}",
            fmt_opt_num(self.in_flight_turns)
        ));
        out.push_str(&format!(
            "\n  failed turns/24h:  {}",
            fmt_opt_num(self.failed_turns_24h)
        ));
        let cost_str = match &self.cost_today {
            Some(c) => format!(
                "${} ({} calls, {} in / {} out tokens)",
                c.cost_usd, c.calls, c.input_tokens, c.output_tokens,
            ),
            None => "(unavailable)".into(),
        };
        out.push_str(&format!("\n  cost today:       {cost_str}"));
    }
}

fn fmt_opt_num(n: Option<usize>) -> String {
    n.map(|n| n.to_string())
        .unwrap_or_else(|| "(unavailable)".into())
}

async fn live_snapshot(ctx: &CommandContext) -> Result<LiveSnapshot> {
    let (in_flight_turns, failed_turns_24h, cost_today) = tokio::try_join!(
        fetch_in_flight(ctx),
        fetch_recent_failures(ctx),
        fetch_cost_today(ctx),
    )?;
    Ok(LiveSnapshot {
        in_flight_turns,
        failed_turns_24h,
        cost_today,
    })
}

async fn fetch_in_flight(ctx: &CommandContext) -> Result<Option<usize>> {
    let Some(jl) = ctx.turn.as_deref() else {
        return Ok(None);
    };
    let turns = jl
        .list(Some(TurnStatusKind::InProgress))
        .await
        .map_err(|e| CliError::Manager(format!("list in-flight turns: {e}")))?;
    Ok(Some(turns.len()))
}

async fn fetch_recent_failures(ctx: &CommandContext) -> Result<Option<usize>> {
    let Some(jl) = ctx.turn.as_deref() else {
        return Ok(None);
    };
    let cutoff = Utc::now() - RECENT_FAILURE_WINDOW;
    let failed = jl
        .list(Some(TurnStatusKind::Failed))
        .await
        .map_err(|e| CliError::Manager(format!("list failed turns: {e}")))?;
    Ok(Some(
        failed
            .iter()
            .filter(|j| j.ended_at.map(|t| t >= cutoff).unwrap_or(false))
            .count(),
    ))
}

async fn fetch_cost_today(ctx: &CommandContext) -> Result<Option<LiveCostSummary>> {
    let Some(api) = ctx.query_api.as_deref() else {
        return Ok(None);
    };
    let from = Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight is a valid time")
        .and_utc();
    let to = from + Duration::days(1);
    let summary = api
        .cost_summary(CostScope::TimeRange(TimeRange { from, to }))
        .await
        .map_err(|e| CliError::Manager(format!("cost summary: {e}")))?;
    Ok(Some(LiveCostSummary {
        cost_usd: format!("{:.4}", summary.total_cost_usd.as_usd_decimal()),
        calls: summary.record_count,
        input_tokens: summary.total_input_tokens,
        output_tokens: summary.total_output_tokens,
    }))
}
