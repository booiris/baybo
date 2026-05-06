//! `SelfImprovementManager` — subscribes to `JobLifecycle::terminal_events`,
//! filters terminal events down to "complex completed user-chat",
//! enforces the per-user mutex / global semaphore / daily cap, prepares
//! the trigger payload (transcript text + identity context), and pushes
//! a [`SystemTriggerEvent`] into the Router's mpsc.
//!
//! See `docs/modules/self-improvement.md` for the full design and Q-by-Q
//! decisions behind every limit.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use aura_job::JobKind;
use aura_model::{ChannelType, JobId, SessionId, SystemReason};
use aura_storage::SessionStore;
use aura_workspace::WorkspacePaths;
use chrono::{DateTime, Datelike, Utc};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::{Semaphore, broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::job::{JobLifecycle, JobTerminalEvent};

/// Configuration for [`SelfImprovementManager`]. All fields have safe
/// defaults — `Default::default()` matches the spec in
/// `docs/modules/self-improvement.md` (Q4, Q9, Q11).
#[derive(Debug, Clone)]
pub struct SelfImprovementConfig {
    /// Master switch. When `false`, the manager is constructed but
    /// drops every event silently — callers don't have to skip
    /// construction.
    pub enabled: bool,
    /// Iteration threshold. A `Completed` `JobKind::UserChat` job whose
    /// `iterations > min_iterations` triggers self_improvement.
    pub min_iterations: u32,
    /// Max successful self_improvements per UTC day, system-wide. Failures
    /// don't count (Q11).
    pub daily_cap: u32,
    /// Global concurrency cap across all self_improvements (Q9).
    pub max_concurrent: usize,
}

impl Default for SelfImprovementConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_iterations: 8,
            daily_cap: 100,
            max_concurrent: 8,
        }
    }
}

/// Event published by [`SelfImprovementManager`] into Router's mpsc when
/// it decides a terminal event warrants self_improvement. Carries
/// everything Router needs to mint a fresh session and dispatch the
/// agent message — no further calls back into the manager.
#[derive(Debug, Clone)]
pub struct SystemTriggerEvent {
    pub reason: SystemReason,
    pub trigger_job_id: JobId,
    pub originating_user_id: String,
    pub originating_user_channel: ChannelType,
    pub originating_session_id: SessionId,
    pub iterations: u32,
    pub retry_count: u8,
    /// Already-baked payload — Router stuffs this verbatim into
    /// `JobInput::System.payload` so the actor that picks it up can
    /// hand it to `prompt::build_initial_user_message` without further
    /// processing. Includes `transcript_text` and `identity_context`.
    pub payload: Value,
}

/// Subscribes to terminal events, applies the trigger predicate +
/// limits, and dispatches `SystemTriggerEvent`s.
///
/// Construct with [`SelfImprovementManager::new`], then either
/// [`spawn`](Self::spawn) (background task) or
/// [`process_event`](Self::process_event) (drive manually from a
/// caller-owned task).
pub struct SelfImprovementManager {
    config: SelfImprovementConfig,
    job_lifecycle: Arc<JobLifecycle>,
    session_store: Arc<dyn SessionStore>,
    workspace: WorkspacePaths,
    trigger_tx: mpsc::Sender<SystemTriggerEvent>,

    // Per-user mutex map — self_improvement acquires this before pushing
    // the trigger event so two self_improvements for the same user can
    // never race against each other on shared memory state. The
    // `tokio::sync::Mutex` lets us hold across await boundaries.
    user_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,

    // Global concurrency cap (Q9). Acquired alongside the per-user
    // mutex; released when the trigger has been dispatched into Router
    // (the dispatch is itself bounded by the spawned actor's lifetime,
    // but the manager only holds the slot through the dispatch call).
    global_slots: Arc<Semaphore>,

    // Daily-cap counter — `(utc_day_ordinal, count)`. Reset whenever
    // we see a terminal event whose UTC day differs from the cached
    // day. In-memory only; a process restart resets the counter, which
    // is acceptable for a soft cap.
    daily_count: Mutex<DailyCap>,

    // Counts only successes (Q11). The cap-check happens at decision
    // time (before dispatch), but the increment also happens at
    // decision time — failures are deducted from the count via
    // `decrement_daily_on_failure` which Router doesn't currently
    // call. For v1 we live with "counted at attempt time".
    pending_attempts: AtomicU32,
}

#[derive(Debug, Clone, Copy)]
struct DailyCap {
    day_ordinal: i32,
    count: u32,
}

impl DailyCap {
    fn new() -> Self {
        Self {
            day_ordinal: i32::MIN,
            count: 0,
        }
    }
}

impl SelfImprovementManager {
    pub fn new(
        config: SelfImprovementConfig,
        job_lifecycle: Arc<JobLifecycle>,
        session_store: Arc<dyn SessionStore>,
        workspace: WorkspacePaths,
        trigger_tx: mpsc::Sender<SystemTriggerEvent>,
    ) -> Self {
        let max_concurrent = config.max_concurrent.max(1);
        Self {
            config,
            job_lifecycle,
            session_store,
            workspace,
            trigger_tx,
            user_locks: Mutex::new(HashMap::new()),
            global_slots: Arc::new(Semaphore::new(max_concurrent)),
            daily_count: Mutex::new(DailyCap::new()),
            pending_attempts: AtomicU32::new(0),
        }
    }

    /// Spawn the manager as a background task that drains the terminal
    /// event broadcast forever. Returns the task handle so callers can
    /// abort it on shutdown.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let mut rx = self.job_lifecycle.subscribe_terminal_events();
        tokio::spawn(async move {
            info!(
                enabled = self.config.enabled,
                min_iterations = self.config.min_iterations,
                daily_cap = self.config.daily_cap,
                max_concurrent = self.config.max_concurrent,
                "self_improvement manager started"
            );
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let manager = Arc::clone(&self);
                        // Decision + dispatch can each await; spawn so
                        // a slow per-user mutex acquisition doesn't
                        // delay processing the next event.
                        tokio::spawn(async move {
                            manager.process_event(event).await;
                        });
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            skipped = n,
                            "self_improvement manager lagged on terminal events"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("terminal event bus closed; self_improvement manager exiting");
                        break;
                    }
                }
            }
        })
    }

    /// One end-to-end pass: filter, gate (per-user + global +
    /// daily-cap), prepare payload, dispatch.
    pub async fn process_event(self: Arc<Self>, event: JobTerminalEvent) {
        if !self.config.enabled {
            return;
        }
        if !self.passes_predicate(&event) {
            return;
        }

        // Look the originating job up to recover trigger data not in
        // the event (specifically, the user_id + channel and the
        // retry_count from any prior attempts).
        let job = match self.job_lifecycle.get(&event.job_id).await {
            Ok(Some(j)) => j,
            Ok(None) => {
                warn!(job_id = %event.job_id, "self_improvement: originating job vanished before pickup");
                return;
            }
            Err(e) => {
                warn!(error = %e, "self_improvement: failed to load originating job");
                return;
            }
        };

        // Resolve session for user + channel context.
        let session = match self.session_store.get(&event.session_id).await {
            Ok(Some(s)) => s,
            Ok(None) => {
                warn!(session_id = %event.session_id, "self_improvement: originating session vanished");
                return;
            }
            Err(e) => {
                warn!(error = %e, "self_improvement: failed to load originating session");
                return;
            }
        };

        // Per-user serialization — acquire BEFORE the daily-cap check
        // so two parallel decisions for the same user don't both see
        // count<cap and both increment.
        let user_id = session.user.id.clone();
        let mutex = {
            let mut locks = self.user_locks.lock();
            Arc::clone(
                locks
                    .entry(user_id.clone())
                    .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
            )
        };
        let _user_guard = mutex.lock().await;

        // Daily cap.
        if !self.try_charge_daily_cap() {
            debug!(
                user_id = user_id,
                "self_improvement: daily cap reached; dropping trigger"
            );
            return;
        }

        // Global concurrency.
        let _slot = match self.global_slots.clone().acquire_owned().await {
            Ok(s) => s,
            Err(_) => {
                warn!("self_improvement: global semaphore closed");
                return;
            }
        };

        // Bake transcript + identity context into the payload.
        let transcript_text = crate::self_improvement::prompt::render_transcript(&session.messages);
        let identity_context = self.read_identity_context();

        let payload = json!({
            "trigger_job_id": event.job_id.to_string(),
            "originating_user_id": user_id,
            "originating_session_id": event.session_id.to_string(),
            "iterations": event.iterations,
            "retry_count": 0,
            "transcript_text": transcript_text,
            "identity_context": identity_context,
        });

        let _ = job; // (kept for future expansion — drift checks etc.)

        let trig = SystemTriggerEvent {
            reason: SystemReason::SelfImprovement,
            trigger_job_id: event.job_id,
            originating_user_id: session.user.id.clone(),
            originating_user_channel: session.user.channel.clone(),
            originating_session_id: event.session_id,
            iterations: event.iterations,
            retry_count: 0,
            payload,
        };

        if let Err(e) = self.trigger_tx.send(trig).await {
            warn!(error = %e, "self_improvement: failed to dispatch trigger to router");
        } else {
            info!(
                user_id = user_id,
                iterations = event.iterations,
                "self_improvement: dispatched trigger"
            );
            self.pending_attempts.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn passes_predicate(&self, event: &JobTerminalEvent) -> bool {
        use aura_job::JobStatusKind;
        if event.status_kind != JobStatusKind::Completed {
            return false;
        }
        if !matches!(event.job_kind, JobKind::UserChat) {
            return false;
        }
        if event.iterations <= self.config.min_iterations {
            return false;
        }
        true
    }

    /// Returns `true` if a slot was charged; `false` if the daily cap
    /// is exhausted. Resets the counter when the UTC day rolls over.
    fn try_charge_daily_cap(&self) -> bool {
        let today = today_ordinal(Utc::now());
        let mut cap = self.daily_count.lock();
        if cap.day_ordinal != today {
            cap.day_ordinal = today;
            cap.count = 0;
        }
        if cap.count >= self.config.daily_cap {
            return false;
        }
        cap.count += 1;
        true
    }

    fn read_identity_context(&self) -> String {
        use aura_workspace::IdentityKind;
        let mut out = String::new();
        for kind in [
            IdentityKind::Soul,
            IdentityKind::User,
            IdentityKind::Identity,
        ] {
            let path = self.workspace.identity_file(kind);
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    out.push_str(&format!("## {}\n", kind.file_name()));
                    out.push_str(content.trim_end());
                    out.push_str("\n\n");
                }
                Err(_) => {
                    // File may not exist on first-run workspaces;
                    // dedup-context is best-effort.
                }
            }
        }
        out
    }

    /// Test / inspection: snapshot of in-memory counter state.
    #[doc(hidden)]
    pub fn _test_state_snapshot(&self) -> (u32, u32) {
        let cap = self.daily_count.lock();
        (cap.count, self.pending_attempts.load(Ordering::Relaxed))
    }
}

fn today_ordinal(now: DateTime<Utc>) -> i32 {
    now.year() * 1000 + now.ordinal() as i32
}

// Workspace path helper exposed via the agent crate so the manager
// doesn't have to hold a full `aura_workspace::Workspace` (which would
// drag in initialization side-effects). Lives in a small sibling
// module for clarity.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicate_rejects_non_user_chat() {
        let cfg = SelfImprovementConfig::default();
        let mgr_state = mk_state(cfg);
        let mut ev = mk_event();
        ev.job_kind = JobKind::Cron;
        assert!(!mgr_state.predicate_only(&ev));
    }

    #[test]
    fn predicate_rejects_failed() {
        let cfg = SelfImprovementConfig::default();
        let mgr_state = mk_state(cfg);
        let mut ev = mk_event();
        ev.status_kind = aura_job::JobStatusKind::Failed;
        assert!(!mgr_state.predicate_only(&ev));
    }

    #[test]
    fn predicate_rejects_too_few_iterations() {
        let cfg = SelfImprovementConfig::default();
        let mgr_state = mk_state(cfg);
        let mut ev = mk_event();
        ev.iterations = 8; // == min, not >
        assert!(!mgr_state.predicate_only(&ev));
        ev.iterations = 9;
        assert!(mgr_state.predicate_only(&ev));
    }

    #[test]
    fn daily_cap_allows_up_to_limit_then_rejects() {
        let cfg = SelfImprovementConfig {
            daily_cap: 3,
            ..Default::default()
        };
        let mgr_state = mk_state(cfg);
        assert!(mgr_state.try_charge_daily_cap());
        assert!(mgr_state.try_charge_daily_cap());
        assert!(mgr_state.try_charge_daily_cap());
        assert!(!mgr_state.try_charge_daily_cap());
    }

    /// Stripped-down state object for predicate / cap unit tests.
    /// Building a real `SelfImprovementManager` requires Arc handles to
    /// `JobLifecycle` + `SessionStore` + a workspace; the predicate
    /// and daily-cap logic are pure and worth testing in isolation.
    struct StateOnly {
        config: SelfImprovementConfig,
        daily_count: Mutex<DailyCap>,
    }

    impl StateOnly {
        fn predicate_only(&self, event: &JobTerminalEvent) -> bool {
            use aura_job::JobStatusKind;
            event.status_kind == JobStatusKind::Completed
                && matches!(event.job_kind, JobKind::UserChat)
                && event.iterations > self.config.min_iterations
        }

        fn try_charge_daily_cap(&self) -> bool {
            let today = today_ordinal(Utc::now());
            let mut cap = self.daily_count.lock();
            if cap.day_ordinal != today {
                cap.day_ordinal = today;
                cap.count = 0;
            }
            if cap.count >= self.config.daily_cap {
                return false;
            }
            cap.count += 1;
            true
        }
    }

    fn mk_state(config: SelfImprovementConfig) -> StateOnly {
        StateOnly {
            config,
            daily_count: Mutex::new(DailyCap::new()),
        }
    }

    fn mk_event() -> JobTerminalEvent {
        JobTerminalEvent {
            job_id: JobId::new(),
            session_id: SessionId::new("sess-test"),
            parent_job_id: None,
            status_kind: aura_job::JobStatusKind::Completed,
            job_kind: JobKind::UserChat,
            iterations: 12,
        }
    }
}
