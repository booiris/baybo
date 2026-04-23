//! Lifecycle loop for embedded sidecars.
//!
//! One supervised task per channel type: materialise a
//! `tokio::process::Command` pointed at `bun <bundle>`, hand it to
//! [`ChannelSpawner`] (which injects the channel WS URL + a fresh
//! capability token via env vars — see `spawn.rs`), and wait on the
//! child. On
//! exit, back off and restart. On shutdown, SIGKILL the child and
//! exit.
//!
//! Backoff is exponential with a 30s cap. A child that ran for ≥60s
//! before dying resets backoff so a long-stable sidecar that finally
//! crashes restarts immediately.

use std::sync::Arc;
use std::time::{Duration, Instant};

use aura_agent::service::ShutdownSignal;
use tokio::process::Command;
use tokio::time::sleep;

use crate::spawn::ChannelSpawner;

use super::assets::SidecarRuntime;

const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// Uptime above which a child's next crash resets backoff to the
/// minimum. Prevents a sidecar that's been stable for hours from
/// taking 30s to come back after a transient failure.
const UPTIME_RESET_THRESHOLD: Duration = Duration::from_secs(60);

/// Drives the restart loop for every embedded sidecar. Clone-cheap
/// (everything is `Arc`).
#[derive(Clone)]
pub struct SidecarSupervisor {
    runtime: Arc<SidecarRuntime>,
    spawner: ChannelSpawner,
}

impl SidecarSupervisor {
    pub fn new(runtime: Arc<SidecarRuntime>, spawner: ChannelSpawner) -> Self {
        Self { runtime, spawner }
    }

    /// Spawn one supervising task per channel type in `channel_types`
    /// that this build actually embeds. Unknown or unembedded types
    /// log a warning and are dropped. Returns when `shutdown` fires
    /// and every child has been signalled + awaited.
    pub async fn run(self, shutdown: ShutdownSignal, channel_types: Vec<String>) {
        let this = Arc::new(self);
        let mut tasks = Vec::new();
        for channel_type in channel_types {
            if this.runtime.bundle_for(&channel_type).is_none() {
                tracing::warn!(
                    %channel_type,
                    "channel type not embedded in this build; sidecar not started",
                );
                continue;
            }
            let sv = Arc::clone(&this);
            let sd = shutdown.clone();
            tasks.push(tokio::spawn(async move {
                sv.supervise_one(channel_type, sd).await;
            }));
        }
        shutdown.wait().await;
        for t in tasks {
            let _ = t.await;
        }
    }

    async fn supervise_one(self: Arc<Self>, channel_type: String, shutdown: ShutdownSignal) {
        let bundle = match self.runtime.bundle_for(&channel_type) {
            Some(p) => p.to_owned(),
            None => return,
        };
        let bun = self.runtime.bun_path().to_owned();

        let mut backoff = BACKOFF_MIN;
        while !shutdown.is_shutdown() {
            let mut cmd = Command::new(&bun);
            cmd.arg(&bundle);
            // Keep child stdout/stderr inherited so its logs show up
            // in the gateway's terminal during development. In
            // production the service manager captures these; we don't
            // need to plumb them through the LogBuffer because the
            // sidecar SDK already forwards its own tracing via
            // `Frame::SidecarLog` over the channel WS.
            let handle = match self.spawner.spawn(cmd, format!("sidecar-{channel_type}")) {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!(
                        %channel_type,
                        error = %e,
                        "spawn sidecar failed; backing off",
                    );
                    if !wait_or_shutdown(&shutdown, backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    continue;
                }
            };
            let pid = handle.pid();
            tracing::info!(
                %channel_type,
                pid,
                bundle = %bundle.display(),
                "sidecar spawned",
            );

            let started = Instant::now();
            let mut handle = handle;
            tokio::select! {
                _ = shutdown.wait() => {
                    // Graceful: try TERM via kill() (tokio sends
                    // SIGKILL; that's acceptable — the sidecar owns
                    // nothing persistent and the WS peer notices the
                    // drop immediately).
                    if let Err(e) = handle.child_mut().start_kill() {
                        tracing::debug!(%channel_type, pid, error = %e, "start_kill failed");
                    }
                    let _ = handle.child_mut().wait().await;
                    tracing::info!(%channel_type, pid, "sidecar stopped on shutdown");
                    return;
                }
                status = handle.child_mut().wait() => {
                    match status {
                        Ok(s) => tracing::warn!(
                            %channel_type,
                            pid,
                            exit = ?s,
                            uptime_ms = started.elapsed().as_millis() as u64,
                            "sidecar exited; restarting after backoff",
                        ),
                        Err(e) => tracing::warn!(
                            %channel_type,
                            pid,
                            error = %e,
                            "sidecar wait failed; restarting",
                        ),
                    }
                }
            }
            drop(handle); // revokes the token early

            if started.elapsed() >= UPTIME_RESET_THRESHOLD {
                backoff = BACKOFF_MIN;
            }
            if !wait_or_shutdown(&shutdown, backoff).await {
                return;
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

/// Sleep `dur` but wake early on shutdown. Returns `false` if shutdown
/// fired (caller should stop looping).
async fn wait_or_shutdown(shutdown: &ShutdownSignal, dur: Duration) -> bool {
    tokio::select! {
        _ = shutdown.wait() => false,
        _ = sleep(dur) => true,
    }
}
