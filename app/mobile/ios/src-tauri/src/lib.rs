//! The Aura iOS companion (Tauri shell).
//!
//! A thin native shell around the host-tested `aura-mobile-core`: the webview
//! drives phase-1's two features — scan-to-connect (the [`pair`] command) and
//! remote notifications (handled out-of-process by the Notification Service
//! Extension under `../apple`). The protocol/crypto live in the shared crates,
//! so interop with the gateway is guaranteed by construction.

mod pairing;

use pairing::PairedSummary;

/// Scan-to-connect: dial the gateway and run the SPAKE2 pairing handshake.
/// Returns the operator-pending summary the UI renders.
#[tauri::command]
async fn pair(endpoint: String, code: String, label: String) -> Result<PairedSummary, String> {
    pairing::run_pairing(&endpoint, &code, &label).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    // The barcode/camera plugin is mobile-only (the QR scan-to-connect path).
    #[cfg(mobile)]
    let builder = builder.plugin(tauri_plugin_barcode_scanner::init());
    let result = builder
        .invoke_handler(tauri::generate_handler![pair])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("aura: fatal error while running the app: {e}");
    }
}
