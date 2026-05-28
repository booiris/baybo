use serde_json::json;

use std::sync::Arc;

use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

use super::probe::{ProbeStatus, probe};
use super::{load_mcp_file, require_vault, vault_keys, workspace_root};

pub async fn run(ctx: &CommandContext, name: String, do_probe: bool) -> Result<CommandOutput> {
    let workspace = workspace_root(ctx);
    let file = load_mcp_file(&workspace).await?;

    let entry = file
        .find(&name)
        .ok_or_else(|| CliError::Manager(format!("mcp server '{name}' not found")))?
        .clone();

    let vault = require_vault(ctx)?;
    let env_present = vault
        .get_secret(&vault_keys::env_bag(&name))
        .await
        .map_err(|e| CliError::Manager(format!("read env bag: {e}")))?
        .is_some();
    let headers_present = vault
        .get_secret(&vault_keys::header_bag(&name))
        .await
        .map_err(|e| CliError::Manager(format!("read header bag: {e}")))?
        .is_some();
    // The credential store collapses access/refresh/scopes/etc. into a
    // single JSON blob so the manager can rotate them atomically. For
    // the inspector we only need to know whether the blob is present.
    let credentials_present = vault
        .get_secret(&vault_keys::oauth_credentials(&name))
        .await
        .map_err(|e| CliError::Manager(format!("read oauth credentials: {e}")))?
        .is_some();

    let auth = match (
        entry.oauth.is_some() && credentials_present,
        headers_present,
    ) {
        (true, _) => "oauth",
        (false, true) => "headers",
        _ => "none",
    };

    let status = if do_probe {
        let vault_arc = Arc::clone(vault);
        probe(&entry, &vault_arc, ctx.proxy_settings().as_ref()).await
    } else {
        ProbeStatus::Skipped
    };

    let mut human = String::new();
    human.push_str(&format!("name: {}\n", entry.name));
    human.push_str(&format!("transport: {}\n", entry.transport.kind_name()));
    match &entry.transport {
        aura_tools::mcp::McpTransportConfig::Stdio { command, args } => {
            human.push_str(&format!("  command: {command}\n"));
            if !args.is_empty() {
                human.push_str(&format!("  args: {}\n", args.join(" ")));
            }
            human.push_str(&format!(
                "  env: {}\n",
                if env_present {
                    "******** (encrypted)"
                } else {
                    "(none)"
                }
            ));
        }
        aura_tools::mcp::McpTransportConfig::Http { url } => {
            human.push_str(&format!("  url: {url}\n"));
            human.push_str(&format!(
                "  headers: {}\n",
                if headers_present {
                    "******** (encrypted)"
                } else {
                    "(none)"
                }
            ));
        }
    }
    human.push_str(&format!("trust_level: {:?}\n", entry.trust_level));
    human.push_str(&format!(
        "capabilities: [{}]\n",
        entry
            .capabilities
            .iter()
            .map(|c| format!("{c:?}").to_lowercase())
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if let Some(oauth) = &entry.oauth {
        human.push_str("oauth:\n");
        human.push_str(&format!("  client_id: {}\n", oauth.client_id));
        if let Some(port) = oauth.callback_port {
            human.push_str(&format!("  callback_port: {port}\n"));
        }
        human.push_str(&format!(
            "  credentials: {}\n",
            if credentials_present {
                "******** (encrypted; auto-refreshed on use)"
            } else {
                "(missing — re-run `aura mcp add` to authorize)"
            }
        ));
    }
    human.push_str(&format!("auth: {auth}\n"));
    human.push_str(&format!("status: {}\n", status.short()));

    Ok(CommandOutput::structured(
        human.trim_end().to_string(),
        &json!({
            "name": entry.name,
            "transport": entry.transport.kind_name(),
            "trust_level": format!("{:?}", entry.trust_level).to_lowercase(),
            "capabilities": entry
                .capabilities
                .iter()
                .map(|c| format!("{c:?}").to_lowercase())
                .collect::<Vec<_>>(),
            "oauth": entry.oauth.as_ref().map(|o| json!({
                "client_id": o.client_id,
                "callback_port": o.callback_port,
                "credentials_present": credentials_present,
            })),
            "env_present": env_present,
            "headers_present": headers_present,
            "auth": auth,
            "status": status.kind(),
            "tool_count": status.tool_count(),
            "error": status.error_message(),
        }),
    ))
}
