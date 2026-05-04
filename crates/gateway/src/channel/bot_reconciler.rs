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

use std::collections::HashMap;
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

/// What the reconciler is waiting on for a given bot. `None` means
/// the entry is settled and the next tick can issue a new operation
/// if `applied` doesn't match `desired`. Non-`None` blocks new emits
/// until a `BotStatus` ack arrives or the sidecar disconnects (which
/// `forget` collapses).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// `StartBot` sent for the given revision. Positive ack moves
    /// `applied` to that revision; negative ack clears `pending` so
    /// the next tick retries.
    Start(i64),
    /// `StopBot` sent because we want to rotate to a new revision.
    /// The target revision lives on the libsql row, not here — once
    /// the stop acks we set `applied = 0` and the next tick
    /// re-derives the start from `desired`.
    StopForRotate,
    /// `StopBot` sent because the bot is no longer in the desired
    /// roster (operator removal). Positive ack drops the entry
    /// entirely; negative ack clears `pending` so the next tick
    /// retries the stop.
    StopForDetach,
}

#[derive(Debug, Default)]
struct TrackedEntry {
    /// Last revision the sidecar positively acked. `0` means the bot
    /// is not currently running on the sidecar (never started, or
    /// stopped via a successful detach).
    applied: i64,
    /// In-flight operation; `None` means no current emit awaiting an
    /// ack and the reconciler is free to issue the next op.
    pending: Option<Pending>,
}

/// Per-channel tracking. Keyed by `ChannelType` so a future Discord
/// sidecar can't collide with the Telegram one. The reconciler only
/// advances `applied` after a positive `BotStatus`; a transient sidecar
/// failure (bad token, network blip) leaves `applied < desired` and
/// the next tick retries instead of declaring the rotation done.
#[derive(Default)]
struct Tracked {
    per_channel: HashMap<ChannelType, HashMap<String, TrackedEntry>>,
}

impl Tracked {
    fn desired_delta(
        &mut self,
        channel_type: &ChannelType,
        desired: HashMap<String, i64>,
    ) -> Delta {
        let current = self.per_channel.entry(channel_type.clone()).or_default();
        let mut to_start: Vec<String> = Vec::new();
        let mut to_stop: Vec<String> = Vec::new();

        for (id, desired_rev) in &desired {
            let entry = current.entry(id.clone()).or_default();
            if entry.pending.is_some() {
                // In-flight; wait for the ack before issuing more.
                continue;
            }
            if entry.applied == 0 {
                // Never started (or last detach completed). Emit Start.
                to_start.push(id.clone());
                entry.pending = Some(Pending::Start(*desired_rev));
            } else if entry.applied < *desired_rev {
                // Rotation: stop first, the next tick will see
                // applied == 0 and emit Start with the new revision.
                to_stop.push(id.clone());
                entry.pending = Some(Pending::StopForRotate);
            }
            // applied >= desired_rev: caught up, no-op.
        }

        // Bots no longer in the desired set: emit Stop (Detach) if
        // the bot is currently running.
        for (id, entry) in current.iter_mut() {
            if desired.contains_key(id) || entry.pending.is_some() {
                continue;
            }
            if entry.applied > 0 {
                to_stop.push(id.clone());
                entry.pending = Some(Pending::StopForDetach);
            }
        }

        // Drop entries that are fully settled and no longer desired
        // (applied=0, pending=None, not in desired). Otherwise the
        // map grows monotonically as bots come and go.
        current.retain(|id, entry| {
            desired.contains_key(id) || entry.applied > 0 || entry.pending.is_some()
        });

        Delta { to_start, to_stop }
    }

    /// Apply a `BotStatus` ack to the tracked entry. Returns whether
    /// the entry was found (the sidecar can ack a bot the reconciler
    /// no longer tracks if the operator removed it concurrently —
    /// dropped silently).
    fn record_ack(&mut self, channel_type: &ChannelType, bot_id: &str, ok: bool) -> bool {
        let Some(channel_state) = self.per_channel.get_mut(channel_type) else {
            return false;
        };
        let Some(entry) = channel_state.get_mut(bot_id) else {
            return false;
        };
        let Some(pending) = entry.pending.take() else {
            return false;
        };
        if !ok {
            // Negative ack: clear pending, leave applied alone, let
            // the next tick retry. Repeated failures produce a
            // tick-cadence stream of warnings — exactly the signal
            // the operator needs to debug the credential.
            return true;
        }
        match pending {
            Pending::Start(rev) => entry.applied = rev,
            Pending::StopForRotate => entry.applied = 0,
            Pending::StopForDetach => {
                channel_state.remove(bot_id);
            }
        }
        true
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

    /// Declare that `bots` (id → revision) have already been streamed
    /// to the sidecar for `channel_type` and the sidecar will reply
    /// with `BotStatus` per entry. Called by the WS route after the
    /// initial `push_live_bots` burst so the first reconciler tick
    /// doesn't double-send. Each seed entry starts with
    /// `applied = 0, pending = Start(rev)` — the sidecar's `BotStatus`
    /// ack will move them into `applied = rev`.
    pub fn seed(&self, channel_type: ChannelType, bots: Vec<(String, i64)>) {
        let entries = bots
            .into_iter()
            .map(|(id, rev)| {
                (
                    id,
                    TrackedEntry {
                        applied: 0,
                        pending: Some(Pending::Start(rev)),
                    },
                )
            })
            .collect();
        self.tracked
            .lock()
            .per_channel
            .insert(channel_type, entries);
    }

    /// Drop the cached set for `channel_type`. Called when a sidecar
    /// disconnects so the next registration starts from a clean slate.
    pub fn forget(&self, channel_type: &ChannelType) {
        self.tracked.lock().forget(channel_type);
    }

    /// Clear an in-flight pending op without applying it. Called when
    /// the reconciler couldn't actually push the frame (vault token
    /// missing, control-plane send failure) so the next tick retries
    /// instead of waiting on an ack that will never come.
    fn clear_pending(&self, channel_type: &ChannelType, bot_id: &str) {
        let mut t = self.tracked.lock();
        if let Some(channel_state) = t.per_channel.get_mut(channel_type)
            && let Some(entry) = channel_state.get_mut(bot_id)
        {
            entry.pending = None;
            // If applied=0 (the entry was newly created for this op)
            // and pending is now None too, drop it so the next tick
            // treats this bot as fresh.
            if entry.applied == 0 {
                channel_state.remove(bot_id);
            }
        }
    }

    /// Apply a `BotStatus` ack from the sidecar. Called from the WS
    /// inbound loop. Acks for entries the reconciler no longer tracks
    /// (operator removed concurrently with the in-flight op) are
    /// dropped silently.
    pub fn record_ack(&self, channel_type: &ChannelType, bot_id: &str, ok: bool) {
        self.tracked.lock().record_ack(channel_type, bot_id, ok);
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
        let desired: HashMap<String, i64> = row_by_id
            .iter()
            .map(|(id, row)| (id.clone(), row.revision))
            .collect();

        let delta = self.tracked.lock().desired_delta(channel_type, desired);

        for bot_id in &delta.to_start {
            let Some(row) = row_by_id.get(bot_id) else {
                // The desired set was just derived from the same map,
                // so a miss can only happen if a concurrent libsql
                // mutation raced this branch. Clear the pending we
                // just speculatively recorded; the next tick will
                // reconverge.
                self.clear_pending(channel_type, bot_id);
                continue;
            };
            let Some(token) = self.load_token(channel_type, bot_id).await else {
                self.clear_pending(channel_type, bot_id);
                continue;
            };
            let metadata = load_start_metadata(&self.vault, row).await;
            let frame = Frame::StartBot {
                bot_id: bot_id.clone(),
                token,
                metadata,
            };
            if !self.push(channel_type, frame, bot_id, "StartBot").await {
                self.clear_pending(channel_type, bot_id);
            }
        }
        for bot_id in &delta.to_stop {
            let frame = Frame::StopBot {
                bot_id: bot_id.clone(),
            };
            if !self.push(channel_type, frame, bot_id, "StopBot").await {
                self.clear_pending(channel_type, bot_id);
            }
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

    async fn push(
        &self,
        channel_type: &ChannelType,
        frame: Frame,
        bot_id: &str,
        label: &str,
    ) -> bool {
        match self.control.send(channel_type, frame).await {
            Ok(_) => true,
            Err(ChannelControlError::NotConnected(_)) => {
                // Raced with a disconnect; `forget` will clear the set
                // next tick after the control registry fully propagates.
                tracing::debug!(
                    %channel_type,
                    %bot_id,
                    label,
                    "sidecar disconnected mid-reconcile; skip",
                );
                false
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    %channel_type,
                    %bot_id,
                    label,
                    "control push failed; will retry next tick",
                );
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired<const N: usize>(items: [(&str, i64); N]) -> HashMap<String, i64> {
        items.into_iter().map(|(k, v)| (k.into(), v)).collect()
    }

    #[test]
    fn first_tick_emits_starts_and_marks_pending() {
        let mut tracked = Tracked::default();
        let ct = ChannelType::telegram();
        let first = tracked.desired_delta(&ct, desired([("a", 1), ("b", 1)]));
        let mut starts: Vec<&str> = first.to_start.iter().map(String::as_str).collect();
        starts.sort();
        assert_eq!(starts, vec!["a", "b"]);
        assert!(first.to_stop.is_empty());

        // Second tick before any ack: pending blocks re-emit.
        let same = tracked.desired_delta(&ct, desired([("a", 1), ("b", 1)]));
        assert!(same.to_start.is_empty());
        assert!(same.to_stop.is_empty());
    }

    #[test]
    fn negative_ack_clears_pending_and_next_tick_retries() {
        // Codex review regression: a transient StartBot failure (bad
        // app_secret, network blip, sidecar bug) used to be logged-
        // only and the reconciler then optimistically declared the
        // bot done. The bot would stay offline until the operator
        // re-registered.
        let mut tracked = Tracked::default();
        let ct = ChannelType::telegram();
        tracked.desired_delta(&ct, desired([("a", 1)]));

        // Sidecar acks failure.
        assert!(tracked.record_ack(&ct, "a", false));

        // Next tick re-emits Start.
        let retry = tracked.desired_delta(&ct, desired([("a", 1)]));
        assert_eq!(retry.to_start, vec!["a".to_string()]);
    }

    #[test]
    fn positive_ack_advances_applied_revision() {
        let mut tracked = Tracked::default();
        let ct = ChannelType::telegram();
        tracked.desired_delta(&ct, desired([("a", 1)]));
        assert!(tracked.record_ack(&ct, "a", true));

        // Same revision: caught up, no-op.
        let same = tracked.desired_delta(&ct, desired([("a", 1)]));
        assert!(same.to_start.is_empty());
        assert!(same.to_stop.is_empty());
    }

    #[test]
    fn rotation_runs_stop_then_start_across_two_ticks() {
        let mut tracked = Tracked::default();
        let ct = ChannelType::telegram();
        tracked.desired_delta(&ct, desired([("a", 1)]));
        assert!(tracked.record_ack(&ct, "a", true));

        // Tick at revision 2: emit Stop.
        let rotate = tracked.desired_delta(&ct, desired([("a", 2)]));
        assert!(rotate.to_start.is_empty());
        assert_eq!(rotate.to_stop, vec!["a".to_string()]);

        // Sidecar acks the stop. applied → 0.
        assert!(tracked.record_ack(&ct, "a", true));

        // Next tick: emit Start with the new revision.
        let restart = tracked.desired_delta(&ct, desired([("a", 2)]));
        assert_eq!(restart.to_start, vec!["a".to_string()]);
        assert!(restart.to_stop.is_empty());

        // Final ack lands at revision 2.
        assert!(tracked.record_ack(&ct, "a", true));
        let stable = tracked.desired_delta(&ct, desired([("a", 2)]));
        assert!(stable.to_start.is_empty());
        assert!(stable.to_stop.is_empty());
    }

    #[test]
    fn rotation_stop_failure_retries_stop_not_start() {
        let mut tracked = Tracked::default();
        let ct = ChannelType::telegram();
        tracked.desired_delta(&ct, desired([("a", 1)]));
        tracked.record_ack(&ct, "a", true);

        // Emit Stop for rotation.
        let rotate = tracked.desired_delta(&ct, desired([("a", 2)]));
        assert_eq!(rotate.to_stop, vec!["a".to_string()]);

        // Sidecar nacks the stop. We must retry the stop, NOT skip
        // to start (which would race the SDK's "already running"
        // short-circuit).
        tracked.record_ack(&ct, "a", false);
        let retry = tracked.desired_delta(&ct, desired([("a", 2)]));
        assert!(retry.to_start.is_empty());
        assert_eq!(retry.to_stop, vec!["a".to_string()]);
    }

    #[test]
    fn detach_runs_stop_and_drops_entry_after_ack() {
        let mut tracked = Tracked::default();
        let ct = ChannelType::telegram();
        tracked.desired_delta(&ct, desired([("a", 1)]));
        tracked.record_ack(&ct, "a", true);

        // Operator removed the bot.
        let stop = tracked.desired_delta(&ct, HashMap::new());
        assert_eq!(stop.to_stop, vec!["a".to_string()]);

        // Sidecar acks the stop. Entry is dropped.
        tracked.record_ack(&ct, "a", true);
        let after = tracked.desired_delta(&ct, HashMap::new());
        assert!(after.to_stop.is_empty());

        // Re-adding produces a Start.
        let re_added = tracked.desired_delta(&ct, desired([("a", 5)]));
        assert_eq!(re_added.to_start, vec!["a".to_string()]);
    }

    #[test]
    fn record_ack_on_unknown_bot_returns_false() {
        let mut tracked = Tracked::default();
        let ct = ChannelType::telegram();
        assert!(!tracked.record_ack(&ct, "nonexistent", true));
    }

    #[test]
    fn forget_clears_channel_state() {
        let mut tracked = Tracked::default();
        let ct = ChannelType::telegram();
        tracked.desired_delta(&ct, desired([("a", 1)]));
        tracked.forget(&ct);
        let delta = tracked.desired_delta(&ct, desired([("a", 1)]));
        assert_eq!(delta.to_start, vec!["a".to_string()]);
        assert!(delta.to_stop.is_empty());
    }
}
