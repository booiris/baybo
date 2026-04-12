use std::path::PathBuf;

use aura_config::AuraConfig;

use crate::cli::ConfigCmd;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: ConfigCmd) -> Result<CommandOutput> {
    match cmd {
        ConfigCmd::Show { section } => show(ctx, section),
        ConfigCmd::Validate { file } => validate(ctx, file).await,
        ConfigCmd::File => file(ctx),
        ConfigCmd::Schema => schema(),
    }
}

fn show(ctx: &CommandContext, section: Option<String>) -> Result<CommandOutput> {
    let full = serde_json::to_value(ctx.config.as_ref())?;
    let value = match section {
        None => full,
        Some(name) => {
            let obj = full
                .as_object()
                .ok_or_else(|| CliError::Serialization("config is not an object".into()))?;
            obj.get(&name)
                .cloned()
                .ok_or_else(|| CliError::UnknownCommand(format!("config section: {name}")))?
        }
    };
    let human = serde_json::to_string_pretty(&value)?;
    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}

async fn validate(ctx: &CommandContext, file: Option<String>) -> Result<CommandOutput> {
    let path: PathBuf = file
        .map(PathBuf::from)
        .or_else(|| ctx.config_path.clone())
        .or_else(|| std::env::var("AURA_CONFIG_PATH").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("aura.json"));

    match AuraConfig::load_from_file(&path).await {
        Ok(_) => Ok(CommandOutput::structured(
            format!("{} is valid", path.display()),
            &serde_json::json!({
                "path": path.display().to_string(),
                "valid": true,
            }),
        )),
        Err(e) => Err(CliError::Config(format!(
            "{} is invalid: {e}",
            path.display()
        ))),
    }
}

fn file(ctx: &CommandContext) -> Result<CommandOutput> {
    let path = ctx
        .config_path
        .clone()
        .or_else(|| std::env::var("AURA_CONFIG_PATH").ok().map(PathBuf::from));
    match path {
        Some(p) => Ok(CommandOutput::structured(
            p.display().to_string(),
            &serde_json::json!({ "path": p.display().to_string(), "resolved": true }),
        )),
        None => Ok(CommandOutput::structured(
            "(no config file — using defaults)",
            &serde_json::json!({ "path": null, "resolved": false }),
        )),
    }
}

fn schema() -> Result<CommandOutput> {
    let default = AuraConfig::default();
    let value = serde_json::to_value(&default)?;
    let human = serde_json::to_string_pretty(&value)?;
    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}
