//! The Baybo iOS core — the SwiftUI app's engine, exported over UniFFI.
//!
//! A thin FFI shell around the host-tested `baybo-mobile-core`: Swift drives
//! scan-to-connect, chat, and attachments through [`BayboClient`], while remote
//! notifications are handled out-of-process by the Notification Service
//! Extension. The protocol/crypto live in the shared crates, so interop with the
//! gateway is guaranteed by construction.
//!
//! Lifted from the Tauri shell (`app/mobile/src-tauri`): the command surface is
//! the same, with Tauri channels/events replaced by the callback interfaces in
//! [`api`] and the ambient Tauri runtime replaced by the owned one in
//! [`runtime`].

mod api;
mod apns;
mod binding;
mod direct;
mod keychain;
mod logging;
mod qr;
mod relay;
mod runtime;
mod transport;

use std::sync::Arc;

use baybo_mobile_core::WireAttachment;

pub use api::{
    ApnsEnvironment, AttachmentKind, AttachmentRef, BayboError, ClientConfig, FrameSink,
    PairAbortListener, PairChallenge, PairTarget, PairedSummary,
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
            relay: relay::RelaySessions::new(apns.clone()),
            direct: direct::DirectSessions::default(),
            pairing: relay::PairingSessions::default(),
            apns,
        })
    }

    /// Store the APNs device token (lowercase hex), delivered by Swift's
    /// `didRegisterForRemoteNotificationsWithDeviceToken`. Read by pairing, the
    /// relay leg's token-refresh opening frame, and direct push registration.
    pub fn set_apns_token(&self, token_hex: String) {
        self.apns.set_token(token_hex);
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

    /// Direct (non-relay) login: validate the gateway base URL + admin token
    /// against `GET /v1/status`, then persist them. Returns the normalized base
    /// URL.
    pub async fn direct_login(
        self: Arc<Self>,
        base_url: String,
        token: String,
    ) -> Result<String, BayboError> {
        runtime::run(direct::login(base_url, token)).await
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
            direct::register_push(token, this.apns.env().as_str()).await
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
            // Run both wipes regardless of which errored, then surface the first
            // failure.
            let direct_wiped = direct::logout();
            let relay_wiped = relay::forget_pairing();
            direct_wiped.and(relay_wiped)
        })
        .await
    }

    /// Mint a fresh chat session for the active binding and return its id. Direct
    /// mints a gateway session over REST (the id is server-assigned + a channel
    /// token is stashed for the WS/blob legs); relay picks a fresh client id (the
    /// relay leg needs no gateway pre-registration).
    pub async fn chat_create_session(self: Arc<Self>) -> Result<String, BayboError> {
        let this = self;
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Direct => direct::session_create(&this.direct).await,
                ActiveLeg::Relay => Ok(uuid::Uuid::new_v4().to_string()),
            }
        })
        .await
    }

    /// Open the chat session for `session_id` on the active binding's leg and
    /// stream frames to `sink`. Relay runs the Noise E2E content leg; direct runs
    /// the raw-MessagePack `/v1/channel-ws` web-identity leg. `since_ordinal` is
    /// the highest ordinal already rendered — the gateway replays only the gap
    /// above it (so a reconnect after a background catches up without re-sending
    /// the whole thread); `None` is a fresh subscribe with no catch-up. Both legs
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
                    transport::connect(&this.relay, session_id, since_ordinal, sink).await
                }
                ActiveLeg::Direct => {
                    transport::connect(&this.direct, session_id, since_ordinal, sink).await
                }
            }
        })
        .await
    }

    /// Send a user message on the live chat session for the active binding's leg.
    /// `msg_id` is a fresh per-send idempotency key so a retry doesn't double-fire
    /// the agent. `attachments` are content-addressed blobs already uploaded over
    /// a blob leg (empty for a text-only send). Relay sends as device/ios, direct
    /// as web-operator/http.
    pub async fn chat_send(
        self: Arc<Self>,
        text: String,
        msg_id: String,
        attachments: Vec<AttachmentRef>,
    ) -> Result<(), BayboError> {
        let this = self;
        runtime::run(async move {
            let attachments: Vec<WireAttachment> =
                attachments.into_iter().map(Into::into).collect();
            match active_leg()? {
                ActiveLeg::Relay => transport::send(&this.relay, text, msg_id, attachments).await,
                ActiveLeg::Direct => transport::send(&this.direct, text, msg_id, attachments).await,
            }
        })
        .await
    }

    /// Request a backward page of the live chat session's transcript over the
    /// active binding's leg. `before_ordinal` pages older (`None` = newest page);
    /// `limit` caps the page. The reply is **not** this call's return value: the
    /// gateway answers with a `HistoryPage` frame that streams back through the
    /// session's sink (mirroring how `Subscribe` catch-up replays arrive). Returns
    /// once the request is enqueued on the live leg.
    pub async fn chat_fetch_history(
        self: Arc<Self>,
        before_ordinal: Option<i64>,
        limit: Option<u32>,
    ) -> Result<(), BayboError> {
        let this = self;
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Relay => {
                    transport::fetch_history(&this.relay, before_ordinal, limit).await
                }
                ActiveLeg::Direct => {
                    transport::fetch_history(&this.direct, before_ordinal, limit).await
                }
            }
        })
        .await
    }

    /// Tear down the live chat session (the user left the chat view). Any
    /// leg-specific durable state survives: the direct leg keeps its session id +
    /// channel token for reconnect; the relay leg reloads its pairing record on
    /// the next connect. At most one leg is ever live (one binding), but the
    /// binding may already be gone — a disconnect/unpair deletes the credentials
    /// *before* this fires — so tear both down unconditionally; a disconnect on an
    /// idle registry is a no-op.
    pub async fn chat_disconnect(self: Arc<Self>) {
        let this = self;
        let _ = runtime::run(async move {
            transport::disconnect(&this.relay).await;
            transport::disconnect(&this.direct).await;
            Ok(())
        })
        .await;
    }

    /// Upload a picked image's raw bytes over the active binding's blob
    /// transport. Relay seals + chunks over a dedicated E2E blob leg; direct
    /// POSTs to plain `/v1/blobs` (channel token). Returns the content-addressed
    /// `blob_id` to reference in the next message.
    pub async fn blob_upload_bytes(
        self: Arc<Self>,
        bytes: Vec<u8>,
        mime_type: String,
    ) -> Result<String, BayboError> {
        let this = self;
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Relay => relay::upload_bytes(bytes, mime_type).await,
                ActiveLeg::Direct => direct::upload_bytes(&this.direct, bytes, mime_type).await,
            }
        })
        .await
    }

    /// Fetch an attachment `blob_id` for display over the active binding's blob
    /// transport, returning the verified bytes. Relay downloads over a dedicated
    /// E2E blob leg into a content-addressed on-device cache (reused on the next
    /// render); direct GETs plain `/v1/blobs/{id}` (channel token).
    pub async fn blob_image(self: Arc<Self>, blob_id: String) -> Result<Vec<u8>, BayboError> {
        let this = self;
        runtime::run(async move {
            match active_leg()? {
                ActiveLeg::Relay => relay::image_data(blob_id).await,
                ActiveLeg::Direct => direct::image_data(&this.direct, blob_id).await,
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

/// Select the rustls crypto provider for the process. `tokio-tungstenite` pulls
/// rustls with `default-features = false` (no provider), so the first `wss://`
/// dial — pairing or content — would panic building its `ClientConfig`. Install
/// `ring` once here, before any call can dial, so every dial finds a provider.
fn install_crypto_provider() {
    // Err only if a provider is already installed — harmless, so ignore it.
    let _ = rustls::crypto::ring::default_provider().install_default();
}
