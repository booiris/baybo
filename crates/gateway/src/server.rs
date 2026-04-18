//! axum server assembly and shared state for the gateway.
//!
//! The server is split into two concerns:
//!
//! * [`ApiState`] — the handle each route handler needs. Cloned into
//!   every request via axum's `State` extractor; all fields are `Arc`
//!   or thin cloneable types so cloning is cheap.
//! * [`GatewayServer`] — lifecycle wrapper that binds the socket,
//!   attaches middleware, and drives `axum::serve` to completion.
//!
//! `GatewayDeps` is the caller-facing input: the runtime builds the
//! managers and passes them here; the gateway doesn't reach into the
//! runtime or construct anything else.

use std::net::SocketAddr;
use std::sync::Arc;

use aura_agent::{
    CronScheduler, JobManager, MemoryManager, SessionManager, service::ShutdownSignal,
};
use aura_channels::ChannelRegistry;
use aura_config::AuraConfig;
use aura_llm::LlmClient;
use aura_skills::SkillRegistry;
use aura_storage::TraceStore;
use aura_tools::ToolRegistry;
use axum::Router;
use axum::middleware;
use tokio::sync::RwLock;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::api;
use crate::auth::{AuthState, require_token};
use crate::config::RuntimeGatewayConfig;
use crate::http_adapter::HttpAdapter;
use crate::{GatewayError, Result};

/// Caller-supplied managers + config needed to run the gateway.
///
/// Built by the runtime (`src/runtime.rs`) and handed to
/// [`GatewayServer::new`]. The gateway takes references (cloning Arcs)
/// into [`ApiState`]; the caller keeps the originals so the same
/// managers can be shared with the router / TUI / cron loop.
pub struct GatewayDeps {
    pub config: Arc<AuraConfig>,
    pub runtime_config: RuntimeGatewayConfig,
    pub adapter: Arc<HttpAdapter>,
    pub session_manager: Arc<SessionManager>,
    pub job_manager: Arc<JobManager>,
    pub cron_scheduler: Arc<CronScheduler>,
    pub memory_manager: Arc<MemoryManager>,
    pub trace_store: Arc<dyn TraceStore>,
    pub skill_registry: Arc<SkillRegistry>,
    pub tool_registry: Arc<ToolRegistry>,
    pub channel_registry: Arc<RwLock<ChannelRegistry>>,
    pub llm_client: Arc<LlmClient>,
    pub auth_token: String,
}

/// Per-request shared state. Cheap to clone — every field is an `Arc`
/// or small owned value.
#[derive(Clone)]
pub struct ApiState {
    pub config: Arc<AuraConfig>,
    pub adapter: Arc<HttpAdapter>,
    pub session_manager: Arc<SessionManager>,
    pub job_manager: Arc<JobManager>,
    pub cron_scheduler: Arc<CronScheduler>,
    pub memory_manager: Arc<MemoryManager>,
    pub trace_store: Arc<dyn TraceStore>,
    pub skill_registry: Arc<SkillRegistry>,
    pub tool_registry: Arc<ToolRegistry>,
    pub channel_registry: Arc<RwLock<ChannelRegistry>>,
    pub llm_client: Arc<LlmClient>,
    /// Pretty form of the bind address for status output. We don't
    /// expose the parsed `SocketAddr` so handlers can't reach through
    /// and mutate it.
    pub bind_display: String,
}

impl ApiState {
    fn from_deps(deps: &GatewayDeps) -> Self {
        Self {
            config: Arc::clone(&deps.config),
            adapter: Arc::clone(&deps.adapter),
            session_manager: Arc::clone(&deps.session_manager),
            job_manager: Arc::clone(&deps.job_manager),
            cron_scheduler: Arc::clone(&deps.cron_scheduler),
            memory_manager: Arc::clone(&deps.memory_manager),
            trace_store: Arc::clone(&deps.trace_store),
            skill_registry: Arc::clone(&deps.skill_registry),
            tool_registry: Arc::clone(&deps.tool_registry),
            channel_registry: Arc::clone(&deps.channel_registry),
            llm_client: Arc::clone(&deps.llm_client),
            bind_display: deps.runtime_config.bind.to_string(),
        }
    }
}

pub struct GatewayServer {
    bind: SocketAddr,
    router: Router,
    shutdown_grace: std::time::Duration,
}

impl GatewayServer {
    pub fn new(deps: GatewayDeps) -> Self {
        let bind = deps.runtime_config.bind;
        let shutdown_grace = deps.runtime_config.shutdown_grace;
        let router = build_router(deps);
        Self {
            bind,
            router,
            shutdown_grace,
        }
    }

    /// Resolve the bind address the server was configured with.
    pub fn bind(&self) -> SocketAddr {
        self.bind
    }

    /// Configured grace period for in-flight requests on shutdown.
    pub fn shutdown_grace(&self) -> std::time::Duration {
        self.shutdown_grace
    }

    /// Run the server to completion. Returns once [`ShutdownSignal`] is
    /// triggered and axum has drained in-flight requests.
    pub async fn run(self, shutdown: ShutdownSignal) -> Result<()> {
        let listener = tokio::net::TcpListener::bind(self.bind)
            .await
            .map_err(|e| GatewayError::Bind {
                addr: self.bind.to_string(),
                reason: e.to_string(),
            })?;
        tracing::info!(bind = %self.bind, "gateway listening");
        let shutdown_fut = async move {
            shutdown.wait().await;
        };
        axum::serve(listener, self.router.into_make_service())
            .with_graceful_shutdown(shutdown_fut)
            .await
            .map_err(|e| GatewayError::Internal(format!("serve error: {e}")))
    }
}

fn build_router(deps: GatewayDeps) -> Router {
    let state = ApiState::from_deps(&deps);
    let auth_state = AuthState::new(deps.auth_token.clone());

    let cors = build_cors(&deps.runtime_config.cors_allowed_origins);

    let v1 = api::v1_router()
        .with_state(state)
        .layer(middleware::from_fn_with_state(auth_state, require_token));

    Router::new()
        .merge(api::health::routes())
        .nest("/v1", v1)
        .layer(cors)
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
