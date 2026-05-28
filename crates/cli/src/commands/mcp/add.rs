use aura_tools::ToolCapability;
use aura_tools::mcp::oauth::run_authorization_flow;
use aura_tools::mcp::{McpServerEntry, McpTransportConfig, OAuthConfig, TrustLevelConfig};
use serde_json::json;

use crate::cli::{McpTransportArg, TrustLevelArg};
use crate::commands::secret_input::read_masked_password;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

use super::probe::{ProbeStatus, http_auth_precheck};
use super::{load_mcp_file, require_vault, vault_keys, workspace_root, write_mcp_file};

pub struct AddArgs {
    pub transport: McpTransportArg,
    pub envs: Vec<String>,
    pub headers: Vec<String>,
    pub client_id: Option<String>,
    pub client_secret: bool,
    pub callback_port: Option<u16>,
    pub trust_level: TrustLevelArg,
    pub name: String,
    pub command_or_url: String,
    pub args: Vec<String>,
}

pub async fn run(ctx: &CommandContext, args: AddArgs) -> Result<CommandOutput> {
    let transport_kind = resolve_transport(args.transport, &args.command_or_url)?;
    let transport = build_transport(transport_kind, &args.command_or_url, &args.args)?;

    let env_pairs = parse_env_pairs(&args.envs)?;
    let header_pairs = parse_header_pairs(&args.headers)?;

    if matches!(transport, McpTransportConfig::Stdio { .. }) && !header_pairs.is_empty() {
        return Err(CliError::Config(
            "--header is only valid for HTTP transports".into(),
        ));
    }
    if matches!(transport, McpTransportConfig::Http { .. }) && !env_pairs.is_empty() {
        return Err(CliError::Config(
            "--env is only valid for stdio transports".into(),
        ));
    }
    if matches!(transport, McpTransportConfig::Stdio { .. })
        && (args.client_id.is_some() || args.client_secret || args.callback_port.is_some())
    {
        return Err(CliError::Config(
            "OAuth flags are only valid for HTTP transports".into(),
        ));
    }

    let trust_level: TrustLevelConfig = match args.trust_level {
        TrustLevelArg::Trusted => TrustLevelConfig::Trusted,
        TrustLevelArg::Installed => TrustLevelConfig::Installed,
        TrustLevelArg::Untrusted => TrustLevelConfig::Untrusted,
    };
    let capabilities = default_capabilities(&transport);

    // OAuth is requested explicitly when any OAuth-shaped flag is passed,
    // and inferred automatically for HTTP transports without a static
    // auth header when the server returns 401 to an unauthenticated HEAD.
    // Stdio servers never use OAuth.
    let explicit_oauth = matches!(transport, McpTransportConfig::Http { .. })
        && (args.client_id.is_some() || args.client_secret || args.callback_port.is_some());

    let mut oauth_intent = explicit_oauth;
    let mut auto_detected_realm: Option<String> = None;
    if !oauth_intent
        && let McpTransportConfig::Http { url } = &transport
        && header_pairs.is_empty()
        && let Some(ProbeStatus::AuthRequired(realm)) =
            http_auth_precheck(url, ctx.proxy_settings().as_ref()).await
    {
        // Only the unambiguous "401/403 + WWW-Authenticate" signal triggers
        // the auto-OAuth path. Connection errors, timeouts, and other
        // inconclusive results fall through — the operator either gets a
        // working tokenless entry, a working entry once the server comes
        // back online, or a clear status message from `aura mcp list`.
        eprintln!("Server requires authorization (HEAD {url} → 401). Running OAuth flow.");
        oauth_intent = true;
        auto_detected_realm = Some(realm);
    }

    let oauth = if oauth_intent {
        Some(OAuthConfig {
            client_id: args.client_id.clone().unwrap_or_default(),
            callback_port: args.callback_port,
        })
    } else {
        None
    };
    let _ = auto_detected_realm;

    let entry = McpServerEntry {
        name: args.name.clone(),
        transport: transport.clone(),
        trust_level,
        capabilities,
        oauth,
    };
    entry
        .validate()
        .map_err(|e| CliError::Config(e.to_string()))?;

    let workspace = workspace_root(ctx);
    let mut file = load_mcp_file(&workspace).await?;
    if file.find(&args.name).is_some() {
        return Err(CliError::Config(format!(
            "mcp server '{}' already exists; remove it first",
            args.name
        )));
    }

    // Read OAuth client secret (interactive) only after we know the
    // entry validates and the name is unique — there's no point asking
    // for a secret we'd then refuse to persist.
    let client_secret_value = if args.client_secret {
        Some(read_masked_password("OAuth client secret: ")?)
    } else {
        None
    };

    let vault = require_vault(ctx)?;

    // If the user opted into OAuth (or the auto-detect found a 401),
    // drive the full authorization flow now so an `add` that fails to
    // authorize never mutates `.mcp.json`. The resulting client_id
    // (which DCR may have invented) is folded back into the entry
    // before persisting.
    let (entry, oauth_bundle) = if oauth_intent {
        let server_url = match &transport {
            McpTransportConfig::Http { url } => url.clone(),
            _ => unreachable!("oauth_intent implies http transport"),
        };
        let bundle = run_authorization_flow(
            &server_url,
            args.callback_port,
            args.client_id.as_deref(),
            client_secret_value.as_deref(),
            vault,
            &args.name,
        )
        .await
        .map_err(|e| CliError::Manager(format!("oauth: {e}")))?;
        let mut entry = entry;
        if let Some(cfg) = entry.oauth.as_mut() {
            cfg.client_id = bundle.client_id.clone();
        } else {
            // Auto-detected OAuth — populate the OAuth config so the
            // reconciler knows the entry is OAuth-managed.
            entry.oauth = Some(aura_tools::mcp::OAuthConfig {
                client_id: bundle.client_id.clone(),
                callback_port: args.callback_port,
            });
        }
        (entry, Some(bundle))
    } else {
        (entry, None)
    };

    if !env_pairs.is_empty() {
        let bag = serde_json::to_vec(&serde_json::Value::Object(env_pairs.into_iter().collect()))?;
        vault
            .store_secret(&vault_keys::env_bag(&args.name), &bag)
            .await
            .map_err(|e| CliError::Manager(format!("store env bag: {e}")))?;
    }
    if !header_pairs.is_empty() {
        let bag = serde_json::to_vec(&serde_json::Value::Object(
            header_pairs.into_iter().collect(),
        ))?;
        vault
            .store_secret(&vault_keys::header_bag(&args.name), &bag)
            .await
            .map_err(|e| CliError::Manager(format!("store header bag: {e}")))?;
    }
    if let Some(secret) = client_secret_value {
        vault
            .store_secret(
                &vault_keys::oauth_client_secret(&args.name),
                secret.as_bytes(),
            )
            .await
            .map_err(|e| CliError::Manager(format!("store oauth client secret: {e}")))?;
    }

    // OAuth tokens are persisted by `run_authorization_flow` through
    // `VaultCredentialStore`. Drop the bundle before persisting the
    // entry to make the relationship explicit.
    drop(oauth_bundle);

    file.upsert(entry.clone());
    write_mcp_file(&file, &workspace).await?;

    let auth = match (
        &entry.oauth,
        !entry_has_headers(&entry, vault, &args.name).await,
    ) {
        (Some(_), _) => "oauth",
        (None, true) => "none",
        (None, false) => "headers",
    };

    let mut human = format!(
        "Registered MCP server '{}' ({}). The reconciler will pick it up within a few seconds.",
        entry.name,
        entry.transport.kind_name()
    );
    if matches!(&entry.transport, McpTransportConfig::Http { .. }) && entry.oauth.is_some() {
        human.push_str(
            "\nNote: OAuth authorization flow runs at first connect; if no refresh token is present, the reconciler will surface an error until you re-run `aura mcp add` interactively.",
        );
    }

    Ok(CommandOutput::structured(
        human,
        &json!({
            "name": entry.name,
            "transport": entry.transport.kind_name(),
            "trust_level": format!("{:?}", entry.trust_level).to_lowercase(),
            "auth": auth,
            "action": "added",
        }),
    ))
}

async fn entry_has_headers(
    _entry: &McpServerEntry,
    vault: &aura_security::SecretVault,
    name: &str,
) -> bool {
    vault
        .get_secret(&vault_keys::header_bag(name))
        .await
        .ok()
        .flatten()
        .is_none()
}

fn resolve_transport(arg: McpTransportArg, command_or_url: &str) -> Result<&'static str> {
    let looks_like_url =
        command_or_url.starts_with("http://") || command_or_url.starts_with("https://");
    Ok(match arg {
        McpTransportArg::Stdio => {
            if looks_like_url {
                return Err(CliError::Config(
                    "transport=stdio but command_or_url looks like a URL — pass --transport http"
                        .into(),
                ));
            }
            "stdio"
        }
        McpTransportArg::Http | McpTransportArg::Sse => {
            if !looks_like_url {
                return Err(CliError::Config(
                    "transport=http requires command_or_url to be an http(s):// URL".into(),
                ));
            }
            "http"
        }
    })
}

fn build_transport(
    kind: &str,
    command_or_url: &str,
    args: &[String],
) -> Result<McpTransportConfig> {
    Ok(match kind {
        "stdio" => McpTransportConfig::Stdio {
            command: command_or_url.to_string(),
            args: args.to_vec(),
        },
        "http" => McpTransportConfig::Http {
            url: command_or_url.to_string(),
        },
        _ => unreachable!(),
    })
}

fn parse_env_pairs(envs: &[String]) -> Result<Vec<(String, serde_json::Value)>> {
    let mut out = Vec::with_capacity(envs.len());
    for raw in envs {
        let (key, value) = raw.split_once('=').ok_or_else(|| {
            CliError::Config(format!("--env entries must be KEY=VALUE; got '{raw}'"))
        })?;
        if key.is_empty() {
            return Err(CliError::Config(format!(
                "--env entries must have a non-empty key; got '{raw}'"
            )));
        }
        out.push((
            key.to_string(),
            serde_json::Value::String(value.to_string()),
        ));
    }
    Ok(out)
}

fn parse_header_pairs(headers: &[String]) -> Result<Vec<(String, serde_json::Value)>> {
    let mut out = Vec::with_capacity(headers.len());
    for raw in headers {
        let (name, value) = raw.split_once(':').ok_or_else(|| {
            CliError::Config(format!(
                "--header entries must be 'Name: Value'; got '{raw}'"
            ))
        })?;
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() {
            return Err(CliError::Config(format!(
                "--header entries must have a non-empty name; got '{raw}'"
            )));
        }
        out.push((
            name.to_string(),
            serde_json::Value::String(value.to_string()),
        ));
    }
    Ok(out)
}

/// Capabilities a server can reach by virtue of its transport. The
/// ceiling is **honest** — a stdio entry always carries `ExecCommand`
/// because connecting it spawns a subprocess. The trust gate in
/// `McpServerEntry::validate` reads this set and rejects servers whose
/// trust level can't carry the implied capability (e.g. an `installed`
/// or `untrusted` stdio server).
fn default_capabilities(transport: &McpTransportConfig) -> Vec<ToolCapability> {
    match transport {
        McpTransportConfig::Stdio { .. } => {
            vec![ToolCapability::Http, ToolCapability::ExecCommand]
        }
        McpTransportConfig::Http { .. } => vec![ToolCapability::Http],
    }
}
