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
    let registry = ctx.channels.read().await;
    let entries: Vec<_> = registry
        .list()
        .into_iter()
        .map(|(ct, status)| (ct.to_string(), format!("{:?}", status)))
        .collect();
    let human = if entries.is_empty() {
        "(no channels registered)".to_string()
    } else {
        let mut buf = String::from("CHANNEL\tSTATUS\n");
        for (ct, st) in &entries {
            buf.push_str(&format!("{ct}\t{st}\n"));
        }
        buf.trim_end().to_string()
    };
    Ok(CommandOutput::structured(
        human,
        &json!({
            "channels": entries
                .iter()
                .map(|(ct, st)| json!({ "channel": ct, "status": st }))
                .collect::<Vec<_>>(),
        }),
    ))
}
