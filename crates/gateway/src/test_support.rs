//! Test-only helpers for gateway crate-level tests.
//!
//! Builds a minimal [`GatewayDeps`] wired against an in-memory sqlite
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
use baybo_agent::service::ShutdownSignal;
use baybo_agent::{CronScheduler, SessionManager};
use baybo_channels::{ChannelRegistry, RouterInbound};
use baybo_config::BayboConfig;
use baybo_job::JobLifecycle;
use baybo_llm::{LlmProviderConfig, LlmProviderRegistry};
use baybo_security::{EncryptionKey, SecretVault};
use baybo_skills::SkillRegistry;
use baybo_storage::Store;
use baybo_tools::ToolRegistry;
use tempfile::TempDir;
use tokio::sync::mpsc;

use crate::config::RuntimeGatewayConfig;
use crate::log_buffer::LogBuffer;
use crate::server::GatewayDeps;

/// Canned outcome the test reloaders return so endpoints that surface a
/// `ReloadOutcome` still produce a 200.
fn canned_reload_outcome() -> crate::reload::ReloadOutcome {
    crate::reload::ReloadOutcome {
        active_model: "stub-model".into(),
        default_entry: "stub".into(),
        entries: vec!["stub".into()],
        dropped: vec![],
    }
}

/// Stub reloader for gateway crate tests — they don't exercise a real
/// reload, they just need the `config_reloader` field populated. Returns
/// a canned outcome so a test that does hit the reload endpoint gets 200.
struct StubConfigReloader;

#[async_trait::async_trait]
impl crate::reload::ConfigReloader for StubConfigReloader {
    async fn reload(
        &self,
    ) -> std::result::Result<crate::reload::ReloadOutcome, crate::reload::ReloadError> {
        Ok(canned_reload_outcome())
    }

    async fn dry_run(
        &self,
        _candidate: &baybo_config::BayboConfig,
    ) -> std::result::Result<(), crate::reload::ReloadError> {
        Ok(())
    }
}

/// Reloader whose `dry_run` always rejects the candidate. Lets a test
/// assert the C4 contract: a rejected dry-run short-circuits the handler
/// with a 400 *before* it writes, so the on-disk config is never dirtied.
pub struct RejectingDryRunReloader;

#[async_trait::async_trait]
impl crate::reload::ConfigReloader for RejectingDryRunReloader {
    async fn reload(
        &self,
    ) -> std::result::Result<crate::reload::ReloadOutcome, crate::reload::ReloadError> {
        Ok(canned_reload_outcome())
    }

    async fn dry_run(
        &self,
        _candidate: &baybo_config::BayboConfig,
    ) -> std::result::Result<(), crate::reload::ReloadError> {
        Err(crate::reload::ReloadError::LlmRebuild(
            "test: default entry unbuildable".into(),
        ))
    }
}

/// Reloader whose `reload` reports a non-hot field is pending a restart (as
/// when a prior `PUT /v1/config` edit awaits one); `dry_run` passes so the
/// admin endpoint still writes. Lets a test assert a hot LLM edit then
/// surfaces `requires_restart: true` rather than a confusing 400.
pub struct NonHotPendingReloader;

#[async_trait::async_trait]
impl crate::reload::ConfigReloader for NonHotPendingReloader {
    async fn reload(
        &self,
    ) -> std::result::Result<crate::reload::ReloadOutcome, crate::reload::ReloadError> {
        Err(crate::reload::ReloadError::NotHotReloadable(
            "gateway".into(),
        ))
    }

    async fn dry_run(
        &self,
        _candidate: &baybo_config::BayboConfig,
    ) -> std::result::Result<(), crate::reload::ReloadError> {
        Ok(())
    }
}

/// Bearer token every test gateway is wired with. Exposed so callers
/// that talk to the admin listener can authenticate without duplicating
/// the constant.
pub const TEST_ADMIN_TOKEN: &str = "test-admin-token-fixed-32-bytes!!";

/// Bundle returned by [`build_test_deps`]. Holds the deps plus the
/// auxiliary handles tests need to keep alive (the tempdir backing
/// sqlite, the shared shutdown signal for orderly teardown).
pub struct TestGateway {
    pub deps: GatewayDeps,
    pub shutdown: ShutdownSignal,
    /// Receiver paired with `deps.incoming_tx`. Exposed so tests that
    /// exercise the router-intake path (e.g. the WS channel server)
    /// can assert on frames forwarded by the gateway.
    pub incoming_rx: mpsc::Receiver<RouterInbound>,
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
    let stores = Store::open(&db_path)
        .await
        .expect("open tempdir-backed test store");

    let config = Arc::new(BayboConfig::default());
    let session_manager = Arc::new(SessionManager::new(
        stores.session.clone(),
        stores.session_summary.clone(),
        stores.session_folder.clone(),
    ));
    let job_lifecycle = Arc::new(JobLifecycle::new(stores.job.clone()));

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
        Arc::new(shutdown.clone()) as Arc<dyn baybo_cron::Shutdown>,
    ));

    let skill_registry = Arc::new(SkillRegistry::new());
    let tool_registry = Arc::new(ToolRegistry::new());
    let channel_registry = Arc::new(ChannelRegistry::new());
    let channel_control = Arc::new(crate::channel::ChannelControlRegistry::new());
    let bot_reconciler = Arc::new(crate::channel::ChannelBotReconciler::new(
        Arc::clone(&channel_control),
        stores.channel_bot.clone(),
        Arc::clone(&secret_vault),
    ));

    let registry = LlmProviderRegistry::with_default_providers();
    let llm_client = registry
        .create_client(
            &LlmProviderConfig {
                provider: "openai".into(),
                api_key: Some("sk-test-placeholder".into()),
                base_url: None,
                model: "gpt-4o-mini".into(),
                supports_vision: None,
                context_window: None,
                pricing: None,
                reasoning_effort: None,
                vault: None,
                proxy: None,
            },
            None,
            baybo_llm::CostHooks::passthrough(),
        )
        .expect("stub LLM client");

    // Wrap the stub client in a single-entry pool handle so GatewayDeps
    // hands the gateway a hot-swappable pool (matches production).
    let llm_pool: baybo_agent::LlmPoolHandle = {
        let name = baybo_model::LlmEntryName::from(llm_client.model_info().id.clone());
        let mut clients = std::collections::HashMap::new();
        clients.insert(name.clone(), llm_client);
        Arc::new(parking_lot::RwLock::new(Arc::new(
            baybo_agent::LlmClientPool::new(clients, name).expect("stub pool default present"),
        )))
    };

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

    // The gateway test harness spawns no actors, so the supervisor stays
    // empty: `route` always returns false (the model-switch endpoint then
    // takes its persist-directly branch) and the response channel is
    // never driven. A throwaway sender satisfies the constructor.
    let (agent_output_tx, _agent_output_rx) = mpsc::channel(1);
    let supervisor = baybo_agent::supervisor::AgentSupervisor::new(agent_output_tx);

    let deps = GatewayDeps {
        config,
        config_path: None,
        runtime_config,
        session_manager,
        job_lifecycle,
        cron_scheduler,
        skill_registry,
        tool_registry,
        channel_registry,
        llm_pool,
        supervisor,
        config_reloader: Arc::new(StubConfigReloader),
        admin_token: TEST_ADMIN_TOKEN.to_string(),
        log_buffer: LogBuffer::new(256),
        incoming_tx,
        channel_tokens: channel_tokens.clone(),
        secret_vault,
        stores,
        channel_control,
        bot_reconciler,
    };

    TestGateway {
        deps,
        shutdown,
        incoming_rx,
        channel_tokens,
        _tempdir: tempdir,
    }
}
