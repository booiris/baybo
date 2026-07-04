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
use baybo_pairing::PairingService;
use baybo_security::SecretVault;
use baybo_store::{BlobStore, ChannelBotStore, DeviceStore, TaskStore};

use tokio::sync::mpsc;

use super::bot_reconciler::ChannelBotReconciler;
use super::control::ChannelControlRegistry;
use super::dedup::InboundDedup;
use super::history::TuiHistoryStore;
use super::session_resolver::ChannelSessionResolver;
use crate::auth::{AdminAuthState, ChannelTokenTable};
use crate::log_buffer::LogBuffer;
use crate::server::GatewayDeps;
use dashmap::DashMap;

/// State bundle used only by the relay API tunnel's in-process HTTP forwarder.
#[derive(Clone)]
pub struct TunnelHttpState {
    /// Admin HTTP state reused by the API tunnel's in-process HTTP forwarder.
    pub admin: crate::server::AdminState,
    /// Admin/device bearer validator reused after the tunnel boundary injects
    /// the device auth headers.
    pub auth: AdminAuthState,
}

/// State passed to the `/v1/channel-ws` handler. Cheap to clone — every
/// field is an `Arc` or a clone-cheap handle.
#[derive(Clone)]
pub struct WsChannelState {
    pub registry: Arc<ChannelRegistry>,
    pub incoming_tx: mpsc::Sender<RouterInbound>,
    pub tokens: ChannelTokenTable,
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
    /// Persisted device registry. The content session looks a device up by the
    /// Noise IK initiator's static key, matching it against an approved row's
    /// `device_pubkey` from pairing.
    pub device_store: Arc<dyn DeviceStore>,
    /// Gateway-only **device dedup** for relay content legs: `device_id` → the
    /// [`AbortHandle`](tokio::task::AbortHandle) of that device's live content leg.
    /// The relay is device-blind (Noise runs after the byte-splice), so a stale,
    /// half-open leg can only be reaped here: when a fresh leg completes its Noise
    /// handshake and resolves the same `device_id`, it aborts the predecessor.
    /// Bounded by the approved-device count (~1 — one gateway = one app). Only the
    /// **chat** content leg dedups; blob legs run concurrently (one per transfer,
    /// bounded by the relay's per-key connection cap), so they are not registered
    /// here.
    pub device_leg_registry: Arc<DashMap<String, tokio::task::AbortHandle>>,
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
    /// Internal HTTP dispatcher state for relay API-tunnel forwarding.
    pub tunnel_http: TunnelHttpState,
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
            device_store: deps.stores.device.clone(),
            device_leg_registry: Arc::new(DashMap::new()),
            blob_store: deps.stores.blob.clone(),
            task_store: deps.stores.task.clone(),
            job_lifecycle: Arc::clone(&deps.job_lifecycle),
            tunnel_http: TunnelHttpState {
                admin: crate::server::AdminState::from_deps(deps),
                auth: AdminAuthState::new(deps.admin_token.clone())
                    .with_device_store(deps.stores.device.clone()),
            },
            inbound_dedup: Arc::new(InboundDedup::new()),
        }
    }
}

/// A relay content leg's handle into the [`WsChannelState::device_leg_registry`].
/// The relay-content manager builds one from the leg's own `AbortHandle`; the
/// content session calls [`install`](Self::install) once Noise resolves the
/// `device_id`, registering this leg as the device's live one and aborting any
/// stale predecessor. Only the relay path carries one — the device-blind relay
/// can't dedup, so the gateway must.
pub(crate) struct LegDedup {
    pub(crate) registry: Arc<DashMap<String, tokio::task::AbortHandle>>,
    pub(crate) abort: tokio::task::AbortHandle,
}

impl LegDedup {
    /// Install this leg as the live one for `device_id`, aborting the stale leg it
    /// displaces (if any). `DashMap::insert` is atomic per key, so two legs racing
    /// for the same `device_id` leave exactly one survivor — the last to install.
    pub(crate) fn install(self, device_id: &str) {
        if let Some(stale) = self.registry.insert(device_id.to_string(), self.abort) {
            stale.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second leg for the same `device_id` displaces and aborts the first; the
    /// survivor is the last to install (the relay-content dedup invariant).
    #[tokio::test]
    async fn dedup_aborts_the_displaced_leg_for_a_device() {
        let registry: Arc<DashMap<String, tokio::task::AbortHandle>> = Arc::new(DashMap::new());
        let first = tokio::spawn(std::future::pending::<()>());
        let second = tokio::spawn(std::future::pending::<()>());
        let second_handle = second.abort_handle();

        LegDedup {
            registry: Arc::clone(&registry),
            abort: first.abort_handle(),
        }
        .install("dev-1");
        LegDedup {
            registry: Arc::clone(&registry),
            abort: second.abort_handle(),
        }
        .install("dev-1");

        assert!(
            first.await.unwrap_err().is_cancelled(),
            "the displaced leg is aborted"
        );
        assert!(
            !second_handle.is_finished(),
            "the surviving leg keeps running"
        );
        second.abort();
    }

    /// Legs for different devices are independent — installing one never aborts
    /// the other.
    #[tokio::test]
    async fn dedup_is_per_device() {
        let registry: Arc<DashMap<String, tokio::task::AbortHandle>> = Arc::new(DashMap::new());
        let a = tokio::spawn(std::future::pending::<()>());
        let b = tokio::spawn(std::future::pending::<()>());
        let (a_handle, b_handle) = (a.abort_handle(), b.abort_handle());

        LegDedup {
            registry: Arc::clone(&registry),
            abort: a.abort_handle(),
        }
        .install("dev-1");
        LegDedup {
            registry: Arc::clone(&registry),
            abort: b.abort_handle(),
        }
        .install("dev-2");

        assert!(!a_handle.is_finished(), "dev-1 leg untouched");
        assert!(!b_handle.is_finished(), "dev-2 leg untouched");
        a.abort();
        b.abort();
    }
}
