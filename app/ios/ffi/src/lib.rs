//! The Baybo iOS core — the SwiftUI app's engine, exported over UniFFI.
//!
//! A thin FFI shell: Swift drives scan-to-connect, chat, and attachments through
//! [`BayboClient`], while remote notifications are handled out-of-process by the
//! Notification Service Extension. The protocol/crypto live in the shared crates,
//! so interop with the gateway is guaranteed by construction.
//!
//! Lifted from the Tauri shell (`app/mobile/src-tauri`): the command surface is
//! the same, with Tauri channels/events replaced by the callback interfaces in
//! [`api`] and the ambient Tauri runtime replaced by the owned one in
//! [`runtime`].

mod api;
mod apns;
mod binding;
mod blob_helper;
mod core;
mod direct;
mod gateway_api;
mod keychain;
mod logging;
mod qr;
mod relay;
mod runtime;
mod transport;

use std::sync::Arc;

use crate::core::WireAttachment;

pub use api::{
    ApnsEnvironment, ApprovalDecision, AttachmentKind, AttachmentRef, BayboError, BlobProgress,
    ChatSessionSummary, ClientConfig, FrameSink, MessageLookup, PairAbortListener, PairChallenge,
    PairTarget, PairedSummary, SessionListSink,
};
use apns::ApnsState;
use binding::{ActiveLeg, active_leg};

uniffi::setup_scaffolding!();

/// Parse a scanned QR payload (`baybo://pair?...`) into a pairing target.
/// `None` = not a pairing QR — keep scanning. A payload without an explicit
/// relay (`h=`) targets the hosted default.
#[uniffi::export]
pub fn parse_pair_qr(text: String) -> Option<PairTarget> {
    qr::parse_pair_qr(&text)
}

#[uniffi::export]
pub fn new_chat_session_id() -> String {
    baybo_model::SessionId::new().into()
}

/// The app's engine: one long-lived instance owns the live transport legs, the
/// in-flight pairing sessions, and the APNs state. Construct once at launch and
/// share it; the pumps it spawns keep running between calls.
#[derive(uniffi::Object)]
pub struct BayboClient {
    relay: relay::RelaySessions,
    direct: direct::DirectSessions,
    pairing: relay::PairingSessions,
    apns: Arc<ApnsState>,
}

#[uniffi::export]
impl BayboClient {
    /// Build the client: selects the process crypto provider (rustls/ring —
    /// without it the first `wss://` dial panics), installs the log bridge, and
    /// seeds the debug push key when asked (simulator NSE testing).
    #[uniffi::constructor]
    pub fn new(config: ClientConfig) -> Arc<Self> {
        install_crypto_provider();
        logging::install(config.log_dir);
        if let Some(dir) = config.blob_cache_dir {
            blob_helper::set_blob_cache_dir(dir.into());
        }
        #[cfg(all(debug_assertions, target_os = "ios"))]
        debug_seed_push_key();
        let apns = Arc::new(ApnsState::new(config.apns_env));
        Arc::new(Self {
            relay: relay::RelaySessions::new(),
            direct: direct::DirectSessions::default(),
            pairing: relay::PairingSessions::default(),
            apns,
        })
    }

    /// Store the APNs device token (lowercase hex), delivered by Swift's
    /// `didRegisterForRemoteNotificationsWithDeviceToken`. Read by pairing, the
    /// relay token-refresh API call, and direct push registration.
    pub fn set_apns_token(&self, token_hex: String) {
        self.apns.set_token(token_hex);
    }

    /// Drop every warm relay API leg, and orphan the ones currently checked out.
    ///
    /// Called on the app's `.background` edge. iOS suspends the process without
    /// warning and takes the sockets with it, so a pooled leg that outlives a
    /// suspend is a zombie: it looks alive, accepts a write, and then goes silent
    /// until the first-byte timeout.
    ///
    /// Deliberately NOT async, and deliberately not hung off `relay_preconnect`.
    /// It must run synchronously on the scene-phase edge, BEFORE anything a later
    /// Task might do with a leg — `preconnect` is guarded by an in-flight latch
    /// that would skip it in exactly the weak-network case that needs it, and it
    /// races the chat list's own foreground refresh besides.
    pub fn relay_invalidate_api_legs(&self) {
        relay::leg_pool::pool().invalidate();
    }

    /// Install the chat list's session-activity sink: the connection-global
    /// `Frame::SessionActivity` pings (for ANY session, subscribed or not) land
    /// here so the list can bump unread + recency without subscribing every
    /// session. Set once at launch; both legs share it (only one is live at a
    /// time), so warming a leg later still delivers to this sink.
    pub fn set_session_list_sink(&self, sink: Arc<dyn SessionListSink>) {
        transport::set_list_sink(&self.relay, Some(sink.clone()));
        transport::set_list_sink(&self.direct, Some(sink));
    }

    /// The device id of a persisted relay pairing, if any — so a relaunch shows
    /// "connected" instead of the pairing form.
    pub fn paired_device(&self) -> Option<String> {
        relay::paired_device()
    }

    /// The current direct connection's base URL (never the token), if direct
    /// credentials are held — so a relaunch shows "connected" instead of the
    /// login form.
    pub fn direct_status(&self) -> Result<Option<String>, BayboError> {
        direct::status().map_err(BayboError::from_msg)
    }

    /// Scan-to-connect: dial the gateway, run the XXpsk0 handshake through
    /// `DeviceHello`, and return the confirmation code the UI shows the user to
    /// compare against the operator's terminal. `on_abort` carries a gateway-side
    /// cancellation that lands before the user decides, so the UI can dismiss the
    /// confirm screen.
    pub async fn pair_begin(
        self: Arc<Self>,
        target: PairTarget,
        on_abort: Arc<dyn PairAbortListener>,
    ) -> Result<PairChallenge, BayboError> {
        let this = self;
        runtime::run(async move {
            relay::pair_begin(&this.pairing, &this.apns, target, on_abort)
                .await
                .map(PairChallenge::from)
        })
        .await
    }

    /// Phase 2: send the user's decision. On accept — and once the operator also
    /// confirms on their terminal — pairing finalizes and the UI renders the
    /// summary.
    pub async fn pair_confirm(
        self: Arc<Self>,
        device_id: String,
        accepted: bool,
    ) -> Result<PairedSummary, BayboError> {
        let this = self;
        runtime::run(async move {
            relay::pair_confirm(&this.pairing, &device_id, accepted)
                .await
                .map(PairedSummary::from)
        })
        .await
    }

    /// Direct (non-relay) login: validate the gateway base URL + access token
    /// against `GET /v1/status`, then persist them. Returns the normalized base
    /// URL.
    pub async fn direct_login(
        self: Arc<Self>,
        base_url: String,
        token: String,
    ) -> Result<String, BayboError> {
        let this = self;
        runtime::run(async move { direct::login(&this.direct, base_url, token).await }).await
    }

    /// Best-effort direct-mode push registration: provision (or refresh) this
    /// app's push binding with the directly-connected gateway so a backgrounded
    /// direct chat can still buzz. `None` when iOS hasn't issued an APNs token
    /// yet or the gateway has no `[push]` remote host. Returns the bound device
    /// id on success.
    pub async fn register_push(self: Arc<Self>) -> Result<Option<String>, BayboError> {
        let this = self;
        runtime::run(async move {
            let token = this.apns.token().unwrap_or_default();
            direct::register_push(&this.direct, token, this.apns.env().as_str()).await
        })
        .await
    }

    /// Log out (the unified "disconnect"): tear down the live leg and wipe the
    /// app's durable credentials, returning it to unbound. Wipes BOTH legs
    /// unconditionally — "one app binds one Baybo", but a hiccuped best-effort
    /// supersede can transiently leave both credential sets, so routing by the
    /// active leg (and dropping only it) could leave the app silently bound to
    /// the superseded gateway. Deleting an absent credential is a no-op, so the
    /// unconditional both-wipe is safe. Idempotent — an already-unbound app is
    /// fine.
    pub async fn logout(self: Arc<Self>) -> Result<(), BayboError> {
        let this = self;
        runtime::run(async move {
            direct::forget(&this.direct).await;
            transport::disconnect(&this.relay).await;
            // Run every wipe regardless of which errored, then surface the first
            // failure.
            let direct_wiped = direct::logout();
            let relay_wiped = relay::forget_pairing();
            let marker_wiped = crate::keychain::delete_active_binding();
            direct_wiped.and(relay_wiped).and(marker_wiped)
        })
        .await
    }

    /// Ensure `session_id` exists for the active binding and return its id. Both
    /// direct and relay use the gateway API (`POST /v1/chat/sessions`); direct
    /// reaches it over REST, relay through the Noise-protected API tunnel.
    pub async fn chat_create_session(
        self: Arc<Self>,
        session_id: String,
    ) -> Result<String, BayboError> {
        runtime::run(async move {
            let requested = session_id.clone();
            let created = match active_leg()? {
                ActiveLeg::Direct => {
                    let client = self.direct.http_client()?;
                    gateway_api::create_session(&client, &session_id).await
                }
                ActiveLeg::Relay => {
                    gateway_api::create_session(&relay::GatewayApi, &session_id).await
                }
            }?;
            if created != requested {
                return Err(format!(
                    "gateway returned session id {created} for requested session id {requested}"
                ));
            }
            Ok(created)
        })
        .await
    }

    /// Archive or unarchive `session_id` for the active binding
    /// (`PUT /v1/chat/sessions/{id}/archive`). Direct reaches it over REST,
    /// relay through the Noise-protected API tunnel.
    pub async fn chat_set_archived(
        self: Arc<Self>,
        session_id: String,
        archived: bool,
    ) -> Result<(), BayboError> {
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Direct => {
                    let client = self.direct.http_client()?;
                    gateway_api::set_archived(&client, session_id, archived).await
                }
                ActiveLeg::Relay => {
                    gateway_api::set_archived(&relay::GatewayApi, session_id, archived).await
                }
            }
        })
        .await
    }

    /// Pin or unpin `session_id` to the top of the list for the active binding
    /// (`PUT /v1/chat/sessions/{id}/pin`). Direct reaches it over REST, relay
    /// through the Noise-protected API tunnel.
    pub async fn chat_set_pinned(
        self: Arc<Self>,
        session_id: String,
        pinned: bool,
    ) -> Result<(), BayboError> {
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Direct => {
                    let client = self.direct.http_client()?;
                    gateway_api::set_pinned(&client, session_id, pinned).await
                }
                ActiveLeg::Relay => {
                    gateway_api::set_pinned(&relay::GatewayApi, session_id, pinned).await
                }
            }
        })
        .await
    }

    /// Soft-hide `session_id` for the active binding (`DELETE
    /// /v1/chat/sessions/{id}` — the gateway sets `hidden`, never deleting the
    /// row or transcript). Direct reaches it over REST, relay through the
    /// Noise-protected API tunnel.
    pub async fn chat_hide_session(self: Arc<Self>, session_id: String) -> Result<(), BayboError> {
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Direct => {
                    let client = self.direct.http_client()?;
                    gateway_api::hide_session(&client, session_id).await
                }
                ActiveLeg::Relay => gateway_api::hide_session(&relay::GatewayApi, session_id).await,
            }
        })
        .await
    }

    /// List the gateway's chat sessions (newest first, hidden/cron filtered) for
    /// the active binding. Direct uses the admin REST surface; relay uses the
    /// Noise-protected API tunnel so a NAT'd gateway can still refresh the
    /// native list from durable session rows.
    pub async fn chat_list_sessions(
        self: Arc<Self>,
    ) -> Result<Vec<ChatSessionSummary>, BayboError> {
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Direct => {
                    let client = self.direct.http_client()?;
                    gateway_api::list_sessions(&client).await
                }
                ActiveLeg::Relay => gateway_api::list_sessions(&relay::GatewayApi).await,
            }
        })
        .await
    }

    /// Warm the relay content leg without subscribing a session. This is
    /// best-effort app-start latency hiding: once the Noise leg is up, opening a
    /// chat only needs to enqueue `Subscribe`. Direct bindings no-op because the
    /// direct leg dials the admin-authenticated WS on demand.
    pub async fn relay_preconnect(self: Arc<Self>) -> Result<(), BayboError> {
        let this = self;
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Relay => {
                    // Snapshot the pool generation BEFORE the first await. If the app
                    // backgrounds while the chat leg is dialing, the barrier's epoch
                    // bump lands in that window — and a `warm()` that read the epoch
                    // afterwards would stamp its leg as CURRENT and park it, handing
                    // the pool the very zombie the barrier exists to keep out (the
                    // suspend takes that socket with it).
                    let epoch = relay::leg_pool::pool().epoch();
                    // The chat leg first: it is what a chat actually needs, and it
                    // should not race a warm-up for a relay connection slot.
                    transport::preconnect(&this.relay).await?;
                    // Then park one API leg, so the list refresh the user is about
                    // to trigger does not pay a dial.
                    relay::leg_pool::warm(epoch).await;
                    refresh_relay_apns_best_effort(this.apns.clone());
                    Ok(())
                }
                ActiveLeg::Direct => Ok(()),
            }
        })
        .await
    }

    /// Warm the direct device leg without subscribing a session — the direct
    /// analogue of [`Self::relay_preconnect`]. Lets the chat list receive live
    /// `SessionActivity` while parked on the list with no chat open. Relay
    /// bindings no-op (they warm via `relay_preconnect`).
    pub async fn direct_preconnect(self: Arc<Self>) -> Result<(), BayboError> {
        let this = self;
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Direct => transport::preconnect(&this.direct).await,
                ActiveLeg::Relay => Ok(()),
            }
        })
        .await
    }

    /// Subscribe `session_id` on the active binding's global chat leg and stream
    /// frames to `sink`. Relay runs the Noise E2E content leg; direct runs the
    /// raw-MessagePack `/v1/channel-ws` direct device leg. The server replays no
    /// history on subscribe (it answers with one `SubscribeState` bundle) —
    /// transcript recovery is the REST sync call ([`Self::chat_fetch_sync`]).
    /// Both legs share one pump (see `transport`); only the establish/codec seam
    /// differs.
    pub async fn chat_connect(
        self: Arc<Self>,
        session_id: String,
        sink: Arc<dyn FrameSink>,
    ) -> Result<(), BayboError> {
        let this = self;
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Relay => {
                    refresh_relay_apns_best_effort(this.apns.clone());
                    transport::connect(&this.relay, session_id, sink).await
                }
                ActiveLeg::Direct => transport::connect(&this.direct, session_id, sink).await,
            }
        })
        .await
    }

    /// Send a user message to `session_id` on the active binding's global chat
    /// leg. `msg_id` is a fresh per-send idempotency key so a retry doesn't
    /// double-fire the agent. `attachments` are content-addressed blobs already
    /// uploaded over a blob leg (empty for a text-only send). Both relay and
    /// direct send as the device identity on channel `device`.
    pub async fn chat_send(
        self: Arc<Self>,
        session_id: String,
        text: String,
        msg_id: String,
        attachments: Vec<AttachmentRef>,
    ) -> Result<(), BayboError> {
        let this = self;
        runtime::run(async move {
            let attachments: Vec<WireAttachment> =
                attachments.into_iter().map(Into::into).collect();
            match active_leg()? {
                ActiveLeg::Relay => {
                    transport::send(&this.relay, session_id, text, msg_id, attachments).await
                }
                ActiveLeg::Direct => {
                    transport::send(&this.direct, session_id, text, msg_id, attachments).await
                }
            }
        })
        .await
    }

    /// Answer a pending tool-approval prompt (`call_id` from the
    /// `approval_requested` frame, or a `subscribe_state` bundle's
    /// `pending_approvals` card). Fire-and-forget on the active binding's chat
    /// leg: the gateway resolves the gate — releasing the blocked tool call —
    /// and broadcasts `approval_resolved`, which arrives back on the frame
    /// sink. A decision the leg can't deliver errors here; the server-side gate
    /// then times out and denies (fail-closed).
    pub async fn chat_resolve_approval(
        self: Arc<Self>,
        call_id: String,
        decision: ApprovalDecision,
    ) -> Result<(), BayboError> {
        let this = self;
        runtime::run(async move {
            let decision = decision.into();
            match active_leg()? {
                ActiveLeg::Relay => {
                    transport::resolve_approval(&this.relay, call_id, decision).await
                }
                ActiveLeg::Direct => {
                    transport::resolve_approval(&this.direct, call_id, decision).await
                }
            }
        })
        .await
    }

    /// Subscribe `session_id` if needed and queue a user message behind that
    /// subscription on the active binding's global chat leg.
    pub async fn chat_send_after_connect(
        self: Arc<Self>,
        session_id: String,
        sink: Arc<dyn FrameSink>,
        text: String,
        msg_id: String,
        attachments: Vec<AttachmentRef>,
    ) -> Result<(), BayboError> {
        let this = self;
        runtime::run(async move {
            let attachments: Vec<WireAttachment> =
                attachments.into_iter().map(Into::into).collect();
            match active_leg()? {
                ActiveLeg::Relay => {
                    refresh_relay_apns_best_effort(this.apns.clone());
                    transport::connect_and_send(
                        &this.relay,
                        session_id,
                        sink,
                        transport::OutboundMessage {
                            text,
                            msg_id,
                            attachments,
                        },
                    )
                    .await
                }
                ActiveLeg::Direct => {
                    transport::connect_and_send(
                        &this.direct,
                        session_id,
                        sink,
                        transport::OutboundMessage {
                            text,
                            msg_id,
                            attachments,
                        },
                    )
                    .await
                }
            }
        })
        .await
    }

    /// Fetch a backward page of `session_id`'s transcript over the active
    /// binding's API surface and return a native-synthesized `history_page` JSON
    /// frame for the web transcript bridge.
    pub async fn chat_fetch_history(
        self: Arc<Self>,
        session_id: String,
        before_ordinal: Option<i64>,
        limit: Option<u32>,
    ) -> Result<String, BayboError> {
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Relay => {
                    gateway_api::fetch_history_page(
                        &relay::GatewayApi,
                        session_id,
                        before_ordinal,
                        limit,
                    )
                    .await
                }
                ActiveLeg::Direct => {
                    let client = self.direct.http_client()?;
                    gateway_api::fetch_history_page(&client, session_id, before_ordinal, limit)
                        .await
                }
            }
        })
        .await
    }

    /// Run the one forward-recovery pull (sync-v2) over the active binding's API
    /// surface: rows strictly after `since_ordinal` (or the newest-page baseline
    /// when `None`), full fidelity (message | work | notice), unfiltered. Returns
    /// a native-synthesized `sync_page` JSON frame for the web transcript bridge.
    pub async fn chat_fetch_sync(
        self: Arc<Self>,
        session_id: String,
        since_ordinal: Option<i64>,
        limit: u32,
    ) -> Result<String, BayboError> {
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Relay => {
                    gateway_api::fetch_sync(&relay::GatewayApi, session_id, since_ordinal, limit)
                        .await
                }
                ActiveLeg::Direct => {
                    let client = self.direct.http_client()?;
                    gateway_api::fetch_sync(&client, session_id, since_ordinal, limit).await
                }
            }
        })
        .await
    }

    /// Per-send durability point lookup over the active binding's API surface:
    /// whether a persisted row carries `platform_msg_id` (and its ordinal).
    /// Resolves a rebase-floor outbox entry (`unknown`) without consuming a
    /// retry transmission — consumed natively, never forwarded to the webview.
    pub async fn chat_lookup_message(
        self: Arc<Self>,
        session_id: String,
        platform_msg_id: String,
    ) -> Result<MessageLookup, BayboError> {
        runtime::run(async move {
            let response = match active_leg()? {
                ActiveLeg::Relay => {
                    gateway_api::lookup_message(&relay::GatewayApi, session_id, &platform_msg_id)
                        .await
                }
                ActiveLeg::Direct => {
                    let client = self.direct.http_client()?;
                    gateway_api::lookup_message(&client, session_id, &platform_msg_id).await
                }
            }?;
            Ok(MessageLookup {
                found: response.found,
                ordinal: response.ordinal,
            })
        })
        .await
    }

    /// Advance the session's chat-list read cursor to `ordinal` (max-wins
    /// server-side) — the viewer has read up to here. Clears the unread badge
    /// on the next list pull. Called when the user opens/views a session.
    pub async fn chat_mark_read(
        self: Arc<Self>,
        session_id: String,
        ordinal: i64,
    ) -> Result<(), BayboError> {
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Relay => {
                    gateway_api::mark_read(&relay::GatewayApi, session_id, ordinal).await
                }
                ActiveLeg::Direct => {
                    let client = self.direct.http_client()?;
                    gateway_api::mark_read(&client, session_id, ordinal).await
                }
            }
        })
        .await
    }

    /// Tear down the active binding's global chat leg. The relay leg reloads its
    /// pairing record on the next connect. At most one binding mode is live, but
    /// the binding may already be gone — a disconnect/unpair deletes the
    /// credentials *before* this fires — so tear both legs down unconditionally; a
    /// disconnect on an idle registry is a no-op.
    pub async fn chat_disconnect(self: Arc<Self>) {
        let this = self;
        let _ = runtime::run(async move {
            transport::disconnect(&this.relay).await;
            transport::disconnect(&this.direct).await;
            Ok(())
        })
        .await;
    }

    /// Drop one session's sink from the active binding's global chat leg without
    /// tearing the leg down — the client's LRU eviction of an idle, offscreen
    /// session. The shared pump and every other subscribed session stay live;
    /// re-opening the session re-subscribes and gateway history replay catches it
    /// up. Both legs are unsubscribed unconditionally (only one is live; the
    /// other's map simply lacks the id), so it needn't resolve the active binding.
    pub async fn chat_unsubscribe(self: Arc<Self>, session_id: String) {
        let this = self;
        let _ = runtime::run(async move {
            transport::unsubscribe(&this.relay, &session_id).await;
            transport::unsubscribe(&this.direct, &session_id).await;
            Ok(())
        })
        .await;
    }

    /// Upload a picked attachment's raw bytes over the active binding's blob
    /// transport. Relay sends `POST /v1/blobs` over a dedicated E2E API tunnel
    /// blob leg; direct POSTs to plain `/v1/blobs` (Bearer + device id).
    /// Returns the content-addressed `blob_id` to reference in the next message.
    pub async fn blob_upload_bytes(
        self: Arc<Self>,
        bytes: Vec<u8>,
        mime_type: String,
    ) -> Result<String, BayboError> {
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Direct => {
                    let client = self.direct.http_client()?;
                    gateway_api::upload_bytes(&client, bytes, mime_type).await
                }
                ActiveLeg::Relay => {
                    gateway_api::upload_bytes(&relay::GatewayApi, bytes, mime_type).await
                }
            }
        })
        .await
    }

    /// Fetch an attachment `blob_id` for display over the active binding's blob
    /// transport, returning the verified bytes. Relay sends `GET /v1/blobs/{id}`
    /// over a dedicated E2E API tunnel blob leg into a content-addressed
    /// on-device cache (reused on the next render); direct GETs plain
    /// `/v1/blobs/{id}` (Bearer + device id). Both legs resume a partial
    /// download rather than restarting it.
    ///
    /// `progress` observes the byte count while it streams; pass `None` when
    /// nobody is watching (a thumbnail fetch). It never fires for a cache hit —
    /// ask [`Self::blob_is_cached`] first if that distinction matters.
    pub async fn blob_download_bytes(
        self: Arc<Self>,
        blob_id: String,
        progress: Option<Arc<dyn BlobProgress>>,
    ) -> Result<Vec<u8>, BayboError> {
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Direct => {
                    direct::download_blob_bytes(&self.direct, blob_id, progress).await
                }
                ActiveLeg::Relay => {
                    gateway_api::download_blob_bytes(&relay::GatewayApi, blob_id, progress).await
                }
            }
        })
        .await
    }

    /// Is this blob already in the on-device cache? Never downloads, never
    /// touches the network. The cache lives under the OS temp dir, which iOS
    /// purges under storage pressure — re-ask rather than remembering a `true`.
    pub async fn blob_is_cached(self: Arc<Self>, blob_id: String) -> bool {
        // A probe that cannot fail: if the core task somehow can't run, "not
        // cached" is the safe answer — the caller just downloads it again.
        runtime::run(async move { Ok::<_, String>(blob_helper::is_cached(&blob_id).await) })
            .await
            .unwrap_or(false)
    }
}

/// Debug-only: seed a known push key into the shared App Group keychain so the
/// NSE decrypt path can be exercised with `xcrun simctl push` without a live
/// gateway pairing (verify-nse.sh drives this). Reads `BAYBO_SEED_PUSH_KEY` as
/// `<bid>:<64-hex-key>` (absent => no-op). Compiled out of release builds; never
/// logs the key or the bid.
#[cfg(all(debug_assertions, target_os = "ios"))]
fn debug_seed_push_key() {
    let Ok(spec) = std::env::var("BAYBO_SEED_PUSH_KEY") else {
        return;
    };
    let Some((bid, key_hex)) = spec.split_once(':') else {
        return;
    };
    let bid = bid.trim();
    let key: [u8; device_proto::aead::KEY_LEN] = match hex::decode(key_hex.trim()) {
        Ok(b) => match b.try_into() {
            Ok(k) => k,
            Err(_) => return,
        },
        Err(_) => return,
    };
    // Store, then read back (the same lookup the NSE does) and report the
    // round-trip to a file in the app container so the host test harness can
    // read it. No secret or bid is written — only the round-trip verdict.
    let result = match keychain::store_push_key(bid, &key) {
        Ok(()) => match keychain::read_push_key(bid) {
            Ok(Some(k)) if k == key => "store=ok readback=match".to_string(),
            Ok(Some(_)) => "store=ok readback=mismatch".to_string(),
            Ok(None) => "store=ok readback=not_found".to_string(),
            Err(e) => format!("store=ok readback_err={e}"),
        },
        Err(e) => format!("store_err={e}"),
    };
    let _ = std::fs::write(std::env::temp_dir().join("baybo-seed-result.txt"), &result);
    log::debug!("keychain self-check: {result}");
}

/// Tell the gateway this device's APNs token, if it does not already know it.
///
/// Detached from every caller's critical path on purpose. It used to be `.await`ed
/// BEFORE `transport::connect`, so every chat-leg subscribe queued behind it — and
/// with the API leg pool underneath, a foreground (which reconnects up to a dozen
/// resident stores at once) would have put a dozen identical POSTs in front of the
/// chat leg the user is waiting on. The steady-state answer is to send nothing at
/// all; when there IS something to send, it does not need to be first.
fn refresh_relay_apns_best_effort(apns: Arc<ApnsState>) {
    let Some(token) = apns.claim_post() else {
        return;
    };
    tokio::spawn(async move {
        let mut token = token;
        loop {
            let delivered = match gateway_api::update_apns_token(
                &relay::GatewayApi,
                &token,
                apns.env().as_str(),
            )
            .await
            {
                Ok(()) => true,
                Err(e) => {
                    log::warn!("relay APNs token refresh failed: {e}");
                    false
                }
            };
            apns.finish_post(token, delivered);
            if !delivered {
                return;
            }
            // iOS rotated the token while that POST was on the wire. Chase it here
            // rather than leave the gateway bound to the one it just retired and
            // hope some later connect notices.
            match apns.claim_post() {
                Some(next) => token = next,
                None => return,
            }
        }
    });
}

/// Select the rustls crypto provider for the process. `tokio-tungstenite` pulls
/// rustls with `default-features = false` (no provider), so the first `wss://`
/// dial — pairing or content — would panic building its `ClientConfig`. Install
/// `ring` once here, before any call can dial, so every dial finds a provider.
fn install_crypto_provider() {
    // Err only if a provider is already installed — harmless, so ignore it.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
