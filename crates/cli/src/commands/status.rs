use serde_json::json;

use crate::context::CommandContext;
use crate::error::Result;
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext) -> Result<CommandOutput> {
    let skills_count = ctx.skills.list().len();
    let tools_count = ctx.tools.tool_definitions().len();
    let channels_count = ctx.channels.len();
    let (provider, model) = match ctx.llm.as_ref() {
        Some(c) => (c.model_info().provider.clone(), c.model_id().to_string()),
        None => ("(not configured)".into(), "(not configured)".into()),
    };

    let value = json!({
        "skills": skills_count,
        "tools": tools_count,
        "channels": channels_count,
        "llm": { "provider": provider, "model": model },
        "config_path": ctx.config_path.as_ref().map(|p| p.display().to_string()),
    });

    let human = format!(
        "skills:   {skills_count}\ntools:    {tools_count}\nchannels: {channels_count}\nllm:      {provider} / {model}\nconfig:   {}",
        ctx.config_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(default)".to_string()),
    );
    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}
