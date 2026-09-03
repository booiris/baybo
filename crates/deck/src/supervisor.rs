//! Always-resident card services under restart supervision.
//!
//! One management task per enabled card: spawn → serve → (crash) →
//! backoff → respawn, with the [`StrikeRecorder`] window deciding when a
//! misbehaving card stops degrading the fleet and quarantines itself
//! (service stopped, `quarantined_at` stamped by the manager, error face
//! on the phone). Pause/quarantine stop the *process* — untrusted code
//! is never trusted to rate-limit itself.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::{DeckError, Result};
use crate::service::{
    EmitSink, HostServices, RunningService, ServiceHandle, SpawnConfig, StrikeRecorder,
    spawn_service,
};

const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Uptime above which the next crash resets backoff (a long-stable
/// service that finally dies shouldn't pay the accumulated penalty).
const STABLE_UPTIME: Duration = Duration::from_secs(60);

/// Manager-side reaction to an exhausted strike budget. Async because it
/// writes the store and broadcasts `DeckChanged`.
#[async_trait]
pub(crate) trait QuarantineSink: Send + Sync + 'static {
    async fn quarantine(&self, card_id: &str, reason: &str);
}

/// What ended one turn of a card's supervision loop.
enum Outcome {
    /// Cancelled by us (stop/replace); no restart, no strike.
    Stopped,
    /// The card's own service died, or failed the SDK handshake. This is
    /// what the strike budget exists to bound.
    Crashed(String),
    /// The host could not launch the JS runtime at all. Retried forever
    /// with backoff and never quarantined — see
    /// [`crate::error::DeckError::HostToolMissing`].
    HostFault(String),
}

impl Outcome {
    /// Why the card is not running, or `None` when we stopped it
    /// ourselves and there is nothing to report or retry.
    fn failure(&self) -> Option<&str> {
        match self {
            Outcome::Stopped => None,
            Outcome::Crashed(reason) | Outcome::HostFault(reason) => Some(reason),
        }
    }

    /// Whether this turn costs the card a strike.
    ///
    /// A host fault never does. The card's code did not run, the same
    /// fault hits every card on the box, and quarantining fixes none of
    /// it — it just writes durable state the operator has to undo by
    /// hand after repairing the host.
    fn spends_strike(&self) -> bool {
        matches!(self, Outcome::Crashed(_))
    }
}

/// The reason stamped on a quarantined card. The budget running out is
/// the trigger, not the explanation — carry the failure that actually
/// caused it, because this string is what the operator sees on the
/// card's error face and it is the only copy that outlives the log.
fn quarantine_reason(cause: &str) -> String {
    format!("crash budget exhausted: {cause}")
}

struct ServiceEntry {
    slot: Arc<Mutex<Option<ServiceHandle>>>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

pub(crate) struct DeckSupervisor {
    host: Arc<dyn HostServices>,
    quarantine: Arc<dyn QuarantineSink>,
    process_manager: Arc<baybo_process::ProcessManager>,
    /// Per-card scratch dirs live under here.
    scratch_root: PathBuf,
    services: Mutex<HashMap<String, ServiceEntry>>,
}

impl DeckSupervisor {
    pub fn new(
        host: Arc<dyn HostServices>,
        quarantine: Arc<dyn QuarantineSink>,
        process_manager: Arc<baybo_process::ProcessManager>,
        scratch_root: PathBuf,
    ) -> Self {
        Self {
            host,
            quarantine,
            process_manager,
            scratch_root,
            services: Mutex::new(HashMap::new()),
        }
    }

    /// Start (or restart) the supervision loop for one card. Replaces any
    /// existing loop for the id.
    pub async fn start(
        &self,
        card_id: &str,
        bundle_dir: PathBuf,
        emit_interval: Duration,
        emit_sink: Arc<dyn EmitSink>,
    ) {
        self.stop(card_id).await;

        let slot: Arc<Mutex<Option<ServiceHandle>>> = Arc::new(Mutex::new(None));
        let cancel = CancellationToken::new();
        let host = self.host.clone();
        let quarantine = self.quarantine.clone();
        let scratch_dir = self.scratch_root.join(card_id);
        let process_manager = Arc::clone(&self.process_manager);
        let card_id = card_id.to_string();
        let service_id = card_id.clone();
        let task_slot = Arc::clone(&slot);
        let task_cancel = cancel.clone();

        let task = tokio::spawn(async move {
            let strikes = Arc::new(StrikeRecorder::default());
            let mut backoff = BACKOFF_MIN;
            // Why the card is not running right now, in the words the
            // operator needs. Carried to the quarantine sink so the
            // stored reason names the actual cause instead of the
            // budget that ran out, and used to keep a stuck host from
            // reprinting one identical line every backoff tick.
            let mut last_failure: Option<String> = None;
            loop {
                if task_cancel.is_cancelled() {
                    break;
                }
                let cfg = SpawnConfig {
                    card_id: card_id.clone(),
                    // A live service's uploader identity IS its process id;
                    // only the dry-run gate splits them.
                    uploader_card_id: card_id.clone(),
                    bundle_dir: bundle_dir.clone(),
                    scratch_dir: scratch_dir.clone(),
                    emit_interval,
                    process_manager: Arc::clone(&process_manager),
                };
                let started = Instant::now();
                let spawn =
                    spawn_service(cfg, host.clone(), emit_sink.clone(), strikes.clone()).await;
                let outcome = match spawn {
                    Ok(RunningService {
                        handle,
                        mut exited,
                        kill,
                    }) => {
                        *task_slot.lock() = Some(handle);
                        let died = tokio::select! {
                            code = &mut exited => Some(format!("service exited ({code:?})")),
                            _ = task_cancel.cancelled() => {
                                let _ = kill.send(()).await;
                                let _ = exited.await;
                                None
                            }
                        };
                        *task_slot.lock() = None;
                        match died {
                            Some(reason) => Outcome::Crashed(reason),
                            None => Outcome::Stopped,
                        }
                    }
                    // The card's code never loaded, so this says nothing
                    // about the card — it is the host's fault and every
                    // card on the box fails identically. Back off and keep
                    // retrying, but spend no strikes: see
                    // `DeckError::HostToolMissing`.
                    Err(e @ DeckError::HostToolMissing(_)) => Outcome::HostFault(e.to_string()),
                    Err(e) => Outcome::Crashed(e.to_string()),
                };
                let Some(failure) = outcome.failure() else {
                    break;
                };
                if started.elapsed() >= STABLE_UPTIME {
                    backoff = BACKOFF_MIN;
                    // Ran fine for a while first, so this is a fresh
                    // problem however familiar the text — say so at WARN
                    // rather than deduping it against hours-old history.
                    last_failure = None;
                }
                // A host that stays broken would otherwise reprint the
                // same line every backoff tick, forever.
                if last_failure.as_deref() == Some(failure) {
                    tracing::debug!(card = %card_id, "deck service still down: {failure}");
                } else {
                    tracing::warn!(card = %card_id, "deck service down: {failure}");
                }
                if outcome.spends_strike() && strikes.record_crash() {
                    quarantine
                        .quarantine(&card_id, &quarantine_reason(failure))
                        .await;
                    break;
                }
                last_failure = Some(failure.to_string());
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = task_cancel.cancelled() => break,
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        });
        self.services
            .lock()
            .insert(service_id, ServiceEntry { slot, cancel, task });
    }

    /// Stop one card's loop and kill its process. Idempotent.
    pub async fn stop(&self, card_id: &str) {
        let entry = self.services.lock().remove(card_id);
        if let Some(entry) = entry {
            entry.cancel.cancel();
            let _ = entry.task.await;
        }
    }

    pub async fn stop_all(&self) {
        let entries: Vec<ServiceEntry> = {
            let mut map = self.services.lock();
            map.drain().map(|(_, e)| e).collect()
        };
        for e in entries {
            e.cancel.cancel();
            let _ = e.task.await;
        }
    }

    /// Route one validated op call to the card's running service. A
    /// supervised-but-not-yet-ready service (just installed / enabled /
    /// mid-restart) is awaited briefly so an install-then-tap doesn't
    /// race the child's boot.
    pub async fn call(&self, card_id: &str, op: &str, params: Value) -> Result<Value> {
        const SLOT_WAIT: Duration = Duration::from_millis(100);
        const SLOT_ATTEMPTS: u32 = 20;
        for attempt in 0..SLOT_ATTEMPTS {
            let (supervised, handle) = {
                let map = self.services.lock();
                match map.get(card_id) {
                    Some(e) => (true, e.slot.lock().clone()),
                    None => (false, None),
                }
            };
            if let Some(h) = handle {
                return h.call(op, params).await;
            }
            if !supervised {
                break;
            }
            if attempt + 1 < SLOT_ATTEMPTS {
                tokio::time::sleep(SLOT_WAIT).await;
            }
        }
        Err(DeckError::ServiceUnavailable(
            "service is not running (disabled, quarantined, or restarting)".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host that cannot launch the JS runtime must never quarantine a
    /// card. The regression this guards is real and cost a whole deck:
    /// moving the gateway under systemd changed its `PATH`, `bun` went
    /// missing, and five retries later every card on the box was
    /// disabled with `quarantined_at` stamped — durable state no
    /// operator asked for, describing a fault none of the cards had.
    #[test]
    fn host_fault_never_spends_a_strike() {
        let outcome = Outcome::HostFault("bun is not on PATH".into());
        assert!(!outcome.spends_strike());
        assert_eq!(outcome.failure(), Some("bun is not on PATH"));
    }

    /// The card's own failures are exactly what the budget is for.
    #[test]
    fn card_crash_spends_a_strike() {
        let outcome = Outcome::Crashed("service exited (Some(1))".into());
        assert!(outcome.spends_strike());
        assert_eq!(outcome.failure(), Some("service exited (Some(1))"));
    }

    /// A deliberate stop is not a failure: nothing to log, nothing to
    /// retry, no strike.
    #[test]
    fn deliberate_stop_is_not_a_failure() {
        assert_eq!(Outcome::Stopped.failure(), None);
        assert!(!Outcome::Stopped.spends_strike());
    }

    /// The stored reason must name the cause, not just the trigger.
    /// "crash budget exhausted" on its own is what the operator reads
    /// off the card's error face, and it explains nothing.
    #[test]
    fn quarantine_reason_carries_the_cause() {
        let reason = quarantine_reason("failed to launch `bun` (No such file or directory)");
        assert!(
            reason.contains("failed to launch `bun`"),
            "quarantine reason dropped the cause: {reason}"
        );
    }
}
