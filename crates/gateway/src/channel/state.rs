//! State shared with the WS channel server.
//!
//! Threaded through the axum router used by [`crate::channel::route`] so
//! per-connection tasks can register a [`crate::channel::adapter::Sidecar`]
//! on the workspace [`ChannelRegistry`], validate the caller's capability
//! token against the live [`ChannelTokenTable`], and forward decoded
//! frames onto the router's incoming mpsc.

use std::sync::Arc;

use baybo_agent::SessionManager;
use baybo_channels::{ChannelRegistry, RouterInbound};
use baybo_pairing::{DevicePairingService, PairingService};
use baybo_security::SecretVault;
use baybo_store::{BlobStore, ChannelBotStore, DeviceStore, TaskStore};

use tokio::sync::mpsc;

use super::bot_reconciler::ChannelBotReconciler;
use super::control::ChannelControlRegistry;
use super::dedup::InboundDedup;
use super::history::TuiHistoryStore;
use super::session_resolver::ChannelSessionResolver;
use super::web_token_janitor::StashedTokenHandle;
use crate::auth::ChannelTokenTable;
use crate::log_buffer::LogBuffer;
use crate::server::GatewayDeps;
use dashmap::DashMap;

/// State passed to the `/v1/channel-ws` handler. Cheap to clone — every
/// field is an `Arc` or a clone-cheap handle.
#[derive(Clone)]
pub struct WsChannelState {
    pub registry: Arc<ChannelRegistry>,
    pub incoming_tx: mpsc::Sender<RouterInbound>,
    pub tokens: ChannelTokenTable,
    /// Stash of live web-chat token handles. The admin mint
    /// endpoint inserts here keyed by the token string; the channel
    /// WS route takes the matching handle out on successful upgrade
    /// and moves it into the resulting [`super::adapter::Sidecar`]
    /// so the token revokes itself when the WS closes. Shared with
    /// [`crate::server::AdminState::web_chat_tokens`].
    pub web_chat_tokens: Arc<DashMap<String, StashedTokenHandle>>,
    pub session_manager: Arc<SessionManager>,
    /// Vault-backed TUI input-history store. Shared across every
    /// concurrent TUI client on this gateway — the server is the single
    /// writer of the `baybo.tui.input_history` vault key, so an
    /// in-process `tokio::sync::Mutex` inside the store is enough to
    /// serialise concurrent appends.
    pub tui_history: Arc<TuiHistoryStore>,
    /// Shared ring buffer of recent tracing events. Sidecars emit
    /// their own log lines as NDJSON on stdout/stderr; the supervisor's
    /// pipe drain parses those into structured records and pushes them
    /// here so the admin `/v1/logs` view surfaces sidecar output
    /// alongside gateway-internal tracing.
    pub log_buffer: Arc<LogBuffer>,
    /// Resolves `(channel_type, user_id)` → baybo `session_id` for
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
    /// triples get a short code back via [`baybo_channels::wire::Frame::Notice`]
    /// and their message is dropped. See `docs/modules/pairing.md`.
    pub pairing: Arc<PairingService>,
    /// iOS-companion device pairing: mints/claims SPAKE2 slots and finalizes a
    /// completed handshake into a pending device row. Drives the token-free
    /// `/v1/device/pair` WS route (A's static Noise key is loaded lazily from
    /// `secret_vault` per handshake).
    pub device_pairing: Arc<DevicePairingService>,
    /// Persisted device registry. The `/v1/device/content` route re-fetches the
    /// authenticated device's row to verify the Noise IK initiator's static key
    /// equals the `device_pubkey` exchanged at pairing.
    pub device_store: Arc<dyn DeviceStore>,
    /// Base WS URL of the blind relay (C), or empty when relay is disabled.
    /// Advertised to the app in `GatewayWelcome.relay_url` so a device that
    /// paired directly can still fall back to the relay; non-empty also gates
    /// whether pairing hands out a `relay_node_id`.
    pub relay_url: String,
    /// Direct-reachability endpoints handed to a pairing device inside the
    /// SPAKE2 K-channel (`GatewayWelcome.direct_candidates`). Empty when
    /// `gateway.direct.enabled` is false.
    pub device_direct_candidates: Vec<String>,
    /// Gateway-mediated APNs registrar: on a successful pairing, A relays the
    /// device's APNs token to the remote host (C). `None` when push isn't
    /// configured, so the device-pair route simply skips registration.
    pub apns_registrar: Option<Arc<dyn crate::push::ApnsRegistrar>>,
    /// Backing store for non-text media. Sidecars upload via
    /// `POST /v1/blobs`, the agent emits replies that reference blobs
    /// the gateway already has, and `GET /v1/blobs/{id}` lets sidecars
    /// fetch outbound bytes back. The wire only carries `blob_id`s; this
    /// store is the source of truth for the actual bytes.
    pub blob_store: Arc<dyn BlobStore>,
    /// Per-session planning checklist. Read on `Subscribe` to hydrate the
    /// client's `Frame::TaskList` snapshot, so a reload / reconnect / view-cache
    /// eviction recovers the durable list without waiting for the next turn.
    pub task_store: Arc<dyn TaskStore>,
    /// Job registry. Read on `Subscribe` to derive the client's
    /// `Frame::TurnState` snapshot (is a turn in flight, since when) —
    /// the live `TurnState` broadcasts cover connected clients; this
    /// covers the late joiner who missed them.
    pub job_lifecycle: Arc<baybo_job::JobLifecycle>,
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
        let device_pairing = Arc::new(DevicePairingService::new(
            deps.stores.device_pairing.clone(),
            deps.stores.device.clone(),
        ));
        let direct = &deps.config.gateway.direct;
        let device_direct_candidates = if direct.enabled {
            direct.advertise.clone()
        } else {
            Vec::new()
        };
        let push = &deps.config.gateway.push;
        // Proxy-aware client (the egress proxy applies to the C `/register`
        // POST). `from_deps` is infallible, so a malformed proxy degrades to
        // None — the same misconfig fails loudly when the dispatcher is built.
        let apns_registrar: Option<Arc<dyn crate::push::ApnsRegistrar>> =
            if push.enabled && !push.gateway_url.is_empty() {
                let proxy =
                    deps.config
                        .proxy
                        .as_ref()
                        .map(|p| baybo_security::http::ProxySettings {
                            url: p.url.clone(),
                            no_proxy: p.no_proxy.clone(),
                        });
                baybo_security::http::client(proxy.as_ref())
                    .ok()
                    .map(|client| {
                        Arc::new(crate::push::HttpApnsRegistrar::new(
                            &push.gateway_url,
                            push.instance_key.clone(),
                            client,
                        )) as Arc<dyn crate::push::ApnsRegistrar>
                    })
            } else {
                None
            };
        Self {
            registry: Arc::clone(&deps.channel_registry),
            incoming_tx: deps.incoming_tx.clone(),
            tokens: deps.channel_tokens.clone(),
            web_chat_tokens: Arc::clone(&deps.web_chat_tokens),
            session_manager: Arc::clone(&deps.session_manager),
            tui_history,
            log_buffer: Arc::clone(&deps.log_buffer),
            session_resolver,
            control: Arc::clone(&deps.channel_control),
            channel_bot_store: deps.stores.channel_bot.clone(),
            secret_vault: Arc::clone(&deps.secret_vault),
            bot_reconciler: Arc::clone(&deps.bot_reconciler),
            pairing,
            device_pairing,
            device_store: deps.stores.device.clone(),
            relay_url: deps
                .runtime_config
                .relay
                .as_ref()
                .map(|r| r.url.clone())
                .unwrap_or_default(),
            device_direct_candidates,
            apns_registrar,
            blob_store: deps.stores.blob.clone(),
            task_store: deps.stores.task.clone(),
            job_lifecycle: Arc::clone(&deps.job_lifecycle),
            inbound_dedup: Arc::new(InboundDedup::new()),
        }
    }
}
