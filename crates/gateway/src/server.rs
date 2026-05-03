//! axum server assembly and shared state for the gateway.
//!
//! The gateway exposes two listeners:
//!
//! * **Admin** — TCP, bearer-token authenticated. Hosts config,
//!   status, jobs, cron, memory, traces, skills, tools, llm, and a
//!   read-only channel list. No chat content flows through these
//!   endpoints.
//! * **Channel** — loopback TCP (`127.0.0.1:<ephemeral>`),
//!   channel-token authenticated against [`ChannelTokenTable`] (see
//!   [`crate::channel_listener`] and [`crate::auth::channel`]). Hosts
//!   the WebSocket endpoint
//!   (`/v1/channel-ws`) — the only surface the TUI and sidecar
//!   channel plugins talk to. Session CRUD lives on the admin
//!   surface; the router creates sessions lazily on first message
//!   frame.
//!
//! [`AdminState`] and [`ChannelState`] split the old monolithic
//! `ApiState` so each listener only sees the managers it needs. Both
//! are cheap to clone — every field is an `Arc` or a small value.
//!
//! [`GatewayServer`] owns the admin listener. The channel-TCP half
//! lives in [`crate::channel_listener`] and is driven by the gateway
//! CLI alongside the admin server.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use aura_agent::{
    CronScheduler, JobManager, MemoryManager, SessionManager, service::ShutdownSignal,
};
use aura_channels::{ChannelRegistry, IncomingMessage};
use aura_config::AuraConfig;
use aura_llm::LlmClient;
use aura_pairing::PairingService;
use aura_security::SecretVault;
use aura_skills::SkillRegistry;
use aura_storage::{ChannelBotStore, Store, TraceStore};
use aura_tools::ToolRegistry;
use axum::Router;
use axum::middleware;
use tokio::sync::mpsc;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::api;
use crate::auth::admin::{AdminAuthState, require_admin_token};
use crate::auth::{ChannelTokenTable, channel as channel_auth};
use crate::config::RuntimeGatewayConfig;
use crate::log_buffer::LogBuffer;
use crate::{GatewayError, Result};

/// Caller-supplied managers + config needed to run the gateway.
///
/// Built by the runtime (`src/runtime.rs`) and handed to
/// [`GatewayServer::new`]. The gateway takes references (cloning Arcs)
/// into [`AdminState`] / [`ChannelState`]; the caller keeps the
/// originals so the same managers can be shared with the router / TUI
/// / cron loop.
pub struct GatewayDeps {
    pub config: Arc<AuraConfig>,
    /// Path to the on-disk `aura.json` the gateway was loaded from, if
    /// any. Needed by `PUT/DELETE /v1/config` so remote clients can
    /// write through to the same file that `aura config set/unset`
    /// targets. `None` when running with defaults only — mutation
    /// endpoints then reject with `ConfigPathUnset`.
    pub config_path: Option<PathBuf>,
    pub runtime_config: RuntimeGatewayConfig,
    pub session_manager: Arc<SessionManager>,
    pub job_manager: Arc<JobManager>,
    pub cron_scheduler: Arc<CronScheduler>,
    pub memory_manager: Arc<MemoryManager>,
    pub skill_registry: Arc<SkillRegistry>,
    pub tool_registry: Arc<ToolRegistry>,
    pub channel_registry: Arc<ChannelRegistry>,
    pub llm_client: Arc<LlmClient>,
    /// Bearer token for the admin TCP listener. Stored in the vault as
    /// `gateway.admin_token`.
    pub admin_token: String,
    /// Shared ring buffer of recent tracing events surfaced by
    /// `/v1/logs`. Installed as a `tracing::Layer` at process init.
    pub log_buffer: Arc<LogBuffer>,
    /// Router intake. Cloned into the WS channel server so sidecar
    /// frames can be forwarded as `IncomingMessage`s.
    pub incoming_tx: mpsc::Sender<IncomingMessage>,
    /// Per-install capability tokens. The channel TCP listener
    /// passes this to the WS server for Register-frame verification.
    pub channel_tokens: ChannelTokenTable,
    /// Vault handle shared with the channel server so the WS route can
    /// build a [`crate::channel::TuiHistoryStore`] without re-opening
    /// libsql. The gateway is the only process that writes the TUI
    /// input-history key; the TUI itself never touches the vault.
    pub secret_vault: Arc<SecretVault>,
    /// Cloneable bundle of every libsql-backed store (trace, channel
    /// session/bot/pairing, …). Exposing the whole [`Store`] here means
    /// adding a new store to the gateway only touches [`Store`] itself,
    /// not every `GatewayDeps`-style wrapper. Handlers read the specific
    /// handle they need via `deps.stores.<name>.clone()`.
    pub stores: Store,
    /// Shared handle the CLI-driven reconciler uses to push control-
    /// plane frames (`StartBot` / `StopBot`) into the WS sidecar pump.
    /// The WS route task registers each sidecar on successful
    /// handshake and removes it on disconnect.
    pub channel_control: Arc<crate::channel::ChannelControlRegistry>,
    /// Reconciler handle spawned at startup. Shared with the WS route
    /// so the initial-register roster push can `seed` its tracked set
    /// (avoiding a double-send on the first tick) and so the
    /// disconnect path can `forget` the cached bots.
    pub bot_reconciler: Arc<crate::channel::ChannelBotReconciler>,
    /// Pending diagnose round-trips. Shared between the WS route
    /// (resolves replies) and the admin endpoint (registers waiters).
    pub diagnose_router: Arc<crate::channel::DiagnoseRouter>,
    /// Per-channel-type capability set advertised on `Register`. The
    /// admin diagnose endpoint reads this to short-circuit before the
    /// WS round-trip when the sidecar didn't claim `"diagnose"`. Other
    /// capability gates today are inline in the inbound loop; this map
    /// exists so admin-side callers can introspect without crossing
    /// the WS task boundary.
    pub channel_capabilities: Arc<crate::channel::ChannelCapabilities>,
    /// Shared MCP tunnel registry. The WS inbound loop uses
    /// `forward_inbound` to route `Frame::Mcp` payloads to whichever
    /// caller opened the matching tunnel; the agent-side caller
    /// (lands in slice 2) holds the open tunnel handles.
    pub mcp_tunnel_router: Arc<crate::channel::McpTunnelRouter>,
    /// Lazy per-session sidecar MCP discovery. Implements the
    /// `aura_tools::mcp::SidecarMcpProvider` trait the agent loop
    /// consults. The runtime threads this into `AgentLoop` via
    /// `with_sidecar_mcp`; the WS route's disconnect path calls
    /// `detach` to drop cached rmcp sessions.
    pub sidecar_mcp_manager: Arc<crate::channel::SidecarMcpManager>,
}

/// State shared with admin TCP handlers. Cheap to clone.
#[derive(Clone)]
pub struct AdminState {
    pub config: Arc<AuraConfig>,
    pub config_path: Option<PathBuf>,
    pub session_manager: Arc<SessionManager>,
    pub job_manager: Arc<JobManager>,
    pub cron_scheduler: Arc<CronScheduler>,
    pub memory_manager: Arc<MemoryManager>,
    pub trace_store: Arc<dyn TraceStore>,
    pub skill_registry: Arc<SkillRegistry>,
    pub tool_registry: Arc<ToolRegistry>,
    pub channel_registry: Arc<ChannelRegistry>,
    pub llm_client: Arc<LlmClient>,
    pub log_buffer: Arc<LogBuffer>,
    pub channel_bot_store: Arc<dyn ChannelBotStore>,
    pub channel_control: Arc<crate::channel::ChannelControlRegistry>,
    pub secret_vault: Arc<SecretVault>,
    pub diagnose_router: Arc<crate::channel::DiagnoseRouter>,
    pub channel_capabilities: Arc<crate::channel::ChannelCapabilities>,
    pub mcp_tunnel_router: Arc<crate::channel::McpTunnelRouter>,
    /// Pretty form of the admin bind address for `/v1/status`.
    pub bind_display: String,
}

/// State shared with channel-TCP handlers. Cheap to clone.
///
/// After the HTTP+SSE surface was retired, all live chat traffic flows
/// through the WS endpoint and its [`WsChannelState`](crate::channel::WsChannelState).
/// `ChannelState` stays around so non-WS channel-listener handlers
/// (`/healthz` is the only one at the moment) have a place to grow.
#[derive(Clone)]
pub struct ChannelState {
    pub session_manager: Arc<SessionManager>,
    pub channel_registry: Arc<ChannelRegistry>,
    pub incoming_tx: mpsc::Sender<IncomingMessage>,
    pub channel_tokens: ChannelTokenTable,
}

impl AdminState {
    fn from_deps(deps: &GatewayDeps) -> Self {
        Self {
            config: Arc::clone(&deps.config),
            config_path: deps.config_path.clone(),
            session_manager: Arc::clone(&deps.session_manager),
            job_manager: Arc::clone(&deps.job_manager),
            cron_scheduler: Arc::clone(&deps.cron_scheduler),
            memory_manager: Arc::clone(&deps.memory_manager),
            trace_store: deps.stores.trace.clone(),
            skill_registry: Arc::clone(&deps.skill_registry),
            tool_registry: Arc::clone(&deps.tool_registry),
            channel_registry: Arc::clone(&deps.channel_registry),
            llm_client: Arc::clone(&deps.llm_client),
            log_buffer: Arc::clone(&deps.log_buffer),
            channel_bot_store: deps.stores.channel_bot.clone(),
            channel_control: Arc::clone(&deps.channel_control),
            secret_vault: Arc::clone(&deps.secret_vault),
            diagnose_router: Arc::clone(&deps.diagnose_router),
            channel_capabilities: Arc::clone(&deps.channel_capabilities),
            mcp_tunnel_router: Arc::clone(&deps.mcp_tunnel_router),
            bind_display: deps.runtime_config.admin_bind.to_string(),
        }
    }
}

impl ChannelState {
    pub fn from_deps(deps: &GatewayDeps) -> Self {
        Self {
            session_manager: Arc::clone(&deps.session_manager),
            channel_registry: Arc::clone(&deps.channel_registry),
            incoming_tx: deps.incoming_tx.clone(),
            channel_tokens: deps.channel_tokens.clone(),
        }
    }
}

/// TCP admin server. Long-lived; owns its own axum `Router` built
/// from the caller-supplied [`GatewayDeps`]. The companion channel
/// listener is started separately (see
/// [`crate::channel_listener::ChannelServer`]) with the same deps.
pub struct GatewayServer {
    bind: SocketAddr,
    router: Router,
    shutdown_grace: std::time::Duration,
}

impl GatewayServer {
    pub fn new(deps: GatewayDeps) -> Self {
        let bind = deps.runtime_config.admin_bind;
        let shutdown_grace = deps.runtime_config.shutdown_grace;
        let router = build_admin_router(deps);
        Self {
            bind,
            router,
            shutdown_grace,
        }
    }

    /// Admin TCP bind address.
    pub fn bind(&self) -> SocketAddr {
        self.bind
    }

    pub fn shutdown_grace(&self) -> std::time::Duration {
        self.shutdown_grace
    }

    /// Run the admin server to completion. Returns once
    /// [`ShutdownSignal`] fires and axum has drained in-flight
    /// requests.
    pub async fn run(self, shutdown: ShutdownSignal) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(self.bind)
            .await
            .map_err(|e| GatewayError::Bind {
                addr: self.bind.to_string(),
                reason: e.to_string(),
            })?;
        tracing::info!(bind = %self.bind, listener = "admin", "gateway listening");
        let shutdown_fut = async move {
            shutdown.wait().await;
        };
        axum::serve(listener, self.router.into_make_service())
            .with_graceful_shutdown(shutdown_fut)
            .await
            .map_err(|e| GatewayError::Internal(format!("serve error: {e}")))
    }
}

fn build_admin_router(deps: GatewayDeps) -> Router {
    let state = AdminState::from_deps(&deps);
    let auth_state = AdminAuthState::new(deps.admin_token.clone());

    let cors = build_cors(&deps.runtime_config.cors_allowed_origins);

    let (admin_router, _admin_spec) = api::admin::v1_router_and_spec();
    // TraceLayer goes *inside* the auth middleware so it sees the
    // URI AFTER `require_admin_token` has stripped `?token=…`. If
    // TraceLayer is on the outside it would log the raw URI (token
    // and all) before auth rewrites it — tower middleware runs outer-
    // to-inner, so "outer" = "logs first".
    let admin_router = admin_router
        .with_state(state)
        .layer(TraceLayer::new_for_http())
        .layer(middleware::from_fn_with_state(
            auth_state,
            require_admin_token,
        ));

    Router::new()
        .merge(api::health::routes().layer(TraceLayer::new_for_http()))
        .merge(admin_router)
        .fallback(api::webui::serve)
        .layer(cors)
}

/// Build the router served on the channel TCP listener. Called by
/// [`crate::channel_listener::ChannelServer::run`]. The auth
/// middleware is applied to the `/v1` routes only — health lives
/// outside so orchestrators can poll it without an auth handshake.
pub fn build_channel_router(
    deps: &GatewayDeps,
    auth_state: channel_auth::ChannelAuthState,
) -> Router {
    let _channel_state = ChannelState::from_deps(deps);
    let tui_history = Arc::new(crate::channel::TuiHistoryStore::new(Arc::clone(
        &deps.secret_vault,
    )));
    let session_resolver = Arc::new(crate::channel::ChannelSessionResolver::new(
        Arc::clone(&deps.session_manager),
        deps.stores.channel_session.clone(),
    ));
    let pairing = Arc::new(PairingService::new(deps.stores.channel_pairing.clone()));
    let ws_state = crate::channel::WsChannelState {
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
        blob_store: deps.stores.blob.clone(),
        inbound_dedup: Arc::new(crate::channel::InboundDedup::new()),
        diagnose_router: Arc::clone(&deps.diagnose_router),
        capabilities: Arc::clone(&deps.channel_capabilities),
        mcp_tunnel_router: Arc::clone(&deps.mcp_tunnel_router),
        sidecar_mcp_manager: Arc::clone(&deps.sidecar_mcp_manager),
    };
    // TraceLayer goes *inside* the auth middleware so it sees the
    // URI AFTER `require_channel_auth` has stripped `?token=…`.
    // Outer-to-inner layer application means "outer" = "first to
    // observe the request", so an outer TraceLayer would log the
    // raw token-bearing URI before auth rewrites it.
    let channel_router: Router<()> = crate::channel::routes().with_state(ws_state);

    let v1_inner = channel_router.layer(TraceLayer::new_for_http());
    let v1 = channel_auth::attach(v1_inner, auth_state);
    Router::new()
        .merge(api::health::routes().layer(TraceLayer::new_for_http()))
        .nest("/v1", v1)
}

fn build_cors(origins: &[String]) -> CorsLayer {
    if origins.is_empty() {
        return CorsLayer::new();
    }
    let parsed: std::result::Result<Vec<_>, _> = origins
        .iter()
        .map(|o| o.parse::<axum::http::HeaderValue>())
        .collect();
    match parsed {
        Ok(list) => CorsLayer::new().allow_origin(AllowOrigin::list(list)),
        Err(e) => {
            tracing::warn!(error = %e, "invalid CORS origin in config; defaulting to none");
            CorsLayer::new()
        }
    }
}
