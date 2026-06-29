//! The Baybo iOS companion (Tauri shell).
//!
//! A thin native shell around the host-tested `baybo-mobile-core`: the webview
//! drives scan-to-connect, chat, and attachments, while remote notifications are
//! handled out-of-process by the Notification Service Extension under `../apple`.
//! The protocol/crypto live in the shared crates, so interop with the gateway is
//! guaranteed by construction.

mod direct;
mod keychain;
mod push_register;
mod relay;
mod transport;

use baybo_mobile_core::{Frame, WireAttachment};
use relay::RelaySessions;
use relay::{PairAborted, PairChallenge, PairedSummary, PairingSessions};
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, State};

/// Which chat leg an IPC command routes to: the relay (Noise E2E) leg or the
/// direct (raw-MessagePack `/v1/channel-ws`) leg. Threaded from the webview as a
/// typed value so the chat/blob commands dispatch with a Rust `match` instead of
/// the caller picking a per-leg command name by string. The wire values
/// (`"relay"` / `"direct"`) match the webview's `ChatTransport` type.
#[derive(serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum ChatLeg {
    Relay,
    Direct,
}

/// Header the raw-body blob upload reads its mime from — the JSON arg slot carries
/// the bytes, so mime + leg ride headers instead.
const BLOB_MIME_HEADER: &str = "x-baybo-mime";
/// Header the raw-body blob upload reads its [`ChatLeg`] from (see [`BLOB_MIME_HEADER`]).
const BLOB_LEG_HEADER: &str = "x-baybo-leg";

impl ChatLeg {
    /// Resolve the leg from a raw-body upload's `x-baybo-leg` header — relay when
    /// absent or unrecognized (the safe default; matches the webview's default).
    /// The value mirrors the `#[serde(rename_all = "lowercase")]` wire form.
    fn from_request(request: &tauri::ipc::Request<'_>) -> Self {
        match request
            .headers()
            .get(BLOB_LEG_HEADER)
            .and_then(|v| v.to_str().ok())
        {
            Some("direct") => ChatLeg::Direct,
            _ => ChatLeg::Relay,
        }
    }
}

/// Scan-to-connect: dial the gateway, run the XXpsk0 handshake through
/// `DeviceHello`, and return the confirmation code the UI shows the user to
/// compare against the operator's terminal. `rendezvous_id` + `secret` come from
/// the QR (the secret is the Noise PSK). `on_abort` carries a gateway-side
/// cancellation that lands before the user decides, so the UI can dismiss the
/// confirm screen.
#[tauri::command]
async fn pair_begin(
    sessions: State<'_, PairingSessions>,
    endpoint: String,
    rendezvous_id: String,
    secret: String,
    remote_api_key: Option<String>,
    on_abort: Channel<PairAborted>,
) -> Result<PairChallenge, String> {
    relay::pair_begin(
        &sessions,
        &endpoint,
        &rendezvous_id,
        &secret,
        remote_api_key,
        on_abort,
    )
    .await
}

/// Phase 2: send the user's decision. On accept — and once the operator also
/// confirms on their terminal — pairing finalizes and the UI renders the summary.
#[tauri::command]
async fn pair_confirm(
    sessions: State<'_, PairingSessions>,
    device_id: String,
    accepted: bool,
) -> Result<PairedSummary, String> {
    relay::pair_confirm(&sessions, &device_id, accepted).await
}

/// The device id of a persisted pairing, if the app is already paired — so a
/// relaunch can show "connected" instead of the pairing form.
#[tauri::command]
fn paired_device() -> Option<String> {
    relay::paired_device()
}

/// Direct (non-relay) login: validate the gateway base URL + admin token against
/// `GET /v1/status`, then persist them. The web-dashboard style of access; see
/// `direct.rs` for the security trade-off vs scan-to-pair.
#[tauri::command]
async fn direct_login(base_url: String, token: String) -> Result<direct::DirectStatus, String> {
    direct::login(base_url, token).await
}

/// The current direct connection (base URL only), if the app holds direct
/// credentials — so a relaunch can show "connected" instead of the login form.
#[tauri::command]
fn direct_status() -> Result<Option<direct::DirectStatus>, String> {
    direct::status()
}

/// Forget the direct-connection credentials (direct "disconnect"): tear down any
/// live chat WS AND drop the in-memory session/channel token, then wipe the stored
/// credentials. (`forget`, not `disconnect`, so the broad channel token doesn't
/// linger in memory and a later reconnect can't resurrect the session.)
#[tauri::command]
async fn direct_logout(sessions: State<'_, direct::DirectSessions>) -> Result<(), String> {
    direct::forget(&sessions).await;
    direct::logout()
}

/// Mint a fresh direct chat session over REST (admin Bearer) and return its
/// gateway-assigned id; the channel token is stashed for the WS + blob legs.
#[tauri::command]
async fn direct_session_create(
    sessions: State<'_, direct::DirectSessions>,
) -> Result<direct::DirectSessionRef, String> {
    direct::session_create(&sessions).await
}

/// REST refetch of a transcript slice after a `Frame::Reset` (admin Bearer).
#[tauri::command]
async fn direct_history(
    session_id: String,
    before_ordinal: Option<i64>,
    limit: Option<u32>,
) -> Result<serde_json::Value, String> {
    direct::history(session_id, before_ordinal, limit).await
}

/// Forget the current pairing (unpair): clear the keychain record + push key so
/// the app returns to the scan screen. One app binds one gateway.
#[tauri::command]
fn forget_pairing() -> Result<(), String> {
    relay::forget_pairing()
}

/// Open the chat session for `sessionId` on `leg` and stream frames to `onFrame`.
/// Relay runs the Noise E2E content leg (connect to the paired gateway, run the
/// handshake, subscribe, decrypt); direct runs the raw-MessagePack
/// `/v1/channel-ws` web-identity leg. `sinceOrdinal` is the highest ordinal the
/// webview has already rendered — the gateway replays only the gap above it (so a
/// reconnect after a background/reload catches up without re-sending the whole
/// thread); `null` is a fresh subscribe with no catch-up. Both legs share one pump
/// (see `transport.rs`); only the establish/codec seam differs, so this command
/// just routes to the leg's session registry.
#[tauri::command]
async fn chat_connect(
    app: AppHandle,
    leg: ChatLeg,
    relay: State<'_, RelaySessions>,
    direct: State<'_, direct::DirectSessions>,
    session_id: String,
    since_ordinal: Option<i64>,
    on_frame: Channel<Frame>,
) -> Result<(), String> {
    match leg {
        ChatLeg::Relay => relay::connect(app, &relay, session_id, since_ordinal, on_frame).await,
        ChatLeg::Direct => direct::connect(app, &direct, session_id, since_ordinal, on_frame).await,
    }
}

/// Send a user message on the live chat session for `leg`. `msgId` is a fresh
/// per-send idempotency key so a retry doesn't double-fire the agent.
/// `attachments` are content-addressed blobs already uploaded over a blob leg
/// (omitted/empty for a text-only send). Relay sends as device/ios, direct as
/// web-operator/http.
#[tauri::command]
async fn chat_send(
    leg: ChatLeg,
    relay: State<'_, RelaySessions>,
    direct: State<'_, direct::DirectSessions>,
    text: String,
    msg_id: String,
    attachments: Option<Vec<WireAttachment>>,
) -> Result<(), String> {
    let attachments = attachments.unwrap_or_default();
    match leg {
        ChatLeg::Relay => relay::send(&relay, text, msg_id, attachments).await,
        ChatLeg::Direct => direct::send(&direct, text, msg_id, attachments).await,
    }
}

/// Tear down the live chat session for `leg` (the user left the chat view). Any
/// leg-specific durable state survives: the direct leg keeps its session id +
/// channel token for reconnect; the relay leg reloads its pairing record on the
/// next connect.
#[tauri::command]
async fn chat_disconnect(
    leg: ChatLeg,
    relay: State<'_, RelaySessions>,
    direct: State<'_, direct::DirectSessions>,
) -> Result<(), String> {
    match leg {
        ChatLeg::Relay => relay::disconnect(&relay).await,
        ChatLeg::Direct => direct::disconnect(&direct).await,
    }
    Ok(())
}

/// Download an attachment `blob_id` to `dest_path` over a dedicated blob leg,
/// resuming from a partial file if present. `on_progress` streams cumulative bytes.
#[tauri::command]
async fn blob_download(
    blob_id: String,
    dest_path: String,
    on_progress: Channel<u64>,
) -> Result<(), String> {
    relay::download(blob_id, dest_path, on_progress).await
}

/// Upload the local file at `src_path` as `mime_type` over a dedicated blob leg,
/// returning the content-addressed `blob_id` to reference in the next message.
#[tauri::command]
async fn blob_upload(
    src_path: String,
    mime_type: String,
    on_progress: Channel<u64>,
) -> Result<String, String> {
    relay::upload(src_path, mime_type, on_progress).await
}

/// Upload a webview-picked image over `leg`'s blob transport. The raw bytes ride
/// the IPC bridge as the request body (efficient — not a JSON number array); the
/// mime rides `x-baybo-mime` and the leg rides `x-baybo-leg` (the JSON arg slot is
/// taken by the raw body). iOS gives the webview a `File` (bytes), not a path, so
/// this is the entry point an image pick uses. Relay seals + chunks over a
/// dedicated E2E blob leg; direct POSTs to plain `/v1/blobs` (channel token).
/// Returns the content-addressed `blob_id`.
#[tauri::command]
async fn blob_upload_bytes(
    direct: State<'_, direct::DirectSessions>,
    request: tauri::ipc::Request<'_>,
) -> Result<String, String> {
    let leg = ChatLeg::from_request(&request);
    let bytes = match request.body() {
        tauri::ipc::InvokeBody::Raw(bytes) => bytes.clone(),
        tauri::ipc::InvokeBody::Json(_) => {
            return Err("blob_upload_bytes expects a raw byte body".into());
        }
    };
    let mime_type = request
        .headers()
        .get(BLOB_MIME_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    match leg {
        ChatLeg::Relay => relay::upload_bytes(bytes, mime_type).await,
        ChatLeg::Direct => direct::upload_bytes(&direct, bytes, mime_type).await,
    }
}

/// Fetch an attachment `blobId` for display over `leg`'s blob transport, returning
/// the verified bytes to the webview as a raw `ArrayBuffer` to wrap in an object
/// URL. Relay downloads over a dedicated E2E blob leg into a content-addressed
/// on-device cache (reused on the next render); direct GETs plain
/// `/v1/blobs/{id}` (channel token).
#[tauri::command]
async fn blob_image(
    leg: ChatLeg,
    direct: State<'_, direct::DirectSessions>,
    blob_id: String,
) -> Result<tauri::ipc::Response, String> {
    let bytes = match leg {
        ChatLeg::Relay => relay::image_data(blob_id).await,
        ChatLeg::Direct => direct::image_data(&direct, blob_id).await,
    }?;
    Ok(tauri::ipc::Response::new(bytes))
}

/// Debug-only: seed a known push key into the shared App Group keychain so the
/// NSE decrypt path can be exercised with `xcrun simctl push` without a live
/// gateway pairing. Reads `BAYBO_SEED_PUSH_KEY` as `<bid>:<64-hex-key>` (absent
/// => no-op). Compiled out of release builds; never logs the key or the bid.
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
    // read it (the eprintln does not reach simctl's console on iOS). No secret
    // or bid is written — only the round-trip verdict.
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
    eprintln!("baybo(debug): keychain self-check: {result}");
}

/// Select the rustls crypto provider for the process. `tokio-tungstenite` pulls
/// rustls with `default-features = false` (no provider), so the first `wss://`
/// dial — pairing or content — would panic building its `ClientConfig`. Install
/// `ring` once here, before any command can run, so every dial finds a provider.
fn install_crypto_provider() {
    // Err only if a provider is already installed — harmless, so ignore it.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    install_crypto_provider();

    #[cfg(all(debug_assertions, target_os = "ios"))]
    debug_seed_push_key();

    let builder = tauri::Builder::default();
    // The barcode/camera + haptics plugins are mobile-only (the QR
    // scan-to-connect path and the scan-success buzz).
    #[cfg(mobile)]
    let builder = builder
        .plugin(tauri_plugin_barcode_scanner::init())
        .plugin(tauri_plugin_haptics::init());
    let app = match builder
        .manage(PairingSessions::default())
        .manage(RelaySessions::default())
        .manage(direct::DirectSessions::default())
        .setup(|_app| {
            // Request provisional notification auth + remote-notification
            // registration once the app is up (main thread). No-op off iOS.
            push_register::register();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            pair_begin,
            pair_confirm,
            paired_device,
            forget_pairing,
            direct_login,
            direct_status,
            direct_logout,
            direct_session_create,
            direct_history,
            chat_connect,
            chat_send,
            chat_disconnect,
            blob_download,
            blob_upload,
            blob_upload_bytes,
            blob_image
        ])
        .build(tauri::generate_context!())
    {
        Ok(app) => app,
        Err(e) => {
            eprintln!("baybo: fatal error while building the app: {e}");
            return;
        }
    };

    // Bridge the iOS app lifecycle into the webview. iOS suspends the whole app
    // without ever marking the WKWebView page hidden, so the page's own
    // `visibilitychange` never fires on resume and the chat view would keep using a
    // relay leg the OS froze. On every foreground (`Resumed`) emit `app-resumed` so
    // the webview re-dials its content session and replays the catch-up gap.
    app.run(|app_handle, event| {
        if matches!(event, tauri::RunEvent::Resumed) {
            if let Err(e) = app_handle.emit("app-resumed", ()) {
                eprintln!("baybo: emit app-resumed failed: {e}");
            }
            // APNs registration can fail transiently at launch and iOS does not
            // retry it on its own. If we still have no token, re-arm on foreground
            // (idempotent — see `push_register::register`); once a token lands the
            // content pump forwards it and the gateway re-registers the binding.
            if push_register::apns_token().is_none() {
                push_register::register();
            }
        }
    });
}
