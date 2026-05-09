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
    /// Max self_improvement attempts per UTC day, system-wide. Counted
    /// at attempt time (cheaper than a post-success accounting hand-back);
    /// `daily_cap` bounds wall-clock LLM API rate / CPU consumption from
    /// this side-channel, *not* dollar spend (CostManager owns the
    /// dollar gate independently).
    ///
    /// Folding: a single origin user-chat job consumes **one** cap slot
    /// no matter how many retries fire for it within 24 h. See
    /// [`SelfImprovementManager::charged_origins`] for the dedupe set.
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
    session_store: Arc<dyn SessionStore>,
    workspace: WorkspacePaths,
    trigger_tx: mpsc::Sender<SystemTriggerEvent>,
    job_lifecycle: Arc<JobLifecycle>,

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

    // Set of origin user-chat job ids the cap has already been charged
    // for, keyed to the time of charge. Same origin showing up again
    // within 24 h (retry path; broadcast lag re-deliveries; same
    // terminal event hitting two subscribers) is a cap-free pass-through
    // — `try_charge_daily_cap` returns `true` without bumping the
    // counter. Pruned lazily on every charge attempt; bounded by
    // `daily_cap × 24 h / spawn-rate` so the worst case is ~
    // `daily_cap` entries (default 100).
    charged_origins: Mutex<HashMap<JobId, DateTime<Utc>>>,
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
            charged_origins: Mutex::new(HashMap::new()),
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
        if !passes_predicate(&self.config, &event) {
            return;
        }

        // Resolve session for user + channel context. Originating job
        // metadata is not loaded — the trigger event already carries
        // every field we need; `retry_count` is 0 for v1 (the manager
        // has no retry path yet).
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

        // Per-user serialization — held across the whole prep+dispatch
        // window so two parallel triggers for the same user don't both
        // bake the same transcript and race the global slot.
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

        // Daily cap (folded per origin job — see field doc).
        if !try_charge_daily_cap(
            &self.daily_count,
            &self.charged_origins,
            self.config.daily_cap,
            event.job_id,
            Utc::now(),
        ) {
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

        // Bake transcript + identity context into the payload. After
        // master's refactor, messages no longer live on `Session` — load
        // the active slice via `SessionStore`, dropping superseded rows.
        let stored = match self
            .session_store
            .load_session_messages_with_supersede(&event.session_id)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "self_improvement: failed to load session messages");
                return;
            }
        };
        let messages: Vec<aura_model::ChatMessage> = stored
            .into_iter()
            .filter(|sm| sm.superseded_by.is_none())
            .map(|sm| sm.message)
            .collect();
        let transcript_text = crate::self_improvement::prompt::render_transcript(&messages);
        let identity_context = self.read_identity_context().await;

        let payload = json!({
            "trigger_job_id": event.job_id.to_string(),
            "originating_user_id": user_id,
            "originating_session_id": event.session_id.to_string(),
            "iterations": event.iterations,
            "retry_count": 0,
            "transcript_text": transcript_text,
            "identity_context": identity_context,
        });

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
        }
    }

    async fn read_identity_context(&self) -> String {
        use aura_workspace::IdentityKind;
        let mut out = String::new();
        for kind in [
            IdentityKind::Soul,
            IdentityKind::User,
            IdentityKind::Identity,
        ] {
            let path = self.workspace.identity_file(kind);
            // Best-effort: file may not exist on first-run workspaces.
            if let Ok(content) = tokio::fs::read_to_string(&path).await {
                out.push_str(&format!("## {}\n", kind.file_name()));
                out.push_str(content.trim_end());
                out.push_str("\n\n");
            }
        }
        out
    }
}

/// Predicate the manager applies to every terminal event. Lifted to a
/// free function so unit tests exercise the same code path as
/// `process_event` instead of a parallel re-implementation.
fn passes_predicate(cfg: &SelfImprovementConfig, event: &JobTerminalEvent) -> bool {
    use aura_job::JobStatusKind;
    event.status_kind == JobStatusKind::Completed
        && matches!(event.job_kind, JobKind::UserChat)
        && event.iterations > cfg.min_iterations
}

/// Atomically check + bump the daily-cap counter, folding repeated
/// charges for the same origin user-chat job into a single cap slot
/// over a 24 h window. Returns `true` if the call is allowed to
/// proceed (charged, or already-charged within window); `false` if
/// the cap is exhausted. Resets the daily counter when the UTC day
/// rolls over.
///
/// Lifted to a free function so unit tests can drive it without
/// standing up the full manager. `now` is injected so tests can
/// fast-forward across day-rollover and 24 h-prune boundaries
/// deterministically.
fn try_charge_daily_cap(
    daily_count: &Mutex<DailyCap>,
    charged_origins: &Mutex<HashMap<JobId, DateTime<Utc>>>,
    daily_cap: u32,
    origin: JobId,
    now: DateTime<Utc>,
) -> bool {
    {
        let mut origins = charged_origins.lock();
        prune_origins(&mut origins, now);
        if origins.contains_key(&origin) {
            return true;
        }
    }

    {
        let today = today_ordinal(now);
        let mut cap = daily_count.lock();
        if cap.day_ordinal != today {
            cap.day_ordinal = today;
            cap.count = 0;
        }
        if cap.count >= daily_cap {
            return false;
        }
        cap.count += 1;
    }

    charged_origins.lock().insert(origin, now);
    true
}

/// Drop entries older than 24 h. Called from `try_charge_daily_cap`
/// before every check so the set never grows beyond a day's worth of
/// origins; no separate sweeper task needed.
fn prune_origins(origins: &mut HashMap<JobId, DateTime<Utc>>, now: DateTime<Utc>) {
    let cutoff = now - chrono::Duration::hours(24);
    origins.retain(|_, t| *t >= cutoff);
}

fn today_ordinal(now: DateTime<Utc>) -> i32 {
    now.year() * 1000 + now.ordinal() as i32
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn predicate_rejects_non_user_chat() {
        let cfg = SelfImprovementConfig::default();
        let mut ev = mk_event();
        ev.job_kind = JobKind::Cron;
        assert!(!passes_predicate(&cfg, &ev));
    }

    #[test]
    fn predicate_rejects_failed() {
        let cfg = SelfImprovementConfig::default();
        let mut ev = mk_event();
        ev.status_kind = aura_job::JobStatusKind::Failed;
        assert!(!passes_predicate(&cfg, &ev));
    }

    #[test]
    fn predicate_rejects_too_few_iterations() {
        let cfg = SelfImprovementConfig::default();
        let mut ev = mk_event();
        ev.iterations = 8; // == min, not >
        assert!(!passes_predicate(&cfg, &ev));
        ev.iterations = 9;
        assert!(passes_predicate(&cfg, &ev));
    }

    #[test]
    fn daily_cap_allows_up_to_limit_then_rejects() {
        let daily_count = Mutex::new(DailyCap::new());
        let origins = Mutex::new(HashMap::new());
        let now = Utc::now();
        // Each call is a distinct origin so each one charges a fresh slot.
        assert!(try_charge_daily_cap(
            &daily_count,
            &origins,
            3,
            JobId::new(),
            now,
        ));
        assert!(try_charge_daily_cap(
            &daily_count,
            &origins,
            3,
            JobId::new(),
            now,
        ));
        assert!(try_charge_daily_cap(
            &daily_count,
            &origins,
            3,
            JobId::new(),
            now,
        ));
        assert!(!try_charge_daily_cap(
            &daily_count,
            &origins,
            3,
            JobId::new(),
            now,
        ));
    }

    #[test]
    fn cap_folds_repeated_charges_for_same_origin() {
        let daily_count = Mutex::new(DailyCap::new());
        let origins = Mutex::new(HashMap::new());
        let now = Utc::now();
        let origin = JobId::new();
        // First charge consumes a slot.
        assert!(try_charge_daily_cap(&daily_count, &origins, 1, origin, now));
        assert_eq!(daily_count.lock().count, 1);
        // Second charge for the same origin within 24h passes through
        // without bumping the counter — simulating a retry path or a
        // re-delivered terminal event.
        assert!(try_charge_daily_cap(&daily_count, &origins, 1, origin, now));
        assert_eq!(daily_count.lock().count, 1);
        // A different origin still hits the cap because the slot is
        // already consumed.
        assert!(!try_charge_daily_cap(
            &daily_count,
            &origins,
            1,
            JobId::new(),
            now,
        ));
    }

    #[test]
    fn origin_fold_expires_after_24h() {
        let daily_count = Mutex::new(DailyCap::new());
        let origins = Mutex::new(HashMap::new());
        let t0 = Utc::now();
        let origin = JobId::new();
        // Charge at t0; counter hits the cap of 1.
        assert!(try_charge_daily_cap(&daily_count, &origins, 1, origin, t0));
        assert_eq!(origins.lock().len(), 1);
        // 25 hours later the prune drops the origin from the dedupe
        // set; the daily-cap counter has also rolled over to a new day,
        // so the same origin can charge a fresh slot.
        let t1 = t0 + chrono::Duration::hours(25);
        assert!(try_charge_daily_cap(&daily_count, &origins, 1, origin, t1));
        assert_eq!(daily_count.lock().count, 1);
        assert_eq!(origins.lock().len(), 1);
    }
}
