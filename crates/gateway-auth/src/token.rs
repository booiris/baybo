//! One-shot tokens for subprocess-spawned channel clients.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use rand::Rng;
use thiserror::Error;

/// HTTP header the child sends to present its one-shot token.
pub const CHANNEL_TOKEN_HEADER: &str = "x-aura-channel-token";

const TOKEN_BYTES: usize = 32;

/// Identity tied to a minted token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    /// Owning subprocess PID, returned by [`tokio::process::Child::id`].
    pub pid: u32,
    /// Operator-visible label for the client (e.g. "telegram", "discord").
    pub label: String,
}

#[derive(Debug, Error)]
pub enum ChannelTokenError {
    #[error("channel token table lock poisoned")]
    Poisoned,
}

/// In-memory registry of active subprocess tokens keyed by the token
/// itself. Tokens are hex-encoded 256-bit values so they can be passed
/// through environment variables without escaping.
#[derive(Clone, Default)]
pub struct ChannelTokenTable {
    inner: Arc<RwLock<HashMap<String, ClientIdentity>>>,
}

impl ChannelTokenTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a new token and register it against `identity`. The returned
    /// [`TokenHandle`] removes the token on drop.
    pub fn mint(&self, identity: ClientIdentity) -> Result<TokenHandle, ChannelTokenError> {
        self.register(generate_token(), identity)
    }

    /// Register a pre-generated token against `identity`. Used by the
    /// subprocess spawn flow: callers generate the token with
    /// [`generate_token`], pass it to the child via env var, and only
    /// after the child has a PID call back here to register. Returns a
    /// [`TokenHandle`] that revokes on drop.
    pub fn register(
        &self,
        token: String,
        identity: ClientIdentity,
    ) -> Result<TokenHandle, ChannelTokenError> {
        {
            let mut guard = self
                .inner
                .write()
                .map_err(|_| ChannelTokenError::Poisoned)?;
            guard.insert(token.clone(), identity);
        }
        Ok(TokenHandle {
            token,
            table: self.clone(),
        })
    }

    /// Look up the identity for a presented token. Returns `None` when
    /// the token is unknown (revoked, never minted, or tampered with).
    pub fn lookup(&self, token: &str) -> Result<Option<ClientIdentity>, ChannelTokenError> {
        let guard = self.inner.read().map_err(|_| ChannelTokenError::Poisoned)?;
        Ok(guard.get(token).cloned())
    }

    /// Remove `token` from the table. Idempotent — unknown tokens do
    /// nothing.
    pub fn revoke(&self, token: &str) -> Result<(), ChannelTokenError> {
        let mut guard = self
            .inner
            .write()
            .map_err(|_| ChannelTokenError::Poisoned)?;
        guard.remove(token);
        Ok(())
    }

    /// Count of currently-live tokens. Exposed for diagnostics.
    pub fn len(&self) -> Result<usize, ChannelTokenError> {
        let guard = self.inner.read().map_err(|_| ChannelTokenError::Poisoned)?;
        Ok(guard.len())
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().is_ok_and(|g| g.is_empty())
    }
}

/// RAII wrapper — when dropped, the underlying token is revoked.
pub struct TokenHandle {
    token: String,
    table: ChannelTokenTable,
}

impl TokenHandle {
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl Drop for TokenHandle {
    fn drop(&mut self) {
        // Revocation may fail if the RwLock is poisoned; log and move on
        // rather than panicking in a Drop.
        if let Err(e) = self.table.revoke(&self.token) {
            tracing::warn!(error = %e, "channel token revoke on drop failed");
        }
    }
}

/// Generate a fresh 256-bit token, hex-encoded. Exposed so the
/// subprocess spawn helper can hand the token to the child via env var
/// *before* calling [`ChannelTokenTable::register`].
pub fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Constant-time equality on byte slices. Exposed here because both the
/// PSK compare and the token compare live in the auth middleware.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(pid: u32, label: &str) -> ClientIdentity {
        ClientIdentity {
            pid,
            label: label.into(),
        }
    }

    #[test]
    fn mint_and_lookup_round_trip() {
        let t = ChannelTokenTable::new();
        let h = t.mint(ident(42, "tg")).unwrap();
        let got = t.lookup(h.token()).unwrap().unwrap();
        assert_eq!(got.pid, 42);
        assert_eq!(got.label, "tg");
    }

    #[test]
    fn token_is_hex_64() {
        let t = ChannelTokenTable::new();
        let h = t.mint(ident(1, "x")).unwrap();
        assert_eq!(h.token().len(), 64);
        assert!(h.token().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn drop_handle_revokes_token() {
        let t = ChannelTokenTable::new();
        let token_str = {
            let h = t.mint(ident(7, "c")).unwrap();
            h.token().to_owned()
        };
        assert!(t.lookup(&token_str).unwrap().is_none());
        assert!(t.is_empty());
    }

    #[test]
    fn explicit_revoke_removes_entry() {
        let t = ChannelTokenTable::new();
        let h = t.mint(ident(3, "c")).unwrap();
        let token_str = h.token().to_owned();
        t.revoke(&token_str).unwrap();
        assert!(t.lookup(&token_str).unwrap().is_none());
        // Drop is now a no-op.
        drop(h);
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abcd", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abce"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn distinct_mints_produce_distinct_tokens() {
        let t = ChannelTokenTable::new();
        let h1 = t.mint(ident(1, "a")).unwrap();
        let h2 = t.mint(ident(2, "b")).unwrap();
        assert_ne!(h1.token(), h2.token());
        assert_eq!(t.len().unwrap(), 2);
    }
}
