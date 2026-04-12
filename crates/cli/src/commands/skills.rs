use serde_json::json;

use crate::cli::SkillsCmd;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub fn handle(ctx: &CommandContext, cmd: SkillsCmd) -> Result<CommandOutput> {
    match cmd {
        SkillsCmd::List => list(ctx),
        SkillsCmd::Info { name } => info(ctx, &name),
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
    // `SkillDefinition` derives Serialize, but its `trigger` variant for
    // regex patterns is only present at runtime. Its custom serde helper
    // handles that.
    let value = serde_json::to_value(skill)?;
    let human = serde_json::to_string_pretty(&value)?;
    Ok(CommandOutput {
        human,
        data: Some(value),
    })
}
