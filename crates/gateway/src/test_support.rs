//! Test-only helpers for gateway crate-level tests.
//!
//! Builds a minimal [`GatewayDeps`] wired against an in-memory libsql
//! store, plus the primitives that the `uds`, `admin_has_no_channels`,
//! and spawn-lifecycle tests share.
//!
//! Gated behind the `test-support` feature so the helpers are available
//! to downstream crate-level tests (`tests/*.rs`) without shipping in
//! release builds. See the per-crate test-support pattern in `CLAUDE.md`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::auth::ChannelTokenTable;
use aura_agent::service::ShutdownSignal;
use aura_agent::{CronScheduler, JobManager, MemoryManager, SessionManager};
use aura_channels::{ChannelRegistry, IncomingMessage};
use aura_config::AuraConfig;
use aura_llm::{LlmProviderConfig, LlmProviderRegistry};
use aura_security::{EncryptionKey, SecretVault};
use aura_skills::SkillRegistry;
use aura_storage::Store;
use aura_tools::ToolRegistry;
use tempfile::TempDir;
use tokio::sync::mpsc;

use crate::config::RuntimeGatewayConfig;
use crate::log_buffer::LogBuffer;
use crate::server::GatewayDeps;

/// Bearer token every test gateway is wired with. Exposed so callers
/// that talk to the admin listener can authenticate without duplicating
/// the constant.
pub const TEST_ADMIN_TOKEN: &str = "test-admin-token-fixed-32-bytes!!";

/// Bundle returned by [`build_test_deps`]. Holds the deps plus the
/// auxiliary handles tests need to keep alive (the tempdir backing
/// libsql, the shared shutdown signal for orderly teardown).
pub struct TestGateway {
    pub deps: GatewayDeps,
    pub shutdown: ShutdownSignal,
    /// Receiver paired with `deps.incoming_tx`. Exposed so tests that
    /// exercise the router-intake path (e.g. the WS channel server)
    /// can assert on frames forwarded by the gateway.
    pub incoming_rx: mpsc::Receiver<IncomingMessage>,
    /// Capability table shared with `deps.channel_tokens`. Tests mint
    /// tokens here to authenticate sidecar clients.
    pub channel_tokens: ChannelTokenTable,
    pub _tempdir: TempDir,
}

/// Construct a fully-wired `GatewayDeps` against in-memory storage.
///
/// `admin_bind` is baked into the runtime config so
/// `AdminState::bind_display` matches what the `/v1/status` handler
/// returns. Use `127.0.0.1:0` when the bind address doesn't matter.
pub async fn build_test_deps(admin_bind: SocketAddr) -> TestGateway {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let db_path = tempdir.path().join("gateway-test.db");
    let stores = Store::open(&db_path).await.expect("open in-memory store");

    let config = Arc::new(AuraConfig::default());
    let session_manager = Arc::new(SessionManager::new(
        stores.session.clone(),
        chrono::Duration::seconds(300),
    ));
    let job_manager = Arc::new(JobManager::new(stores.job.clone()));

    let secret_vault = Arc::new(SecretVault::new(
        EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec())
            .expect("build test encryption key"),
        stores.secret.clone(),
    ));

    let shutdown = ShutdownSignal::new();
    let (cron_tx, _cron_rx) = mpsc::channel(16);
    let cron_scheduler = Arc::new(CronScheduler::new(
        stores.cron.clone(),
        cron_tx,
        Arc::new(shutdown.clone()) as Arc<dyn aura_cron::Shutdown>,
    ));

    let memory_manager = Arc::new(MemoryManager::without_embedder(stores.memory.clone()));
    let skill_registry = Arc::new(SkillRegistry::new());
    let tool_registry = Arc::new(ToolRegistry::new());
    let channel_registry = Arc::new(ChannelRegistry::new());
    let channel_control = Arc::new(crate::channel::ChannelControlRegistry::new());
    let diagnose_router = Arc::new(crate::channel::DiagnoseRouter::new());
    let channel_capabilities = Arc::new(crate::channel::ChannelCapabilities::new());
    let mcp_tunnel_router = Arc::new(crate::channel::McpTunnelRouter::new(Arc::clone(
        &channel_control,
    )));
    let sidecar_mcp_manager = Arc::new(crate::channel::SidecarMcpManager::new(Arc::clone(
        &mcp_tunnel_router,
    )));
    let bot_reconciler = Arc::new(crate::channel::ChannelBotReconciler::new(
        Arc::clone(&channel_control),
        stores.channel_bot.clone(),
        Arc::clone(&secret_vault),
    ));

    let registry = LlmProviderRegistry::with_default_providers();
    let llm_client = Arc::new(
        registry
            .create_client(&LlmProviderConfig {
                provider: "openai".into(),
                api_key: Some("sk-test-placeholder".into()),
                base_url: None,
                model: "gpt-4o-mini".into(),
                supports_vision: None,
            })
            .expect("stub LLM client"),
    );

    let runtime_config = RuntimeGatewayConfig {
        admin_bind,
        cors_allowed_origins: Vec::new(),
        shutdown_grace: Duration::from_millis(250),
    };

    // Tests that need to observe router intake (e.g. the WS channel
    // server round-trip) pull `incoming_rx` off `TestGateway`; others
    // just drop it.
    let (incoming_tx, incoming_rx) = mpsc::channel(16);
    let channel_tokens = ChannelTokenTable::new();

    let deps = GatewayDeps {
        config,
        config_path: None,
        runtime_config,
        session_manager,
        job_manager,
        cron_scheduler,
        memory_manager,
        skill_registry,
        tool_registry,
        channel_registry,
        llm_client,
        admin_token: TEST_ADMIN_TOKEN.to_string(),
        log_buffer: LogBuffer::new(256),
        incoming_tx,
        channel_tokens: channel_tokens.clone(),
        secret_vault,
        stores,
        channel_control,
        bot_reconciler,
        diagnose_router,
        channel_capabilities,
        mcp_tunnel_router,
        sidecar_mcp_manager,
    };

    TestGateway {
        deps,
        shutdown,
        incoming_rx,
        channel_tokens,
        _tempdir: tempdir,
    }
}
