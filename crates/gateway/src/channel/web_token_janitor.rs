//! TTL reaper for the web-chat token stash.
//!
//! The admin chat handler mints a [`crate::auth::TokenHandle`] and
//! stashes it in `web_chat_tokens` keyed by the token string; the
//! channel-WS route claims it (and binds it to a [`super::adapter::
//! Sidecar`]) on the next successful upgrade. Most mints get claimed
//! within milliseconds, but a tab that closes / errors / 5-seconds
//! out between mint and upgrade strands its entry — without a sweeper
//! those strands persist for the gateway's lifetime.
//!
//! The janitor wakes up on a coarse tick, scans the map, and drops
//! any handle whose `minted_at` is older than [`DEFAULT_TTL`]. Drop
//! revokes the token from [`crate::auth::ChannelTokenTable`] via
//! `TokenHandle`'s `Drop` impl, so a reaped token can no longer
//! authenticate a WS upgrade.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aura_agent::service::ShutdownSignal;
use dashmap::DashMap;

use crate::auth::TokenHandle;

/// One stash entry. The instant lets [`WebTokenJanitor::sweep`]
/// decide when the handle has gone unclaimed long enough to reap.
pub struct StashedTokenHandle {
    pub handle: TokenHandle,
    pub minted_at: Instant,
}

impl StashedTokenHandle {
    pub fn new(handle: TokenHandle) -> Self {
        Self {
            handle,
            minted_at: Instant::now(),
        }
    }
}

/// How long a minted token can sit unclaimed before the janitor
/// reaps it. Generous compared to the natural mint→WS latency
/// (sub-second on loopback) so a slow page-load doesn't lose its
/// credential, but bounded enough that an abandoned-tab leak can't
/// grow without bound.
pub const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);

/// How often the janitor wakes up to scan the stash. Coarse — the
/// upper bound on a reap's lateness is `tick + TTL`, which at the
/// defaults is ~6 minutes; well below "user notices the leak".
pub const DEFAULT_TICK: Duration = Duration::from_secs(60);

/// Long-lived task that reaps stale entries from the web-chat token
/// stash. One per gateway process; constructed in the bin and spawned
/// alongside the other gateway loops on the shared
/// [`ShutdownSignal`].
pub struct WebTokenJanitor {
    stash: Arc<DashMap<String, StashedTokenHandle>>,
    ttl: Duration,
    tick: Duration,
}

impl WebTokenJanitor {
    pub fn new(stash: Arc<DashMap<String, StashedTokenHandle>>) -> Arc<Self> {
        Self::with_intervals(stash, DEFAULT_TTL, DEFAULT_TICK)
    }

    pub fn with_intervals(
        stash: Arc<DashMap<String, StashedTokenHandle>>,
        ttl: Duration,
        tick: Duration,
    ) -> Arc<Self> {
        Arc::new(Self { stash, ttl, tick })
    }

    /// Run the reaper loop until `shutdown` fires. The first tick
    /// fires immediately (tokio's default `interval` behaviour), then
    /// every [`Self::tick`] thereafter. Missed ticks are absorbed
    /// (`Delay`) so a slow sweep doesn't burst-fire a queue of
    /// catch-up ticks once the system unblocks.
    pub async fn run(self: Arc<Self>, shutdown: ShutdownSignal) {
        let mut ticker = tokio::time::interval(self.tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.wait() => {
                    tracing::debug!("web_token_janitor: shutdown signal received");
                    break;
                }
                _ = ticker.tick() => self.sweep(),
            }
        }
    }

    /// Drop every entry whose `minted_at` is older than `ttl`. Drop
    /// triggers `TokenHandle::drop` → token is revoked from the live
    /// table. `pub(crate)` so tests can drive the reaper without
    /// spawning the loop.
    pub(crate) fn sweep(&self) {
        let now = Instant::now();
        let mut reaped = 0usize;
        self.stash.retain(|token, entry| {
            let age = now.duration_since(entry.minted_at);
            if age >= self.ttl {
                tracing::debug!(
                    token_prefix = &token[..token.len().min(8)],
                    age_secs = age.as_secs(),
                    "web_chat_tokens: reaping unclaimed handle",
                );
                reaped += 1;
                false
            } else {
                true
            }
        });
        if reaped > 0 {
            tracing::info!(
                reaped,
                remaining = self.stash.len(),
                "web_chat_tokens: TTL sweep dropped unclaimed handles",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{ChannelTokenTable, ClientIdentity};

    fn ident(label: &str) -> ClientIdentity {
        ClientIdentity {
            pid: 1,
            label: label.into(),
            bound_channel_type: None,
        }
    }

    #[test]
    fn sweep_drops_only_expired_entries() {
        let tokens = ChannelTokenTable::new();
        let stash: Arc<DashMap<String, StashedTokenHandle>> = Arc::new(DashMap::new());

        let fresh = tokens.mint(ident("fresh"));
        let fresh_tok = fresh.token().to_owned();
        stash.insert(fresh_tok.clone(), StashedTokenHandle::new(fresh));

        let stale = tokens.mint(ident("stale"));
        let stale_tok = stale.token().to_owned();
        stash.insert(
            stale_tok.clone(),
            StashedTokenHandle {
                handle: stale,
                minted_at: Instant::now() - Duration::from_secs(120),
            },
        );

        let janitor = WebTokenJanitor::with_intervals(
            Arc::clone(&stash),
            Duration::from_secs(60),
            // tick unused — we drive `sweep()` directly.
            Duration::from_secs(60),
        );
        janitor.sweep();

        assert!(stash.contains_key(&fresh_tok), "fresh entry survives");
        assert!(!stash.contains_key(&stale_tok), "stale entry reaped");
        assert!(
            tokens.lookup(&fresh_tok).is_some(),
            "fresh token still live in the table",
        );
        assert!(
            tokens.lookup(&stale_tok).is_none(),
            "stale token revoked after reap",
        );
    }
}
