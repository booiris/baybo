//! Helper for spawning channel sidecars as gateway subprocesses.
//!
//! A subprocess channel plugin connects to the gateway's channel UDS
//! listener just like the TUI, but — unlike the TUI — its identity is
//! bound to its process, not a shared PSK. At spawn time the gateway:
//!
//! 1. Generates a fresh capability token.
//! 2. Sets `AURA_CHANNEL_SOCKET` and `AURA_CHANNEL_TOKEN` in the child's
//!    environment so the child knows where to connect and how to
//!    authenticate.
//! 3. Spawns the child, captures its PID, and registers the token with
//!    that PID in the gateway's [`ChannelTokenTable`].
//! 4. Returns a [`ChildHandle`] whose `Drop` revokes the token — so a
//!    crashed or killed child's token stops being valid the moment the
//!    handle is dropped.
//!
//! This module provides only the spawn primitive. Lifecycle management
//! (restart policy, back-off, plugin manifest loading, …) is the
//! supervisor's job and lives elsewhere.

use std::path::{Path, PathBuf};

use aura_gateway_auth::{
    CHANNEL_TOKEN_HEADER, ChannelTokenTable, ClientIdentity, TokenHandle, generate_token,
};
use tokio::process::{Child, Command};

use crate::{GatewayError, Result};

/// Env var: path of the channel UDS the child must dial.
pub const ENV_CHANNEL_SOCKET: &str = "AURA_CHANNEL_SOCKET";
/// Env var: hex-encoded capability token the child presents as
/// [`CHANNEL_TOKEN_HEADER`].
pub const ENV_CHANNEL_TOKEN: &str = "AURA_CHANNEL_TOKEN";

/// Spawns channel-plugin subprocesses and mints their tokens. Cheap to
/// clone — every field is already an `Arc` or a small value.
#[derive(Clone)]
pub struct ChannelSpawner {
    socket_path: PathBuf,
    tokens: ChannelTokenTable,
}

impl ChannelSpawner {
    pub fn new(socket_path: PathBuf, tokens: ChannelTokenTable) -> Self {
        Self {
            socket_path,
            tokens,
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Spawn `cmd` as a channel client.
    ///
    /// `label` is recorded on the minted token for diagnostics (e.g.
    /// `"telegram"`). The returned [`ChildHandle`] revokes the token on
    /// drop; the child itself is *not* auto-killed (callers own process
    /// lifecycle — the supervisor applies its own restart/kill policy).
    pub fn spawn(&self, mut cmd: Command, label: impl Into<String>) -> Result<ChildHandle> {
        let label = label.into();
        let token = generate_token();
        cmd.env(ENV_CHANNEL_SOCKET, &self.socket_path);
        cmd.env(ENV_CHANNEL_TOKEN, &token);

        let child = cmd.spawn().map_err(GatewayError::Io)?;
        let pid = match child.id() {
            Some(p) => p,
            None => {
                // Already reaped before we could register the token.
                // Nothing to do — the token was never inserted.
                return Err(GatewayError::Internal(
                    "spawned child has no PID (already reaped?)".into(),
                ));
            }
        };
        let token_handle = self.tokens.register(
            token,
            ClientIdentity {
                pid,
                label: label.clone(),
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

    /// Header the child must present on every request. Re-exported so
    /// callers don't need a direct dep on `aura-gateway-auth`.
    pub fn token_header() -> &'static str {
        CHANNEL_TOKEN_HEADER
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_names_stable() {
        // Children rely on these; changing them is a protocol break so
        // pin them in a test.
        assert_eq!(ENV_CHANNEL_SOCKET, "AURA_CHANNEL_SOCKET");
        assert_eq!(ENV_CHANNEL_TOKEN, "AURA_CHANNEL_TOKEN");
    }

    #[tokio::test]
    async fn spawn_registers_token_with_child_pid() {
        // `true` exits zero immediately — enough to sample its PID.
        let tokens = ChannelTokenTable::new();
        let spawner =
            ChannelSpawner::new(PathBuf::from("/tmp/unused-channel.sock"), tokens.clone());
        let handle = spawner.spawn(Command::new("true"), "test").unwrap();
        assert!(!tokens.is_empty());
        assert_eq!(tokens.len(), 1);
        let pid = handle.pid();
        drop(handle);
        // Drop revokes the token.
        assert!(tokens.is_empty(), "handle drop must revoke token");
        // The child process is not auto-killed; with `true` it exits on
        // its own anyway. Nothing else to assert on `pid` beyond that
        // it's a plausible positive value.
        assert!(pid > 0);
    }
}
