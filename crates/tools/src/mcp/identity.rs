use baybo_model::McpTransportIdentity;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::{ToolCapability, ToolTriggerScope};

use super::config::{McpServerEntry, McpTransportConfig, OAuthConfig, TrustLevelConfig};
use super::error::{McpError, McpResult};

const MCP_TRANSPORT_IDENTITY_VERSION: u8 = 1;

#[derive(Serialize)]
struct CanonicalTransportIdentity<'a> {
    version: u8,
    transport: CanonicalTransport<'a>,
    trust_level: &'static str,
    capabilities: Vec<&'static str>,
    trigger_scope: &'static str,
    oauth: Option<CanonicalOAuth<'a>>,
    env_names: Vec<String>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CanonicalTransport<'a> {
    Stdio {
        command: &'a str,
        args: &'a [String],
    },
    Http {
        url: String,
    },
}

#[derive(Serialize)]
struct CanonicalOAuth<'a> {
    client_id: &'a str,
    callback_port: Option<u16>,
}

/// Build the stable authorization identity of an exact MCP server config.
///
/// Values from secret env bags are intentionally not accepted by this API;
/// callers pass names only. Server display name is also excluded: authority
/// follows the transport and governance config, not a renameable label.
pub fn transport_identity<I, S>(
    entry: &McpServerEntry,
    env_names: I,
) -> McpResult<McpTransportIdentity>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let transport = match &entry.transport {
        McpTransportConfig::Stdio { command, args } => CanonicalTransport::Stdio { command, args },
        McpTransportConfig::Http { url } => CanonicalTransport::Http {
            url: normalize_url(url)?,
        },
    };
    let mut capabilities: Vec<&'static str> =
        entry.capabilities.iter().map(capability_name).collect();
    capabilities.sort_unstable();
    capabilities.dedup();
    let mut env_names: Vec<String> = env_names.into_iter().map(Into::into).collect();
    env_names.sort_unstable();
    env_names.dedup();
    let canonical = CanonicalTransportIdentity {
        version: MCP_TRANSPORT_IDENTITY_VERSION,
        transport,
        trust_level: trust_name(entry.trust_level),
        capabilities,
        trigger_scope: trigger_scope_name(entry.trigger_scope),
        oauth: entry.oauth.as_ref().map(canonical_oauth),
        env_names,
    };
    let bytes = serde_json::to_vec(&canonical).map_err(|e| {
        McpError::InvalidConfig(format!(
            "failed to canonicalize MCP transport identity: {e}"
        ))
    })?;
    Ok(McpTransportIdentity::from_sha256(
        Sha256::digest(bytes).into(),
    ))
}

fn normalize_url(raw: &str) -> McpResult<String> {
    url::Url::parse(raw)
        .map(|url| url.to_string())
        .map_err(|e| {
            McpError::InvalidConfig(format!("cannot identify invalid MCP URL '{raw}': {e}"))
        })
}

fn canonical_oauth(config: &OAuthConfig) -> CanonicalOAuth<'_> {
    CanonicalOAuth {
        client_id: &config.client_id,
        callback_port: config.callback_port,
    }
}

fn trust_name(trust: TrustLevelConfig) -> &'static str {
    match trust {
        TrustLevelConfig::Trusted => "trusted",
        TrustLevelConfig::Installed => "installed",
        TrustLevelConfig::Untrusted => "untrusted",
    }
}

fn capability_name(capability: &ToolCapability) -> &'static str {
    match capability {
        ToolCapability::ReadFile => "read_file",
        ToolCapability::WriteFile => "write_file",
        ToolCapability::Http => "http",
        ToolCapability::ExecCommand => "exec_command",
    }
}

fn trigger_scope_name(scope: ToolTriggerScope) -> &'static str {
    match scope {
        ToolTriggerScope::Any => "any",
        ToolTriggerScope::CronConversation => "cron_conversation",
        ToolTriggerScope::ProjectBoard => "project_board",
        ToolTriggerScope::SharedWorkspace => "shared_workspace",
        ToolTriggerScope::BackgroundHost => "background_host",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> McpServerEntry {
        McpServerEntry {
            name: name.to_string(),
            transport: McpTransportConfig::Stdio {
                command: "uvx".to_string(),
                args: vec!["server".to_string(), "--flag".to_string()],
            },
            trust_level: TrustLevelConfig::Trusted,
            capabilities: vec![ToolCapability::Http, ToolCapability::ExecCommand],
            oauth: None,
            description: None,
            trigger_scope: ToolTriggerScope::Any,
            defer: true,
        }
    }

    #[test]
    fn identity_is_versioned_sha256_over_canonical_config() {
        let identity =
            transport_identity(&entry("github"), ["ZONE", "API_TOKEN"]).expect("identity");
        assert_eq!(
            identity.as_str(),
            "mcp-transport:v1:sha256:5025e4c606dbea97c062dba2ccaea68eff0f6e7bda2c6780d2eed5108501495f"
        );
    }

    #[test]
    fn unordered_sets_and_server_labels_do_not_change_identity() {
        let base = entry("first-label");
        let mut reordered = entry("renamed");
        reordered.capabilities.reverse();
        reordered.capabilities.push(ToolCapability::Http);
        assert_eq!(
            transport_identity(&base, ["B", "A", "B"]).expect("base"),
            transport_identity(&reordered, ["A", "B"]).expect("reordered")
        );
    }

    #[test]
    fn stdio_argument_order_is_part_of_the_exact_identity() {
        let base = entry("x");
        let mut reordered = base.clone();
        let McpTransportConfig::Stdio { args, .. } = &mut reordered.transport else {
            panic!("stdio fixture");
        };
        args.reverse();
        assert_ne!(
            transport_identity(&base, Vec::<String>::new()).expect("base"),
            transport_identity(&reordered, Vec::<String>::new()).expect("reordered")
        );
    }

    #[test]
    fn equivalent_http_urls_have_one_identity() {
        let mut first = entry("x");
        first.transport = McpTransportConfig::Http {
            url: "HTTPS://EXAMPLE.COM:443/a/../mcp".to_string(),
        };
        let mut second = first.clone();
        second.transport = McpTransportConfig::Http {
            url: "https://example.com/mcp".to_string(),
        };
        assert_eq!(
            transport_identity(&first, Vec::<String>::new()).expect("first"),
            transport_identity(&second, Vec::<String>::new()).expect("second")
        );
    }

    #[test]
    fn oauth_non_secrets_and_env_names_are_identity_inputs() {
        let base = entry("x");
        let mut oauth = base.clone();
        oauth.oauth = Some(OAuthConfig {
            client_id: "public-client".to_string(),
            callback_port: Some(49152),
        });
        assert_ne!(
            transport_identity(&base, ["TOKEN"]).expect("base"),
            transport_identity(&oauth, ["TOKEN"]).expect("oauth")
        );
        assert_ne!(
            transport_identity(&base, ["TOKEN"]).expect("one env"),
            transport_identity(&base, ["OTHER_TOKEN"]).expect("other env")
        );
    }
}
