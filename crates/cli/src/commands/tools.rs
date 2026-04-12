use serde_json::json;

use crate::cli::ToolsCmd;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub fn handle(ctx: &CommandContext, cmd: ToolsCmd) -> Result<CommandOutput> {
    match cmd {
        ToolsCmd::List => list(ctx),
        ToolsCmd::Info { name } => info(ctx, &name),
    }
}

fn list(ctx: &CommandContext) -> Result<CommandOutput> {
    let defs = ctx.tools.tool_definitions();
    let human = if defs.is_empty() {
        "(no tools registered)".to_string()
    } else {
        let mut buf = String::from("NAME\tDESCRIPTION\n");
        for d in &defs {
            let desc = d.description.lines().next().unwrap_or("");
            buf.push_str(&format!("{}\t{}\n", d.name, desc));
        }
        buf.trim_end().to_string()
    };
    Ok(CommandOutput::structured(human, &json!({ "tools": defs })))
}

fn info(ctx: &CommandContext, name: &str) -> Result<CommandOutput> {
    // Built-in and extension tools both share the Tool trait; only extension
    // tools carry a `ToolManifest`. Report whichever is available.
    if let Some(manifest) = ctx.tools.get_manifest(name) {
        let value = serde_json::to_value(manifest)?;
        return Ok(CommandOutput {
            human: serde_json::to_string_pretty(&value)?,
            data: Some(value),
        });
    }
    if let Some(tool) = ctx.tools.get(name) {
        let value = json!({
            "name": tool.name(),
            "description": tool.description(),
            "parameters_schema": tool.parameters_schema(),
            "manifest": null,
        });
        return Ok(CommandOutput {
            human: serde_json::to_string_pretty(&value)?,
            data: Some(value),
        });
    }
    Err(CliError::UnknownCommand(format!("tool: {name}")))
}
