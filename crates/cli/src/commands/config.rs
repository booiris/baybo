use std::path::PathBuf;

use aura_config::AuraConfig;
use aura_workspace::paths::{ENV_CONFIG_PATH, default_config_file};

use crate::cli::ConfigCmd;
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: ConfigCmd) -> Result<CommandOutput> {
    match cmd {
        ConfigCmd::Show { section } => show(ctx, section),
        ConfigCmd::Validate { file } => validate(ctx, file).await,
        ConfigCmd::File => file(ctx),
        ConfigCmd::Schema => schema(),
        ConfigCmd::Get { path } => get(ctx, &path),
        ConfigCmd::Set { path, value, yes } => set(ctx, &path, &value, yes).await,
        ConfigCmd::Unset { path, yes } => unset(ctx, &path, yes).await,
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
        .or_else(|| std::env::var(ENV_CONFIG_PATH).ok().map(PathBuf::from))
        .unwrap_or_else(default_config_file);

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
        .or_else(|| std::env::var(ENV_CONFIG_PATH).ok().map(PathBuf::from));
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

fn dotted_to_pointer(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else if path.is_empty() {
        String::new()
    } else {
        let mut out = String::with_capacity(path.len() + 1);
        for seg in path.split('.') {
            if seg.is_empty() {
                continue;
            }
            out.push('/');
            for ch in seg.chars() {
                match ch {
                    '~' => out.push_str("~0"),
                    '/' => out.push_str("~1"),
                    c => out.push(c),
                }
            }
        }
        out
    }
}

fn resolve_target_path(ctx: &CommandContext) -> Result<PathBuf> {
    ctx.config_path
        .clone()
        .or_else(|| std::env::var(ENV_CONFIG_PATH).ok().map(PathBuf::from))
        .ok_or_else(|| {
            CliError::Config(format!(
                "no config file resolved; set {ENV_CONFIG_PATH} (or pass --config <path>) so the \
                 mutation has a destination"
            ))
        })
}

fn get(ctx: &CommandContext, path: &str) -> Result<CommandOutput> {
    if path.is_empty() {
        return Err(CliError::Config("path is empty".into()));
    }
    let pointer = dotted_to_pointer(path);
    let root = serde_json::to_value(ctx.config.as_ref())?;
    let value = root
        .pointer(&pointer)
        .cloned()
        .ok_or_else(|| CliError::Config(format!("no such path: {path}")))?;
    let human = match &value {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other)?,
    };
    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}

/// Parse `raw` as JSON; on failure, treat it as a bare string literal. This
/// keeps `aura config set llm.model gpt-5` ergonomic while still supporting
/// `aura config set agent.max_iterations 100` and `aura config set cost.enabled true`.
fn parse_value(raw: &str) -> serde_json::Value {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => v,
        Err(_) => serde_json::Value::String(raw.to_string()),
    }
}

async fn set(
    ctx: &CommandContext,
    path: &str,
    raw_value: &str,
    yes: bool,
) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && !yes {
        return Err(CliError::ConfirmationRequired(format!(
            "would set {path} = {raw_value}; re-run with --yes to confirm"
        )));
    }
    let target = resolve_target_path(ctx)?;
    let value = parse_value(raw_value);
    let new_config = ctx.config.set_at_path(path, value.clone())?;
    new_config.write_to_file(&target).await?;
    Ok(CommandOutput {
        human: format!(
            "wrote {path} → {raw_value} to {}\n(note: running process still holds the old config; restart to pick up)",
            target.display()
        ),
        data: Some(serde_json::json!({
            "path": path,
            "value": value,
            "written_to": target.display().to_string(),
            "requires_restart": true,
        })),
    })
}

async fn unset(ctx: &CommandContext, path: &str, yes: bool) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && !yes {
        return Err(CliError::ConfirmationRequired(format!(
            "would unset {path}; re-run with --yes to confirm"
        )));
    }
    let target = resolve_target_path(ctx)?;
    let new_config = ctx.config.unset_at_path(path)?;
    new_config.write_to_file(&target).await?;
    Ok(CommandOutput {
        human: format!(
            "unset {path} in {} (value reset to default)\n(note: running process still holds the old config; restart to pick up)",
            target.display()
        ),
        data: Some(serde_json::json!({
            "path": path,
            "written_to": target.display().to_string(),
            "requires_restart": true,
        })),
    })
}
