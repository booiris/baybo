use serde_json::json;

use crate::cli::ChannelsCmd;
use crate::context::CommandContext;
use crate::error::Result;
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: ChannelsCmd) -> Result<CommandOutput> {
    match cmd {
        ChannelsCmd::List => list(ctx).await,
    }
}

async fn list(ctx: &CommandContext) -> Result<CommandOutput> {
    let entries: Vec<String> = ctx
        .channels
        .list()
        .into_iter()
        .map(|ct| ct.to_string())
        .collect();
    let human = if entries.is_empty() {
        "(no channels registered)".to_string()
    } else {
        let mut buf = String::from("CHANNEL\n");
        for ct in &entries {
            buf.push_str(&format!("{ct}\n"));
        }
        buf.trim_end().to_string()
    };
    Ok(CommandOutput::structured(
        human,
        &json!({
            "channels": entries
                .iter()
                .map(|ct| json!({ "channel": ct }))
                .collect::<Vec<_>>(),
        }),
    ))
}
