//! axum server assembly and shared state for the gateway.
//!
//! The gateway exposes two listeners:
//!
//! * **Admin** — TCP, bearer-token authenticated. Hosts config,
//!   status, jobs, cron, memory, traces, skills, tools, llm, and a
//!   read-only channel list. No chat content flows through these
//!   endpoints.
//! * **Channel** — Unix domain socket, peer-credential +
//!   PSK/token authenticated (see `uds` and `auth_channel` modules).
//!   Hosts the WebSocket endpoint (`/v1/channel-ws`) — the only surface
//!   the TUI and sidecar channel plugins talk to. Session CRUD lives
//!   on the admin surface; the router creates sessions lazily on first
//!   message frame.
//!
//! [`AdminState`] and [`ChannelState`] split the old monolithic
//! `ApiState` so each listener only sees the managers it needs. Both
//! are cheap to clone — every field is an `Arc` or a small value.
//!
//! [`GatewayServer`] owns the admin listener. The UDS half lives in
//! [`crate::uds`] and is driven by the gateway CLI alongside the
//! admin server.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use aura_agent::{
    CronScheduler, JobManager, MemoryManager, SessionManager, service::ShutdownSignal,
};
use aura_channels::{ChannelRegistry, IncomingMessage};
use aura_config::AuraConfig;
use aura_gateway_auth::ChannelTokenTable;
use aura_llm::LlmClient;
use aura_security::SecretVault;
use aura_skills::SkillRegistry;
use aura_storage::TraceStore;
use aura_tools::ToolRegistry;
use axum::Router;
use axum::middleware;
use tokio::sync::mpsc;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::api;
use crate::auth_admin::{AdminAuthState, require_admin_token};
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
    pub trace_store: Arc<dyn TraceStore>,
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
    /// Per-install capability tokens. The channel UDS listener passes
    /// this to the WS server for Register-frame verification.
    pub channel_tokens: ChannelTokenTable,
    /// Vault handle shared with the channel server so the WS route can
    /// build a [`crate::channel::TuiHistoryStore`] without re-opening
    /// libsql. The gateway is the only process that writes the TUI
    /// input-history key; the TUI itself never touches the vault.
    pub secret_vault: Arc<SecretVault>,
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
    /// Pretty form of the admin bind address for `/v1/status`.
    pub bind_display: String,
}

/// State shared with channel UDS handlers. Cheap to clone.
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
            trace_store: Arc::clone(&deps.trace_store),
            skill_registry: Arc::clone(&deps.skill_registry),
            tool_registry: Arc::clone(&deps.tool_registry),
            channel_registry: Arc::clone(&deps.channel_registry),
            llm_client: Arc::clone(&deps.llm_client),
            log_buffer: Arc::clone(&deps.log_buffer),
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

/// TCP admin server. Long-lived; owns its own axum `Router` built from
/// the caller-supplied [`GatewayDeps`]. The companion UDS listener is
/// started separately (see `crate::uds::serve`) with the same deps.
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
    let admin_router = admin_router
        .with_state(state)
        .layer(middleware::from_fn_with_state(
            auth_state,
            require_admin_token,
        ));

    Router::new()
        .merge(api::health::routes())
        .merge(admin_router)
        .fallback(api::webui::serve)
        .layer(cors)
        .layer(TraceLayer::new_for_http())
}

/// Build the router served on the channel UDS. Called by
/// [`crate::uds::serve`]. The auth middleware is applied to the `/v1`
/// routes only — health lives outside so orchestrators can poll it
/// without an auth handshake.
pub fn build_channel_router(
    deps: &GatewayDeps,
    auth_state: crate::auth_channel::ChannelAuthState,
) -> Router {
    let _channel_state = ChannelState::from_deps(deps);
    let tui_history = Arc::new(crate::channel::TuiHistoryStore::new(Arc::clone(
        &deps.secret_vault,
    )));
    let ws_state = crate::channel::WsChannelState {
        registry: Arc::clone(&deps.channel_registry),
        incoming_tx: deps.incoming_tx.clone(),
        tokens: deps.channel_tokens.clone(),
        session_manager: Arc::clone(&deps.session_manager),
        tui_history,
    };
    let v1 = crate::auth_channel::attach(crate::channel::routes().with_state(ws_state), auth_state);
    Router::new()
        .merge(api::health::routes())
        .nest("/v1", v1)
        .layer(TraceLayer::new_for_http())
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
