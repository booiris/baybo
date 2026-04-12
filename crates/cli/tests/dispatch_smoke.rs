//! Smoke-tests for the dispatcher.
//!
//! Each test builds a `CommandContext` from in-memory defaults and runs a
//! read-only command end-to-end. These guard against regressions in the
//! wiring between clap, `dispatch::run`, and individual command handlers,
//! without spinning up the full bootstrap.

use std::path::PathBuf;
use std::sync::Arc;

use aura_agent::{CronScheduler, JobManager, MemoryManager, ShutdownSignal};
use aura_channels::ChannelRegistry;
use aura_cli::cli::{
    ChannelsCmd, Commands, ConfigCmd, CronCmd, JobCmd, JobStatusArg, LlmCmd, MemoryCmd, SessionCmd,
    SkillsCmd, ToolsCmd, WorkspaceCmd,
};
use aura_cli::{ContextBuilder, Invocation, OutputFormat, dispatch};
use aura_config::AuraConfig;
use aura_cron::CronRunMode;
use aura_job::{Job, JobError, JobStatus, JobTransition, OperationKind};
use aura_model::{MemoryCategory, MemoryEntry};
use aura_session::store::SessionStore;
use aura_session::{ChannelType, Session, SessionError, SessionManager, SessionState, User};
use aura_skills::SkillRegistry;
use aura_storage::{
    CronExecutionRow, CronJobRow, CronStore, CronStoreError, JobStore, MemoryStore,
    cron::Result as CronResult, memory::Result as MemoryResult,
};
use aura_tools::ToolRegistry;
use aura_workspace::WorkspaceManager;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::{RwLock, mpsc};

fn context() -> aura_cli::CommandContext {
    let config = Arc::new(AuraConfig::default());
    ContextBuilder::new(config)
        .skills(Arc::new(SkillRegistry::new()))
        .tools(Arc::new(ToolRegistry::new()))
        .channels(Arc::new(RwLock::new(ChannelRegistry::new())))
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
    async fn get(&self, session_id: &str) -> Result<Option<Session>, SessionError> {
        Ok(self.data.lock().unwrap().get(session_id).cloned())
    }

    async fn save(&self, session: &Session) -> Result<(), SessionError> {
        self.data
            .lock()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        Ok(())
    }

    async fn delete(&self, session_id: &str) -> Result<(), SessionError> {
        self.data.lock().unwrap().remove(session_id);
        Ok(())
    }

    async fn list_expired(&self, before: DateTime<Utc>) -> Result<Vec<String>, SessionError> {
        Ok(self
            .data
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.last_active < before)
            .map(|s| s.id.clone())
            .collect())
    }

    async fn list_all(&self) -> Result<Vec<Session>, SessionError> {
        Ok(self.data.lock().unwrap().values().cloned().collect())
    }
}

fn seeded_session_manager(ids: &[&str]) -> (Arc<SessionManager>, Vec<String>) {
    let store = Box::new(MemorySessionStore::new());
    let mut populated = Vec::with_capacity(ids.len());
    for id in ids {
        let session = Session {
            id: (*id).to_string(),
            user: User {
                id: "user-1".to_string(),
                name: Some("Alice".to_string()),
                channel: ChannelType::Cli,
            },
            channel: ChannelType::Cli,
            messages: vec![],
            created_at: Utc::now(),
            last_active: Utc::now(),
            state: SessionState::default(),
        };
        store
            .data
            .lock()
            .unwrap()
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
        .channels(Arc::new(RwLock::new(ChannelRegistry::new())))
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
        self.jobs.lock().unwrap().push(job.clone());
        Ok(())
    }

    async fn get(&self, job_id: &str) -> Result<Option<Job>, JobError> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .find(|j| j.id == job_id)
            .cloned())
    }

    async fn save(&self, job: &Job) -> Result<(), JobError> {
        let mut jobs = self.jobs.lock().unwrap();
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
            .unwrap()
            .iter()
            .filter(|j| j.session_id == session_id)
            .cloned()
            .collect())
    }

    async fn list_by_status(&self, status: JobStatus) -> Result<Vec<Job>, JobError> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .filter(|j| j.status == status)
            .cloned()
            .collect())
    }

    async fn list_children(&self, parent_job_id: &str) -> Result<Vec<Job>, JobError> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .filter(|j| j.parent_job_id.as_deref() == Some(parent_job_id))
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> Result<Vec<Job>, JobError> {
        Ok(self.jobs.lock().unwrap().clone())
    }

    async fn record_transition(&self, t: &JobTransition) -> Result<(), JobError> {
        self.transitions.lock().unwrap().push(t.clone());
        Ok(())
    }

    async fn get_transitions(&self, job_id: &str) -> Result<Vec<JobTransition>, JobError> {
        Ok(self
            .transitions
            .lock()
            .unwrap()
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
    let mgr = JobManager::new(Box::new(MemoryJobStore::new()));
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
        .channels(Arc::new(RwLock::new(ChannelRegistry::new())))
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
        self.jobs.lock().unwrap().push(row.clone());
        Ok(())
    }

    async fn get(&self, id: &str) -> CronResult<Option<CronJobRow>> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id == id)
            .cloned())
    }

    async fn save(&self, row: &CronJobRow) -> CronResult<()> {
        let mut jobs = self.jobs.lock().unwrap();
        if let Some(existing) = jobs.iter_mut().find(|r| r.id == row.id) {
            *existing = row.clone();
            Ok(())
        } else {
            Err(CronStoreError::NotFound(row.id.clone()))
        }
    }

    async fn delete(&self, id: &str) -> CronResult<()> {
        let mut jobs = self.jobs.lock().unwrap();
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
            .unwrap()
            .iter()
            .filter(|r| r.user_id == user)
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> CronResult<Vec<CronJobRow>> {
        Ok(self.jobs.lock().unwrap().clone())
    }

    async fn list_enabled(&self) -> CronResult<Vec<CronJobRow>> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.status == "enabled")
            .cloned()
            .collect())
    }

    async fn list_due(&self, now: &str) -> CronResult<Vec<CronJobRow>> {
        Ok(self
            .jobs
            .lock()
            .unwrap()
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
        self.executions.lock().unwrap().push(row.clone());
        Ok(())
    }

    async fn list_executions_by_job(&self, job_id: &str) -> CronResult<Vec<CronExecutionRow>> {
        Ok(self
            .executions
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.job_id == job_id)
            .cloned()
            .collect())
    }

    async fn list_executions_by_user(&self, user: &str) -> CronResult<Vec<CronExecutionRow>> {
        Ok(self
            .executions
            .lock()
            .unwrap()
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
            .unwrap()
            .iter()
            .any(|r| r.job_id == job_id && r.scheduled_fire_time == scheduled_fire_time))
    }

    async fn update_execution_status(&self, id: &str, status: &str) -> CronResult<()> {
        let mut execs = self.executions.lock().unwrap();
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
            .unwrap()
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
    let scheduler = CronScheduler::new(Box::new(MemoryCronStore::new()), tx, ShutdownSignal::new());
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
        .channels(Arc::new(RwLock::new(ChannelRegistry::new())))
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
async fn tools_list_on_empty_registry_returns_placeholder() {
    let ctx = context();
    let out = dispatch::run(
        &ctx,
        Commands::Tools {
            cmd: ToolsCmd::List,
        },
    )
    .await
    .expect("tools list");
    assert!(out.human.contains("no tools"));
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
    let mgr = Arc::new(JobManager::new(Box::new(MemoryJobStore::new())));
    let ctx = ContextBuilder::new(Arc::new(AuraConfig::default()))
        .skills(Arc::new(SkillRegistry::new()))
        .tools(Arc::new(ToolRegistry::new()))
        .channels(Arc::new(RwLock::new(ChannelRegistry::new())))
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
            ChannelType::Cli,
            "0 9 * * *",
            "morning",
            CronRunMode::Recurring,
        )
        .await
        .unwrap();
    sched
        .create_job(
            "bob",
            ChannelType::Cli,
            "0 18 * * *",
            "evening",
            CronRunMode::Recurring,
        )
        .await
        .unwrap();

    let out = dispatch::run(&ctx, Commands::Cron { cmd: CronCmd::List })
        .await
        .expect("cron list");
    let data = out.data.expect("structured payload");
    assert_eq!(data["jobs"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn cron_show_errors_for_missing_id() {
    let (ctx, _rx, _sched) = context_with_cron().await;
    let err = dispatch::run(
        &ctx,
        Commands::Cron {
            cmd: CronCmd::Show { id: "ghost".into() },
        },
    )
    .await
    .expect_err("show missing should fail");
    assert!(format!("{err}").contains("ghost"));
}

#[tokio::test]
async fn cron_show_returns_metadata_for_known_id() {
    let (ctx, _rx, sched) = context_with_cron().await;
    let job = sched
        .create_job(
            "alice",
            ChannelType::Cli,
            "0 9 * * *",
            "hello",
            CronRunMode::Recurring,
        )
        .await
        .unwrap();

    let out = dispatch::run(
        &ctx,
        Commands::Cron {
            cmd: CronCmd::Show { id: job.id.clone() },
        },
    )
    .await
    .expect("cron show");
    let data = out.data.expect("structured payload");
    assert_eq!(data["id"], job.id);
    assert_eq!(data["prompt"], "hello");
}

#[tokio::test]
async fn cron_rm_requires_yes_in_slash_mode() {
    let (ctx, _rx, sched) = context_with_cron().await;
    let job = sched
        .create_job(
            "alice",
            ChannelType::Cli,
            "0 9 * * *",
            "test",
            CronRunMode::Recurring,
        )
        .await
        .unwrap();
    let slash_ctx = ctx.with_invocation(Invocation::Slash);
    let err = dispatch::run(
        &slash_ctx,
        Commands::Cron {
            cmd: CronCmd::Rm {
                id: job.id,
                yes: false,
            },
        },
    )
    .await
    .expect_err("rm without --yes should fail");
    assert!(matches!(err, aura_cli::CliError::ConfirmationRequired(_)));
}

#[tokio::test]
async fn cron_rm_with_yes_deletes() {
    let (ctx, _rx, sched) = context_with_cron().await;
    let job = sched
        .create_job(
            "alice",
            ChannelType::Cli,
            "0 9 * * *",
            "test",
            CronRunMode::Recurring,
        )
        .await
        .unwrap();

    let out = dispatch::run(
        &ctx,
        Commands::Cron {
            cmd: CronCmd::Rm {
                id: job.id.clone(),
                yes: true,
            },
        },
    )
    .await
    .expect("rm should succeed");
    assert_eq!(out.data.unwrap()["deleted"], job.id);
    assert!(sched.get_job(&job.id).await.unwrap().is_none());
}

#[tokio::test]
async fn cron_enable_disable_round_trip() {
    let (ctx, _rx, sched) = context_with_cron().await;
    let job = sched
        .create_job(
            "alice",
            ChannelType::Cli,
            "0 9 * * *",
            "test",
            CronRunMode::Recurring,
        )
        .await
        .unwrap();

    dispatch::run(
        &ctx,
        Commands::Cron {
            cmd: CronCmd::Disable { id: job.id.clone() },
        },
    )
    .await
    .expect("disable");
    assert!(
        sched
            .get_job(&job.id)
            .await
            .unwrap()
            .unwrap()
            .next_trigger_at
            .is_none()
    );

    dispatch::run(
        &ctx,
        Commands::Cron {
            cmd: CronCmd::Enable { id: job.id.clone() },
        },
    )
    .await
    .expect("enable");
    assert!(
        sched
            .get_job(&job.id)
            .await
            .unwrap()
            .unwrap()
            .next_trigger_at
            .is_some()
    );
}

#[tokio::test]
async fn cron_run_requires_yes_in_slash_mode() {
    let (ctx, _rx, sched) = context_with_cron().await;
    let job = sched
        .create_job(
            "alice",
            ChannelType::Cli,
            "0 9 * * *",
            "fire",
            CronRunMode::Recurring,
        )
        .await
        .unwrap();
    let slash_ctx = ctx.with_invocation(Invocation::Slash);
    let err = dispatch::run(
        &slash_ctx,
        Commands::Cron {
            cmd: CronCmd::Run {
                id: job.id,
                yes: false,
            },
        },
    )
    .await
    .expect_err("run without --yes should fail in slash mode");
    assert!(matches!(err, aura_cli::CliError::ConfirmationRequired(_)));
}

#[tokio::test]
async fn cron_run_dispatches_and_records_execution() {
    let (ctx, mut rx, sched) = context_with_cron().await;
    let job = sched
        .create_job(
            "alice",
            ChannelType::Cli,
            "0 9 * * *",
            "fire",
            CronRunMode::Recurring,
        )
        .await
        .unwrap();

    let out = dispatch::run(
        &ctx,
        Commands::Cron {
            cmd: CronCmd::Run {
                id: job.id.clone(),
                yes: true,
            },
        },
    )
    .await
    .expect("run should succeed");
    assert_eq!(out.data.unwrap()["job"], job.id);

    let event = rx.try_recv().expect("trigger dispatched");
    assert_eq!(event.job_id, job.id);
    assert_eq!(event.prompt, "fire");
}

#[tokio::test]
async fn cron_runs_returns_empty_for_unfired_job() {
    let (ctx, _rx, sched) = context_with_cron().await;
    let job = sched
        .create_job(
            "alice",
            ChannelType::Cli,
            "0 9 * * *",
            "fresh",
            CronRunMode::Recurring,
        )
        .await
        .unwrap();
    let out = dispatch::run(
        &ctx,
        Commands::Cron {
            cmd: CronCmd::Runs { id: job.id.clone() },
        },
    )
    .await
    .expect("runs");
    assert!(out.human.contains("no executions"));
    assert!(out.data.unwrap()["runs"].as_array().unwrap().is_empty());
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
        let mut lock = self.entries.lock().unwrap();
        if let Some(slot) = lock.iter_mut().find(|e| e.id == entry.id) {
            *slot = entry.clone();
        } else {
            lock.push(entry.clone());
        }
        Ok(())
    }

    async fn retrieve(&self, user_id: &str, key: &str) -> MemoryResult<Option<MemoryEntry>> {
        let lock = self.entries.lock().unwrap();
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
        let lock = self.entries.lock().unwrap();
        let mut out: Vec<MemoryEntry> = lock
            .iter()
            .filter(|e| e.user_id == user_id && e.content.to_lowercase().contains(&needle))
            .cloned()
            .collect();
        out.truncate(limit);
        Ok(out)
    }

    async fn delete(&self, id: &str) -> MemoryResult<()> {
        let mut lock = self.entries.lock().unwrap();
        lock.retain(|e| e.id != id);
        Ok(())
    }

    async fn list_by_user(&self, user_id: &str) -> MemoryResult<Vec<MemoryEntry>> {
        let lock = self.entries.lock().unwrap();
        Ok(lock
            .iter()
            .filter(|e| e.user_id == user_id)
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> MemoryResult<Vec<MemoryEntry>> {
        Ok(self.entries.lock().unwrap().clone())
    }

    async fn get_by_id(&self, id: &str) -> MemoryResult<Option<MemoryEntry>> {
        let lock = self.entries.lock().unwrap();
        Ok(lock.iter().find(|e| e.id == id).cloned())
    }
}

fn context_with_memory() -> (aura_cli::CommandContext, Arc<MemoryManager>) {
    let store = Box::new(MemoryMemStore::new());
    let mgr = Arc::new(MemoryManager::without_embedder(store));
    let ctx = ContextBuilder::new(Arc::new(AuraConfig::default()))
        .skills(Arc::new(SkillRegistry::new()))
        .tools(Arc::new(ToolRegistry::new()))
        .channels(Arc::new(RwLock::new(ChannelRegistry::new())))
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
        .channels(Arc::new(RwLock::new(ChannelRegistry::new())))
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
