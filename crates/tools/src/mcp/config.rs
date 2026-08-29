use std::path::{Path, PathBuf};

use baybo_model::TrustLevel;
use baybo_workspace::WorkspacePaths;
use serde::{Deserialize, Serialize};

use crate::mcp::error::{McpError, McpResult};
use crate::{ToolCapability, ToolTriggerScope};

/// Mirror of [`baybo_model::TrustLevel`] for the on-disk representation.
/// Kept separate from the runtime enum so the serde format is stable
/// even if `baybo-model` reshapes its variants. Convert via `From`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevelConfig {
    Trusted,
    Installed,
    Untrusted,
}

impl From<TrustLevelConfig> for TrustLevel {
    fn from(value: TrustLevelConfig) -> Self {
        match value {
            TrustLevelConfig::Trusted => TrustLevel::Trusted,
            TrustLevelConfig::Installed => TrustLevel::Installed,
            TrustLevelConfig::Untrusted => TrustLevel::Untrusted,
        }
    }
}

impl From<TrustLevel> for TrustLevelConfig {
    fn from(value: TrustLevel) -> Self {
        match value {
            TrustLevel::Trusted => Self::Trusted,
            TrustLevel::Installed => Self::Installed,
            TrustLevel::Untrusted => Self::Untrusted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransportConfig {
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
    },
    Http {
        url: String,
    },
}

impl McpTransportConfig {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { .. } => "http",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OAuthConfig {
    /// Public OAuth client identifier. The corresponding client secret
    /// (if any), refresh token, and access token live in the
    /// `SecretVault`, never in this file.
    pub client_id: String,

    /// Optional fixed redirect-URI port. Used when the OAuth provider
    /// requires a pre-registered redirect URI; otherwise the OAuth flow
    /// binds an ephemeral localhost port at `add` time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callback_port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerEntry {
    pub name: String,
    pub transport: McpTransportConfig,
    pub trust_level: TrustLevelConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<ToolCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthConfig>,
    /// Which sessions this server's tools are offered to. Defaults to
    /// [`ToolTriggerScope::Any`], so a config file written before this field
    /// existed keeps every server everywhere.
    #[serde(default)]
    pub trigger_scope: ToolTriggerScope,
}

impl McpServerEntry {
    pub fn transport_identity<I, S>(
        &self,
        env_names: I,
    ) -> McpResult<baybo_model::McpTransportIdentity>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        crate::mcp::identity::transport_identity(self, env_names)
    }

    pub fn validate(&self) -> McpResult<()> {
        if self.name.is_empty() {
            return Err(McpError::InvalidConfig(
                "server name must not be empty".into(),
            ));
        }
        if self.name.len() > 64 {
            return Err(McpError::InvalidConfig(format!(
                "server name '{}' is longer than 64 characters",
                self.name
            )));
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(McpError::InvalidConfig(format!(
                "server name '{}' may only contain alphanumerics, '-', and '_'",
                self.name
            )));
        }
        match &self.transport {
            McpTransportConfig::Stdio { command, .. } => {
                if command.is_empty() {
                    return Err(McpError::InvalidConfig(
                        "stdio transport requires a non-empty command".into(),
                    ));
                }
            }
            McpTransportConfig::Http { url } => {
                let parsed = url::Url::parse(url)
                    .map_err(|e| McpError::InvalidConfig(format!("invalid url '{url}': {e}")))?;
                if parsed.scheme() != "http" && parsed.scheme() != "https" {
                    return Err(McpError::InvalidConfig(format!(
                        "url scheme must be http or https, got '{}'",
                        parsed.scheme()
                    )));
                }
            }
        }

        let trust: TrustLevel = self.trust_level.into();

        // Stdio servers spawn a subprocess at connect time, which is
        // `ExecCommand` by definition — only `Trusted` may carry it.
        // Reject misconfigured installed/untrusted stdio entries up
        // front so the reconciler never spawns them.
        if matches!(self.transport, McpTransportConfig::Stdio { .. })
            && !matches!(trust, TrustLevel::Trusted)
        {
            return Err(McpError::InvalidConfig(format!(
                "server '{}': stdio transports spawn a local subprocess (ExecCommand) and require \
                 trust_level=trusted. Re-add with `--trust-level trusted` after auditing the \
                 command, or use an HTTP transport.",
                self.name
            )));
        }

        match trust {
            TrustLevel::Trusted => {}
            TrustLevel::Installed | TrustLevel::Untrusted => {
                for cap in &self.capabilities {
                    if matches!(cap, ToolCapability::WriteFile | ToolCapability::ExecCommand) {
                        return Err(McpError::InvalidConfig(format!(
                            "server '{}': trust level '{:?}' may not declare capability '{:?}' \
                             — only `trusted` servers may auto-execute side effects",
                            self.name, trust, cap
                        )));
                    }
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub servers: Vec<McpServerEntry>,
}

fn default_version() -> u32 {
    1
}

impl Default for McpFile {
    fn default() -> Self {
        Self {
            version: 1,
            servers: Vec::new(),
        }
    }
}

impl McpFile {
    pub fn path_for(workspace_root: &Path) -> PathBuf {
        WorkspacePaths::new(workspace_root.to_path_buf()).mcp_config()
    }

    /// Read `.mcp.json` from the workspace root. Returns `Ok(default)` if
    /// the file does not exist — this is the steady-state when no MCP
    /// servers have been configured yet.
    pub async fn load(workspace_root: &Path) -> McpResult<Self> {
        let path = Self::path_for(workspace_root);
        match tokio::fs::read_to_string(&path).await {
            Ok(contents) => Self::parse(&contents),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(McpError::FileRead {
                path: path.display().to_string(),
                reason: e.to_string(),
            }),
        }
    }

    pub fn parse(contents: &str) -> McpResult<Self> {
        let file: McpFile =
            serde_json::from_str(contents).map_err(|e| McpError::Parse(e.to_string()))?;
        for entry in &file.servers {
            entry.validate()?;
        }
        let mut seen = std::collections::HashSet::new();
        for entry in &file.servers {
            if !seen.insert(entry.name.as_str()) {
                return Err(McpError::DuplicateName(entry.name.clone()));
            }
        }
        Ok(file)
    }

    /// Write `.mcp.json` atomically (tmp + rename, matching
    /// `BayboConfig::write_to_file`).
    pub async fn write(&self, workspace_root: &Path) -> McpResult<()> {
        for entry in &self.servers {
            entry.validate()?;
        }
        let path = Self::path_for(workspace_root);
        let json = serde_json::to_string_pretty(self).map_err(|e| McpError::FileWrite {
            path: path.display().to_string(),
            reason: format!("serialize: {e}"),
        })?;
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| McpError::FileWrite {
                    path: parent.display().to_string(),
                    reason: format!("mkdir: {e}"),
                })?;
        }
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, json.as_bytes())
            .await
            .map_err(|e| McpError::FileWrite {
                path: tmp.display().to_string(),
                reason: e.to_string(),
            })?;
        tokio::fs::rename(&tmp, &path)
            .await
            .map_err(|e| McpError::FileWrite {
                path: path.display().to_string(),
                reason: format!("rename from tmp: {e}"),
            })?;
        Ok(())
    }

    pub fn upsert(&mut self, entry: McpServerEntry) {
        if let Some(slot) = self.servers.iter_mut().find(|s| s.name == entry.name) {
            *slot = entry;
        } else {
            self.servers.push(entry);
        }
    }

    pub fn remove(&mut self, name: &str) -> Option<McpServerEntry> {
        let idx = self.servers.iter().position(|s| s.name == name)?;
        Some(self.servers.remove(idx))
    }

    pub fn find(&self, name: &str) -> Option<&McpServerEntry> {
        self.servers.iter().find(|s| s.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_http_entry() {
        let json = r#"{
            "version": 1,
            "servers": [
                {
                    "name": "github",
                    "transport": { "type": "http", "url": "https://api.githubcopilot.com/mcp/" },
                    "trust_level": "trusted"
                }
            ]
        }"#;
        let file = McpFile::parse(json).expect("valid mcp file");
        assert_eq!(file.servers.len(), 1);
        assert!(matches!(
            file.servers[0].transport,
            McpTransportConfig::Http { .. }
        ));
    }

    /// A file written before the field existed keeps every server
    /// everywhere; naming a scope narrows exactly that server.
    #[test]
    fn a_server_without_a_scope_is_offered_to_every_session() {
        let json = r#"{
            "servers": [
                {
                    "name": "github",
                    "transport": { "type": "http", "url": "https://example.invalid/mcp/" },
                    "trust_level": "trusted"
                },
                {
                    "name": "browser",
                    "transport": { "type": "http", "url": "https://example.invalid/b/" },
                    "trust_level": "trusted",
                    "trigger_scope": "shared_workspace"
                }
            ]
        }"#;
        let file = McpFile::parse(json).expect("valid mcp file");
        assert_eq!(file.servers[0].trigger_scope, ToolTriggerScope::Any);
        assert_eq!(
            file.servers[1].trigger_scope,
            ToolTriggerScope::SharedWorkspace
        );
    }

    #[test]
    fn parses_stdio_entry_with_args() {
        let json = r#"{
            "servers": [
                {
                    "name": "memory",
                    "transport": {
                        "type": "stdio",
                        "command": "npx",
                        "args": ["@modelcontextprotocol/server-memory"]
                    },
                    "trust_level": "trusted",
                    "capabilities": ["http", "exec_command"]
                }
            ]
        }"#;
        let file = McpFile::parse(json).expect("valid stdio entry");
        assert_eq!(file.version, 1);
        match &file.servers[0].transport {
            McpTransportConfig::Stdio { command, args } => {
                assert_eq!(command, "npx");
                assert_eq!(
                    args,
                    &vec!["@modelcontextprotocol/server-memory".to_string()]
                );
            }
            _ => panic!("expected stdio"),
        }
    }

    #[test]
    fn rejects_stdio_with_installed_trust() {
        let entry = McpServerEntry {
            name: "x".into(),
            transport: McpTransportConfig::Stdio {
                command: "any".into(),
                args: vec![],
            },
            trust_level: TrustLevelConfig::Installed,
            capabilities: vec![ToolCapability::Http],
            oauth: None,
            trigger_scope: ToolTriggerScope::Any,
        };
        let err = entry.validate().unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("require trust_level=trusted"),
            "expected stdio-trust hint, got: {msg}"
        );
    }

    #[test]
    fn rejects_untrusted_with_exec_command() {
        let entry = McpServerEntry {
            name: "x".into(),
            transport: McpTransportConfig::Http {
                url: "https://example.com/".into(),
            },
            trust_level: TrustLevelConfig::Untrusted,
            capabilities: vec![ToolCapability::ExecCommand],
            oauth: None,
            trigger_scope: ToolTriggerScope::Any,
        };
        let err = entry.validate().unwrap_err();
        assert!(matches!(err, McpError::InvalidConfig(_)));
    }

    #[test]
    fn rejects_duplicate_names() {
        let json = r#"{
            "servers": [
                { "name": "a", "transport": { "type": "http", "url": "https://x/" }, "trust_level": "trusted" },
                { "name": "a", "transport": { "type": "http", "url": "https://y/" }, "trust_level": "trusted" }
            ]
        }"#;
        let err = McpFile::parse(json).unwrap_err();
        assert!(matches!(err, McpError::DuplicateName(name) if name == "a"));
    }

    #[test]
    fn rejects_installed_with_exec_command() {
        let entry = McpServerEntry {
            name: "x".into(),
            transport: McpTransportConfig::Stdio {
                command: "true".into(),
                args: vec![],
            },
            trust_level: TrustLevelConfig::Installed,
            capabilities: vec![ToolCapability::ExecCommand],
            oauth: None,
            trigger_scope: ToolTriggerScope::Any,
        };
        let err = entry.validate().unwrap_err();
        assert!(matches!(err, McpError::InvalidConfig(_)));
    }

    #[tokio::test]
    async fn missing_file_yields_default() {
        let dir = tempfile::tempdir().unwrap();
        let file = McpFile::load(dir.path()).await.unwrap();
        assert!(file.servers.is_empty());
        assert_eq!(file.version, 1);
    }

    #[tokio::test]
    async fn write_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = McpFile::default();
        file.upsert(McpServerEntry {
            name: "x".into(),
            transport: McpTransportConfig::Http {
                url: "https://example.com/mcp".into(),
            },
            trust_level: TrustLevelConfig::Trusted,
            capabilities: vec![ToolCapability::Http],
            oauth: None,
            trigger_scope: ToolTriggerScope::Any,
        });
        file.write(dir.path()).await.unwrap();
        let loaded = McpFile::load(dir.path()).await.unwrap();
        assert_eq!(loaded, file);
    }
}
