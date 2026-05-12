//! State shared with the WS channel server.
//!
//! Threaded through the axum router used by [`crate::channel::route`] so
//! per-connection tasks can register a [`crate::channel::adapter::Sidecar`]
//! on the workspace [`ChannelRegistry`], validate the caller's capability
//! token against the live [`ChannelTokenTable`], and forward decoded
//! frames onto the router's incoming mpsc.

use std::sync::Arc;

use aura_agent::SessionManager;
use aura_channels::{ChannelRegistry, IncomingMessage};
use aura_pairing::PairingService;
use aura_security::SecretVault;
use aura_storage::{BlobStore, ChannelBotStore};

use tokio::sync::mpsc;

use super::bot_reconciler::ChannelBotReconciler;
use super::control::ChannelControlRegistry;
use super::dedup::InboundDedup;
use super::history::TuiHistoryStore;
use super::session_resolver::ChannelSessionResolver;
use crate::auth::ChannelTokenTable;
use crate::log_buffer::LogBuffer;
use crate::server::GatewayDeps;

/// State passed to the `/v1/channel-ws` handler. Cheap to clone — every
/// field is an `Arc` or a clone-cheap handle.
#[derive(Clone)]
pub struct WsChannelState {
    pub registry: Arc<ChannelRegistry>,
    pub incoming_tx: mpsc::Sender<IncomingMessage>,
    pub tokens: ChannelTokenTable,
    pub session_manager: Arc<SessionManager>,
    /// Vault-backed TUI input-history store. Shared across every
    /// concurrent TUI client on this gateway — the server is the single
    /// writer of the `aura.tui.input_history` vault key, so an
    /// in-process `tokio::sync::Mutex` inside the store is enough to
    /// serialise concurrent appends.
    pub tui_history: Arc<TuiHistoryStore>,
    /// Shared ring buffer of recent tracing events. Sidecars emit
    /// their own log lines as NDJSON on stdout/stderr; the supervisor's
    /// pipe drain parses those into structured records and pushes them
    /// here so the admin `/v1/logs` view surfaces sidecar output
    /// alongside gateway-internal tracing.
    pub log_buffer: Arc<LogBuffer>,
    /// Resolves `(channel_type, user_id)` → aura `session_id` for
    /// sidecars that send `Frame::Message` with an empty `session_id`.
    /// The TUI (which picks its own UUID) bypasses this path entirely.
    pub session_resolver: Arc<ChannelSessionResolver>,
    /// Per-channel-type control-plane handle. The admin thread pushes
    /// `Frame::StartBot` / `Frame::StopBot` frames through this to the
    /// currently-connected sidecar. The WS route task inserts the
    /// entry on successful register and removes it on disconnect.
    pub control: Arc<ChannelControlRegistry>,
    /// Registry of per-channel bot credentials (the token itself lives
    /// in the vault). The WS route reads this on register to stream
    /// `StartBot` for every live bot to the newly-connected sidecar.
    pub channel_bot_store: Arc<dyn ChannelBotStore>,
    /// Shared vault for decrypting bot tokens before shipping them
    /// to a sidecar over the (already-authenticated) WS.
    pub secret_vault: Arc<SecretVault>,
    /// Reconciler handle. The WS route uses its `seed` / `forget`
    /// methods to keep the reconciler's per-sidecar tracked sets in
    /// sync with the initial-register push and disconnect cleanup.
    pub bot_reconciler: Arc<ChannelBotReconciler>,
    /// Gate that decides whether an inbound sidecar message can reach
    /// the agent loop. Unpaired `(channel_type, bot_id, user_id)`
    /// triples get a short code back via [`aura_channels::wire::Frame::Notice`]
    /// and their message is dropped. See `docs/modules/pairing.md`.
    pub pairing: Arc<PairingService>,
    /// Backing store for non-text media. Sidecars upload via
    /// `POST /v1/blobs`, the agent emits replies that reference blobs
    /// the gateway already has, and `GET /v1/blobs/{id}` lets sidecars
    /// fetch outbound bytes back. The wire only carries `blob_id`s; this
    /// store is the source of truth for the actual bytes.
    pub blob_store: Arc<dyn BlobStore>,
    /// Recent-window dedup for sidecar-supplied
    /// `(channel_type, bot_id, platform_msg_id)` triples. Sidecars that
    /// replay their long-poll buffer after a restart hit this and the
    /// agent sees each upstream event exactly once. Sidecars that omit
    /// `platform_msg_id` opt out — every frame is admitted.
    pub inbound_dedup: Arc<InboundDedup>,
}

impl WsChannelState {
    /// Build the WS channel state from the shared [`GatewayDeps`].
    /// Used by both the loopback channel listener and the admin
    /// listener (which co-hosts `/v1/channel-ws` so the browser-side
    /// web chat page can reach the WS over the public admin port).
    pub fn from_deps(deps: &GatewayDeps) -> Self {
        let tui_history = Arc::new(TuiHistoryStore::new(Arc::clone(&deps.secret_vault)));
        let session_resolver = Arc::new(ChannelSessionResolver::new(
            Arc::clone(&deps.session_manager),
            deps.stores.channel_session.clone(),
        ));
        let pairing = Arc::new(PairingService::new(deps.stores.channel_pairing.clone()));
        Self {
            registry: Arc::clone(&deps.channel_registry),
            incoming_tx: deps.incoming_tx.clone(),
            tokens: deps.channel_tokens.clone(),
            session_manager: Arc::clone(&deps.session_manager),
            tui_history,
            log_buffer: Arc::clone(&deps.log_buffer),
            session_resolver,
            control: Arc::clone(&deps.channel_control),
            channel_bot_store: deps.stores.channel_bot.clone(),
            secret_vault: Arc::clone(&deps.secret_vault),
            bot_reconciler: Arc::clone(&deps.bot_reconciler),
            pairing,
            blob_store: deps.stores.blob.clone(),
            inbound_dedup: Arc::new(InboundDedup::new()),
        }
    }
}
