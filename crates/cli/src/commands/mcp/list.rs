use std::sync::Arc;

use aura_tools::mcp::McpServerEntry;
use serde_json::json;

use crate::context::CommandContext;
use crate::error::Result;
use crate::format::CommandOutput;

use super::probe::{ProbeStatus, probe};
use super::{load_mcp_file, require_vault, workspace_root};

pub async fn run(ctx: &CommandContext, do_probe: bool) -> Result<CommandOutput> {
    let workspace = workspace_root(ctx);
    let file = load_mcp_file(&workspace).await?;

    if file.servers.is_empty() {
        return Ok(CommandOutput::structured(
            "(no MCP servers registered)".to_string(),
            &json!({ "servers": [] }),
        ));
    }

    let results: Vec<(McpServerEntry, ProbeStatus)> = if do_probe {
        let vault = Arc::clone(require_vault(ctx)?);
        let proxy = ctx.proxy_settings();
        let probes = file.servers.iter().map(|entry| {
            let entry = entry.clone();
            let vault = Arc::clone(&vault);
            let proxy = proxy.clone();
            async move {
                let status = probe(&entry, &vault, proxy.as_ref()).await;
                (entry, status)
            }
        });
        futures::future::join_all(probes).await
    } else {
        file.servers
            .iter()
            .map(|e| (e.clone(), ProbeStatus::Skipped))
            .collect()
    };

    let mut human = String::from("NAME\tTRANSPORT\tTRUST\tAUTH\tSTATUS\n");
    let mut rows = Vec::new();
    for (entry, status) in results {
        let auth = if entry.oauth.is_some() {
            "oauth"
        } else {
            "none"
        };
        human.push_str(&format!(
            "{}\t{}\t{:?}\t{}\t{}\n",
            entry.name,
            entry.transport.kind_name(),
            entry.trust_level,
            auth,
            status.short()
        ));
        rows.push(json!({
            "name": entry.name,
            "transport": entry.transport.kind_name(),
            "trust_level": format!("{:?}", entry.trust_level).to_lowercase(),
            "auth": auth,
            "status": status.kind(),
            "tool_count": status.tool_count(),
            "error": status.error_message(),
        }));
    }

    Ok(CommandOutput::structured(
        human.trim_end().to_string(),
        &json!({ "servers": rows }),
    ))
}
