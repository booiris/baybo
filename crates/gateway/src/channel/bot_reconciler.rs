//! Background loop that keeps each connected sidecar's bot roster in
//! sync with the `channel_bots` libsql table.
//!
//! Tokens live in the vault, rows in libsql describe which bots are
//! live for a given `channel_type`. The CLI (`aura channel
//! add/remove`) writes to those two stores directly; the gateway
//! doesn't know when that happens. This loop polls every
//! `reconcile_interval` and, per currently-connected sidecar, computes
//! the delta between what the store says is live and what we've
//! already pushed, then streams `StartBot` / `StopBot` frames to
//! catch the sidecar up.
//!
//! Tradeoffs:
//!
//! * **Latency**: up to `reconcile_interval` — a few seconds, which is
//!   fine for operator workflows. If low-latency matters, add a
//!   SIGHUP-triggered early tick; the loop is structured to support it.
//! * **Idempotency**: `StartBot` on a sidecar that already runs the
//!   bot is a no-op (the Telegram sidecar's `onStartBot` short-circuits
//!   on re-registration). So a missed tick or a cross-over with the
//!   initial-register push just turns into a harmless re-send.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use aura_agent::service::ShutdownSignal;
use aura_channels::wire::Frame;
use aura_model::ChannelType;
use aura_security::SecretVault;
use aura_storage::{ChannelBotRow, ChannelBotStore};
use parking_lot::Mutex;

use super::control::{ChannelControlError, ChannelControlRegistry};
use super::route::bot_secret_name;
use super::secrets::load_start_metadata;

/// Default polling cadence. Short enough to feel interactive for CLI
/// add/remove; long enough that the cost is negligible.
pub const DEFAULT_RECONCILE_INTERVAL: Duration = Duration::from_secs(2);

/// Bot ids the gateway has already pushed to each connected sidecar.
/// Keyed by `ChannelType` so two sidecars of different types (future
/// Discord sidecar) don't collide.
#[derive(Default)]
struct Tracked {
    per_channel: HashMap<ChannelType, HashSet<String>>,
}

impl Tracked {
    fn desired_delta(&mut self, channel_type: &ChannelType, desired: HashSet<String>) -> Delta {
        let current = self.per_channel.entry(channel_type.clone()).or_default();
        let to_start: Vec<String> = desired
            .iter()
            .filter(|id| !current.contains(*id))
            .cloned()
            .collect();
        let to_stop: Vec<String> = current
            .iter()
            .filter(|id| !desired.contains(*id))
            .cloned()
            .collect();
        *current = desired;
        Delta { to_start, to_stop }
    }

    fn forget(&mut self, channel_type: &ChannelType) {
        self.per_channel.remove(channel_type);
    }
}

struct Delta {
    to_start: Vec<String>,
    to_stop: Vec<String>,
}

/// Long-running reconciliation loop. Spawn once per gateway process.
pub struct ChannelBotReconciler {
    control: Arc<ChannelControlRegistry>,
    store: Arc<dyn ChannelBotStore>,
    vault: Arc<SecretVault>,
    tracked: Arc<Mutex<Tracked>>,
    interval: Duration,
}

impl ChannelBotReconciler {
    pub fn new(
        control: Arc<ChannelControlRegistry>,
        store: Arc<dyn ChannelBotStore>,
        vault: Arc<SecretVault>,
    ) -> Self {
        Self::with_interval(control, store, vault, DEFAULT_RECONCILE_INTERVAL)
    }

    pub fn with_interval(
        control: Arc<ChannelControlRegistry>,
        store: Arc<dyn ChannelBotStore>,
        vault: Arc<SecretVault>,
        interval: Duration,
    ) -> Self {
        Self {
            control,
            store,
            vault,
            tracked: Arc::new(Mutex::new(Tracked::default())),
            interval,
        }
    }

    /// Declare that `bot_ids` have already been streamed to the sidecar
    /// for `channel_type`. Called by the WS route after the initial
    /// `push_live_bots` burst so the first reconciler tick doesn't
    /// double-send.
    pub fn seed(&self, channel_type: ChannelType, bot_ids: Vec<String>) {
        self.tracked
            .lock()
            .per_channel
            .insert(channel_type, bot_ids.into_iter().collect());
    }

    /// Drop the cached set for `channel_type`. Called when a sidecar
    /// disconnects so the next registration starts from a clean slate.
    pub fn forget(&self, channel_type: &ChannelType) {
        self.tracked.lock().forget(channel_type);
    }

    /// Run the loop until `shutdown` fires.
    pub async fn run(self: Arc<Self>, shutdown: ShutdownSignal) {
        let mut ticker = tokio::time::interval(self.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.wait() => return,
                _ = ticker.tick() => {
                    self.reconcile_once().await;
                }
            }
        }
    }

    async fn reconcile_once(&self) {
        // Snapshot the connected channel types first so we don't hold
        // the DashMap guard across the awaits below.
        let connected: Vec<ChannelType> = self.control.connected_channel_types();
        for channel_type in connected {
            self.reconcile_channel(&channel_type).await;
        }
    }

    async fn reconcile_channel(&self, channel_type: &ChannelType) {
        let rows = match self.store.list_live(channel_type).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    %channel_type,
                    "list live bots failed; skipping reconcile tick",
                );
                return;
            }
        };
        let row_by_id: HashMap<String, ChannelBotRow> =
            rows.into_iter().map(|r| (r.bot_id.clone(), r)).collect();
        let desired: HashSet<String> = row_by_id.keys().cloned().collect();

        let delta = self.tracked.lock().desired_delta(channel_type, desired);

        for bot_id in &delta.to_start {
            let Some(row) = row_by_id.get(bot_id) else {
                // The desired set was just derived from the same map,
                // so a miss can only happen if a concurrent libsql
                // mutation raced this branch. Skip and let the next
                // tick reconverge.
                continue;
            };
            let Some(token) = self.load_token(channel_type, bot_id).await else {
                // Mark the bot as not-yet-sent so a later tick retries
                // (the token might have been inserted between the row
                // insert and the vault write — racy CLI writes).
                self.tracked
                    .lock()
                    .per_channel
                    .entry(channel_type.clone())
                    .or_default()
                    .remove(bot_id);
                continue;
            };
            let metadata = load_start_metadata(&self.vault, row).await;
            let frame = Frame::StartBot {
                bot_id: bot_id.clone(),
                token,
                metadata,
            };
            self.push(channel_type, frame, bot_id, "StartBot").await;
        }
        for bot_id in &delta.to_stop {
            let frame = Frame::StopBot {
                bot_id: bot_id.clone(),
            };
            self.push(channel_type, frame, bot_id, "StopBot").await;
        }
    }

    async fn load_token(&self, channel_type: &ChannelType, bot_id: &str) -> Option<String> {
        let name = bot_secret_name(channel_type, bot_id);
        match self.vault.get_secret(&name).await {
            Ok(Some(v)) => match String::from_utf8(v.as_bytes().to_vec()) {
                Ok(s) => Some(s),
                Err(_) => {
                    tracing::warn!(
                        %channel_type,
                        %bot_id,
                        "bot token was not valid UTF-8; will retry next tick",
                    );
                    None
                }
            },
            Ok(None) => {
                tracing::warn!(
                    %channel_type,
                    %bot_id,
                    secret = %name,
                    "bot row exists but vault secret missing; will retry next tick",
                );
                None
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    %channel_type,
                    %bot_id,
                    "decrypt bot token failed; will retry next tick",
                );
                None
            }
        }
    }

    async fn push(&self, channel_type: &ChannelType, frame: Frame, bot_id: &str, label: &str) {
        match self.control.send(channel_type, frame).await {
            Ok(_) => {}
            Err(ChannelControlError::NotConnected(_)) => {
                // Raced with a disconnect; `forget` will clear the set
                // next tick after the control registry fully propagates.
                tracing::debug!(
                    %channel_type,
                    %bot_id,
                    label,
                    "sidecar disconnected mid-reconcile; skip",
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    %channel_type,
                    %bot_id,
                    label,
                    "control push failed; will retry next tick",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desired_delta_computes_add_and_remove() {
        let mut tracked = Tracked::default();
        let ct = ChannelType::telegram();
        let first = tracked.desired_delta(&ct, ["a".into(), "b".into()].into_iter().collect());
        let mut starts: Vec<&str> = first.to_start.iter().map(String::as_str).collect();
        starts.sort();
        assert_eq!(starts, vec!["a", "b"]);
        assert!(first.to_stop.is_empty());

        let second = tracked.desired_delta(&ct, ["b".into(), "c".into()].into_iter().collect());
        assert_eq!(second.to_start, vec!["c".to_string()]);
        assert_eq!(second.to_stop, vec!["a".to_string()]);

        let third = tracked.desired_delta(&ct, HashSet::new());
        let mut stops: Vec<&str> = third.to_stop.iter().map(String::as_str).collect();
        stops.sort();
        assert_eq!(stops, vec!["b", "c"]);
        assert!(third.to_start.is_empty());
    }

    #[test]
    fn forget_clears_channel_state() {
        let mut tracked = Tracked::default();
        let ct = ChannelType::telegram();
        tracked.desired_delta(&ct, ["a".into()].into_iter().collect());
        tracked.forget(&ct);
        // After forget, the next desired_delta treats `a` as new again.
        let delta = tracked.desired_delta(&ct, ["a".into()].into_iter().collect());
        assert_eq!(delta.to_start, vec!["a".to_string()]);
        assert!(delta.to_stop.is_empty());
    }
}
