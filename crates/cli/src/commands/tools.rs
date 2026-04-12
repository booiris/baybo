use aura_tools::ToolOutput;
use serde_json::{Value, json};

use crate::cli::ToolsCmd;
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: ToolsCmd) -> Result<CommandOutput> {
    match cmd {
        ToolsCmd::List => list(ctx),
        ToolsCmd::Info { name } => info(ctx, &name),
        ToolsCmd::Test { name, args, yes } => test(ctx, &name, &args, yes).await,
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

async fn test(ctx: &CommandContext, name: &str, args: &str, yes: bool) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && !yes {
        return Err(CliError::ConfirmationRequired(format!(
            "would execute tool '{name}' outside an agent turn; \
             re-run with --yes to confirm"
        )));
    }

    if ctx.tools.get(name).is_none() {
        return Err(CliError::UnknownCommand(format!("tool: {name}")));
    }

    let params: Value = serde_json::from_str(args)
        .map_err(|e| CliError::Parse(format!("--args must be a JSON value: {e}")))?;

    let executor = ctx.tool_executor.as_deref().ok_or_else(|| {
        CliError::Manager("tool executor is not available in this invocation".into())
    })?;
    let recorder = ctx.recorder.as_deref().ok_or_else(|| {
        CliError::Manager("observability recorder is not available in this invocation".into())
    })?;

    let output = executor
        .test_execute(name, params, recorder)
        .await
        .map_err(|e| CliError::Manager(format!("tools test: {e}")))?;

    let (kind, payload, human) = match &output {
        ToolOutput::Text(t) => ("text", Value::String(t.clone()), t.clone()),
        ToolOutput::Json(v) => (
            "json",
            v.clone(),
            serde_json::to_string_pretty(v).unwrap_or_default(),
        ),
        ToolOutput::Error(e) => ("error", Value::String(e.clone()), format!("error: {e}")),
    };

    let data = json!({
        "tool": name,
        "kind": kind,
        "output": payload,
    });

    Ok(CommandOutput {
        human,
        data: Some(data),
    })
}
