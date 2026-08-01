use std::path::PathBuf;
use std::sync::Arc;

use baybo_agent::{CronScheduler, SecurityGateway, SessionManager};
use baybo_channels::ChannelRegistry;
use baybo_config::BayboConfig;
use baybo_cost::CostStore;
use baybo_llm::BillableLlm;
use baybo_pairing::DevicePairingService;
use baybo_query::QueryApi;
use baybo_security::{LeakDetector, SecretVault};
use baybo_skills::SkillRegistry;
use baybo_skills_assessor::SkillAssessor;
use baybo_store::{ChannelBotStore, ChannelPairingStore};
use baybo_tools::ToolRegistry;
use baybo_trace::TraceStore;
use baybo_turn::TurnLifecycle;
use baybo_workspace::WorkspacePaths;

use crate::format::OutputFormat;

/// Where a command was invoked from.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Invoked via `baybo <cmd>` at the shell.
    Argv,
    /// Invoked via `/<cmd>` in a chat channel.
    Slash,
}

/// Shared runtime handles available to every command.
///
/// Constructed once at startup and reused for both argv and slash dispatch.
/// All fields are `Arc` so the context itself is cheap to clone.
pub struct CommandContext {
    pub config: Arc<BayboConfig>,
    pub config_path: Option<PathBuf>,
    pub skills: Arc<SkillRegistry>,
    pub tools: Arc<ToolRegistry>,
    pub channels: Arc<ChannelRegistry>,
    pub llm: Option<Arc<BillableLlm>>,
    pub workspace: Arc<WorkspacePaths>,
    pub session: Option<Arc<SessionManager>>,
    pub turn: Option<Arc<TurnLifecycle>>,
    pub cron: Option<Arc<CronScheduler>>,
    pub trace: Option<Arc<dyn TraceStore>>,
    /// Pre-built `QueryApi` so trace / turn / session commands don't
    /// allocate one per invocation. `None` when the context lacks any
    /// of session / turn / trace (e.g. argv commands that don't touch
    /// the trace surface — `baybo skills info`).
    pub query_api: Option<Arc<QueryApi>>,
    pub security: Option<Arc<SecurityGateway>>,
    pub leak_detector: Option<Arc<LeakDetector>>,
    pub skill_assessor: Option<Arc<SkillAssessor>>,
    /// Per-tenant credential metadata. Populated for one-shot argv
    /// commands that need to mutate or read the roster (`baybo
    /// channel list/add/remove …`); `None` during TUI / slash dispatch so those
    /// paths can't accidentally rotate tokens.
    pub channel_bot_store: Option<Arc<dyn ChannelBotStore>>,
    /// Per-user pairing gate store. Populated alongside
    /// `channel_bot_store` for one-shot CLI commands that drive
    /// `baybo pair {list,approve,revoke}`.
    pub channel_pairing_store: Option<Arc<dyn ChannelPairingStore>>,
    /// iOS-companion device pairing service. Populated for one-shot CLI
    /// commands that drive `baybo device {pair,approve,list,revoke}`.
    pub device_pairing_service: Option<Arc<DevicePairingService>>,
    /// Shared vault — populated for the same subset of commands as
    /// `channel_bot_store`. Used to read/write bot tokens keyed as
    /// `channel.<channel_type>.bot.<bot_id>.token`.
    pub secret_vault: Option<Arc<SecretVault>>,
    pub format: OutputFormat,
    pub invocation: Invocation,
    pub confirmed: bool,
}

impl CommandContext {
    /// The optional egress proxy in runtime form, mapped from `config.proxy`.
    /// Every HTTP client a command builds threads this through.
    pub fn proxy_settings(&self) -> Option<baybo_security::http::ProxySettings> {
        self.config
            .proxy
            .as_ref()
            .map(|p| baybo_security::http::ProxySettings {
                url: p.url.clone(),
                no_proxy: p.no_proxy.clone(),
            })
    }

    pub fn with_format(mut self, format: OutputFormat) -> Self {
        self.format = format;
        self
    }

    pub fn with_invocation(mut self, invocation: Invocation) -> Self {
        self.invocation = invocation;
        self
    }

    pub fn with_confirmed(mut self, confirmed: bool) -> Self {
        self.confirmed = confirmed;
        self
    }
}

/// Builder for `CommandContext`.
///
/// `crates/baybo/src/main.rs` populates the builder after bootstrapping the domain graph;
/// each field corresponds to an `Arc` already held by the main process.
pub struct ContextBuilder {
    config: Arc<BayboConfig>,
    config_path: Option<PathBuf>,
    skills: Option<Arc<SkillRegistry>>,
    tools: Option<Arc<ToolRegistry>>,
    channels: Option<Arc<ChannelRegistry>>,
    llm: Option<Arc<BillableLlm>>,
    workspace: Option<Arc<WorkspacePaths>>,
    session: Option<Arc<SessionManager>>,
    turn: Option<Arc<TurnLifecycle>>,
    cron: Option<Arc<CronScheduler>>,
    trace: Option<Arc<dyn TraceStore>>,
    cost_store: Option<Arc<dyn CostStore>>,
    security: Option<Arc<SecurityGateway>>,
    leak_detector: Option<Arc<LeakDetector>>,
    skill_assessor: Option<Arc<SkillAssessor>>,
    channel_bot_store: Option<Arc<dyn ChannelBotStore>>,
    channel_pairing_store: Option<Arc<dyn ChannelPairingStore>>,
    device_pairing_service: Option<Arc<DevicePairingService>>,
    secret_vault: Option<Arc<SecretVault>>,
}

impl ContextBuilder {
    pub fn new(config: Arc<BayboConfig>) -> Self {
        Self {
            config,
            config_path: None,
            skills: None,
            tools: None,
            channels: None,
            llm: None,
            workspace: None,
            session: None,
            turn: None,
            cron: None,
            trace: None,
            cost_store: None,
            security: None,
            leak_detector: None,
            skill_assessor: None,
            channel_bot_store: None,
            channel_pairing_store: None,
            device_pairing_service: None,
            secret_vault: None,
        }
    }

    pub fn config_path(mut self, path: Option<PathBuf>) -> Self {
        self.config_path = path;
        self
    }

    pub fn skills(mut self, skills: Arc<SkillRegistry>) -> Self {
        self.skills = Some(skills);
        self
    }

    pub fn tools(mut self, tools: Arc<ToolRegistry>) -> Self {
        self.tools = Some(tools);
        self
    }

    pub fn channels(mut self, channels: Arc<ChannelRegistry>) -> Self {
        self.channels = Some(channels);
        self
    }

    pub fn llm(mut self, llm: Arc<BillableLlm>) -> Self {
        self.llm = Some(llm);
        self
    }

    pub fn workspace(mut self, workspace: Arc<WorkspacePaths>) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn session(mut self, session: Arc<SessionManager>) -> Self {
        self.session = Some(session);
        self
    }

    pub fn turn(mut self, turn: Arc<TurnLifecycle>) -> Self {
        self.turn = Some(turn);
        self
    }

    pub fn cron(mut self, cron: Arc<CronScheduler>) -> Self {
        self.cron = Some(cron);
        self
    }

    pub fn trace(mut self, trace: Arc<dyn TraceStore>) -> Self {
        self.trace = Some(trace);
        self
    }

    /// Optional. When set alongside `session`/`turn`/`trace`, the
    /// auto-derived `QueryApi` is built via `QueryApi::new` (cost
    /// queries supported); otherwise `QueryApi::without_costs` is used
    /// and `cost_summary` returns `Unsupported`.
    pub fn cost_store(mut self, cost_store: Arc<dyn CostStore>) -> Self {
        self.cost_store = Some(cost_store);
        self
    }

    pub fn security(mut self, security: Arc<SecurityGateway>) -> Self {
        self.security = Some(security);
        self
    }

    pub fn leak_detector(mut self, detector: Arc<LeakDetector>) -> Self {
        self.leak_detector = Some(detector);
        self
    }

    pub fn skill_assessor(mut self, assessor: Arc<SkillAssessor>) -> Self {
        self.skill_assessor = Some(assessor);
        self
    }

    pub fn channel_bot_store(mut self, store: Arc<dyn ChannelBotStore>) -> Self {
        self.channel_bot_store = Some(store);
        self
    }

    pub fn channel_pairing_store(mut self, store: Arc<dyn ChannelPairingStore>) -> Self {
        self.channel_pairing_store = Some(store);
        self
    }

    pub fn device_pairing_service(mut self, service: Arc<DevicePairingService>) -> Self {
        self.device_pairing_service = Some(service);
        self
    }

    pub fn secret_vault(mut self, vault: Arc<SecretVault>) -> Self {
        self.secret_vault = Some(vault);
        self
    }

    pub fn build(self) -> CommandContext {
        let query_api = match (&self.session, &self.turn, &self.trace) {
            (Some(s), Some(j), Some(t)) => Some(Arc::new(match &self.cost_store {
                Some(c) => QueryApi::new(s.store(), Arc::clone(j), Arc::clone(t), Arc::clone(c)),
                None => QueryApi::without_costs(s.store(), Arc::clone(j), Arc::clone(t)),
            })),
            _ => None,
        };
        CommandContext {
            config: self.config,
            config_path: self.config_path,
            skills: self
                .skills
                .unwrap_or_else(|| Arc::new(SkillRegistry::new())),
            tools: self.tools.unwrap_or_else(|| Arc::new(ToolRegistry::new())),
            channels: self
                .channels
                .unwrap_or_else(|| Arc::new(ChannelRegistry::new())),
            llm: self.llm,
            workspace: self
                .workspace
                .unwrap_or_else(|| Arc::new(WorkspacePaths::new(PathBuf::from(".")))),
            session: self.session,
            turn: self.turn,
            cron: self.cron,
            trace: self.trace,
            query_api,
            security: self.security,
            leak_detector: self.leak_detector,
            skill_assessor: self.skill_assessor,
            channel_bot_store: self.channel_bot_store,
            channel_pairing_store: self.channel_pairing_store,
            device_pairing_service: self.device_pairing_service,
            secret_vault: self.secret_vault,
            format: OutputFormat::Human,
            invocation: Invocation::Argv,
            confirmed: false,
        }
    }
}
