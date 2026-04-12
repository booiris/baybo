use serde_json::json;

use crate::cli::LlmCmd;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub fn handle(ctx: &CommandContext, cmd: LlmCmd) -> Result<CommandOutput> {
    match cmd {
        LlmCmd::Status => status(ctx),
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
