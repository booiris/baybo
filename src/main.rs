use aura_agent::agent_loop::AgentLoop;
use aura_agent::observability::ObservabilityRecorder;
use aura_agent::policy::ExecutionPolicy;
use aura_agent::router::Router;
use aura_agent::service::{ShutdownSignal, TaskTracker};
use aura_agent::soul::Soul;
use aura_agent::supervisor::AgentSupervisor;
use aura_agent::tool_executor::ToolExecutor;
use aura_context::Tokenizer;
use aura_context::sliding_window::SlidingWindowContext;
use aura_core::{ChatMessage, ContentBlock};
use aura_cost::CostTracker;
use aura_hook::HookManager;
use aura_job::JobManager;
use aura_llm::{LlmClient, ModelInfo, ModelPricing, ResponseParseMode};
use aura_memory::MemoryManager;
use aura_security::{EncryptionKey, LeakDetector, SecretVault, SecurityGateway};
use aura_session::SessionManager;
use aura_storage::StorageSet;
use aura_tools::ToolRegistry;
use aura_trace::TraceCollector;
use aura_workspace::WorkspaceManager;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tracing::info;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

fn init_tracing() {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("aura=info"));

    let log_format = std::env::var("AURA_LOG_FORMAT").unwrap_or_default();

    if log_format == "json" {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().json().with_target(true).with_span_list(true))
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(fmt::layer().with_target(true))
            .init();
    }
}

/// Simple character-based tokenizer for Phase 1.
/// Estimates ~4 characters per token (rough English average).
struct SimpleTokenizer;

impl Tokenizer for SimpleTokenizer {
    fn count_text(&self, text: &str) -> usize {
        text.len() / 4 + 1
    }

    fn count_image(&self, _width: u32, _height: u32) -> usize {
        85 // fixed token estimate for images
    }

    fn count_message(&self, msg: &ChatMessage) -> usize {
        let mut tokens = 4; // structural overhead
        for block in &msg.content {
            match block {
                ContentBlock::Text(t) => tokens += self.count_text(t.as_str()),
                ContentBlock::Image { .. } => tokens += self.count_image(0, 0),
                ContentBlock::Audio { .. } => tokens += 100,
                ContentBlock::File { .. } => tokens += 50,
            }
        }
        tokens
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    info!("Aura - Intelligent Assistant Framework starting");

    // Storage layer (in-memory for Phase 1)
    let storage = StorageSet::in_memory();

    // Session manager
    let session_manager = SessionManager::new(storage.session, chrono::Duration::minutes(30));

    // Job manager
    let job_manager = Arc::new(JobManager::new(storage.job));

    // Cost tracker
    let cost_tracker = Arc::new(CostTracker::new(storage.cost));

    // Trace collector
    let trace_store = Arc::from(storage.trace);
    let trace_collector = Arc::new(Mutex::new(TraceCollector::new(
        "global",
        trace_store,
        true,
        5,
    )));

    // Observability recorder
    let _recorder = Arc::new(ObservabilityRecorder::new(
        job_manager,
        trace_collector,
        cost_tracker,
    ));

    // Secret vault
    let master_key = EncryptionKey::new(b"aura-dev-master-key-32-bytes-ok!".to_vec())?;
    let secret_vault = Arc::new(SecretVault::new(master_key, Arc::from(storage.secret)));

    // WASM runtime + Tool registry
    let wasm_runtime = Arc::new(aura_sandbox::WasmRuntime::new()?);
    let tool_registry = Arc::new(ToolRegistry::new(wasm_runtime));

    // Tool executor
    let tool_executor = Arc::new(ToolExecutor::new(
        Arc::clone(&tool_registry),
        Arc::clone(&secret_vault),
        std::time::Duration::from_secs(30),
    ));

    // Memory manager (without embedder for Phase 1)
    let memory_manager = Arc::new(MemoryManager::without_embedder(storage.memory));

    // LLM client (stub for Phase 1)
    let llm_client = Arc::new(LlmClient::new(
        ModelInfo {
            id: "stub".to_string(),
            provider: "none".to_string(),
            context_window: 128_000,
            supports_tools: true,
            supports_vision: true,
            pricing: ModelPricing {
                input_per_1m_tokens: 0.0,
                output_per_1m_tokens: 0.0,
            },
        },
        ResponseParseMode::NativeFunctionCalling,
    ));

    // Context manager (sliding window)
    let tokenizer: Arc<dyn Tokenizer> = Arc::new(SimpleTokenizer);
    let context_manager = Box::new(SlidingWindowContext::new(Arc::clone(&tokenizer), 100));

    // Workspace and Soul
    let workspace = WorkspaceManager::new(PathBuf::from("."));
    let soul = Soul::from_workspace(&workspace)
        .await
        .unwrap_or_else(|_| Soul::custom("You are Aura, an intelligent assistant.".to_string()));

    // Execution policy
    let policy = ExecutionPolicy::default();

    // Hook manager (empty)
    let _hook_manager = Arc::new(HookManager::new());

    // Agent loop
    let _agent_loop = AgentLoop::new(
        llm_client,
        tool_registry,
        tool_executor,
        context_manager,
        memory_manager,
        policy,
        soul,
    );

    // Message channels
    let (incoming_tx, incoming_rx) = mpsc::channel(256);
    let (response_tx, response_rx) = mpsc::channel(256);

    // Security gateway
    let leak_detector = LeakDetector::with_default_rules();
    let security_gateway = Arc::new(SecurityGateway::with_deny_all_policy(
        leak_detector,
        Arc::clone(&secret_vault),
    ));

    // Supervisor and Router
    let supervisor = AgentSupervisor::new(response_tx);
    let router = Router::new(session_manager, supervisor, vec![], security_gateway);

    // Shutdown coordination
    let shutdown = ShutdownSignal::new();
    let mut task_tracker = TaskTracker::new();

    // Signal handler
    let sig_shutdown = shutdown.clone();
    task_tracker.track(tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM");
            tokio::select! {
                _ = ctrl_c => { info!("received SIGINT"); }
                _ = sigterm.recv() => { info!("received SIGTERM"); }
            }
        }
        #[cfg(not(unix))]
        {
            ctrl_c.await.expect("failed to listen for ctrl_c");
            info!("received SIGINT");
        }
        sig_shutdown.trigger();
    }));

    info!("all components initialized, starting router");

    // Drop the incoming sender so the router can detect shutdown
    // In a real deployment, channels would hold these senders
    drop(incoming_tx);

    // Run router with shutdown awareness
    let router_shutdown = shutdown.clone();
    tokio::select! {
        _ = router.run(incoming_rx, response_rx) => {}
        _ = router_shutdown.wait() => {
            info!("shutdown signal received, stopping router");
        }
    }

    // Cleanup
    task_tracker.shutdown().await;
    info!("Aura shutdown complete");
    Ok(())
}
