//! Smoke-tests for the dispatcher.
//!
//! Each test builds a `CommandContext` from in-memory defaults and runs a
//! read-only command end-to-end. These guard against regressions in the
//! wiring between clap, `dispatch::run`, and individual command handlers,
//! without spinning up the full bootstrap.

use std::path::PathBuf;
use std::sync::Arc;

use aura_agent::JobManager;
use aura_channels::ChannelRegistry;
use aura_cli::cli::{
    ChannelsCmd, Commands, ConfigCmd, JobCmd, JobStatusArg, LlmCmd, SessionCmd, SkillsCmd,
    ToolsCmd, WorkspaceCmd,
};
use aura_cli::{ContextBuilder, Invocation, OutputFormat, dispatch};
use aura_config::AuraConfig;
use aura_job::{Job, JobError, JobStatus, JobTransition, OperationKind};
use aura_session::store::SessionStore;
use aura_session::{ChannelType, Session, SessionError, SessionManager, SessionState, User};
use aura_skills::SkillRegistry;
use aura_storage::JobStore;
use aura_tools::ToolRegistry;
use aura_workspace::WorkspaceManager;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::RwLock;

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
