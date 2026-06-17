//! Tauri shell for the macOS app. Embeds the Aura runtime in-process (no child
//! process, no shipped CLI) and hands the webview the loopback connection to
//! the in-process `GatewayServer`. See `docs/mac-app.md` §2 / §5.

mod setup;

use std::sync::Arc;

use parking_lot::Mutex;
use serde::Serialize;
use tauri::{AppHandle, Manager, State};

/// Ring-buffer capacity for the in-process gateway's recent-log surface. The
/// app owns its own tracing, so this buffer only backs `GatewayDeps`.
const LOG_BUFFER_CAPACITY: usize = 1024;

/// What the webview needs to talk to the in-process gateway: the loopback base
/// URL (real ephemeral port) and the bearer admin token.
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub base_url: String,
    pub admin_token: String,
}

#[derive(Default)]
pub struct AppState {
    conn: Mutex<Option<Connection>>,
    /// Cross-command setup state (held between wizard steps). See `setup`.
    draft: Mutex<Option<setup::SetupDraft>>,
    /// True once a boot has been kicked off, so we never start two gateways.
    booting: Mutex<bool>,
}

/// Hand the webview the loopback base URL + admin token. Returns the
/// `not_ready` error until the runtime finishes booting, so the frontend shows
/// a boot-in-progress state and retries (docs/mac-app.md §6).
#[tauri::command]
fn get_connection(state: State<'_, AppState>) -> Result<Connection, String> {
    state
        .conn
        .lock()
        .clone()
        .ok_or_else(|| "not_ready".to_string())
}

/// A config is "ready to chat" once it has at least one LLM entry and a
/// `default-llm` that names one of them (docs/mac-app.md §5).
pub(crate) fn is_configured(config: &aura_config::AuraConfig) -> bool {
    !config.llm.is_empty() && config.llm.iter().any(|e| e.name == config.default_llm)
}

/// Kick off the embedded gateway boot exactly once, storing the connection on
/// success. Called from `setup` (auto-boot when already configured) and from
/// `finish_setup` / `start_runtime`.
pub(crate) fn trigger_boot(handle: AppHandle, root: std::path::PathBuf) {
    {
        let state = handle.state::<AppState>();
        let mut booting = state.booting.lock();
        if *booting {
            return;
        }
        *booting = true;
    }
    tauri::async_runtime::spawn(async move {
        match boot_runtime(root).await {
            Ok(conn) => {
                *handle.state::<AppState>().conn.lock() = Some(conn);
                tracing::info!("aura runtime booted");
            }
            Err(e) => {
                tracing::error!(error = format!("{e:#}"), "aura runtime boot failed");
                // Allow a later retry (e.g. the wizard re-running finish_setup).
                *handle.state::<AppState>().booting.lock() = false;
            }
        }
    });
}

/// Bootstrap the app-owned workspace, override the gateway config for loopback
/// + an ephemeral port, start the embedded gateway, and return its connection.
async fn boot_runtime(root: std::path::PathBuf) -> anyhow::Result<Connection> {
    let ctx = aura_setup::bootstrap_workspace_if_needed(root.clone())
        .await
        .map_err(|e| anyhow::anyhow!("workspace bootstrap: {e}"))?;
    let config_path = ctx.config_path.clone();
    let mut config = ctx.config.clone();
    drop(ctx);

    // The app owns the gateway config: bind loopback on an ephemeral port and
    // allow the webview origins through CORS. Applied in memory only — never
    // written back to aura.json.
    config.workspace.path = root.to_string_lossy().into_owned();
    config.gateway.enabled = true;
    config.gateway.bind_address = "127.0.0.1".to_string();
    config.gateway.port = 0;
    config.gateway.cors_allowed_origins = vec![
        "tauri://localhost".to_string(),
        "http://localhost:1420".to_string(),
    ];

    let running = aura_runtime::start_gateway(aura_runtime::StartGatewayOpts {
        config: Arc::new(config),
        config_path: Some(config_path),
        install_signals: false,
        start_channel_listener: false,
        setup_tracing: Box::new(|_paths, _leak_detector| {
            // Tauri owns tracing (installed in `run`); the gateway just needs a
            // log buffer for `GatewayDeps`.
            let buf = aura_gateway::LogBuffer::new(LOG_BUFFER_CAPACITY);
            (buf, Box::new(()) as aura_runtime::TracingGuard)
        }),
    })
    .await?;

    let conn = Connection {
        base_url: format!("http://{}", running.admin_addr),
        admin_token: running.admin_token.clone(),
    };
    // Drive the gateway servers for the app's lifetime.
    tokio::spawn(running.serve());
    Ok(conn)
}

pub fn run() {
    tracing_subscriber::fmt::init();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.set_focus();
            }
        }))
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            get_connection,
            setup::setup_status,
            setup::list_providers,
            setup::submit_api_key,
            setup::list_models,
            setup::finish_setup,
            setup::start_oauth,
            setup::start_runtime,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            let root = app
                .path()
                .app_data_dir()
                .map_err(|e| format!("resolve app_data_dir: {e}"))?;
            tauri::async_runtime::spawn(async move {
                // Auto-boot when already configured; otherwise the frontend
                // drives the setup wizard, which boots via `finish_setup`.
                match aura_setup::bootstrap_workspace_if_needed(root.clone()).await {
                    Ok(ctx) if is_configured(&ctx.config) => {
                        drop(ctx);
                        trigger_boot(handle, root);
                    }
                    Ok(_) => tracing::info!("aura: not configured; awaiting setup wizard"),
                    Err(e) => {
                        tracing::error!(error = format!("{e:#}"), "aura: bootstrap failed")
                    }
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "fatal: tauri run failed");
            std::process::exit(1);
        });
}
