//! Channel-listener token primitives shared by the gateway server, the
//! bundled TUI, and the bin's `baybo gateway` / `baybo tui` boot paths.
//!
//! Two flavours of token end up in the same [`ChannelTokenTable`]:
//!
//! * **TUI token** — generated when the gateway boots, stashed in the
//!   secret vault under [`TUI_TOKEN_VAULT_KEY`], and registered with
//!   the reserved [`TUI_CLIENT_LABEL`]. The bundled `baybo tui` reads
//!   it from the vault and presents it on the channel WebSocket
//!   upgrade in [`CHANNEL_TOKEN_HEADER`]. The gateway holds the
//!   returned [`TokenHandle`] for its whole lifetime so the token is
//!   revoked from the live table on shutdown.
//! * **Subprocess capability tokens** — generated when the gateway
//!   spawns a channel sidecar, handed to the child via env var, and
//!   revoked when the owning [`crate::spawn::ChildHandle`] (which
//!   holds the [`TokenHandle`]) drops.

use std::sync::Arc;

use dashmap::DashMap;
use rand::Rng;

/// HTTP header the child or TUI sends to present its capability token.
pub const CHANNEL_TOKEN_HEADER: &str = "x-baybo-channel-token";

/// Reserved [`ClientIdentity::label`] value the gateway uses when it
/// registers the bundled-TUI token at startup. The auth middleware uses
/// this constant to distinguish a TUI-flavoured connection from a
/// subprocess sidecar connection — no other client may register under
/// this label.
pub const TUI_CLIENT_LABEL: &str = "tui";

/// Prefix for tokens minted to embedded tool sidecars (the browser
/// MCP server today). The auth middleware recognises any label
/// starting with this prefix as [`crate::AuthedClient::Tool`], which
/// bypasses pairing on `/v1/blobs` (tool sidecars are session-scoped
/// like the TUI, not per-bot/per-user) and is rejected from the
/// channel-WS handshake (tool sidecars don't register channels).
pub const TOOL_CLIENT_LABEL_PREFIX: &str = "tool/";

/// Prefix for tokens minted to admin-side web chat tabs. The admin API
/// `POST /v1/chat/session` exchanges the operator's admin bearer for a
/// short-lived channel-token under this label; the channel auth
/// middleware turns it into [`crate::AuthedClient::Web`] which is the
/// sole identity allowed to claim the otherwise-reserved `"http"`
/// channel type on `/v1/channel-ws`.
///
/// Lifecycle: the admin mint stashes the [`TokenHandle`] in
/// `AdminState::web_chat_tokens` keyed by token string. The channel
/// WS route removes the matching entry on successful upgrade and
/// moves the handle into the `Sidecar`, so the token revokes itself
/// when the WS closes. Handles still in the map (mint without a
/// follow-up WS upgrade) are released at process exit.
pub const WEB_CLIENT_LABEL_PREFIX: &str = "web/";

/// Synthetic `User.id` the chat API stamps on sessions originated from
/// the browser. Sessions on the `http` channel don't have an external
/// per-user identity the way Telegram/WeChat do — every web tab the
/// operator opens is the same human at the keyboard — so we collapse
/// them under one well-known id. Used both server-side
/// (`create_session` user, dispatch routing) and on the wire when the
/// web client constructs outbound `Frame::Send` envelopes.
pub const WEB_OPERATOR_USER_ID: &str = "web-operator";

/// Secret-vault key under which the gateway publishes the current
/// generation of the TUI channel token. Rotated on every `baybo
/// gateway start`; the bundled `baybo tui` reads it back from the vault
/// to authenticate against the channel listener. Both ends must agree
/// on the key, hence pinning it here.
pub const TUI_TOKEN_VAULT_KEY: &str = "gateway.tui_token";

const TOKEN_BYTES: usize = 32;

/// Identity tied to a minted token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    /// Owning subprocess PID, returned by [`tokio::process::Child::id`].
    /// Gateway-owned credentials (the bundled TUI) report the gateway's
    /// own pid here for diagnostic symmetry.
    pub pid: u32,
    /// Operator-visible label for the client (e.g. "sidecar-telegram",
    /// "sidecar-discord", or [`TUI_CLIENT_LABEL`]).
    pub label: String,
    /// When `Some(ct)`, the bearer of this token may *only* register
    /// as channel type `ct`. The handshake rejects any other claimed
    /// type so a compromised sidecar can't impersonate a different
    /// channel and trick the gateway into pushing that channel's bot
    /// secrets to it. `None` for the gateway-issued TUI token: the
    /// TUI's channel type is enforced via [`TUI_CLIENT_LABEL`] in the
    /// handshake instead.
    pub bound_channel_type: Option<String>,
}

/// In-memory registry of active subprocess tokens keyed by the token
/// itself. Tokens are hex-encoded 256-bit values so they can be passed
/// through environment variables without escaping.
#[derive(Clone, Default)]
pub struct ChannelTokenTable {
    inner: Arc<DashMap<String, ClientIdentity>>,
}

impl ChannelTokenTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a new token and register it against `identity`. The returned
    /// [`TokenHandle`] removes the token on drop.
    pub fn mint(&self, identity: ClientIdentity) -> TokenHandle {
        self.register(generate_token(), identity)
    }

    /// Register a pre-generated token against `identity`. Used by the
    /// subprocess spawn flow: callers generate the token with
    /// [`generate_token`], pass it to the child via env var, and only
    /// after the child has a PID call back here to register. Returns a
    /// [`TokenHandle`] that revokes on drop.
    pub fn register(&self, token: String, identity: ClientIdentity) -> TokenHandle {
        self.inner.insert(token.clone(), identity);
        TokenHandle {
            token,
            table: self.clone(),
        }
    }

    /// Look up the identity for a presented token. Returns `None` when
    /// the token is unknown (revoked, never minted, or tampered with).
    pub fn lookup(&self, token: &str) -> Option<ClientIdentity> {
        self.inner.get(token).map(|e| e.value().clone())
    }

    /// Remove `token` from the table. Idempotent — unknown tokens do
    /// nothing.
    pub fn revoke(&self, token: &str) {
        self.inner.remove(token);
    }

    /// Count of currently-live tokens. Exposed for diagnostics.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
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
        self.table.revoke(&self.token);
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

/// Constant-time equality on byte slices. Exposed here so callers
/// outside the auth middleware can reuse the same compare helper for
/// any other token-shaped credential without pulling in `subtle`.
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
            bound_channel_type: None,
        }
    }

    #[test]
    fn mint_and_lookup_round_trip() {
        let t = ChannelTokenTable::new();
        let h = t.mint(ident(42, "tg"));
        let got = t.lookup(h.token()).unwrap();
        assert_eq!(got.pid, 42);
        assert_eq!(got.label, "tg");
    }

    #[test]
    fn token_is_hex_64() {
        let t = ChannelTokenTable::new();
        let h = t.mint(ident(1, "x"));
        assert_eq!(h.token().len(), 64);
        assert!(h.token().chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn drop_handle_revokes_token() {
        let t = ChannelTokenTable::new();
        let token_str = {
            let h = t.mint(ident(7, "c"));
            h.token().to_owned()
        };
        assert!(t.lookup(&token_str).is_none());
        assert!(t.is_empty());
    }

    #[test]
    fn explicit_revoke_removes_entry() {
        let t = ChannelTokenTable::new();
        let h = t.mint(ident(3, "c"));
        let token_str = h.token().to_owned();
        t.revoke(&token_str);
        assert!(t.lookup(&token_str).is_none());
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
        let h1 = t.mint(ident(1, "a"));
        let h2 = t.mint(ident(2, "b"));
        assert_ne!(h1.token(), h2.token());
        assert_eq!(t.len(), 2);
    }
}
