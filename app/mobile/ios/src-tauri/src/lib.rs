//! The Baybo iOS companion (Tauri shell).
//!
//! A thin native shell around the host-tested `baybo-mobile-core`: the webview
//! drives phase-1's two features — scan-to-connect (the [`pair`] command) and
//! remote notifications (handled out-of-process by the Notification Service
//! Extension under `../apple`). The protocol/crypto live in the shared crates,
//! so interop with the gateway is guaranteed by construction.

mod content;
mod keychain;
mod pairing;
mod push_register;

use baybo_mobile_core::Frame;
use content::ContentSessions;
use pairing::{PairChallenge, PairedSummary, PairingSessions};
use tauri::State;
use tauri::ipc::Channel;

/// Scan-to-connect, phase 1: dial the gateway, run SPAKE2 + send `DeviceHello`,
/// and return the Bluetooth-style confirmation code the UI shows the user to
/// compare against the operator's terminal.
#[tauri::command]
async fn pair_begin(
    sessions: State<'_, PairingSessions>,
    endpoint: String,
    code: String,
    label: String,
    relay: bool,
) -> Result<PairChallenge, String> {
    pairing::pair_begin(&sessions, &endpoint, &code, &label, relay).await
}

/// Phase 2: send the user's decision. On accept — and once the operator also
/// confirms on their terminal — pairing finalizes and the UI renders the summary.
#[tauri::command]
async fn pair_confirm(
    sessions: State<'_, PairingSessions>,
    device_id: String,
    accepted: bool,
) -> Result<PairedSummary, String> {
    pairing::pair_confirm(&sessions, &device_id, accepted).await
}

/// The owning user of a persisted pairing, if the app is already paired — so a
/// relaunch can show "connected" instead of the pairing form.
#[tauri::command]
fn paired_user() -> Option<String> {
    pairing::paired_user()
}

/// Open the E2E content session for `sessionId`: connect to the paired gateway,
/// run the Noise handshake, subscribe, and stream decrypted frames to the
/// webview over `onFrame`.
#[tauri::command]
async fn content_connect(
    sessions: State<'_, ContentSessions>,
    session_id: String,
    on_frame: Channel<Frame>,
) -> Result<(), String> {
    content::connect(&sessions, session_id, on_frame).await
}

/// Send a user message on the live content session. `msgId` is a fresh per-send
/// idempotency key so a retry doesn't double-fire the agent.
#[tauri::command]
async fn content_send(
    sessions: State<'_, ContentSessions>,
    text: String,
    msg_id: String,
) -> Result<(), String> {
    content::send(&sessions, text, msg_id).await
}

/// Tear down the live content session (the user left the chat view).
#[tauri::command]
async fn content_disconnect(sessions: State<'_, ContentSessions>) -> Result<(), String> {
    content::disconnect(&sessions).await;
    Ok(())
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(all(debug_assertions, target_os = "ios"))]
    debug_seed_push_key();

    let builder = tauri::Builder::default();
    // The barcode/camera plugin is mobile-only (the QR scan-to-connect path).
    #[cfg(mobile)]
    let builder = builder.plugin(tauri_plugin_barcode_scanner::init());
    let result = builder
        .manage(PairingSessions::default())
        .manage(ContentSessions::default())
        .setup(|_app| {
            // Request provisional notification auth + remote-notification
            // registration once the app is up (main thread). No-op off iOS.
            push_register::register();
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            pair_begin,
            pair_confirm,
            paired_user,
            content_connect,
            content_send,
            content_disconnect
        ])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("baybo: fatal error while running the app: {e}");
    }
}
