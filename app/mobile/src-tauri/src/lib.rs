//! The Baybo iOS companion (Tauri shell).
//!
//! A thin native shell around the host-tested `baybo-mobile-core`: the webview
//! drives scan-to-connect, chat, and attachments, while remote notifications are
//! handled out-of-process by the Notification Service Extension under `../apple`.
//! The protocol/crypto live in the shared crates, so interop with the gateway is
//! guaranteed by construction.

mod binding;
mod direct;
mod keyboard;
mod keychain;
mod push_register;
mod relay;
mod transport;

use baybo_mobile_core::{Frame, WireAttachment};
use binding::{ActiveLeg, active_leg};
use relay::RelaySessions;
use relay::{PairAborted, PairChallenge, PairedSummary, PairingSessions};
use tauri::ipc::Channel;
use tauri::{AppHandle, Emitter, State};

/// Header the raw-body blob upload reads its mime from — the JSON arg slot carries
/// the bytes, so the mime rides a header instead. The leg is no longer threaded from
/// the webview: it's resolved from the active binding (see [`binding`]).
const BLOB_MIME_HEADER: &str = "x-baybo-mime";

/// Rotating on-device log file (`<app log dir>/baybo.log`): size cap per file and
/// how many rotated files to keep, so an exported sysdiagnose/log bundle stays small.
const LOG_FILE_NAME: &str = "baybo";
const LOG_FILE_MAX_BYTES: u128 = 2 * 1024 * 1024;
const LOG_FILES_KEPT: usize = 3;

/// The logging facade: stdout (which tauri-plugin-log forwards to `os_log` on iOS,
/// so lines surface in Console.app / sysdiagnose) plus a rotating file in the app's
/// log dir a user can export. Warn globally, debug for our own crates.
fn log_plugin<R: tauri::Runtime>() -> tauri::plugin::TauriPlugin<R> {
    use tauri_plugin_log::{RotationStrategy, Target, TargetKind};
    tauri_plugin_log::Builder::new()
        .targets([
            Target::new(TargetKind::Stdout),
            Target::new(TargetKind::LogDir {
                file_name: Some(LOG_FILE_NAME.into()),
            }),
        ])
        .max_file_size(LOG_FILE_MAX_BYTES)
        .rotation_strategy(RotationStrategy::KeepSome(LOG_FILES_KEPT))
        // The plugin's mobile default format is bare `[target] message`; the
        // exported file needs a timestamp + level to sequence a session against
        // gateway/relay logs.
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} [{}][{}] {}",
                tauri_plugin_log::TimezoneStrategy::UseUtc.get_now(),
                record.level(),
                record.target(),
                message
            ))
        })
        .level(log::LevelFilter::Warn)
        .level_for("baybo_mobile_app_lib", log::LevelFilter::Debug)
        .level_for("baybo_mobile_core", log::LevelFilter::Debug)
        .build()
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

/// Best-effort direct-mode push registration: provision (or refresh) this app's
/// push binding with the directly-connected gateway so a backgrounded direct chat
/// can still buzz. No-op when iOS hasn't issued an APNs token yet or the gateway
/// has no `[push]` remote host configured. Returns the bound device id on success.
#[tauri::command]
async fn direct_push_register() -> Result<Option<String>, String> {
    let token = push_register::apns_token().unwrap_or_default();
    // Debug / TestFlight builds get a sandbox APNs token; release a production one.
    let env = if cfg!(debug_assertions) {
        "sandbox"
    } else {
        "production"
    };
    direct::register_push(token, env).await
}

/// Mint a fresh chat session for the active binding and return its id. Direct mints
/// a gateway session over REST (the id is server-assigned + a channel token is stashed
/// for the WS/blob legs); relay picks a fresh client id (the relay leg needs no
/// gateway pre-registration). The webview no longer branches on the binding — it just
/// asks for an id.
#[tauri::command]
async fn chat_create_session(direct: State<'_, direct::DirectSessions>) -> Result<String, String> {
    match active_leg()? {
        ActiveLeg::Direct => Ok(direct::session_create(&direct).await?.session_id),
        ActiveLeg::Relay => Ok(uuid::Uuid::new_v4().to_string()),
    }
}

/// Log out (the unified "disconnect"): tear down the live leg and wipe the app's
/// durable credentials, returning it to unbound. Wipes BOTH legs unconditionally —
/// "one app binds one Baybo", but a hiccuped best-effort supersede can transiently
/// leave both credential sets, so routing by the active leg (and dropping only it)
/// could leave the app silently bound to the superseded gateway. Deleting an absent
/// credential is a no-op, so the unconditional both-wipe is safe (mirrors how
/// `chat_disconnect` tears down both registries). Idempotent — an already-unbound app
/// is fine.
#[tauri::command]
async fn logout(
    relay: State<'_, RelaySessions>,
    direct: State<'_, direct::DirectSessions>,
) -> Result<(), String> {
    direct::forget(&direct).await;
    transport::disconnect(&*relay).await;
    // Run both wipes regardless of which errored, then surface the first failure.
    let direct_wiped = direct::logout();
    let relay_wiped = relay::forget_pairing();
    direct_wiped.and(relay_wiped)
}

/// Open the chat session for `sessionId` on the active binding's leg and stream
/// frames to `onFrame`. Relay runs the Noise E2E content leg (connect to the paired
/// gateway, run the handshake, subscribe, decrypt); direct runs the raw-MessagePack
/// `/v1/channel-ws` web-identity leg. Which one is resolved from durable identity
/// (see [`binding::active_leg`]) — the webview no longer tags the call. `sinceOrdinal`
/// is the highest ordinal the webview has already rendered — the gateway replays only
/// the gap above it (so a reconnect after a background/reload catches up without
/// re-sending the whole thread); `null` is a fresh subscribe with no catch-up. Both
/// legs share one pump (see `transport.rs`); only the establish/codec seam differs, so
/// this command just routes to the leg's session registry.
#[tauri::command]
async fn chat_connect(
    app: AppHandle,
    relay: State<'_, RelaySessions>,
    direct: State<'_, direct::DirectSessions>,
    session_id: String,
    since_ordinal: Option<i64>,
    on_frame: Channel<Frame>,
) -> Result<(), String> {
    match active_leg()? {
        ActiveLeg::Relay => {
            transport::connect(&*relay, app, session_id, since_ordinal, on_frame).await
        }
        ActiveLeg::Direct => {
            transport::connect(&*direct, app, session_id, since_ordinal, on_frame).await
        }
    }
}

/// Send a user message on the live chat session for the active binding's leg.
/// `msgId` is a fresh per-send idempotency key so a retry doesn't double-fire the
/// agent. `attachments` are content-addressed blobs already uploaded over a blob leg
/// (omitted/empty for a text-only send). Relay sends as device/ios, direct as
/// web-operator/http.
#[tauri::command]
async fn chat_send(
    relay: State<'_, RelaySessions>,
    direct: State<'_, direct::DirectSessions>,
    text: String,
    msg_id: String,
    attachments: Option<Vec<WireAttachment>>,
) -> Result<(), String> {
    let attachments = attachments.unwrap_or_default();
    match active_leg()? {
        ActiveLeg::Relay => transport::send(&*relay, text, msg_id, attachments).await,
        ActiveLeg::Direct => transport::send(&*direct, text, msg_id, attachments).await,
    }
}

/// Request a backward page of the live chat session's transcript over the active
/// binding's leg — the live-leg recovery/pagination primitive (no admin REST, unlike
/// `direct_history`). `beforeOrdinal` pages older (`null` = newest page); `limit` caps
/// the page. The reply is **not** this command's return value: the gateway answers
/// with a `Frame::HistoryPage` that streams back through the session's `onFrame`
/// channel (mirroring how `Subscribe` catch-up replays arrive), so the webview
/// consumes it in its frame switch. Returns once the request is enqueued on the live
/// leg.
#[tauri::command]
async fn chat_fetch_history(
    relay: State<'_, RelaySessions>,
    direct: State<'_, direct::DirectSessions>,
    before_ordinal: Option<i64>,
    limit: Option<u32>,
) -> Result<(), String> {
    match active_leg()? {
        ActiveLeg::Relay => transport::fetch_history(&*relay, before_ordinal, limit).await,
        ActiveLeg::Direct => transport::fetch_history(&*direct, before_ordinal, limit).await,
    }
}

/// Tear down the live chat session (the user left the chat view). Any leg-specific
/// durable state survives: the direct leg keeps its session id + channel token for
/// reconnect; the relay leg reloads its pairing record on the next connect. At most
/// one leg is ever live (one binding), but the binding may already be gone — a
/// disconnect/unpair deletes the credentials *before* this fires — so we can't resolve
/// which leg to stop. Tear both down unconditionally; a disconnect on an idle registry
/// is a no-op, so no pump ever lingers regardless of binding state.
#[tauri::command]
async fn chat_disconnect(
    relay: State<'_, RelaySessions>,
    direct: State<'_, direct::DirectSessions>,
) -> Result<(), String> {
    transport::disconnect(&*relay).await;
    transport::disconnect(&*direct).await;
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

/// Upload a webview-picked image over the active binding's blob transport. The raw
/// bytes ride the IPC bridge as the request body (efficient — not a JSON number
/// array); the mime rides `x-baybo-mime` (the JSON arg slot is taken by the raw body).
/// The leg is resolved from the active binding (see [`binding`]), not tagged by the
/// caller. iOS gives the webview a `File` (bytes), not a path, so this is the entry
/// point an image pick uses. Relay seals + chunks over a dedicated E2E blob leg; direct
/// POSTs to plain `/v1/blobs` (channel token). Returns the content-addressed `blob_id`.
#[tauri::command]
async fn blob_upload_bytes(
    direct: State<'_, direct::DirectSessions>,
    request: tauri::ipc::Request<'_>,
) -> Result<String, String> {
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
    match active_leg()? {
        ActiveLeg::Relay => relay::upload_bytes(bytes, mime_type).await,
        ActiveLeg::Direct => direct::upload_bytes(&direct, bytes, mime_type).await,
    }
}

/// Fetch an attachment `blobId` for display over the active binding's blob transport,
/// returning the verified bytes to the webview as a raw `ArrayBuffer` to wrap in an
/// object URL. Relay downloads over a dedicated E2E blob leg into a content-addressed
/// on-device cache (reused on the next render); direct GETs plain `/v1/blobs/{id}`
/// (channel token). The leg is resolved from the active binding (see [`binding`]).
#[tauri::command]
async fn blob_image(
    direct: State<'_, direct::DirectSessions>,
    blob_id: String,
) -> Result<tauri::ipc::Response, String> {
    let bytes = match active_leg()? {
        ActiveLeg::Relay => relay::image_data(blob_id).await,
        ActiveLeg::Direct => direct::image_data(&direct, blob_id).await,
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
    // read it (the log line does not reach simctl's console on iOS). No secret
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
    log::debug!("keychain self-check: {result}");
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

    let builder = tauri::Builder::default().plugin(log_plugin());
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
            // Drop the WKWebView keyboard form-assistant bar (prev/next + Done)
            // shown above the keyboard for web inputs. No-op off iOS.
            keyboard::hide_input_accessory_bar();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            pair_begin,
            pair_confirm,
            paired_device,
            direct_login,
            direct_status,
            direct_push_register,
            logout,
            chat_create_session,
            chat_connect,
            chat_send,
            chat_fetch_history,
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
            // The log plugin attaches its logger during `build` — but if the
            // failure WAS the log plugin (e.g. unwritable log dir), log::error!
            // is a no-op, so mirror the message on stderr unconditionally.
            eprintln!("baybo: fatal error while building the app: {e}");
            log::error!("fatal error while building the app: {e}");
            return;
        }
    };

    // Runs after `build` so the logger is attached; the seed itself only needs to
    // happen before the event loop.
    #[cfg(all(debug_assertions, target_os = "ios"))]
    debug_seed_push_key();

    // Bridge the iOS app lifecycle into the webview. iOS suspends the whole app
    // without ever marking the WKWebView page hidden, so the page's own
    // `visibilitychange` never fires on resume and the chat view would keep using a
    // relay leg the OS froze. On every foreground (`Resumed`) emit `app-resumed` so
    // the webview re-dials its content session and replays the catch-up gap.
    app.run(|app_handle, event| {
        if matches!(event, tauri::RunEvent::Resumed) {
            if let Err(e) = app_handle.emit("app-resumed", ()) {
                log::warn!("emit app-resumed failed: {e}");
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
