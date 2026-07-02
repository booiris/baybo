//! axum server assembly and shared state for the gateway.
//!
//! The gateway exposes two listeners:
//!
//! * **Admin** — TCP, bearer-token authenticated. Hosts config,
//!   status, jobs, cron, memory, traces, skills, tools, llm, and a
//!   read-only channel list. Also co-hosts the channel-token web chat
//!   routes (`/v1/channel-ws`, `/v1/blobs/*`) so browser clients can
//!   reach them over the public bind.
//! * **Channel** — loopback TCP (`127.0.0.1:<ephemeral>`),
//!   channel-token authenticated against [`ChannelTokenTable`] (see
//!   [`crate::channel_listener`] and [`crate::auth::channel`]). Hosts
//!   the WebSocket endpoint
//!   (`/v1/channel-ws`) for the TUI and subprocess sidecars (which
//!   live on the same host and reach it via the discovered ephemeral
//!   port). The admin listener co-hosts the same `/v1/channel-ws`
//!   route under the same channel-token middleware so browser-side
//!   web chat clients can open the WS over the public bind without
//!   port discovery. Session CRUD lives on the admin surface; the
//!   router creates sessions lazily on first message frame.
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

use baybo_agent::LlmPoolHandle;
use baybo_agent::supervisor::AgentSupervisor;
use baybo_agent::{CronScheduler, SessionManager, service::ShutdownSignal};
use baybo_channels::{ChannelRegistry, RouterInbound};
use baybo_config::BayboConfig;
use baybo_job::JobLifecycle;

use crate::reload::ConfigReloader;
use axum::Router;
use axum::middleware;
use baybo_security::SecretVault;
use baybo_skills::SkillRegistry;
use baybo_storage::Store;
use baybo_store::ChannelBotStore;
use baybo_tools::ToolRegistry;
use baybo_trace::TraceStore;
use tokio::sync::mpsc;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::api;
use crate::auth::admin::{AdminAuthState, require_admin_token};
use crate::auth::{ChannelTokenTable, channel as channel_auth};
use crate::channel::WsChannelState;
use crate::config::RuntimeGatewayConfig;
use crate::log_buffer::LogBuffer;
use crate::{GatewayError, Result};

/// Caller-supplied managers + config needed to run the gateway.
///
/// Built by the runtime (`crates/baybo/src/runtime.rs`) and handed to
/// [`GatewayServer::new`]. The gateway takes references (cloning Arcs)
/// into [`AdminState`] / [`ChannelState`]; the caller keeps the
/// originals so the same managers can be shared with the router / TUI
/// / cron loop.
pub struct GatewayDeps {
    pub config: Arc<BayboConfig>,
    /// Path to the on-disk `baybo.json` the gateway was loaded from, if
    /// any. Needed by `PUT/DELETE /v1/config` so remote clients can
    /// write through to the same file that `baybo config set/unset`
    /// targets. `None` when running with defaults only — mutation
    /// endpoints then reject with `ConfigPathUnset`.
    pub config_path: Option<PathBuf>,
    pub runtime_config: RuntimeGatewayConfig,
    pub session_manager: Arc<SessionManager>,
    pub job_lifecycle: Arc<JobLifecycle>,
    pub cron_scheduler: Arc<CronScheduler>,
    pub skill_registry: Arc<SkillRegistry>,
    pub tool_registry: Arc<ToolRegistry>,
    pub channel_registry: Arc<ChannelRegistry>,
    pub llm_pool: LlmPoolHandle,
    /// Actor registry, used by `PUT /v1/chat/sessions/{id}/model` to
    /// route an `AgentMessage::SetModel` to a live actor (re-pin in
    /// place). Cheap to clone — backed by an `Arc<DashMap>`.
    pub supervisor: AgentSupervisor,
    /// Triggers an in-process config hot-reload; held so admin endpoints
    /// and the SIGHUP handler can call it. See docs/config-hot-reload.md.
    pub config_reloader: Arc<dyn ConfigReloader>,
    /// Bearer token for the admin TCP listener. Stored in the vault as
    /// `gateway.admin_token`.
    pub admin_token: String,
    /// Shared ring buffer of recent tracing events surfaced by
    /// `/v1/logs`. Installed as a `tracing::Layer` at process init.
    pub log_buffer: Arc<LogBuffer>,
    /// Router intake. Cloned into the WS channel server so sidecar
    /// frames can be forwarded as `IncomingMessage`s.
    pub incoming_tx: mpsc::Sender<RouterInbound>,
    /// Per-install capability tokens. The channel TCP listener
    /// passes this to the WS server for Register-frame verification.
    pub channel_tokens: ChannelTokenTable,
    /// Stash of [`crate::channel::StashedTokenHandle`]s for web chat
    /// tabs, keyed by the token string. The admin chat handler
    /// inserts on mint; the channel WS route takes the handle on
    /// successful upgrade so it rides the connection lifetime — when
    /// the WS closes the handle drops and the token revokes itself
    /// out of [`Self::channel_tokens`]. The
    /// [`crate::channel::WebTokenJanitor`] sweeps unclaimed entries
    /// on a TTL so mints that never reach a WS upgrade don't leak.
    /// Shared by `AdminState` and `WsChannelState` so the mint side
    /// and the take side address the same map.
    pub web_chat_tokens: Arc<dashmap::DashMap<String, crate::channel::StashedTokenHandle>>,
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
}

/// State shared with admin TCP handlers. Cheap to clone.
#[derive(Clone)]
pub struct AdminState {
    pub config: Arc<BayboConfig>,
    pub config_path: Option<PathBuf>,
    pub session_manager: Arc<SessionManager>,
    pub job_lifecycle: Arc<JobLifecycle>,
    pub cron_scheduler: Arc<CronScheduler>,
    pub trace_store: Arc<dyn TraceStore>,
    pub cost_store: Arc<dyn baybo_cost::CostStore>,
    /// Pre-built `QueryApi` so `/v1/traces/{id}` and any future
    /// query-shaped endpoint don't allocate one per request.
    pub query_api: Arc<baybo_query::QueryApi>,
    pub skill_registry: Arc<SkillRegistry>,
    pub tool_registry: Arc<ToolRegistry>,
    pub channel_registry: Arc<ChannelRegistry>,
    pub llm_pool: LlmPoolHandle,
    /// Actor registry — lets the chat model-switch endpoint re-pin a
    /// live session's LLM via `AgentMessage::SetModel`.
    pub supervisor: AgentSupervisor,
    pub config_reloader: Arc<dyn ConfigReloader>,
    pub log_buffer: Arc<LogBuffer>,
    pub channel_bot_store: Arc<dyn ChannelBotStore>,
    pub channel_control: Arc<crate::channel::ChannelControlRegistry>,
    pub secret_vault: Arc<SecretVault>,
    /// Shared with the channel listener so `POST /v1/chat/session`
    /// can mint short-lived web channel-tokens that the same
    /// `require_channel_auth` middleware accepts on `/v1/channel-ws`.
    pub channel_tokens: crate::auth::ChannelTokenTable,
    /// Stash of live web-chat token handles, keyed by the token
    /// string. The admin mint endpoint inserts here; the channel WS
    /// route removes (and moves the handle into the resulting
    /// `Sidecar`) on successful upgrade. Handles still in the map
    /// are tokens that were minted but never used to open a WS —
    /// they sit here until claimed, reaped by the
    /// [`crate::channel::WebTokenJanitor`] on TTL expiry, or
    /// released at process exit. Shared with
    /// `GatewayDeps::web_chat_tokens` and `WsChannelState`.
    pub web_chat_tokens: Arc<dashmap::DashMap<String, crate::channel::StashedTokenHandle>>,
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
    pub incoming_tx: mpsc::Sender<RouterInbound>,
    pub channel_tokens: ChannelTokenTable,
}

impl AdminState {
    fn from_deps(deps: &GatewayDeps) -> Self {
        let query_api = Arc::new(baybo_query::QueryApi::new(
            deps.session_manager.store(),
            Arc::clone(&deps.job_lifecycle),
            deps.stores.trace.clone(),
            deps.stores.cost.clone(),
        ));
        Self {
            config: Arc::clone(&deps.config),
            config_path: deps.config_path.clone(),
            session_manager: Arc::clone(&deps.session_manager),
            job_lifecycle: Arc::clone(&deps.job_lifecycle),
            cron_scheduler: Arc::clone(&deps.cron_scheduler),
            trace_store: Arc::clone(&deps.stores.trace),
            cost_store: Arc::clone(&deps.stores.cost),
            query_api,
            skill_registry: Arc::clone(&deps.skill_registry),
            tool_registry: Arc::clone(&deps.tool_registry),
            channel_registry: Arc::clone(&deps.channel_registry),
            llm_pool: Arc::clone(&deps.llm_pool),
            supervisor: deps.supervisor.clone(),
            config_reloader: Arc::clone(&deps.config_reloader),
            log_buffer: Arc::clone(&deps.log_buffer),
            channel_bot_store: Arc::clone(&deps.stores.channel_bot),
            channel_control: Arc::clone(&deps.channel_control),
            secret_vault: Arc::clone(&deps.secret_vault),
            channel_tokens: deps.channel_tokens.clone(),
            web_chat_tokens: Arc::clone(&deps.web_chat_tokens),
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

/// Spawn the relay-**content** control manager and return its join handle for the
/// caller to track under the shared shutdown drain.
///
/// Holds an outbound A→C control link so a phone can reach this (possibly NAT'd)
/// gateway for chat via the relay. The manager self-gates on the approved device
/// row (idle until one is paired), reading the relay URL + admission key from it —
/// there is no `relay` config block. It installs its own rustls CryptoProvider (it
/// owns the wss dial). Spawned alongside the other gateway managers so it rides the
/// same `ShutdownSignal` + task tracker and is drained on shutdown.
pub fn spawn_relay_content(
    deps: &GatewayDeps,
    shutdown: ShutdownSignal,
) -> tokio::task::JoinHandle<()> {
    crate::channel::relay_content::spawn(WsChannelState::from_deps(deps), shutdown)
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

    // Browser-facing `/v1/channel-ws` + `/v1/blobs`. Same handlers the
    // loopback channel listener serves, but mounted here so the web
    // chat page (which loads from this admin origin) can open its WS
    // without discovering the ephemeral loopback port. The channel-
    // token auth path is independent of the admin bearer; tokens are
    // only issued after the operator hits `POST /v1/chat/sessions`
    // through the bearer-protected admin surface above.
    let channel_v1 = build_channel_v1_subrouter(
        WsChannelState::from_deps(&deps),
        channel_auth::ChannelAuthState::new(deps.channel_tokens.clone())
            .with_device_store(deps.stores.device.clone()),
    );

    Router::new()
        .merge(api::health::routes().layer(TraceLayer::new_for_http()))
        .merge(admin_router)
        .merge(channel_v1)
        .fallback(api::webui::serve)
        .layer(cors)
}

/// `/v1/channel-ws` + `/v1/blobs` subtree, wrapped in channel-token
/// auth. Shared between the admin listener (for the browser-side web
/// chat page) and the loopback channel listener (for the TUI and
/// subprocess sidecars).
fn build_channel_v1_subrouter(
    state: WsChannelState,
    auth_state: channel_auth::ChannelAuthState,
) -> Router {
    // TraceLayer goes *inside* the auth middleware so it sees the
    // URI AFTER `require_channel_auth` has stripped `?token=…`.
    let inner: Router<()> = crate::channel::routes()
        .with_state(state)
        .layer(TraceLayer::new_for_http());
    let v1_authed = channel_auth::attach(inner, auth_state);
    Router::new().nest("/v1", v1_authed)
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
    let channel_v1 = build_channel_v1_subrouter(WsChannelState::from_deps(deps), auth_state);
    Router::new()
        .merge(api::health::routes().layer(TraceLayer::new_for_http()))
        .merge(channel_v1)
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
