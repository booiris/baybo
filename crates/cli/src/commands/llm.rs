use aura_llm::LlmProviderRegistry;
use serde_json::{Value, json};

use crate::cli::LlmCmd;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: LlmCmd) -> Result<CommandOutput> {
    match cmd {
        LlmCmd::Status => status(ctx),
        LlmCmd::Models => models(),
        LlmCmd::Probe => probe(ctx).await,
    }
}

fn status(ctx: &CommandContext) -> Result<CommandOutput> {
    let client = ctx
        .llm
        .as_ref()
        .ok_or_else(|| CliError::Manager("llm client not initialised".into()))?;
    let info = client.model_info();
    let value = json!({
        "provider": info.provider,
        "model": info.id,
        "context_window": info.context_window,
        "supports_tools": info.supports_tools,
        "supports_vision": info.supports_vision,
        "pricing": {
            "input_per_1m_tokens": info.pricing.input_per_1m_tokens,
            "output_per_1m_tokens": info.pricing.output_per_1m_tokens,
        }
    });
    let human = format!(
        "provider: {}\nmodel:    {}\ncontext:  {} tokens\ntools:    {}\nvision:   {}\npricing:  ${:.2}/1M in, ${:.2}/1M out",
        info.provider,
        info.id,
        info.context_window,
        info.supports_tools,
        info.supports_vision,
        info.pricing.input_per_1m_tokens,
        info.pricing.output_per_1m_tokens,
    );
    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}

fn models() -> Result<CommandOutput> {
    let registry = LlmProviderRegistry::with_default_providers();
    let catalog = registry.list_models();

    if catalog.is_empty() {
        return Ok(CommandOutput {
            human: "(no providers registered)".into(),
            data: Some(json!({ "providers": [] })),
        });
    }

    let data: Vec<Value> = catalog
        .iter()
        .map(|p| json!({ "provider": p.provider, "models": p.models }))
        .collect();

    let mut human = String::new();
    for entry in &catalog {
        human.push_str(&format!("{}:\n", entry.provider));
        if entry.models.is_empty() {
            human.push_str("  (catalog not advertised)\n");
        } else {
            for m in &entry.models {
                human.push_str(&format!("  {m}\n"));
            }
        }
    }

    Ok(CommandOutput {
        human: human.trim_end().to_string(),
        data: Some(json!({ "providers": data })),
    })
}

async fn probe(ctx: &CommandContext) -> Result<CommandOutput> {
    let client = ctx
        .llm
        .as_ref()
        .ok_or_else(|| CliError::Manager("llm client not initialised".into()))?;

    let report = client
        .probe()
        .await
        .map_err(|e| CliError::Manager(format!("llm probe: {e}")))?;

    let value = json!({
        "provider": report.provider,
        "model": report.model,
        "latency_ms": report.latency_ms,
        "tokens": {
            "input": report.tokens.input_tokens,
            "output": report.tokens.output_tokens,
        },
    });

    let human = format!(
        "ok  provider={}  model={}  latency={}ms  tokens={}/{} (in/out)",
        report.provider,
        report.model,
        report.latency_ms,
        report.tokens.input_tokens,
        report.tokens.output_tokens,
    );

    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}
