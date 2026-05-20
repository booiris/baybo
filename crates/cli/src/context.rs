use std::path::PathBuf;
use std::sync::Arc;

use aura_agent::{CronScheduler, SecurityGateway, SessionManager};
use aura_channels::ChannelRegistry;
use aura_config::AuraConfig;
use aura_cost::CostStore;
use aura_job::JobLifecycle;
use aura_llm::GuardedLlm;
use aura_memory::MemoryManager;
use aura_query::QueryApi;
use aura_security::{LeakDetector, SecretVault};
use aura_skills::SkillRegistry;
use aura_skills_assessor::SkillAssessor;
use aura_storage::{ChannelBotStore, ChannelPairingStore};
use aura_subagent::SubagentRegistry;
use aura_tools::{SubagentDispatchLimiter, ToolRegistry};
use aura_trace::TraceStore;
use aura_workspace::WorkspaceManager;

use crate::format::OutputFormat;

/// Where a command was invoked from.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// Invoked via `aura <cmd>` at the shell.
    Argv,
    /// Invoked via `/<cmd>` in a chat channel.
    Slash,
}

/// Shared runtime handles available to every command.
///
/// Constructed once at startup and reused for both argv and slash dispatch.
/// All fields are `Arc` so the context itself is cheap to clone.
pub struct CommandContext {
    pub config: Arc<AuraConfig>,
    pub config_path: Option<PathBuf>,
    pub skills: Arc<SkillRegistry>,
    /// Catalogue of typed subagent profiles. Powers `aura agents list /
    /// info / search`. Always populated; defaults to an empty registry
    /// when the caller didn't wire it.
    pub subagent_profiles: Arc<SubagentRegistry>,
    /// Live fan-out limiter. Powers `aura agents in-flight`. `None`
    /// for one-shot argv contexts that bypass the runtime (the spawn
    /// path isn't reachable from those anyway).
    pub subagent_dispatch_limiter: Option<Arc<dyn SubagentDispatchLimiter>>,
    pub tools: Arc<ToolRegistry>,
    pub channels: Arc<ChannelRegistry>,
    pub llm: Option<Arc<GuardedLlm>>,
    pub workspace: Arc<WorkspaceManager>,
    pub session: Option<Arc<SessionManager>>,
    pub job: Option<Arc<JobLifecycle>>,
    pub cron: Option<Arc<CronScheduler>>,
    pub memory: Option<Arc<MemoryManager>>,
    pub trace: Option<Arc<dyn TraceStore>>,
    /// Pre-built `QueryApi` so trace / job / session commands don't
    /// allocate one per invocation. `None` when the context lacks any
    /// of session / job / trace (e.g. argv commands that don't touch
    /// the trace surface — `aura skills info`).
    pub query_api: Option<Arc<QueryApi>>,
    pub security: Option<Arc<SecurityGateway>>,
    pub leak_detector: Option<Arc<LeakDetector>>,
    pub skill_assessor: Option<Arc<SkillAssessor>>,
    /// Per-tenant credential metadata. Populated for one-shot argv
    /// commands that need to mutate or read the roster (`aura
    /// channel list/add/remove …`); `None` during TUI / slash dispatch so those
    /// paths can't accidentally rotate tokens.
    pub channel_bot_store: Option<Arc<dyn ChannelBotStore>>,
    /// Per-user pairing gate store. Populated alongside
    /// `channel_bot_store` for one-shot CLI commands that drive
    /// `aura pair {list,approve,revoke}`.
    pub channel_pairing_store: Option<Arc<dyn ChannelPairingStore>>,
    /// Shared vault — populated for the same subset of commands as
    /// `channel_bot_store`. Used to read/write bot tokens keyed as
    /// `channel.<channel_type>.bot.<bot_id>.token`.
    pub secret_vault: Option<Arc<SecretVault>>,
    pub format: OutputFormat,
    pub invocation: Invocation,
    pub confirmed: bool,
}

impl CommandContext {
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
/// `src/main.rs` populates the builder after bootstrapping the domain graph;
/// each field corresponds to an `Arc` already held by the main process.
pub struct ContextBuilder {
    config: Arc<AuraConfig>,
    config_path: Option<PathBuf>,
    skills: Option<Arc<SkillRegistry>>,
    subagent_profiles: Option<Arc<SubagentRegistry>>,
    subagent_dispatch_limiter: Option<Arc<dyn SubagentDispatchLimiter>>,
    tools: Option<Arc<ToolRegistry>>,
    channels: Option<Arc<ChannelRegistry>>,
    llm: Option<Arc<GuardedLlm>>,
    workspace: Option<Arc<WorkspaceManager>>,
    session: Option<Arc<SessionManager>>,
    job: Option<Arc<JobLifecycle>>,
    cron: Option<Arc<CronScheduler>>,
    memory: Option<Arc<MemoryManager>>,
    trace: Option<Arc<dyn TraceStore>>,
    cost_store: Option<Arc<dyn CostStore>>,
    security: Option<Arc<SecurityGateway>>,
    leak_detector: Option<Arc<LeakDetector>>,
    skill_assessor: Option<Arc<SkillAssessor>>,
    channel_bot_store: Option<Arc<dyn ChannelBotStore>>,
    channel_pairing_store: Option<Arc<dyn ChannelPairingStore>>,
    secret_vault: Option<Arc<SecretVault>>,
}

impl ContextBuilder {
    pub fn new(config: Arc<AuraConfig>) -> Self {
        Self {
            config,
            config_path: None,
            skills: None,
            subagent_profiles: None,
            subagent_dispatch_limiter: None,
            tools: None,
            channels: None,
            llm: None,
            workspace: None,
            session: None,
            job: None,
            cron: None,
            memory: None,
            trace: None,
            cost_store: None,
            security: None,
            leak_detector: None,
            skill_assessor: None,
            channel_bot_store: None,
            channel_pairing_store: None,
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

    pub fn subagent_profiles(mut self, profiles: Arc<SubagentRegistry>) -> Self {
        self.subagent_profiles = Some(profiles);
        self
    }

    pub fn subagent_dispatch_limiter(
        mut self,
        limiter: Arc<dyn SubagentDispatchLimiter>,
    ) -> Self {
        self.subagent_dispatch_limiter = Some(limiter);
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

    pub fn llm(mut self, llm: Arc<GuardedLlm>) -> Self {
        self.llm = Some(llm);
        self
    }

    pub fn workspace(mut self, workspace: Arc<WorkspaceManager>) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn session(mut self, session: Arc<SessionManager>) -> Self {
        self.session = Some(session);
        self
    }

    pub fn job(mut self, job: Arc<JobLifecycle>) -> Self {
        self.job = Some(job);
        self
    }

    pub fn cron(mut self, cron: Arc<CronScheduler>) -> Self {
        self.cron = Some(cron);
        self
    }

    pub fn memory(mut self, memory: Arc<MemoryManager>) -> Self {
        self.memory = Some(memory);
        self
    }

    pub fn trace(mut self, trace: Arc<dyn TraceStore>) -> Self {
        self.trace = Some(trace);
        self
    }

    /// Optional. When set alongside `session`/`job`/`trace`, the
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

    pub fn secret_vault(mut self, vault: Arc<SecretVault>) -> Self {
        self.secret_vault = Some(vault);
        self
    }

    pub fn build(self) -> CommandContext {
        let query_api = match (&self.session, &self.job, &self.trace) {
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
            subagent_profiles: self
                .subagent_profiles
                .unwrap_or_else(|| Arc::new(SubagentRegistry::new())),
            subagent_dispatch_limiter: self.subagent_dispatch_limiter,
            tools: self.tools.unwrap_or_else(|| Arc::new(ToolRegistry::new())),
            channels: self
                .channels
                .unwrap_or_else(|| Arc::new(ChannelRegistry::new())),
            llm: self.llm,
            workspace: self
                .workspace
                .unwrap_or_else(|| Arc::new(WorkspaceManager::new(PathBuf::from(".")))),
            session: self.session,
            job: self.job,
            cron: self.cron,
            memory: self.memory,
            trace: self.trace,
            query_api,
            security: self.security,
            leak_detector: self.leak_detector,
            skill_assessor: self.skill_assessor,
            channel_bot_store: self.channel_bot_store,
            channel_pairing_store: self.channel_pairing_store,
            secret_vault: self.secret_vault,
            format: OutputFormat::Human,
            invocation: Invocation::Argv,
            confirmed: false,
        }
    }
}
