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
    ApnsEnvironment, AttachmentKind, AttachmentRef, BayboError, ChatSessionSummary, ClientConfig,
    FrameSink, PairAbortListener, PairChallenge, PairTarget, PairedSummary, SessionListSink,
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
                    refresh_relay_apns_best_effort(&this.apns).await;
                    transport::preconnect(&this.relay).await
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
    /// raw-MessagePack `/v1/channel-ws` direct device leg. `since_ordinal`
    /// is the highest ordinal already rendered — the gateway replays only the
    /// gap above it (so a reconnect after a background catches up without re-sending the
    /// whole thread); `None` is a fresh subscribe with no catch-up. Both legs
    /// share one pump (see `transport`); only the establish/codec seam differs.
    pub async fn chat_connect(
        self: Arc<Self>,
        session_id: String,
        since_ordinal: Option<i64>,
        sink: Arc<dyn FrameSink>,
    ) -> Result<(), BayboError> {
        let this = self;
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Relay => {
                    refresh_relay_apns_best_effort(&this.apns).await;
                    transport::connect(&this.relay, session_id, since_ordinal, sink).await
                }
                ActiveLeg::Direct => {
                    transport::connect(&this.direct, session_id, since_ordinal, sink).await
                }
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

    /// Subscribe `session_id` if needed and queue a user message behind that
    /// subscription on the active binding's global chat leg.
    pub async fn chat_send_after_connect(
        self: Arc<Self>,
        session_id: String,
        since_ordinal: Option<i64>,
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
                    refresh_relay_apns_best_effort(&this.apns).await;
                    transport::connect_and_send(
                        &this.relay,
                        session_id,
                        since_ordinal,
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
                        since_ordinal,
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

    /// Fetch the forward reconnect catch-up page over the active binding's API
    /// surface and return a native-synthesized `catch_up` JSON frame for the web
    /// transcript bridge.
    pub async fn chat_catch_up(
        self: Arc<Self>,
        session_id: String,
        since_ordinal: i64,
    ) -> Result<String, BayboError> {
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Relay => {
                    gateway_api::fetch_catch_up(&relay::GatewayApi, session_id, since_ordinal).await
                }
                ActiveLeg::Direct => {
                    let client = self.direct.http_client()?;
                    gateway_api::fetch_catch_up(&client, session_id, since_ordinal).await
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
    /// `/v1/blobs/{id}` (Bearer + device id).
    pub async fn blob_download_bytes(
        self: Arc<Self>,
        blob_id: String,
    ) -> Result<Vec<u8>, BayboError> {
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Direct => direct::download_blob_bytes(&self.direct, blob_id).await,
                ActiveLeg::Relay => {
                    gateway_api::download_blob_bytes(&relay::GatewayApi, blob_id).await
                }
            }
        })
        .await
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

async fn refresh_relay_apns_best_effort(apns: &ApnsState) {
    let Some(token) = apns.token() else {
        log::info!("connecting without an APNs token; relay push binding not refreshed");
        return;
    };
    if let Err(e) =
        gateway_api::update_apns_token(&relay::GatewayApi, &token, apns.env().as_str()).await
    {
        log::warn!("relay APNs token refresh failed: {e}");
    }
}

/// Select the rustls crypto provider for the process. `tokio-tungstenite` pulls
/// rustls with `default-features = false` (no provider), so the first `wss://`
/// dial — pairing or content — would panic building its `ClientConfig`. Install
/// `ring` once here, before any call can dial, so every dial finds a provider.
fn install_crypto_provider() {
    // Err only if a provider is already installed — harmless, so ignore it.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
