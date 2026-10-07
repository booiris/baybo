//! State shared with the WS channel server.
//!
//! Threaded through the axum router used by [`crate::channel::route`] so
//! per-connection tasks can register a [`crate::channel::adapter::Sidecar`]
//! on the workspace [`ChannelRegistry`], validate the caller's capability
//! token against the live [`ChannelTokenTable`], and forward decoded
//! frames onto the router's incoming mpsc.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use baybo_agent::SessionManager;
use baybo_channels::{ChannelRegistry, RouterInbound};
use baybo_pairing::PairingService;
use baybo_security::SecretVault;
use baybo_store::{BlobStore, ChannelBotStore, DeviceStore, TaskStore};

use tokio::sync::mpsc;

use super::bot_reconciler::ChannelBotReconciler;
use super::control::ChannelControlRegistry;
use super::history::TuiHistoryStore;
use super::links::DeviceLinks;
use super::session_resolver::ChannelSessionResolver;
use crate::auth::{AdminAuthState, ChannelTokenTable};
use crate::log_buffer::LogBuffer;
use crate::relay::dial::RelayDialer;
use crate::server::GatewayDeps;
use baybo_channels::InboundDedup;
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;

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
    pub(crate) chat_legs: Arc<ChatLegs>,
    /// The link table: each device's live legs and last direct offer. The
    /// relay legs and the carrier runtime write it; every state built from
    /// the same [`GatewayDeps`] shares one table.
    pub device_links: DeviceLinks,
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
    /// Turn registry. Read on `Subscribe` to derive the client's
    /// `Frame::TurnState` snapshot (is a turn in flight, since when) —
    /// the live `TurnState` broadcasts cover connected clients; this
    /// covers the late joiner who missed them.
    pub turn_lifecycle: Arc<baybo_turn::TurnLifecycle>,
    /// Internal HTTP dispatcher state for relay API-tunnel forwarding.
    pub tunnel_http: TunnelHttpState,
    /// Recent-window dedup for sidecar-supplied
    /// `(channel_type, bot_id, platform_msg_id)` triples. Sidecars that
    /// replay their long-poll buffer after a restart hit this and the
    /// agent sees each upstream event exactly once. Sidecars that omit
    /// `platform_msg_id` opt out — every frame is admitted.
    pub inbound_dedup: Arc<InboundDedup>,
    /// Dials the relay control connection and its data legs.
    pub relay_dialer: RelayDialer,
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
            chat_legs: Arc::new(ChatLegs::new(FIRST_TRANSPORT_MESSAGE_DEADLINE)),
            device_links: deps.device_links.clone(),
            blob_store: deps.stores.blob.clone(),
            task_store: deps.stores.task.clone(),
            turn_lifecycle: Arc::clone(&deps.turn_lifecycle),
            tunnel_http: TunnelHttpState {
                admin: crate::server::AdminState::from_deps(deps),
                auth: AdminAuthState::new(deps.admin_token.clone())
                    .with_device_store(deps.stores.device.clone()),
            },
            inbound_dedup: Arc::clone(&deps.inbound_dedup),
            relay_dialer: deps.relay_dialer.clone(),
        }
    }
}

/// How long a relay chat leg may wait for P's first transport message before
/// it is closed uninstalled. P's `Subscribe` is immediate; at the latest it
/// answers A's first keepalive `Ping`, sent one interval after the leg starts.
pub(crate) const FIRST_TRANSPORT_MESSAGE_DEADLINE: Duration = Duration::from_secs(30);

const _: () = assert!(
    FIRST_TRANSPORT_MESSAGE_DEADLINE.as_millis()
        > super::adapter::KEEPALIVE_PING_INTERVAL.as_millis()
);

/// Every device's live chat leg. A stamps each chat leg with a sequence when it
/// opens it, relay or carrier, and a leg installs only over one opened before
/// it: a leg C held back can never displace a leg opened after it.
pub(crate) struct ChatLegs {
    live: DashMap<String, LiveChatLeg>,
    next_sequence: AtomicU64,
    first_message_deadline: Duration,
}

struct LiveChatLeg {
    sequence: u64,
    abort: tokio::task::AbortHandle,
}

impl ChatLegs {
    pub(crate) fn new(first_message_deadline: Duration) -> Self {
        Self {
            live: DashMap::new(),
            next_sequence: AtomicU64::new(0),
            first_message_deadline,
        }
    }

    /// A chat leg A has just opened, identified by its task's `abort` handle.
    pub(crate) fn opened(self: &Arc<Self>, abort: tokio::task::AbortHandle) -> LegDedup {
        LegDedup {
            legs: Arc::clone(self),
            sequence: self.next_sequence.fetch_add(1, Ordering::Relaxed),
            abort,
        }
    }

    /// How long a relay chat leg waits for P's first transport message.
    pub(crate) fn first_message_deadline(&self) -> Duration {
        self.first_message_deadline
    }
    /// The task of `device_id`'s live chat leg.
    #[cfg(test)]
    pub(crate) fn live_task(&self, device_id: &str) -> Option<tokio::task::Id> {
        self.live.get(device_id).map(|live| live.abort.id())
    }
}

/// A chat leg's handle into [`ChatLegs`]. The relay-content manager takes one
/// for a relay leg, and the carrier runtime for a carrier chat session; the
/// content session calls [`install`](Self::install) once Noise has resolved
/// the `device_id` and proven the initiator live (its handshake confirmation
/// on a carrier, its first decrypted transport message on the relay). A does
/// not rank carriers: the phone decides which chat leg is current, and the
/// leg it opened last wins. The device-blind relay can't dedup, so the
/// gateway must.
pub(crate) struct LegDedup {
    legs: Arc<ChatLegs>,
    sequence: u64,
    abort: tokio::task::AbortHandle,
}

impl LegDedup {
    /// Installs this leg as the live one for `device_id` and aborts the leg it
    /// displaces, unless the live leg was opened after this one: then nothing
    /// changes and this leg must close. Returns whether it installed. The
    /// entry is locked across the check, so two legs racing for one device
    /// leave the later-opened one live.
    pub(crate) fn install(self, device_id: &str) -> bool {
        let leg = LiveChatLeg {
            sequence: self.sequence,
            abort: self.abort,
        };
        match self.legs.live.entry(device_id.to_owned()) {
            Entry::Occupied(mut live) => {
                if live.get().sequence > leg.sequence {
                    return false;
                }
                live.insert(leg).abort.abort();
            }
            Entry::Vacant(vacant) => {
                vacant.insert(leg);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legs() -> Arc<ChatLegs> {
        Arc::new(ChatLegs::new(FIRST_TRANSPORT_MESSAGE_DEADLINE))
    }

    /// A later-opened leg for the same `device_id` displaces and aborts the
    /// earlier one.
    #[tokio::test]
    async fn dedup_aborts_the_displaced_leg_for_a_device() {
        let legs = legs();
        let first = tokio::spawn(std::future::pending::<()>());
        let second = tokio::spawn(std::future::pending::<()>());
        let second_handle = second.abort_handle();

        assert!(legs.opened(first.abort_handle()).install("dev-1"));
        assert!(legs.opened(second.abort_handle()).install("dev-1"));

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

    /// A leg opened before the live one installs late, as a leg whose first
    /// message C held back would: it is refused and the live leg survives.
    #[tokio::test]
    async fn a_leg_opened_earlier_never_displaces_one_opened_later() {
        let legs = legs();
        let held = tokio::spawn(std::future::pending::<()>());
        let live = tokio::spawn(std::future::pending::<()>());
        let live_handle = live.abort_handle();
        let held_dedup = legs.opened(held.abort_handle());
        assert!(legs.opened(live.abort_handle()).install("dev-1"));

        assert!(
            !held_dedup.install("dev-1"),
            "the earlier-opened leg is refused"
        );
        assert!(!live_handle.is_finished(), "the live leg is untouched");
        live.abort();
        held.abort();
    }

    /// Legs for different devices are independent — installing one never aborts
    /// the other.
    #[tokio::test]
    async fn dedup_is_per_device() {
        let legs = legs();
        let a = tokio::spawn(std::future::pending::<()>());
        let b = tokio::spawn(std::future::pending::<()>());
        let (a_handle, b_handle) = (a.abort_handle(), b.abort_handle());

        let a_dedup = legs.opened(a.abort_handle());
        assert!(legs.opened(b.abort_handle()).install("dev-2"));
        assert!(a_dedup.install("dev-1"));

        assert!(!a_handle.is_finished(), "dev-1 leg untouched");
        assert!(!b_handle.is_finished(), "dev-2 leg untouched");
        a.abort();
        b.abort();
    }
}
