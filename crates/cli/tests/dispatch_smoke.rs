//! Smoke-tests for the dispatcher.
//!
//! Each test builds a `CommandContext` from in-memory defaults and runs a
//! read-only command end-to-end. These guard against regressions in the
//! wiring between clap, `dispatch::run`, and individual command handlers,
//! without spinning up the full bootstrap.

use std::path::PathBuf;
use std::sync::Arc;

use aura_agent::{CronScheduler, JobManager, MemoryManager, SessionManager};
use aura_channels::ChannelRegistry;
use aura_cli::cli::{
    AgentCmd, ChannelsCmd, Commands, ConfigCmd, CronCmd, JobCmd, JobStatusArg, LlmCmd, MemoryCmd,
    SessionCmd, SkillsCmd, TraceCmd, WorkspaceCmd,
};
use aura_cli::{ContextBuilder, Invocation, OutputFormat, dispatch};
use aura_config::AuraConfig;
use aura_cron::{CronSchedule, TriggerAction};
use aura_job::{Job, JobError, JobStatus, JobTransition, OperationKind};
use aura_model::{ChannelType, MemoryCategory, MemoryEntry, Session, SessionState, User};
use aura_skills::SkillRegistry;
use aura_storage::{
    CronExecutionRow, CronJobRow, CronStore, CronStoreError, JobStore, MemoryStore, SessionStore,
    TraceStore, cron::Result as CronResult, memory::Result as MemoryResult,
    session::Result as SessionResult, trace::Result as TraceResult,
};
use aura_tools::ToolRegistry;
use aura_trace::{
    ExecutionProvenance, SessionTrace, SpanInput, TraceFilter, TraceNode, TraceNodeId, TraceSpan,
};
use aura_workspace::WorkspaceManager;
use chrono::{DateTime, Duration, Utc};
use parking_lot::Mutex;
use std::collections::HashMap;
use tokio::sync::mpsc;

fn context() -> aura_cli::CommandContext {
    let config = Arc::new(AuraConfig::default());
    ContextBuilder::new(config)
        .skills(Arc::new(SkillRegistry::new()))
        .tools(Arc::new(ToolRegistry::new()))
        .channels(Arc::new(ChannelRegistry::new()))
        .workspace(Arc::new(WorkspaceManager::new(PathBuf::from("."))))
        .build()
        .with_invocation(Invocation::Argv)
        .with_format(OutputFormat::Plain)
}

struct MemorySessionStore {
    data: Mutex<HashMap<String, Session>>,
}

impl MemorySessionStore {
    fn new() -> Self {
        Self {
            data: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl SessionStore for MemorySessionStore {
    async fn get(&self, session_id: &str) -> SessionResult<Option<Session>> {
        Ok(self.data.lock().get(session_id).cloned())
    }

    async fn save(&self, session: &Session) -> SessionResult<()> {
        self.data.lock().insert(session.id.clone(), session.clone());
        Ok(())
    }

    async fn delete(&self, session_id: &str) -> SessionResult<()> {
        self.data.lock().remove(session_id);
        Ok(())
    }

    async fn list_expired(&self, before: DateTime<Utc>) -> SessionResult<Vec<String>> {
        Ok(self
            .data
            .lock()
            .values()
            .filter(|s| s.last_active < before)
            .map(|s| s.id.clone())
            .collect())
    }

    async fn list_all(&self) -> SessionResult<Vec<Session>> {
        Ok(self.data.lock().values().cloned().collect())
    }
}

fn seeded_session_manager(ids: &[&str]) -> (Arc<SessionManager>, Vec<String>) {
    let store = Arc::new(MemorySessionStore::new());
    let mut populated = Vec::with_capacity(ids.len());
    for id in ids {
        let session = Session {
            id: (*id).to_string(),
            user: User {
                id: "user-1".to_string(),
                name: Some("Alice".to_string()),
                channel: ChannelType::tui(),
            },
            channel: ChannelType::tui(),
            messages: vec![],
            created_at: Utc::now(),
            last_active: Utc::now(),
            state: SessionState::default(),
        };
        store
            .data
            .lock()
            .insert(session.id.clone(), session.clone());
        populated.push(session.id);
    }
    let mgr = SessionManager::new(store, Duration::minutes(30));
    (Arc::new(mgr), populated)
}

fn context_with_sessions(ids: &[&str]) -> (aura_cli::CommandContext, Vec<String>) {
    let (mgr, populated) = seeded_session_manager(ids);
    let ctx = ContextBuilder::new(Arc::new(AuraConfig::default()))
        .skills(Arc::new(SkillRegistry::new()))
        .tools(Arc::new(ToolRegistry::new()))
        .channels(Arc::new(ChannelRegistry::new()))
        .workspace(Arc::new(WorkspaceManager::new(PathBuf::from("."))))
        .session(mgr)
        .build()
        .with_invocation(Invocation::Argv)
        .with_format(OutputFormat::Plain);
    (ctx, populated)
}

struct MemoryJobStore {
    jobs: Mutex<Vec<Job>>,
    transitions: Mutex<Vec<JobTransition>>,
}

impl MemoryJobStore {
    fn new() -> Self {
        Self {
            jobs: Mutex::new(Vec::new()),
            transitions: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl JobStore for MemoryJobStore {
    async fn create(&self, job: &Job) -> Result<(), JobError> {
        self.jobs.lock().push(job.clone());
        Ok(())
    }

    async fn get(&self, job_id: &str) -> Result<Option<Job>, JobError> {
        Ok(self.jobs.lock().iter().find(|j| j.id == job_id).cloned())
    }

    async fn save(&self, job: &Job) -> Result<(), JobError> {
        let mut jobs = self.jobs.lock();
        let stored = jobs
            .iter_mut()
            .find(|j| j.id == job.id)
            .ok_or_else(|| JobError::NotFound(format!("job {}", job.id)))?;
        *stored = job.clone();
        Ok(())
    }

    async fn list_by_session(&self, session_id: &str) -> Result<Vec<Job>, JobError> {
        Ok(self
            .jobs
            .lock()
            .iter()
            .filter(|j| j.session_id == session_id)
            .cloned()
            .collect())
    }

    async fn list_by_status(&self, status: JobStatus) -> Result<Vec<Job>, JobError> {
        Ok(self
            .jobs
            .lock()
            .iter()
            .filter(|j| j.status == status)
            .cloned()
            .collect())
    }

    async fn list_children(&self, parent_job_id: &str) -> Result<Vec<Job>, JobError> {
        Ok(self
            .jobs
            .lock()
            .iter()
            .filter(|j| j.parent_job_id.as_deref() == Some(parent_job_id))
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> Result<Vec<Job>, JobError> {
        Ok(self.jobs.lock().to_vec())
    }

    async fn record_transition(&self, t: &JobTransition) -> Result<(), JobError> {
        self.transitions.lock().push(t.clone());
        Ok(())
    }

    async fn get_transitions(&self, job_id: &str) -> Result<Vec<JobTransition>, JobError> {
        Ok(self
            .transitions
            .lock()
            .iter()
            .filter(|t| t.job_id == job_id)
            .cloned()
            .collect())
    }
}

fn test_kind() -> OperationKind {
    OperationKind::LlmCall {
        model: "test-model".into(),
    }
}

async fn seeded_job_manager() -> (Arc<JobManager>, Vec<(String, JobStatus)>) {
    let mgr = JobManager::new(Arc::new(MemoryJobStore::new()));
    // Pending job
    let pending = mgr.create_job("s1", test_kind(), None).await.unwrap();
    // In-progress job
    let in_progress = mgr.create_job("s1", test_kind(), None).await.unwrap();
    mgr.start(&in_progress.id).await.unwrap();
    // Failed (terminal) job
    let failed = mgr.create_job("s2", test_kind(), None).await.unwrap();
    mgr.start(&failed.id).await.unwrap();
    mgr.fail(&failed.id, "boom").await.unwrap();

    let seeded = vec![
        (pending.id, JobStatus::Pending),
        (in_progress.id, JobStatus::InProgress),
        (failed.id, JobStatus::Failed),
    ];
    (Arc::new(mgr), seeded)
}

async fn context_with_jobs() -> (aura_cli::CommandContext, Vec<(String, JobStatus)>) {
    let (mgr, seeded) = seeded_job_manager().await;
    let ctx = ContextBuilder::new(Arc::new(AuraConfig::default()))
        .skills(Arc::new(SkillRegistry::new()))
        .tools(Arc::new(ToolRegistry::new()))
        .channels(Arc::new(ChannelRegistry::new()))
        .workspace(Arc::new(WorkspaceManager::new(PathBuf::from("."))))
        .job(mgr)
        .build()
        .with_invocation(Invocation::Argv)
        .with_format(OutputFormat::Plain);
    (ctx, seeded)
}

struct MemoryCronStore {
    jobs: Mutex<Vec<CronJobRow>>,
    executions: Mutex<Vec<CronExecutionRow>>,
}

impl MemoryCronStore {
    fn new() -> Self {
        Self {
            jobs: Mutex::new(Vec::new()),
            executions: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl CronStore for MemoryCronStore {
    async fn create(&self, row: &CronJobRow) -> CronResult<()> {
        self.jobs.lock().push(row.clone());
        Ok(())
    }

    async fn get(&self, id: &str) -> CronResult<Option<CronJobRow>> {
        Ok(self.jobs.lock().iter().find(|r| r.id == id).cloned())
    }

    async fn save(&self, row: &CronJobRow) -> CronResult<()> {
        let mut jobs = self.jobs.lock();
        if let Some(existing) = jobs.iter_mut().find(|r| r.id == row.id) {
            *existing = row.clone();
            Ok(())
        } else {
            Err(CronStoreError::NotFound(row.id.clone()))
        }
    }

    async fn delete(&self, id: &str) -> CronResult<()> {
        let mut jobs = self.jobs.lock();
        let before = jobs.len();
        jobs.retain(|r| r.id != id);
        if jobs.len() == before {
            Err(CronStoreError::NotFound(id.into()))
        } else {
            Ok(())
        }
    }

    async fn list_by_user(&self, user: &str) -> CronResult<Vec<CronJobRow>> {
        Ok(self
            .jobs
            .lock()
            .iter()
            .filter(|r| r.user_id == user)
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> CronResult<Vec<CronJobRow>> {
        Ok(self.jobs.lock().to_vec())
    }

    async fn list_enabled(&self) -> CronResult<Vec<CronJobRow>> {
        Ok(self
            .jobs
            .lock()
            .iter()
            .filter(|r| r.status == "enabled")
            .cloned()
            .collect())
    }

    async fn list_due(&self, now: &str) -> CronResult<Vec<CronJobRow>> {
        Ok(self
            .jobs
            .lock()
            .iter()
            .filter(|r| {
                r.status == "enabled"
                    && !r.next_trigger_at.is_empty()
                    && r.next_trigger_at.as_str() <= now
            })
            .cloned()
            .collect())
    }

    async fn record_execution(&self, row: &CronExecutionRow) -> CronResult<()> {
        self.executions.lock().push(row.clone());
        Ok(())
    }

    async fn list_executions_by_job(&self, job_id: &str) -> CronResult<Vec<CronExecutionRow>> {
        Ok(self
            .executions
            .lock()
            .iter()
            .filter(|r| r.job_id == job_id)
            .cloned()
            .collect())
    }

    async fn list_executions_by_user(&self, user: &str) -> CronResult<Vec<CronExecutionRow>> {
        Ok(self
            .executions
            .lock()
            .iter()
            .filter(|r| r.user_id == user)
            .cloned()
            .collect())
    }

    async fn has_execution_for_schedule(
        &self,
        job_id: &str,
        scheduled_fire_time: &str,
    ) -> CronResult<bool> {
        Ok(self
            .executions
            .lock()
            .iter()
            .any(|r| r.job_id == job_id && r.scheduled_fire_time == scheduled_fire_time))
    }

    async fn update_execution_status(&self, id: &str, status: &str) -> CronResult<()> {
        let mut execs = self.executions.lock();
        if let Some(e) = execs.iter_mut().find(|r| r.id == id) {
            e.status = status.into();
            Ok(())
        } else {
            Err(CronStoreError::NotFound(id.into()))
        }
    }

    async fn list_executions_by_status(&self, status: &str) -> CronResult<Vec<CronExecutionRow>> {
        Ok(self
            .executions
            .lock()
            .iter()
            .filter(|r| r.status == status)
            .cloned()
            .collect())
    }
}

/// Build a cron scheduler wired to an in-memory store. Returns the scheduler
/// and the receiver half of its trigger channel (kept alive so `send` does
/// not fail with a closed channel).
fn make_cron_scheduler() -> (
    Arc<CronScheduler>,
    mpsc::Receiver<aura_agent::CronTriggerEvent>,
) {
    let (tx, rx) = mpsc::channel(16);
    let scheduler = CronScheduler::new(
        Arc::new(MemoryCronStore::new()),
        tx,
        Arc::new(aura_cron::NeverShutdown),
    );
    (Arc::new(scheduler), rx)
}

async fn context_with_cron() -> (
    aura_cli::CommandContext,
    mpsc::Receiver<aura_agent::CronTriggerEvent>,
    Arc<CronScheduler>,
) {
    let (sched, rx) = make_cron_scheduler();
    let ctx = ContextBuilder::new(Arc::new(AuraConfig::default()))
        .skills(Arc::new(SkillRegistry::new()))
        .tools(Arc::new(ToolRegistry::new()))
        .channels(Arc::new(ChannelRegistry::new()))
        .workspace(Arc::new(WorkspaceManager::new(PathBuf::from("."))))
        .cron(Arc::clone(&sched))
        .build()
        .with_invocation(Invocation::Argv)
        .with_format(OutputFormat::Plain);
    (ctx, rx, sched)
}

#[tokio::test]
async fn config_show_emits_json_shaped_payload() {
    let ctx = context();
    let out = dispatch::run(
        &ctx,
        Commands::Config {
            cmd: ConfigCmd::Show { section: None },
        },
    )
    .await
    .expect("config show");
    let data = out.data.expect("structured payload");
    assert!(data.get("llm").is_some(), "llm section should be present");
    assert!(data.get("agent").is_some());
}

#[tokio::test]
async fn config_show_unknown_section_errors() {
    let ctx = context();
    let err = dispatch::run(
        &ctx,
        Commands::Config {
            cmd: ConfigCmd::Show {
                section: Some("nope".into()),
            },
        },
    )
    .await
    .expect_err("unknown section should error");
    assert!(format!("{err}").contains("nope"));
}

#[tokio::test]
async fn config_schema_returns_default_shape() {
    let ctx = context();
    let out = dispatch::run(
        &ctx,
        Commands::Config {
            cmd: ConfigCmd::Schema,
        },
    )
    .await
    .expect("config schema");
    assert!(out.data.is_some());
    assert!(!out.human.is_empty());
}

#[tokio::test]
async fn config_file_reports_no_config_when_unset() {
    let ctx = context();
    // SAFETY: test environment; removing the var is scoped to this process.
    unsafe {
        std::env::remove_var("AURA_CONFIG_PATH");
    }
    let out = dispatch::run(
        &ctx,
        Commands::Config {
            cmd: ConfigCmd::File,
        },
    )
    .await
    .expect("config file");
    let data = out.data.expect("structured payload");
    assert_eq!(data["resolved"], false);
}

#[tokio::test]
async fn skills_list_on_empty_registry_returns_placeholder() {
    let ctx = context();
    let out = dispatch::run(
        &ctx,
        Commands::Skills {
            cmd: SkillsCmd::List,
        },
    )
    .await
    .expect("skills list");
    assert!(out.human.contains("no skills"));
}

#[tokio::test]
async fn channels_list_on_empty_registry_returns_placeholder() {
    let ctx = context();
    let out = dispatch::run(
        &ctx,
        Commands::Channels {
            cmd: ChannelsCmd::List,
        },
    )
    .await
    .expect("channels list");
    assert!(out.human.contains("no channels"));
}

#[tokio::test]
async fn llm_status_errors_when_client_missing() {
    let ctx = context();
    let err = dispatch::run(
        &ctx,
        Commands::Llm {
            cmd: LlmCmd::Status,
        },
    )
    .await
    .expect_err("llm status without client should fail");
    assert!(format!("{err}").to_lowercase().contains("llm"));
}

fn tmp_workspace_dir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("aura-cli-workspace-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn context_with_workspace(root: PathBuf) -> aura_cli::CommandContext {
    ContextBuilder::new(Arc::new(AuraConfig::default()))
        .skills(Arc::new(SkillRegistry::new()))
        .tools(Arc::new(ToolRegistry::new()))
        .channels(Arc::new(ChannelRegistry::new()))
        .workspace(Arc::new(WorkspaceManager::new(root)))
        .build()
        .with_invocation(Invocation::Argv)
        .with_format(OutputFormat::Plain)
}

#[tokio::test]
async fn workspace_set_identity_slash_requires_yes() {
    let dir = tmp_workspace_dir("slash-yes");
    let ctx = context_with_workspace(dir.clone()).with_invocation(Invocation::Slash);
    let err = dispatch::run(
        &ctx,
        Commands::Workspace {
            cmd: WorkspaceCmd::SetIdentity {
                name: "soul".into(),
                file: None,
                content: Some("hi".into()),
                yes: false,
            },
        },
    )
    .await
    .expect_err("slash without --yes");
    assert!(err.to_string().contains("--yes"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn workspace_set_identity_rejects_unknown_name() {
    let dir = tmp_workspace_dir("unknown");
    let ctx = context_with_workspace(dir.clone());
    let err = dispatch::run(
        &ctx,
        Commands::Workspace {
            cmd: WorkspaceCmd::SetIdentity {
                name: "claude".into(),
                file: None,
                content: Some("hi".into()),
                yes: false,
            },
        },
    )
    .await
    .expect_err("unknown identity name");
    assert!(err.to_string().contains("unknown identity"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn workspace_set_identity_writes_file_in_argv_mode() {
    let dir = tmp_workspace_dir("argv-write");
    let ctx = context_with_workspace(dir.clone());
    let out = dispatch::run(
        &ctx,
        Commands::Workspace {
            cmd: WorkspaceCmd::SetIdentity {
                name: "soul".into(),
                file: None,
                content: Some("You are helpful.".into()),
                yes: false,
            },
        },
    )
    .await
    .expect("set-identity soul");

    let data = out.data.expect("payload");
    assert_eq!(data.get("kind").and_then(|v| v.as_str()), Some("SOUL.md"));
    let on_disk = std::fs::read_to_string(dir.join("SOUL.md")).unwrap();
    assert_eq!(on_disk, "You are helpful.");
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn workspace_set_identity_requires_file_or_content() {
    let dir = tmp_workspace_dir("missing-source");
    let ctx = context_with_workspace(dir.clone());
    let err = dispatch::run(
        &ctx,
        Commands::Workspace {
            cmd: WorkspaceCmd::SetIdentity {
                name: "soul".into(),
                file: None,
                content: None,
                yes: false,
            },
        },
    )
    .await
    .expect_err("no source");
    assert!(err.to_string().contains("--file") || err.to_string().contains("--content"));
    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn workspace_show_reports_identity_flags() {
    let ctx = context();
    let out = dispatch::run(
        &ctx,
        Commands::Workspace {
            cmd: WorkspaceCmd::Show,
        },
    )
    .await
    .expect("workspace show");
    let data = out.data.expect("structured payload");
    let files = data.get("identity_files").expect("identity_files key");
    for key in ["agents", "soul", "user", "identity"] {
        assert!(files.get(key).is_some(), "missing identity flag {key}");
    }
}

#[tokio::test]
async fn status_reports_zero_counts_on_defaults() {
    let ctx = context();
    let out = dispatch::run(&ctx, Commands::Status).await.expect("status");
    let data = out.data.expect("structured payload");
    assert_eq!(data["skills"], 0);
    assert_eq!(data["tools"], 0);
    assert_eq!(data["channels"], 0);
}

#[tokio::test]
async fn session_list_reports_empty_when_no_sessions() {
    let (ctx, _) = context_with_sessions(&[]);
    let out = dispatch::run(
        &ctx,
        Commands::Session {
            cmd: SessionCmd::List,
        },
    )
    .await
    .expect("session list");
    assert!(out.human.contains("no sessions"));
    let data = out.data.expect("structured payload");
    assert!(data["sessions"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn session_list_returns_populated_sessions() {
    let (ctx, ids) = context_with_sessions(&["sid-a", "sid-b"]);
    let out = dispatch::run(
        &ctx,
        Commands::Session {
            cmd: SessionCmd::List,
        },
    )
    .await
    .expect("session list");
    let data = out.data.expect("structured payload");
    let listed = data["sessions"].as_array().unwrap();
    assert_eq!(listed.len(), 2);
    for id in &ids {
        assert!(listed.iter().any(|s| s["id"] == id.as_str()));
    }
}

#[tokio::test]
async fn session_show_returns_error_for_missing_id() {
    let (ctx, _) = context_with_sessions(&[]);
    let err = dispatch::run(
        &ctx,
        Commands::Session {
            cmd: SessionCmd::Show { id: "ghost".into() },
        },
    )
    .await
    .expect_err("show missing");
    assert!(format!("{err}").contains("ghost"));
}

#[tokio::test]
async fn session_history_returns_error_for_missing_id() {
    let (ctx, _) = context_with_sessions(&[]);
    let err = dispatch::run(
        &ctx,
        Commands::Session {
            cmd: SessionCmd::History { id: "ghost".into() },
        },
    )
    .await
    .expect_err("history missing");
    assert!(format!("{err}").contains("ghost"));
}

#[tokio::test]
async fn session_kill_requires_yes_in_slash_mode() {
    let (ctx, ids) = context_with_sessions(&["sid-a"]);
    let slash_ctx = ctx.with_invocation(Invocation::Slash);
    let err = dispatch::run(
        &slash_ctx,
        Commands::Session {
            cmd: SessionCmd::Kill {
                id: ids[0].clone(),
                yes: false,
            },
        },
    )
    .await
    .expect_err("kill without --yes should fail");
    assert!(matches!(err, aura_cli::CliError::ConfirmationRequired(_)));
}

#[tokio::test]
async fn session_kill_with_yes_deletes_in_slash_mode() {
    let (ctx, ids) = context_with_sessions(&["sid-a"]);
    let slash_ctx = ctx.with_invocation(Invocation::Slash);
    let out = dispatch::run(
        &slash_ctx,
        Commands::Session {
            cmd: SessionCmd::Kill {
                id: ids[0].clone(),
                yes: true,
            },
        },
    )
    .await
    .expect("kill with --yes should succeed");
    let data = out.data.expect("structured payload");
    assert_eq!(data["deleted"], ids[0]);
}

#[tokio::test]
async fn session_kill_succeeds_in_argv_mode_without_yes() {
    let (ctx, ids) = context_with_sessions(&["sid-a"]);
    let out = dispatch::run(
        &ctx,
        Commands::Session {
            cmd: SessionCmd::Kill {
                id: ids[0].clone(),
                yes: false,
            },
        },
    )
    .await
    .expect("argv kill should not require --yes");
    let data = out.data.expect("structured payload");
    assert_eq!(data["deleted"], ids[0]);
}

#[tokio::test]
async fn session_list_without_manager_reports_unavailable() {
    let ctx = context();
    let err = dispatch::run(
        &ctx,
        Commands::Session {
            cmd: SessionCmd::List,
        },
    )
    .await
    .expect_err("without session manager should error");
    assert!(format!("{err}").contains("session manager"));
}

#[tokio::test]
async fn doctor_runs_and_aggregates_checks() {
    let ctx = context();
    let out = dispatch::run(&ctx, Commands::Doctor).await.expect("doctor");
    let data = out.data.expect("structured payload");
    assert!(data.get("status").is_some());
    let checks = data.get("checks").and_then(|v| v.as_array()).unwrap();
    assert!(
        checks.iter().any(|c| c["name"] == "llm.client"),
        "expected llm.client check row"
    );
}

#[tokio::test]
async fn job_list_without_manager_reports_unavailable() {
    let ctx = context();
    let err = dispatch::run(
        &ctx,
        Commands::Job {
            cmd: JobCmd::List { status: None },
        },
    )
    .await
    .expect_err("without job manager should error");
    assert!(format!("{err}").contains("job manager"));
}

#[tokio::test]
async fn job_list_reports_empty_when_no_jobs() {
    let mgr = Arc::new(JobManager::new(Arc::new(MemoryJobStore::new())));
    let ctx = ContextBuilder::new(Arc::new(AuraConfig::default()))
        .skills(Arc::new(SkillRegistry::new()))
        .tools(Arc::new(ToolRegistry::new()))
        .channels(Arc::new(ChannelRegistry::new()))
        .workspace(Arc::new(WorkspaceManager::new(PathBuf::from("."))))
        .job(mgr)
        .build()
        .with_invocation(Invocation::Argv)
        .with_format(OutputFormat::Plain);
    let out = dispatch::run(
        &ctx,
        Commands::Job {
            cmd: JobCmd::List { status: None },
        },
    )
    .await
    .expect("job list");
    assert!(out.human.contains("no jobs"));
    let data = out.data.expect("structured payload");
    assert!(data["jobs"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn job_list_returns_all_seeded_jobs() {
    let (ctx, seeded) = context_with_jobs().await;
    let out = dispatch::run(
        &ctx,
        Commands::Job {
            cmd: JobCmd::List { status: None },
        },
    )
    .await
    .expect("job list");
    let data = out.data.expect("structured payload");
    let listed = data["jobs"].as_array().unwrap();
    assert_eq!(listed.len(), seeded.len());
}

#[tokio::test]
async fn job_list_filters_by_status() {
    let (ctx, seeded) = context_with_jobs().await;
    let out = dispatch::run(
        &ctx,
        Commands::Job {
            cmd: JobCmd::List {
                status: Some(JobStatusArg::Failed),
            },
        },
    )
    .await
    .expect("job list --status failed");
    let data = out.data.expect("structured payload");
    let listed = data["jobs"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    let failed_id = seeded
        .iter()
        .find(|(_, s)| *s == JobStatus::Failed)
        .map(|(id, _)| id.clone())
        .unwrap();
    assert_eq!(listed[0]["id"], failed_id);
}

#[tokio::test]
async fn job_show_returns_error_for_missing_id() {
    let (ctx, _) = context_with_jobs().await;
    let err = dispatch::run(
        &ctx,
        Commands::Job {
            cmd: JobCmd::Show { id: "ghost".into() },
        },
    )
    .await
    .expect_err("show missing");
    assert!(format!("{err}").contains("ghost"));
}

#[tokio::test]
async fn job_show_returns_metadata_for_known_id() {
    let (ctx, seeded) = context_with_jobs().await;
    let id = seeded[0].0.clone();
    let out = dispatch::run(
        &ctx,
        Commands::Job {
            cmd: JobCmd::Show { id: id.clone() },
        },
    )
    .await
    .expect("job show");
    let data = out.data.expect("structured payload");
    assert_eq!(data["id"], id);
}

#[tokio::test]
async fn job_cancel_requires_yes_in_slash_mode() {
    let (ctx, seeded) = context_with_jobs().await;
    let slash_ctx = ctx.with_invocation(Invocation::Slash);
    let pending_id = seeded
        .iter()
        .find(|(_, s)| *s == JobStatus::Pending)
        .map(|(id, _)| id.clone())
        .unwrap();
    let err = dispatch::run(
        &slash_ctx,
        Commands::Job {
            cmd: JobCmd::Cancel {
                id: pending_id,
                yes: false,
            },
        },
    )
    .await
    .expect_err("cancel without --yes should fail");
    assert!(matches!(err, aura_cli::CliError::ConfirmationRequired(_)));
}

#[tokio::test]
async fn job_cancel_pending_transitions_to_failed() {
    let (ctx, seeded) = context_with_jobs().await;
    let pending_id = seeded
        .iter()
        .find(|(_, s)| *s == JobStatus::Pending)
        .map(|(id, _)| id.clone())
        .unwrap();
    let out = dispatch::run(
        &ctx,
        Commands::Job {
            cmd: JobCmd::Cancel {
                id: pending_id.clone(),
                yes: false,
            },
        },
    )
    .await
    .expect("argv cancel should not require --yes");
    let data = out.data.expect("structured payload");
    assert_eq!(data["cancelled"], pending_id);
    assert_eq!(data["status"], "Failed");
}

#[tokio::test]
async fn job_cancel_terminal_job_errors() {
    let (ctx, seeded) = context_with_jobs().await;
    let failed_id = seeded
        .iter()
        .find(|(_, s)| *s == JobStatus::Failed)
        .map(|(id, _)| id.clone())
        .unwrap();
    let err = dispatch::run(
        &ctx,
        Commands::Job {
            cmd: JobCmd::Cancel {
                id: failed_id,
                yes: true,
            },
        },
    )
    .await
    .expect_err("cancel of terminal job must fail");
    assert!(format!("{err}").to_lowercase().contains("cannot cancel"));
}

#[tokio::test]
async fn cron_list_without_manager_reports_unavailable() {
    let ctx = context();
    let err = dispatch::run(&ctx, Commands::Cron { cmd: CronCmd::List })
        .await
        .expect_err("without cron scheduler should error");
    assert!(format!("{err}").contains("cron scheduler"));
}

#[tokio::test]
async fn cron_list_reports_empty_when_none_scheduled() {
    let (ctx, _rx, _sched) = context_with_cron().await;
    let out = dispatch::run(&ctx, Commands::Cron { cmd: CronCmd::List })
        .await
        .expect("cron list");
    assert!(out.human.contains("no cron jobs"));
    let data = out.data.expect("structured payload");
    assert!(data["jobs"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn cron_list_returns_all_scheduled_jobs() {
    let (ctx, _rx, sched) = context_with_cron().await;
    sched
        .create_job(
            "alice",
            ChannelType::tui(),
            CronSchedule::cron("0 9 * * *"),
            TriggerAction::Prompt {
                prompt: "morning".into(),
            },
            None,
        )
        .await
        .unwrap();
    sched
        .create_job(
            "bob",
            ChannelType::tui(),
            CronSchedule::cron("0 18 * * *"),
            TriggerAction::Prompt {
                prompt: "evening".into(),
            },
            None,
        )
        .await
        .unwrap();

    let out = dispatch::run(&ctx, Commands::Cron { cmd: CronCmd::List })
        .await
        .expect("cron list");
    let data = out.data.expect("structured payload");
    assert_eq!(data["jobs"].as_array().unwrap().len(), 2);
}

// ----------------------- memory family --------------------------------------

struct MemoryMemStore {
    entries: Mutex<Vec<MemoryEntry>>,
}

impl MemoryMemStore {
    fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl MemoryStore for MemoryMemStore {
    async fn store(&self, entry: &MemoryEntry) -> MemoryResult<()> {
        let mut lock = self.entries.lock();
        if let Some(slot) = lock.iter_mut().find(|e| e.id == entry.id) {
            *slot = entry.clone();
        } else {
            lock.push(entry.clone());
        }
        Ok(())
    }

    async fn retrieve(&self, user_id: &str, key: &str) -> MemoryResult<Option<MemoryEntry>> {
        let lock = self.entries.lock();
        Ok(lock
            .iter()
            .find(|e| e.user_id == user_id && e.id == key)
            .cloned())
    }

    async fn search(
        &self,
        user_id: &str,
        query: &str,
        limit: usize,
    ) -> MemoryResult<Vec<MemoryEntry>> {
        let needle = query.to_lowercase();
        let lock = self.entries.lock();
        let mut out: Vec<MemoryEntry> = lock
            .iter()
            .filter(|e| e.user_id == user_id && e.content.to_lowercase().contains(&needle))
            .cloned()
            .collect();
        out.truncate(limit);
        Ok(out)
    }

    async fn delete(&self, id: &str) -> MemoryResult<()> {
        let mut lock = self.entries.lock();
        lock.retain(|e| e.id != id);
        Ok(())
    }

    async fn list_by_user(&self, user_id: &str) -> MemoryResult<Vec<MemoryEntry>> {
        let lock = self.entries.lock();
        Ok(lock
            .iter()
            .filter(|e| e.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> MemoryResult<Vec<MemoryEntry>> {
        Ok(self.entries.lock().to_vec())
    }

    async fn get_by_id(&self, id: &str) -> MemoryResult<Option<MemoryEntry>> {
        let lock = self.entries.lock();
        Ok(lock.iter().find(|e| e.id == id).cloned())
    }
}

fn context_with_memory() -> (aura_cli::CommandContext, Arc<MemoryManager>) {
    let store = Arc::new(MemoryMemStore::new());
    let mgr = Arc::new(MemoryManager::without_embedder(store));
    let ctx = ContextBuilder::new(Arc::new(AuraConfig::default()))
        .skills(Arc::new(SkillRegistry::new()))
        .tools(Arc::new(ToolRegistry::new()))
        .channels(Arc::new(ChannelRegistry::new()))
        .workspace(Arc::new(WorkspaceManager::new(PathBuf::from("."))))
        .memory(Arc::clone(&mgr))
        .build()
        .with_invocation(Invocation::Argv)
        .with_format(OutputFormat::Plain);
    (ctx, mgr)
}

async fn seed_entry(
    mgr: &MemoryManager,
    user: &str,
    content: &str,
    session: Option<&str>,
    importance: f32,
) -> String {
    let mut entry = MemoryEntry::new(
        user.into(),
        content.into(),
        MemoryCategory::KeyFact,
        importance,
    );
    entry.source_session_id = session.map(str::to_string);
    let id = entry.id.clone();
    mgr.store(entry).await.unwrap();
    id
}

#[tokio::test]
async fn memory_list_without_manager_reports_unavailable() {
    let ctx = context();
    let err = dispatch::run(
        &ctx,
        Commands::Memory {
            cmd: MemoryCmd::List {
                user: None,
                limit: 10,
            },
        },
    )
    .await
    .expect_err("expected error");
    assert!(err.to_string().contains("not available"));
}

#[tokio::test]
async fn memory_list_reports_empty_when_none_stored() {
    let (ctx, _mgr) = context_with_memory();
    let out = dispatch::run(
        &ctx,
        Commands::Memory {
            cmd: MemoryCmd::List {
                user: None,
                limit: 10,
            },
        },
    )
    .await
    .expect("list");
    assert!(out.human.contains("no memories"));
    assert!(
        out.data
            .unwrap()
            .get("entries")
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn memory_list_scopes_to_user_when_provided() {
    let (ctx, mgr) = context_with_memory();
    seed_entry(&mgr, "u1", "alpha", None, 0.9).await;
    seed_entry(&mgr, "u2", "beta", None, 0.5).await;
    seed_entry(&mgr, "u1", "gamma", None, 0.3).await;

    let out = dispatch::run(
        &ctx,
        Commands::Memory {
            cmd: MemoryCmd::List {
                user: Some("u1".into()),
                limit: 10,
            },
        },
    )
    .await
    .expect("list");
    let entries = out.data.unwrap();
    let arr = entries["entries"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert!(arr.iter().all(|v| v["user"] == "u1"));
}

#[tokio::test]
async fn memory_list_respects_limit() {
    let (ctx, mgr) = context_with_memory();
    for i in 0..5 {
        seed_entry(&mgr, "u1", &format!("entry {i}"), None, 0.5).await;
    }
    let out = dispatch::run(
        &ctx,
        Commands::Memory {
            cmd: MemoryCmd::List {
                user: None,
                limit: 2,
            },
        },
    )
    .await
    .expect("list");
    let arr = out.data.unwrap()["entries"].as_array().unwrap().clone();
    assert_eq!(arr.len(), 2);
}

#[tokio::test]
async fn memory_search_returns_only_matching_rows() {
    let (ctx, mgr) = context_with_memory();
    seed_entry(&mgr, "u1", "likes Rust programming", None, 0.7).await;
    seed_entry(&mgr, "u1", "dislikes broccoli", None, 0.4).await;
    seed_entry(&mgr, "u2", "writes Rust macros", None, 0.5).await;

    let out = dispatch::run(
        &ctx,
        Commands::Memory {
            cmd: MemoryCmd::Search {
                query: "rust".into(),
                user: None,
                limit: 10,
            },
        },
    )
    .await
    .expect("search");
    let arr = out.data.unwrap()["entries"].as_array().unwrap().clone();
    assert_eq!(arr.len(), 2);
}

#[tokio::test]
async fn memory_show_errors_for_missing_id() {
    let (ctx, _mgr) = context_with_memory();
    let err = dispatch::run(
        &ctx,
        Commands::Memory {
            cmd: MemoryCmd::Show {
                id: "missing".into(),
            },
        },
    )
    .await
    .expect_err("expected not-found");
    assert!(err.to_string().contains("not found"));
}

#[tokio::test]
async fn memory_show_returns_metadata_for_known_id() {
    let (ctx, mgr) = context_with_memory();
    let id = seed_entry(&mgr, "u1", "hello", Some("s1"), 0.6).await;
    let out = dispatch::run(
        &ctx,
        Commands::Memory {
            cmd: MemoryCmd::Show { id: id.clone() },
        },
    )
    .await
    .expect("show");
    assert!(out.human.contains(&id));
    let data = out.data.unwrap();
    assert_eq!(data["user"], "u1");
    assert_eq!(data["source_session_id"], "s1");
}

#[tokio::test]
async fn memory_promote_requires_yes_in_slash_mode() {
    let (ctx, mgr) = context_with_memory();
    let id = seed_entry(&mgr, "u1", "x", None, 0.4).await;
    let slash_ctx = ContextBuilder::new(Arc::clone(&ctx.config))
        .skills(Arc::clone(&ctx.skills))
        .tools(Arc::clone(&ctx.tools))
        .channels(Arc::clone(&ctx.channels))
        .workspace(Arc::clone(&ctx.workspace))
        .memory(Arc::clone(&mgr))
        .build()
        .with_invocation(Invocation::Slash)
        .with_format(OutputFormat::Plain);
    let err = dispatch::run(
        &slash_ctx,
        Commands::Memory {
            cmd: MemoryCmd::Promote {
                id: id.clone(),
                to: 1.0,
                yes: false,
            },
        },
    )
    .await
    .expect_err("expected confirmation required");
    assert!(err.to_string().contains("--yes"));
}

#[tokio::test]
async fn memory_promote_clamps_and_persists() {
    let (ctx, mgr) = context_with_memory();
    let id = seed_entry(&mgr, "u1", "anchor", None, 0.2).await;
    let out = dispatch::run(
        &ctx,
        Commands::Memory {
            cmd: MemoryCmd::Promote {
                id: id.clone(),
                to: 5.0,
                yes: true,
            },
        },
    )
    .await
    .expect("promote");
    let data = out.data.unwrap();
    assert!((data["importance"].as_f64().unwrap() - 1.0).abs() < 1e-6);
    let reloaded = mgr.get(&id).await.unwrap().unwrap();
    assert!((reloaded.importance - 1.0).abs() < f32::EPSILON);
}

#[tokio::test]
async fn memory_clear_requires_yes_in_slash_mode() {
    let (_ctx, mgr) = context_with_memory();
    seed_entry(&mgr, "u1", "x", Some("s1"), 0.5).await;
    let slash_ctx = ContextBuilder::new(Arc::new(AuraConfig::default()))
        .skills(Arc::new(SkillRegistry::new()))
        .tools(Arc::new(ToolRegistry::new()))
        .channels(Arc::new(ChannelRegistry::new()))
        .workspace(Arc::new(WorkspaceManager::new(PathBuf::from("."))))
        .memory(Arc::clone(&mgr))
        .build()
        .with_invocation(Invocation::Slash)
        .with_format(OutputFormat::Plain);
    let err = dispatch::run(
        &slash_ctx,
        Commands::Memory {
            cmd: MemoryCmd::Clear {
                session: "s1".into(),
                yes: false,
            },
        },
    )
    .await
    .expect_err("expected confirmation required");
    assert!(err.to_string().contains("--yes"));
}

#[tokio::test]
async fn memory_clear_removes_only_matching_session() {
    let (ctx, mgr) = context_with_memory();
    seed_entry(&mgr, "u1", "keep me", None, 0.5).await;
    seed_entry(&mgr, "u1", "from s1 one", Some("s1"), 0.5).await;
    seed_entry(&mgr, "u2", "from s1 two", Some("s1"), 0.5).await;
    seed_entry(&mgr, "u1", "different session", Some("s2"), 0.5).await;

    let out = dispatch::run(
        &ctx,
        Commands::Memory {
            cmd: MemoryCmd::Clear {
                session: "s1".into(),
                yes: true,
            },
        },
    )
    .await
    .expect("clear");
    let data = out.data.unwrap();
    assert_eq!(data["cleared"].as_u64().unwrap(), 2);

    let remaining = mgr.list(None).await.unwrap();
    assert_eq!(remaining.len(), 2);
    assert!(
        remaining
            .iter()
            .all(|e| e.source_session_id.as_deref() != Some("s1"))
    );
}

// ----------------------- trace family ---------------------------------------

struct TraceMemStore {
    traces: Mutex<HashMap<String, SessionTrace>>,
}

impl TraceMemStore {
    fn new() -> Self {
        Self {
            traces: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl TraceStore for TraceMemStore {
    async fn save_trace(&self, trace: &SessionTrace) -> TraceResult<()> {
        self.traces
            .lock()
            .insert(trace.session_id.clone(), trace.clone());
        Ok(())
    }

    async fn load_trace(&self, session_id: &str) -> TraceResult<Option<SessionTrace>> {
        Ok(self.traces.lock().get(session_id).cloned())
    }

    async fn query_traces(&self, filter: TraceFilter) -> TraceResult<Vec<SessionTrace>> {
        let lock = self.traces.lock();
        let values: Vec<SessionTrace> = match filter.session_id {
            Some(sid) => lock.get(&sid).cloned().into_iter().collect(),
            None => lock.values().cloned().collect(),
        };
        Ok(values)
    }

    async fn load_node(
        &self,
        session_id: &str,
        node_id: &TraceNodeId,
    ) -> TraceResult<Option<TraceNode>> {
        let lock = self.traces.lock();
        Ok(lock
            .get(session_id)
            .and_then(|t| t.nodes.get(node_id).cloned()))
    }
}

fn make_trace(session: &str, started_at: DateTime<Utc>) -> SessionTrace {
    let node_id: TraceNodeId = format!("{session}-root");
    let node = TraceNode {
        id: node_id.clone(),
        parent: None,
        children: vec![],
        span: TraceSpan {
            kind: OperationKind::LlmCall {
                model: "gpt-5".into(),
            },
            job_id: None,
            provenance: ExecutionProvenance::default(),
            input: SpanInput::None,
            started_at,
            ended_at: None,
            result: None,
        },
        context_snapshot: None,
    };
    let mut nodes = HashMap::new();
    nodes.insert(node_id.clone(), node);
    SessionTrace {
        session_id: session.into(),
        root: node_id.clone(),
        nodes,
        forks: vec![],
        active_leaf: node_id,
    }
}

fn context_with_trace() -> (aura_cli::CommandContext, Arc<TraceMemStore>) {
    let store = Arc::new(TraceMemStore::new());
    let ctx = ContextBuilder::new(Arc::new(AuraConfig::default()))
        .skills(Arc::new(SkillRegistry::new()))
        .tools(Arc::new(ToolRegistry::new()))
        .channels(Arc::new(ChannelRegistry::new()))
        .workspace(Arc::new(WorkspaceManager::new(PathBuf::from("."))))
        .trace(Arc::clone(&store) as Arc<dyn TraceStore>)
        .build()
        .with_invocation(Invocation::Argv)
        .with_format(OutputFormat::Plain);
    (ctx, store)
}

#[tokio::test]
async fn trace_list_without_store_reports_unavailable() {
    let ctx = context();
    let err = dispatch::run(
        &ctx,
        Commands::Trace {
            cmd: TraceCmd::List {
                session: None,
                limit: 10,
            },
        },
    )
    .await
    .expect_err("expected error");
    assert!(err.to_string().contains("not available"));
}

#[tokio::test]
async fn trace_list_returns_newest_first_and_respects_limit() {
    let (ctx, store) = context_with_trace();
    let t0 = Utc::now();
    let older = make_trace("sess-old", t0 - Duration::minutes(30));
    let newer = make_trace("sess-new", t0);
    store.save_trace(&older).await.unwrap();
    store.save_trace(&newer).await.unwrap();

    let out = dispatch::run(
        &ctx,
        Commands::Trace {
            cmd: TraceCmd::List {
                session: None,
                limit: 10,
            },
        },
    )
    .await
    .expect("list");
    let data = out.data.unwrap();
    let rows = data["traces"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["session"].as_str().unwrap(), "sess-new");
    assert_eq!(rows[1]["session"].as_str().unwrap(), "sess-old");

    let out = dispatch::run(
        &ctx,
        Commands::Trace {
            cmd: TraceCmd::List {
                session: None,
                limit: 1,
            },
        },
    )
    .await
    .expect("list limit");
    assert_eq!(out.data.unwrap()["traces"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn trace_list_filters_by_session() {
    let (ctx, store) = context_with_trace();
    store
        .save_trace(&make_trace("s-a", Utc::now()))
        .await
        .unwrap();
    store
        .save_trace(&make_trace("s-b", Utc::now()))
        .await
        .unwrap();

    let out = dispatch::run(
        &ctx,
        Commands::Trace {
            cmd: TraceCmd::List {
                session: Some("s-a".into()),
                limit: 50,
            },
        },
    )
    .await
    .expect("list filter");
    let rows = out.data.unwrap();
    let rows = rows["traces"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["session"].as_str().unwrap(), "s-a");
}

#[tokio::test]
async fn trace_list_empty_returns_friendly_text() {
    let (ctx, _store) = context_with_trace();
    let out = dispatch::run(
        &ctx,
        Commands::Trace {
            cmd: TraceCmd::List {
                session: None,
                limit: 10,
            },
        },
    )
    .await
    .expect("list empty");
    assert!(out.human.contains("no session traces"));
    assert!(out.data.unwrap()["traces"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn trace_show_returns_summary() {
    let (ctx, store) = context_with_trace();
    store
        .save_trace(&make_trace("sess-x", Utc::now()))
        .await
        .unwrap();

    let out = dispatch::run(
        &ctx,
        Commands::Trace {
            cmd: TraceCmd::Show {
                id: "sess-x".into(),
            },
        },
    )
    .await
    .expect("show");
    let data = out.data.unwrap();
    assert_eq!(data["session"].as_str().unwrap(), "sess-x");
    assert_eq!(data["nodes"].as_u64().unwrap(), 1);
    assert!(out.human.contains("sess-x"));
}

#[tokio::test]
async fn trace_show_missing_reports_error() {
    let (ctx, _store) = context_with_trace();
    let err = dispatch::run(
        &ctx,
        Commands::Trace {
            cmd: TraceCmd::Show { id: "nope".into() },
        },
    )
    .await
    .expect_err("expected missing");
    assert!(err.to_string().contains("no trace recorded"));
}

#[tokio::test]
async fn trace_export_to_stdout_returns_pretty_json() {
    let (ctx, store) = context_with_trace();
    store
        .save_trace(&make_trace("sess-e", Utc::now()))
        .await
        .unwrap();

    let out = dispatch::run(
        &ctx,
        Commands::Trace {
            cmd: TraceCmd::Export {
                id: "sess-e".into(),
                out: None,
                yes: false,
            },
        },
    )
    .await
    .expect("export");
    assert!(out.human.contains("\"session_id\": \"sess-e\""));
    let data = out.data.unwrap();
    assert_eq!(data["session_id"].as_str().unwrap(), "sess-e");
}

#[tokio::test]
async fn trace_export_writes_file_in_argv_mode() {
    let (ctx, store) = context_with_trace();
    store
        .save_trace(&make_trace("sess-w", Utc::now()))
        .await
        .unwrap();

    let tmp = std::env::temp_dir().join(format!(
        "aura-cli-trace-{}.json",
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let tmp_str = tmp.to_string_lossy().into_owned();

    let out = dispatch::run(
        &ctx,
        Commands::Trace {
            cmd: TraceCmd::Export {
                id: "sess-w".into(),
                out: Some(tmp_str.clone()),
                yes: false,
            },
        },
    )
    .await
    .expect("export to file");
    assert!(out.human.contains(&tmp_str));

    let written = std::fs::read_to_string(&tmp).expect("file written");
    assert!(written.contains("\"session_id\": \"sess-w\""));
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn trace_export_slash_requires_yes_when_out_set() {
    let (ctx, store) = context_with_trace();
    store
        .save_trace(&make_trace("sess-s", Utc::now()))
        .await
        .unwrap();
    let ctx = ctx.with_invocation(Invocation::Slash);

    let err = dispatch::run(
        &ctx,
        Commands::Trace {
            cmd: TraceCmd::Export {
                id: "sess-s".into(),
                out: Some("/tmp/should-not-write.json".into()),
                yes: false,
            },
        },
    )
    .await
    .expect_err("expected confirmation");
    assert!(err.to_string().contains("--yes"));
}

#[tokio::test]
async fn trace_export_slash_stdout_does_not_require_yes() {
    let (ctx, store) = context_with_trace();
    store
        .save_trace(&make_trace("sess-ss", Utc::now()))
        .await
        .unwrap();
    let ctx = ctx.with_invocation(Invocation::Slash);

    let out = dispatch::run(
        &ctx,
        Commands::Trace {
            cmd: TraceCmd::Export {
                id: "sess-ss".into(),
                out: None,
                yes: false,
            },
        },
    )
    .await
    .expect("stdout export");
    assert!(out.human.contains("sess-ss"));
}

fn make_trace_with_snapshot(
    session: &str,
    tokens: usize,
    messages: Vec<aura_model::ChatMessage>,
) -> SessionTrace {
    let root_id: TraceNodeId = format!("{session}-root");
    let child_id: TraceNodeId = format!("{session}-leaf");
    let started_at = Utc::now();

    let root = TraceNode {
        id: root_id.clone(),
        parent: None,
        children: vec![child_id.clone()],
        span: TraceSpan {
            kind: OperationKind::LlmCall {
                model: "gpt-5".into(),
            },
            job_id: None,
            provenance: ExecutionProvenance::default(),
            input: SpanInput::None,
            started_at,
            ended_at: None,
            result: None,
        },
        context_snapshot: Some(aura_context::ContextSnapshot {
            messages,
            token_count: tokens,
        }),
    };
    let child = TraceNode {
        id: child_id.clone(),
        parent: Some(root_id.clone()),
        children: vec![],
        span: TraceSpan {
            kind: OperationKind::LlmCall {
                model: "gpt-5".into(),
            },
            job_id: None,
            provenance: ExecutionProvenance::default(),
            input: SpanInput::None,
            started_at,
            ended_at: None,
            result: None,
        },
        context_snapshot: None,
    };

    let mut nodes = HashMap::new();
    nodes.insert(root_id.clone(), root);
    nodes.insert(child_id.clone(), child);
    SessionTrace {
        session_id: session.into(),
        root: root_id,
        nodes,
        forks: vec![],
        active_leaf: child_id,
    }
}

fn sample_user_msg(text: &str) -> aura_model::ChatMessage {
    aura_model::ChatMessage {
        role: aura_model::Role::User,
        content: vec![aura_model::ContentBlock::Text(text.into())],
    }
}

#[tokio::test]
async fn trace_snapshot_walks_ancestors_from_active_leaf() {
    let (ctx, store) = context_with_trace();
    let trace = make_trace_with_snapshot(
        "sess-snap",
        123,
        vec![sample_user_msg("hi"), sample_user_msg("again")],
    );
    store.save_trace(&trace).await.unwrap();

    let out = dispatch::run(
        &ctx,
        Commands::Trace {
            cmd: TraceCmd::Snapshot {
                id: "sess-snap".into(),
                node: None,
                full: false,
            },
        },
    )
    .await
    .expect("snapshot lookup");
    let data = out.data.expect("payload");
    assert_eq!(data["session"], "sess-snap");
    assert_eq!(data["token_count"], 123);
    assert_eq!(data["message_count"], 2);
    assert!(
        data.get("messages_full").is_none(),
        "summary mode hides bodies"
    );
    assert!(out.human.contains("tokens:       123"));
}

#[tokio::test]
async fn trace_snapshot_full_includes_message_bodies() {
    let (ctx, store) = context_with_trace();
    let trace = make_trace_with_snapshot("sess-full", 7, vec![sample_user_msg("payload text")]);
    store.save_trace(&trace).await.unwrap();

    let out = dispatch::run(
        &ctx,
        Commands::Trace {
            cmd: TraceCmd::Snapshot {
                id: "sess-full".into(),
                node: None,
                full: true,
            },
        },
    )
    .await
    .expect("snapshot full");
    let data = out.data.expect("payload");
    let full = data["messages_full"].as_array().expect("full bodies");
    assert_eq!(full.len(), 1);
    assert_eq!(full[0]["role"], "user");
}

#[tokio::test]
async fn trace_snapshot_errors_when_ancestor_chain_has_no_snapshot() {
    let (ctx, store) = context_with_trace();
    store
        .save_trace(&make_trace("sess-bare", Utc::now()))
        .await
        .unwrap();

    let err = dispatch::run(
        &ctx,
        Commands::Trace {
            cmd: TraceCmd::Snapshot {
                id: "sess-bare".into(),
                node: None,
                full: false,
            },
        },
    )
    .await
    .expect_err("no snapshots in chain");
    assert!(
        err.to_string().contains("snapshot"),
        "expected snapshot-not-found: {err}"
    );
}

#[tokio::test]
async fn trace_snapshot_rejects_unknown_node() {
    let (ctx, store) = context_with_trace();
    let trace = make_trace_with_snapshot("sess-unk", 1, vec![]);
    store.save_trace(&trace).await.unwrap();

    let err = dispatch::run(
        &ctx,
        Commands::Trace {
            cmd: TraceCmd::Snapshot {
                id: "sess-unk".into(),
                node: Some("not-a-real-node".into()),
                full: false,
            },
        },
    )
    .await
    .expect_err("bad node id");
    assert!(err.to_string().contains("not-a-real-node"));
}

#[tokio::test]
async fn trace_snapshot_unknown_session_errors() {
    let (ctx, _store) = context_with_trace();
    let err = dispatch::run(
        &ctx,
        Commands::Trace {
            cmd: TraceCmd::Snapshot {
                id: "ghost".into(),
                node: None,
                full: false,
            },
        },
    )
    .await
    .expect_err("unknown session");
    assert!(err.to_string().contains("ghost"));
}

// ----------------------- config get/set/unset --------------------------------

fn tmp_config_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "aura-cli-cfg-{}-{}.json",
        label,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn context_with_config_path(path: PathBuf) -> aura_cli::CommandContext {
    ContextBuilder::new(Arc::new(AuraConfig::default()))
        .config_path(Some(path))
        .skills(Arc::new(SkillRegistry::new()))
        .tools(Arc::new(ToolRegistry::new()))
        .channels(Arc::new(ChannelRegistry::new()))
        .workspace(Arc::new(WorkspaceManager::new(PathBuf::from("."))))
        .build()
        .with_invocation(Invocation::Argv)
        .with_format(OutputFormat::Plain)
}

#[tokio::test]
async fn config_get_returns_known_field() {
    let ctx = context();
    let out = dispatch::run(
        &ctx,
        Commands::Config {
            cmd: ConfigCmd::Get {
                path: "llm.model".into(),
            },
        },
    )
    .await
    .expect("get");
    let data = out.data.unwrap();
    assert_eq!(data, serde_json::to_value(&ctx.config.llm.model).unwrap());
}

#[tokio::test]
async fn config_get_missing_path_errors() {
    let ctx = context();
    let err = dispatch::run(
        &ctx,
        Commands::Config {
            cmd: ConfigCmd::Get {
                path: "not.a.path".into(),
            },
        },
    )
    .await
    .expect_err("expected missing path error");
    assert!(err.to_string().contains("no such path"));
}

#[tokio::test]
async fn config_set_persists_value_to_resolved_path() {
    let tmp = tmp_config_path("set-ok");
    let ctx = context_with_config_path(tmp.clone());
    let out = dispatch::run(
        &ctx,
        Commands::Config {
            cmd: ConfigCmd::Set {
                path: "llm.model".into(),
                value: "gpt-5".into(),
                yes: false,
            },
        },
    )
    .await
    .expect("set");
    assert!(out.human.contains("gpt-5"));
    assert!(out.human.contains("restart"));

    let on_disk = std::fs::read_to_string(&tmp).expect("file exists");
    let reparsed: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
    assert_eq!(reparsed["llm"]["model"].as_str().unwrap(), "gpt-5");
    std::fs::remove_file(&tmp).ok();
}

#[tokio::test]
async fn config_set_slash_requires_yes() {
    let tmp = tmp_config_path("slash-yes");
    let ctx = context_with_config_path(tmp.clone()).with_invocation(Invocation::Slash);
    let err = dispatch::run(
        &ctx,
        Commands::Config {
            cmd: ConfigCmd::Set {
                path: "llm.model".into(),
                value: "gpt-5".into(),
                yes: false,
            },
        },
    )
    .await
    .expect_err("expected confirmation required");
    assert!(err.to_string().contains("--yes"));
    assert!(!tmp.exists());
}

#[tokio::test]
async fn config_set_rejects_invalid_value() {
    let tmp = tmp_config_path("set-bad");
    let ctx = context_with_config_path(tmp.clone());
    let err = dispatch::run(
        &ctx,
        Commands::Config {
            cmd: ConfigCmd::Set {
                path: "llm.provider".into(),
                value: "\"\"".into(),
                yes: false,
            },
        },
    )
    .await
    .expect_err("empty provider invalid");
    assert!(err.to_string().contains("validation failed"));
    assert!(!tmp.exists());
}

#[tokio::test]
async fn config_unset_persists_and_resets() {
    let tmp = tmp_config_path("unset-ok");
    let seeded = AuraConfig::default()
        .set_at_path("llm.model", serde_json::json!("gpt-5"))
        .unwrap();
    seeded.write_to_file(&tmp).await.unwrap();

    let ctx = ContextBuilder::new(Arc::new(seeded))
        .config_path(Some(tmp.clone()))
        .skills(Arc::new(SkillRegistry::new()))
        .tools(Arc::new(ToolRegistry::new()))
        .channels(Arc::new(ChannelRegistry::new()))
        .workspace(Arc::new(WorkspaceManager::new(PathBuf::from("."))))
        .build()
        .with_invocation(Invocation::Argv)
        .with_format(OutputFormat::Plain);

    dispatch::run(
        &ctx,
        Commands::Config {
            cmd: ConfigCmd::Unset {
                path: "llm.model".into(),
                yes: false,
            },
        },
    )
    .await
    .expect("unset");

    let loaded = AuraConfig::load_from_file(&tmp).await.unwrap();
    assert_eq!(loaded.llm.model, AuraConfig::default().llm.model);
    std::fs::remove_file(&tmp).ok();
}

#[tokio::test]
async fn llm_models_lists_default_providers() {
    let ctx = context();
    let out = dispatch::run(
        &ctx,
        Commands::Llm {
            cmd: LlmCmd::Models,
        },
    )
    .await
    .expect("llm models");
    let data = out.data.expect("json payload");
    let providers = data
        .get("providers")
        .and_then(|v| v.as_array())
        .expect("providers array");
    let names: Vec<_> = providers
        .iter()
        .filter_map(|p| p.get("provider").and_then(|v| v.as_str()))
        .collect();
    assert!(names.contains(&"openai"));
    assert!(names.contains(&"anthropic"));
}

#[tokio::test]
async fn llm_probe_without_client_reports_unavailable() {
    let ctx = context();
    let err = dispatch::run(&ctx, Commands::Llm { cmd: LlmCmd::Probe })
        .await
        .expect_err("no client configured");
    assert!(err.to_string().contains("llm client"));
}

// --- skills ---

fn context_with_skills(registry: SkillRegistry) -> aura_cli::CommandContext {
    let config = Arc::new(AuraConfig::default());
    ContextBuilder::new(config)
        .skills(Arc::new(registry))
        .tools(Arc::new(ToolRegistry::new()))
        .channels(Arc::new(ChannelRegistry::new()))
        .workspace(Arc::new(WorkspaceManager::new(PathBuf::from("."))))
        .build()
        .with_invocation(Invocation::Argv)
        .with_format(OutputFormat::Plain)
}

fn sample_skill(name: &str, description: &str) -> aura_skills::SkillDefinition {
    aura_skills::SkillDefinition {
        name: name.into(),
        version: "0.1.0".into(),
        description: description.into(),
        command: Some(name.into()),
        agent_invocable: true,
        argument_hint: None,
        prompt_template: "be helpful".into(),
        allowed_tools: vec![],
        source: aura_model::ArtifactSource::Workspace,
        trust_level: aura_model::TrustLevel::Trusted,
        requirements: aura_skills::SkillRequirements::default(),
        token_budget_hint: 1024,
        source_path: None,
    }
}

#[tokio::test]
async fn skills_search_returns_hits_for_substring() {
    let registry = SkillRegistry::new();
    registry.register(sample_skill("summarize", "condense long text"));
    registry.register(sample_skill("translate", "convert between languages"));
    let ctx = context_with_skills(registry);

    let out = dispatch::run(
        &ctx,
        Commands::Skills {
            cmd: SkillsCmd::Search {
                query: Some("LANG".into()),
            },
        },
    )
    .await
    .expect("skills search");
    let data = out.data.expect("payload");
    assert_eq!(data["hit_count"], 1);
    assert_eq!(data["hits"][0]["name"], "translate");
}

#[tokio::test]
async fn skills_search_empty_query_returns_everything() {
    let registry = SkillRegistry::new();
    registry.register(sample_skill("a", "x"));
    registry.register(sample_skill("b", "y"));
    let ctx = context_with_skills(registry);

    let out = dispatch::run(
        &ctx,
        Commands::Skills {
            cmd: SkillsCmd::Search { query: None },
        },
    )
    .await
    .expect("skills search");
    assert_eq!(out.data.expect("payload")["hit_count"], 2);
}

#[tokio::test]
async fn skills_check_reports_ok_for_clean_skill() {
    let registry = SkillRegistry::new();
    registry.register(sample_skill("clean", "no external reqs"));
    let ctx = context_with_skills(registry);

    let out = dispatch::run(
        &ctx,
        Commands::Skills {
            cmd: SkillsCmd::Check { name: None },
        },
    )
    .await
    .expect("skills check");
    let data = out.data.expect("payload");
    assert_eq!(data["total"], 1);
    assert_eq!(data["ok"], 1);
    assert_eq!(data["failing"], 0);
}

#[tokio::test]
async fn skills_check_flags_missing_binary() {
    let mut skill = sample_skill("needs-bin", "requires a missing binary");
    skill.requirements.required_bins = vec!["aura_not_a_binary_xyz_98765".into()];
    let registry = SkillRegistry::new();
    registry.register(skill);
    let ctx = context_with_skills(registry);

    let out = dispatch::run(
        &ctx,
        Commands::Skills {
            cmd: SkillsCmd::Check {
                name: Some("needs-bin".into()),
            },
        },
    )
    .await
    .expect("skills check single");
    let data = out.data.expect("payload");
    assert_eq!(data["total"], 1);
    assert_eq!(data["failing"], 1);
    let issue_kind = data["reports"][0]["issues"][0]["kind"]
        .as_str()
        .expect("issue kind");
    assert_eq!(issue_kind, "missing_binary");
}

#[tokio::test]
async fn skills_check_unknown_skill_errors() {
    let ctx = context();
    let err = dispatch::run(
        &ctx,
        Commands::Skills {
            cmd: SkillsCmd::Check {
                name: Some("ghost".into()),
            },
        },
    )
    .await
    .expect_err("missing skill");
    assert!(err.to_string().contains("ghost"));
}

// --- agent ---

#[tokio::test]
async fn agent_send_slash_mode_is_forbidden() {
    let ctx = context().with_invocation(Invocation::Slash);
    let err = dispatch::run(
        &ctx,
        Commands::Agent {
            cmd: AgentCmd::Send {
                session: "s-1".into(),
                message: "hi".into(),
                yes: true,
            },
        },
    )
    .await
    .expect_err("slash agent send forbidden");
    assert!(
        matches!(err, aura_cli::CliError::AgentSendForbiddenInSlash(_)),
        "expected AgentSendForbiddenInSlash, got: {err:?}"
    );
}

#[tokio::test]
async fn agent_send_argv_mode_reports_deferred() {
    let ctx = context();
    let err = dispatch::run(
        &ctx,
        Commands::Agent {
            cmd: AgentCmd::Send {
                session: "s-2".into(),
                message: "hello world".into(),
                yes: false,
            },
        },
    )
    .await
    .expect_err("argv deferred");
    let msg = err.to_string();
    assert!(
        msg.contains("not yet available"),
        "expected deferred notice, got: {msg}"
    );
    assert!(msg.contains("s-2"), "session id surfaced: {msg}");
}
