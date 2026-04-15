use std::path::PathBuf;
use std::sync::Arc;

use aura_agent::{
    CronScheduler, JobManager, MemoryManager, ObservabilityRecorder, SecurityGateway,
    SessionManager, ToolExecutor,
};
use aura_channels::ChannelRegistry;
use aura_config::AuraConfig;
use aura_llm::LlmClient;
use aura_security::LeakDetector;
use aura_skills::SkillRegistry;
use aura_skills_assessor::SkillAssessor;
use aura_storage::TraceStore;
use aura_tools::ToolRegistry;
use aura_workspace::WorkspaceManager;
use tokio::sync::RwLock;

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
    pub tools: Arc<ToolRegistry>,
    pub channels: Arc<RwLock<ChannelRegistry>>,
    pub llm: Option<Arc<LlmClient>>,
    pub workspace: Arc<WorkspaceManager>,
    pub session: Option<Arc<SessionManager>>,
    pub job: Option<Arc<JobManager>>,
    pub cron: Option<Arc<CronScheduler>>,
    pub memory: Option<Arc<MemoryManager>>,
    pub trace: Option<Arc<dyn TraceStore>>,
    pub tool_executor: Option<Arc<ToolExecutor>>,
    pub recorder: Option<Arc<ObservabilityRecorder>>,
    pub security: Option<Arc<SecurityGateway>>,
    pub leak_detector: Option<Arc<LeakDetector>>,
    pub skill_assessor: Option<Arc<SkillAssessor>>,
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
    tools: Option<Arc<ToolRegistry>>,
    channels: Option<Arc<RwLock<ChannelRegistry>>>,
    llm: Option<Arc<LlmClient>>,
    workspace: Option<Arc<WorkspaceManager>>,
    session: Option<Arc<SessionManager>>,
    job: Option<Arc<JobManager>>,
    cron: Option<Arc<CronScheduler>>,
    memory: Option<Arc<MemoryManager>>,
    trace: Option<Arc<dyn TraceStore>>,
    tool_executor: Option<Arc<ToolExecutor>>,
    recorder: Option<Arc<ObservabilityRecorder>>,
    security: Option<Arc<SecurityGateway>>,
    leak_detector: Option<Arc<LeakDetector>>,
    skill_assessor: Option<Arc<SkillAssessor>>,
}

impl ContextBuilder {
    pub fn new(config: Arc<AuraConfig>) -> Self {
        Self {
            config,
            config_path: None,
            skills: None,
            tools: None,
            channels: None,
            llm: None,
            workspace: None,
            session: None,
            job: None,
            cron: None,
            memory: None,
            trace: None,
            tool_executor: None,
            recorder: None,
            security: None,
            leak_detector: None,
            skill_assessor: None,
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

    pub fn channels(mut self, channels: Arc<RwLock<ChannelRegistry>>) -> Self {
        self.channels = Some(channels);
        self
    }

    pub fn llm(mut self, llm: Arc<LlmClient>) -> Self {
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

    pub fn job(mut self, job: Arc<JobManager>) -> Self {
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

    pub fn tool_executor(mut self, exec: Arc<ToolExecutor>) -> Self {
        self.tool_executor = Some(exec);
        self
    }

    pub fn recorder(mut self, recorder: Arc<ObservabilityRecorder>) -> Self {
        self.recorder = Some(recorder);
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

    pub fn build(self) -> CommandContext {
        CommandContext {
            config: self.config,
            config_path: self.config_path,
            skills: self
                .skills
                .unwrap_or_else(|| Arc::new(SkillRegistry::new())),
            tools: self.tools.unwrap_or_else(|| Arc::new(ToolRegistry::new())),
            channels: self
                .channels
                .unwrap_or_else(|| Arc::new(RwLock::new(ChannelRegistry::new()))),
            llm: self.llm,
            workspace: self
                .workspace
                .unwrap_or_else(|| Arc::new(WorkspaceManager::new(PathBuf::from(".")))),
            session: self.session,
            job: self.job,
            cron: self.cron,
            memory: self.memory,
            trace: self.trace,
            tool_executor: self.tool_executor,
            recorder: self.recorder,
            security: self.security,
            leak_detector: self.leak_detector,
            skill_assessor: self.skill_assessor,
            format: OutputFormat::Human,
            invocation: Invocation::Argv,
            confirmed: false,
        }
    }
}
