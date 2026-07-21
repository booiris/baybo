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

struct ServiceEntry {
    slot: Arc<Mutex<Option<ServiceHandle>>>,
    cancel: CancellationToken,
}

pub(crate) struct DeckSupervisor {
    host: Arc<dyn HostServices>,
    emit_sink: Arc<dyn EmitSink>,
    quarantine: Arc<dyn QuarantineSink>,
    /// Per-card scratch dirs live under here.
    scratch_root: PathBuf,
    services: Mutex<HashMap<String, ServiceEntry>>,
}

impl DeckSupervisor {
    pub fn new(
        host: Arc<dyn HostServices>,
        emit_sink: Arc<dyn EmitSink>,
        quarantine: Arc<dyn QuarantineSink>,
        scratch_root: PathBuf,
    ) -> Self {
        Self {
            host,
            emit_sink,
            quarantine,
            scratch_root,
            services: Mutex::new(HashMap::new()),
        }
    }

    /// Start (or restart) the supervision loop for one card. Replaces any
    /// existing loop for the id.
    pub async fn start(&self, card_id: &str, bundle_dir: PathBuf, emit_interval: Duration) {
        self.stop(card_id).await;

        let slot: Arc<Mutex<Option<ServiceHandle>>> = Arc::new(Mutex::new(None));
        let cancel = CancellationToken::new();
        self.services.lock().insert(
            card_id.to_string(),
            ServiceEntry {
                slot: slot.clone(),
                cancel: cancel.clone(),
            },
        );

        let host = self.host.clone();
        let emit_sink = self.emit_sink.clone();
        let quarantine = self.quarantine.clone();
        let scratch_dir = self.scratch_root.join(card_id);
        let card_id = card_id.to_string();

        tokio::spawn(async move {
            let strikes = Arc::new(StrikeRecorder::default());
            let mut backoff = BACKOFF_MIN;
            loop {
                if cancel.is_cancelled() {
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
                };
                let started = Instant::now();
                let spawn =
                    spawn_service(cfg, host.clone(), emit_sink.clone(), strikes.clone()).await;
                let crashed = match spawn {
                    Ok(RunningService {
                        handle,
                        exited,
                        kill,
                    }) => {
                        *slot.lock() = Some(handle);
                        let died = tokio::select! {
                            code = exited => {
                                tracing::warn!(card = %card_id, code = ?code, "deck service exited");
                                true
                            }
                            _ = cancel.cancelled() => {
                                let _ = kill.send(()).await;
                                false
                            }
                        };
                        *slot.lock() = None;
                        died
                    }
                    Err(e) => {
                        tracing::warn!(card = %card_id, "deck service spawn failed: {e}");
                        true
                    }
                };
                if !crashed {
                    break;
                }
                if started.elapsed() >= STABLE_UPTIME {
                    backoff = BACKOFF_MIN;
                }
                if strikes.record_crash() {
                    quarantine
                        .quarantine(&card_id, "crash budget exhausted")
                        .await;
                    break;
                }
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = cancel.cancelled() => break,
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        });
    }

    /// Stop one card's loop and kill its process. Idempotent.
    pub async fn stop(&self, card_id: &str) {
        let entry = self.services.lock().remove(card_id);
        if let Some(entry) = entry {
            entry.cancel.cancel();
        }
    }

    pub async fn stop_all(&self) {
        let entries: Vec<ServiceEntry> = {
            let mut map = self.services.lock();
            map.drain().map(|(_, e)| e).collect()
        };
        for e in entries {
            e.cancel.cancel();
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
