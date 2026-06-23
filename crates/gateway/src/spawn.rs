//! Helper for spawning channel sidecars as gateway subprocesses.
//!
//! A subprocess channel plugin connects to the gateway's loopback TCP
//! channel listener just like the TUI, but — unlike the TUI's
//! gateway-wide token published to the vault — its identity is a
//! one-shot token bound to its process. At spawn time the gateway:
//!
//! 1. Generates a fresh capability token.
//! 2. Sets `BAYBO_CHANNEL_URL` and `BAYBO_CHANNEL_TOKEN` in the child's
//!    environment so the child knows where to connect and how to
//!    authenticate.
//! 3. Spawns the child, captures its PID, and registers the token
//!    with that PID in the gateway's [`ChannelTokenTable`] — the PID
//!    is retained for diagnostics (log lines, `/v1/status`), not
//!    used for auth (see [`crate::auth::channel`]).
//! 4. Returns a [`ChildHandle`] whose `Drop` revokes the token — so
//!    a crashed or killed child's token stops being valid the moment
//!    the handle is dropped.
//!
//! This module provides only the spawn primitive. Lifecycle
//! management (restart policy, back-off, plugin manifest loading, …)
//! is the supervisor's job ([`crate::sidecar::SidecarSupervisor`]).

use tokio::process::{Child, Command};

use crate::auth::{
    CHANNEL_TOKEN_HEADER, ChannelTokenTable, ClientIdentity, TokenHandle, generate_token,
};
use crate::{GatewayError, Result};

/// Env var: WebSocket URL of the gateway's channel listener, e.g.
/// `ws://127.0.0.1:42111/v1/channel-ws`. The child dials this
/// directly with a standard WebSocket client — no custom scheme.
pub const ENV_CHANNEL_URL: &str = "BAYBO_CHANNEL_URL";
/// Env var: hex-encoded capability token the child presents as
/// [`CHANNEL_TOKEN_HEADER`].
pub const ENV_CHANNEL_TOKEN: &str = "BAYBO_CHANNEL_TOKEN";

/// Env vars a supervised channel sidecar is allowed to inherit from
/// the gateway. Anything else (`OPENAI_API_KEY`, every `BAYBO_*` from
/// the operator's shell, cloud / proxy creds, …) is scrubbed before
/// `execve` so a compromised JS bundle can't read the gateway's
/// secret env. The one exception is the operator-configured egress
/// proxy (`baybo.json`'s `proxy`): `ChannelSpawner::spawn` re-injects
/// the standard `*_PROXY` vars from it after the scrub, since shell-
/// inherited proxy env is wiped but sidecars still need to reach the
/// network through the configured proxy. Mirrors the registration-mode
/// allowlist in
/// `cli::commands::channel::register::scrubbed_env`. Tool-domain
/// sidecars (the embedded browser MCP server today) flow through
/// `baybo_tools::mcp::transport::connect_with_extra_env`, which
/// applies its own scrubbing list — keep these two in mind together
/// when adding a new env var that needs to reach a child.
pub const SIDECAR_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "TERM",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_COLLATE",
    "LC_MONETARY",
    "TZ",
    "TMPDIR",
    // Browser tool sidecar resolves its profile dir under
    // `$XDG_CACHE_HOME/baybo/browser/...`. Channel sidecars don't
    // need it but inheriting it costs nothing.
    "XDG_CACHE_HOME",
];

/// Spawns channel-plugin subprocesses and mints their tokens. Cheap
/// to clone — every field is already a small value.
#[derive(Clone)]
pub struct ChannelSpawner {
    url: String,
    tokens: ChannelTokenTable,
    /// Egress proxy injected into each sidecar's env. `env_clear` below
    /// wipes any inherited `*_PROXY`, so we re-add the standard vars
    /// explicitly when configured. `None` = sidecars run direct.
    proxy: Option<baybo_security::http::ProxySettings>,
}

impl ChannelSpawner {
    pub fn new(
        url: String,
        tokens: ChannelTokenTable,
        proxy: Option<baybo_security::http::ProxySettings>,
    ) -> Self {
        Self { url, tokens, proxy }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Spawn `cmd` as a channel client.
    ///
    /// `label` is recorded on the minted token for diagnostics (e.g.
    /// `"sidecar-telegram"`). `bound_channel_type` constrains the
    /// `channel_type` the child can claim in its `Register` frame —
    /// the handshake rejects any mismatch so a compromised sidecar
    /// can't impersonate another channel and steal its bot secrets.
    /// The returned [`ChildHandle`] revokes the token on drop; the
    /// child itself is *not* auto-killed (callers own process
    /// lifecycle — the supervisor applies its own restart / kill
    /// policy).
    ///
    /// The child's environment is scrubbed to [`SIDECAR_ENV_ALLOWLIST`]
    /// plus the two channel env vars before exec, so the operator's
    /// `OPENAI_API_KEY`, `BAYBO_*`, cloud creds, etc. never reach the JS bundle.
    pub fn spawn(
        &self,
        mut cmd: Command,
        label: impl Into<String>,
        bound_channel_type: impl Into<String>,
    ) -> Result<ChildHandle> {
        let label = label.into();
        let bound_channel_type = bound_channel_type.into();
        let token = generate_token();

        // Drop everything inherited from the gateway, then re-add the
        // allowlist + the two channel-protocol vars. Order matters:
        // `env_clear` wipes the in-Command env table, *then* the
        // explicit `env(...)` calls populate it.
        cmd.env_clear();
        for key in SIDECAR_ENV_ALLOWLIST {
            if let Ok(v) = std::env::var(key) {
                cmd.env(key, v);
            }
        }
        // Re-add the egress proxy (env_clear wiped any inherited one).
        // Loopback stays direct (see `ProxySettings::env_vars`).
        if let Some(proxy) = &self.proxy {
            for (k, v) in proxy.env_vars() {
                cmd.env(k, v);
            }
        }
        cmd.env(ENV_CHANNEL_URL, &self.url);
        cmd.env(ENV_CHANNEL_TOKEN, &token);

        let child = cmd.spawn().map_err(GatewayError::Io)?;
        let pid = match child.id() {
            Some(p) => p,
            None => {
                return Err(GatewayError::Internal(
                    "spawned child has no PID (already reaped?)".into(),
                ));
            }
        };
        let token_prefix: String = token.chars().take(6).collect();
        tracing::debug!(
            label = %label,
            channel_type = %bound_channel_type,
            pid,
            url = %self.url,
            token_prefix = %token_prefix,
            "channel spawn: minted token + injected env for child",
        );
        let token_handle = self.tokens.register(
            token,
            ClientIdentity {
                pid,
                label: label.clone(),
                bound_channel_type: Some(bound_channel_type),
            },
        );
        Ok(ChildHandle {
            child,
            _token: token_handle,
            label,
            pid,
        })
    }
}

/// Owns a running child and the token it authenticates with. When
/// dropped, the token is revoked in the gateway's table so later
/// connections presenting it are rejected.
pub struct ChildHandle {
    child: Child,
    _token: TokenHandle,
    label: String,
    pid: u32,
}

impl ChildHandle {
    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Header the child must present on every request. Re-exported
    /// so callers don't need to import the constant from
    /// [`crate::auth::token`] directly.
    pub fn token_header() -> &'static str {
        CHANNEL_TOKEN_HEADER
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_names_stable() {
        // Children rely on these; changing them is a protocol break
        // so pin them in a test.
        assert_eq!(ENV_CHANNEL_URL, "BAYBO_CHANNEL_URL");
        assert_eq!(ENV_CHANNEL_TOKEN, "BAYBO_CHANNEL_TOKEN");
    }

    /// End-to-end check that the spawned child does NOT inherit
    /// arbitrary gateway env vars. The previous implementation just
    /// `cmd.env(...)`'d the channel vars on top of the inherited
    /// environment, leaking `OPENAI_API_KEY`-shaped secrets straight
    /// into untrusted JS bundles.
    #[tokio::test]
    async fn spawn_scrubs_inherited_env_vars() {
        struct EnvCleanup(&'static str);
        impl Drop for EnvCleanup {
            fn drop(&mut self) {
                // SAFETY: env mutation is unsynchronised; the var name
                // is unique enough that no other test touches it.
                unsafe { std::env::remove_var(self.0) };
            }
        }

        let secret_var = "BAYBO_TEST_SECRET_THAT_MUST_NOT_LEAK";
        // SAFETY: see EnvCleanup::drop.
        unsafe { std::env::set_var(secret_var, "hunter2") };
        let _cleanup = EnvCleanup(secret_var);

        let tokens = ChannelTokenTable::new();
        let spawner = ChannelSpawner::new(
            "ws://127.0.0.1:1/v1/channel-ws".to_owned(),
            tokens.clone(),
            None,
        );

        let mut cmd = Command::new("/usr/bin/env");
        cmd.stdout(std::process::Stdio::piped());
        let mut handle = spawner.spawn(cmd, "test", "test-channel").unwrap();
        let stdout = handle.child_mut().stdout.take().expect("piped stdout");
        let mut buf = Vec::new();
        use tokio::io::AsyncReadExt;
        let mut stdout = stdout;
        stdout.read_to_end(&mut buf).await.unwrap();
        let _ = handle.child_mut().wait().await;

        let env_dump = String::from_utf8_lossy(&buf);
        assert!(
            !env_dump.contains(secret_var),
            "leaked env var {secret_var} into spawned child:\n{env_dump}",
        );
        // Channel-protocol vars must still be present.
        assert!(env_dump.contains(ENV_CHANNEL_URL), "missing channel URL");
        assert!(
            env_dump.contains(ENV_CHANNEL_TOKEN),
            "missing channel token"
        );
    }

    #[tokio::test]
    async fn spawn_registers_token_with_child_pid() {
        // `true` exits zero immediately — enough to sample its PID.
        let tokens = ChannelTokenTable::new();
        let spawner = ChannelSpawner::new(
            "ws://127.0.0.1:1/v1/channel-ws".to_owned(),
            tokens.clone(),
            None,
        );
        let handle = spawner
            .spawn(Command::new("true"), "test", "test-channel")
            .unwrap();
        assert!(!tokens.is_empty());
        assert_eq!(tokens.len(), 1);
        // The minted token must be bound to "test-channel" so a
        // compromised sidecar can't claim a different channel type.
        let token = handle._token.token().to_string();
        let identity = tokens.lookup(&token).unwrap();
        assert_eq!(identity.bound_channel_type.as_deref(), Some("test-channel"));
        let pid = handle.pid();
        drop(handle);
        // Drop revokes the token.
        assert!(tokens.is_empty(), "handle drop must revoke token");
        assert!(pid > 0);
    }
}
