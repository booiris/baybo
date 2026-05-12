//! Gateway authentication surface.
//!
//! Three sub-modules, one per listener / credential family:
//!
//! * [`admin`] — bearer-token middleware for the admin TCP listener
//!   plus the [`AdminToken`] vault-backed lifecycle helper.
//! * [`channel`] — channel-listener middleware that turns the
//!   `x-aura-channel-token` header into an [`AuthedClient`] for
//!   downstream handlers, distinguishing the bundled TUI from
//!   subprocess sidecars by the reserved [`token::TUI_CLIENT_LABEL`].
//! * [`token`] — the in-memory [`ChannelTokenTable`] keyed on
//!   capability tokens, the [`ClientIdentity`] / [`TokenHandle`]
//!   types, and the wire-level constants
//!   ([`token::CHANNEL_TOKEN_HEADER`],
//!   [`token::TUI_TOKEN_VAULT_KEY`]).
//!
//! Common entry points are re-exported here so internal call sites can
//! write `use crate::auth::{AdminToken, AuthedClient, ChannelTokenTable}`
//! without remembering which sub-module owns each name. The crate root
//! also re-exports the most-used items (see `lib.rs`) so external
//! consumers stay on `aura_gateway::AdminToken` etc.

pub mod admin;
pub mod channel;
pub mod token;

pub use admin::{AdminAuthState, AdminToken, require_admin_token};
pub use channel::{AuthedClient, ChannelAuthState, attach as attach_channel_auth};
pub use token::{
    CHANNEL_TOKEN_HEADER, ChannelTokenTable, ClientIdentity, TOOL_CLIENT_LABEL_PREFIX,
    TUI_CLIENT_LABEL, TUI_TOKEN_VAULT_KEY, TokenHandle, WEB_CLIENT_LABEL_PREFIX, constant_time_eq,
    generate_token,
};
