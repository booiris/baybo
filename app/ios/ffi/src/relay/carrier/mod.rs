//! The phone (P) side of a relay binding's direct carriers: see
//! `docs/modules/mobile/direct-carriers.md`.
//!
//! - [`state`]: the pure state machine — single-flight probes, the epoch, the
//!   per-network failure cache, suspension and cool-off.
//! - [`network`]: the primary interface's fingerprint, the network key, and
//!   the path a probe runs on.
//! - [`probe`]: one probe, from the offer through C to the proof leg.
//! - [`quic`]: the live carrier's handle and the legs dialed on it.
//!
//! This file is the hub: the one [`CarrierHub`] the app holds, which runs
//! probes off the critical path, schedules their retries, applies every
//! carrier transition — each one invalidates the API leg pool — and reports
//! state to Swift's [`CarrierSink`]. Nothing waits on a probe: every leg dials
//! the relay unless a proven carrier is live.

mod network;
mod probe;
mod quic;
mod state;

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use carrier::interfaces;
use parking_lot::Mutex;
use tokio::sync::watch;
use tokio::task::AbortHandle;

use self::network::{LocalPath, NetworkKey, primary_addresses};
use self::probe::{ProbeRun, label_str};
use self::state::{
    CarrierState, Finished, PathChange, PathState, ProbeEnd, ProbeTicket, Reproof, Reproved,
    Retired,
};
use super::leg_pool::{BindingKey, PooledLeg, pool};
use super::pairing::{PairedRecord, load_paired_record};
use super::tunnel::LegIo;
use crate::api::ConnectionLogStage as Stage;
use crate::api::{CarrierSink, CarrierStatus, NetworkPath, ProbeReport};
use crate::binding::{ActiveLeg, active_leg};
use crate::connection_diagnostics::record as trace;

pub(crate) use self::quic::{CarrierHandle, DialFailure, dial_leg};
pub(crate) use self::state::{Lease, Trigger};

/// Probes start this long after the last path change, so a network still
/// settling (Wi-Fi associating, DHCP) is probed once, on its final shape.
pub(crate) const NETWORK_SETTLE: Duration = Duration::from_secs(1);

pub(crate) struct CarrierHub {
    inner: Mutex<Inner>,
    /// Bumped on every carrier transition and every epoch change. The chat
    /// supervisor watches it to start or abandon a rotation.
    generation: watch::Sender<u64>,
}

struct Inner {
    state: CarrierState<CarrierHandle>,
    probe_task: Option<(u64, AbortHandle)>,
    settle_task: Option<AbortHandle>,
    last_probe: Option<ProbeReport>,
    sink: Option<Arc<dyn CarrierSink>>,
}

pub(crate) fn hub() -> &'static CarrierHub {
    static HUB: OnceLock<CarrierHub> = OnceLock::new();
    HUB.get_or_init(|| CarrierHub {
        inner: Mutex::new(Inner {
            state: CarrierState::default(),
            probe_task: None,
            settle_task: None,
            last_probe: None,
            sink: None,
        }),
        generation: watch::channel(0).0,
    })
}

/// The pairing a probe or a re-proof runs for: only a relay binding with a
/// relay route has direct carriers.
fn relay_record() -> Option<Arc<PairedRecord>> {
    if !matches!(active_leg(), Ok(ActiveLeg::Relay)) {
        return None;
    }
    load_paired_record()
        .ok()
        .flatten()
        .filter(|record| !record.relay_node_id.is_empty() && !record.relay_url.is_empty())
        .map(Arc::new)
}

fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

impl CarrierHub {
    /// The live carrier for `binding`, when a leg may dial it: proven, not
    /// suspended, not cooling off.
    pub(crate) fn lease(&self, binding: &BindingKey) -> Option<Lease<CarrierHandle>> {
        self.inner.lock().state.lease(binding, Instant::now())
    }

    /// Watches carrier transitions and epoch changes.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }

    /// A leg dial on carrier `id` failed, and the leg went to the relay.
    /// Connection-level evidence retires the carrier; a stream-level failure
    /// pauses new carrier legs for `CARRIER_DIAL_COOLOFF`.
    pub(crate) fn dial_failed(&'static self, id: u64, failure: &DialFailure) {
        let now = Instant::now();
        match failure {
            DialFailure::Connection(reason) => {
                let retired = self.inner.lock().state.dial_failed(id, true, now);
                self.retired(retired, &format!("dial: {reason}"));
            }
            DialFailure::Stream(reason) => {
                let until = {
                    let mut inner = self.inner.lock();
                    inner.state.dial_failed(id, false, now);
                    inner.state.cooloff_until(id)
                };
                if let Some(at) = until {
                    crate::runtime::runtime().spawn(async move {
                        tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
                        self.generation.send_modify(|generation| *generation += 1);
                    });
                }
                log::info!("carrier_cooloff reason=\"{reason}\"");
            }
            DialFailure::Withdrawn => {}
        }
    }

    /// Ask for a probe. It runs in the background, and only when the binding
    /// is on the relay with nothing live or in flight.
    pub(crate) fn request_probe(&'static self, trigger: Trigger) {
        crate::runtime::runtime().spawn(self.try_probe(trigger));
    }

    /// One `NWPathMonitor` delivery, handled before this returns: a primary
    /// change retires the carrier and aborts any probe synchronously. Every
    /// satisfied, non-duplicate path then asks for a probe once the path
    /// settles.
    pub(crate) fn network_changed(&'static self, path: NetworkPath) {
        trace(
            Stage::Network,
            format!(
                "path satisfied={} interface={:?} ipv4={} ipv6={}",
                path.satisfied, path.interface_kind, path.supports_ipv4, path.supports_ipv6
            ),
        );
        let satisfied = path.satisfied;
        let next = satisfied.then(|| {
            let interfaces = interfaces::enumerate();
            let network = NetworkKey::of(&path, &primary_addresses(&path, &interfaces));
            let local = LocalPath::new(&path, &interfaces);
            PathState::new(path, network, local)
        });
        let change = {
            let mut inner = self.inner.lock();
            let change = inner.state.network_changed(next, Instant::now());
            if matches!(change, PathChange::Moved(_)) {
                inner.abort_probe();
            }
            change
        };
        match change {
            PathChange::Duplicate => return,
            PathChange::Kept => {}
            PathChange::Moved(retired) => {
                self.generation.send_modify(|generation| *generation += 1);
                self.retired(retired, "network_changed");
            }
        }
        if satisfied {
            self.settle();
        }
    }

    /// The `.background` barrier: aborts any probe and suspends the carrier.
    /// Its connection and open streams are left alone, as a relay chat leg
    /// is; no new leg dials it until `.active` re-proves it.
    pub(crate) fn background(&self) {
        {
            let mut inner = self.inner.lock();
            inner.state.background();
            inner.abort_probe();
            pool().invalidate();
        }
        self.generation.send_modify(|generation| *generation += 1);
    }

    /// `.active`: re-prove a suspended carrier — its connection must still be
    /// open and a fresh proof leg must complete in time, and that leg is
    /// parked — or retire it. Then ask for a probe.
    pub(crate) fn foreground(&'static self) {
        let (suspended, pool_epoch) = {
            let mut inner = self.inner.lock();
            (inner.state.foreground(), pool().epoch())
        };
        if let Some(ticket) = suspended {
            crate::runtime::runtime().spawn(self.reprove(ticket, pool_epoch));
        } else {
            self.request_probe(Trigger::Foreground);
        }
    }

    async fn reprove(&'static self, ticket: Reproof<CarrierHandle>, pool_epoch: u64) {
        let proof = match relay_record() {
            Some(record) => probe::prove(&ticket.lease.handle, &record)
                .await
                .map(|io| (io, record)),
            None => Err("the binding is gone".to_owned()),
        };
        let verdict = self
            .inner
            .lock()
            .state
            .reproved(&ticket, proof.is_ok(), Instant::now());
        match verdict {
            Reproved::Current => {
                if let Ok((io, record)) = proof {
                    park(io, &record, pool_epoch).await;
                }
                self.generation.send_modify(|generation| *generation += 1);
            }
            Reproved::Retired(retired) => self.retired(Some(retired), "reproof_failed"),
            Reproved::Stale => {}
        }
        self.notify();
        self.request_probe(Trigger::Foreground);
    }

    /// Pairing or forgetting: the carrier, the epoch's probe, the failure
    /// cache and the last probe all go.
    pub(crate) fn clear(&'static self) {
        let retired = {
            let mut inner = self.inner.lock();
            inner.abort_probe();
            if let Some(settle) = inner.settle_task.take() {
                settle.abort();
            }
            inner.last_probe = None;
            inner.state.clear(Instant::now())
        };
        self.generation.send_modify(|generation| *generation += 1);
        self.retired(retired, "binding_changed");
        self.notify();
    }

    /// Install Swift's sink and deliver the current state to it.
    pub(crate) fn set_sink(&self, sink: Arc<dyn CarrierSink>) {
        self.inner.lock().sink = Some(sink);
        self.notify();
    }

    fn settle(&'static self) {
        let task = crate::runtime::runtime().spawn(async move {
            tokio::time::sleep(NETWORK_SETTLE).await;
            self.request_probe(Trigger::NetworkSettled);
        });
        if let Some(previous) = self.inner.lock().settle_task.replace(task.abort_handle()) {
            previous.abort();
        }
    }

    fn schedule(&'static self, at: Instant, trigger: Trigger) {
        crate::runtime::runtime().spawn(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await;
            self.request_probe(trigger);
        });
    }

    async fn try_probe(&'static self, trigger: Trigger) {
        let Some(record) = relay_record() else {
            return;
        };
        let admitted =
            self.inner
                .lock()
                .state
                .admit(BindingKey::from(&*record), trigger, Instant::now());
        let ticket = match admitted {
            Ok(ticket) => ticket,
            Err(skip) => {
                trace(
                    Stage::Probe,
                    format!("trigger={} skipped={}", trigger.as_str(), skip.as_str()),
                );
                log::debug!(
                    "direct_probe trigger={} skipped={}",
                    trigger.as_str(),
                    skip.as_str()
                );
                return;
            }
        };
        let started = Instant::now();
        trace(
            Stage::Probe,
            format!(
                "probe={} trigger={} network={:?} started",
                ticket.id,
                trigger.as_str(),
                ticket.local.kind
            ),
        );
        let task = {
            let ticket = ticket.clone();
            let record = record.clone();
            tokio::spawn(async move { probe::run(&ticket, &record).await })
        };
        {
            let mut inner = self.inner.lock();
            if inner.state.probe_in_flight() != Some(ticket.id) {
                // The epoch moved between admission and here.
                task.abort();
                return;
            }
            inner.probe_task = Some((ticket.id, task.abort_handle()));
        }
        let Ok(run) = task.await else {
            // Aborted by an epoch change, which already discarded it.
            return;
        };
        self.finish(&ticket, run, &record, started).await;
    }

    async fn finish(
        &'static self,
        ticket: &ProbeTicket,
        run: ProbeRun,
        record: &PairedRecord,
        started: Instant,
    ) {
        let ProbeRun { end, tiers, proof } = run;
        let outcome = match &end {
            ProbeEnd::Carrier { kind, .. } => label_str(*kind),
            ProbeEnd::Failed => "relay",
            ProbeEnd::RetryAfter(_) => "retry_after",
            ProbeEnd::Denied => "denied",
        };
        let carrier = match &end {
            ProbeEnd::Carrier { handle, kind, .. } => Some((handle.clone(), *kind)),
            _ => None,
        };
        let (finished, live_id, pool_epoch) = {
            let mut inner = self.inner.lock();
            if inner
                .probe_task
                .as_ref()
                .is_some_and(|(id, _)| *id == ticket.id)
            {
                inner.probe_task = None;
            }
            let finished = inner.state.finish(ticket, end, Instant::now());
            if !matches!(finished, Finished::Stale(_)) {
                inner.last_probe = Some(ProbeReport {
                    finished_at_ms: unix_ms(),
                    network: ticket.local.kind,
                    ended_on: inner.state.label(),
                    tiers: tiers.reports(),
                });
            }
            if matches!(finished, Finished::Up) {
                pool().invalidate();
            }
            (finished, inner.state.live_id(), pool().epoch())
        };
        log::info!(
            "direct_probe probe={} trigger={} network={} outcome={} elapsed_ms={} tiers=\"{}\"",
            ticket.id,
            ticket.trigger.as_str(),
            ticket.network.tag(),
            outcome,
            started.elapsed().as_millis(),
            tiers.summary()
        );
        trace(
            Stage::Probe,
            format!(
                "probe={} outcome={} elapsed_ms={} {}",
                ticket.id,
                outcome,
                started.elapsed().as_millis(),
                tiers.summary()
            ),
        );
        match finished {
            Finished::Up => {
                if let (Some((handle, kind)), Some(id)) = (carrier, live_id) {
                    log::info!("carrier_up kind={}", label_str(kind));
                    trace(Stage::Quic, format!("carrier up: {}", label_str(kind)));
                    self.generation.send_modify(|generation| *generation += 1);
                    if let Some(io) = proof {
                        park(io, record, pool_epoch).await;
                    }
                    self.watch_close(id, handle);
                }
            }
            Finished::Stale(handle) => {
                if let Some(handle) = handle {
                    handle.close("stale probe");
                }
            }
            Finished::RetryAt(at, trigger) => self.schedule(at, trigger),
        }
        self.notify();
    }

    /// Retires carrier `id` once its connection closes without P asking: a
    /// `CONNECTION_CLOSE` from A (a gateway stop, a revoked device) or a QUIC
    /// idle timeout. Its legs end with it; a chat leg on it goes through the
    /// supervisor's leg death and reconnects over the relay, whose going live
    /// asks for a new probe.
    fn watch_close(&'static self, id: u64, handle: CarrierHandle) {
        crate::runtime::runtime().spawn(async move {
            let error = handle.closed().await;
            let retired = self.inner.lock().state.died(id, Instant::now());
            self.retired(retired, &format!("closed: {error}"));
            self.notify();
        });
    }

    /// Close a carrier that just stopped being live and apply the transition.
    fn retired(&'static self, retired: Option<Retired<CarrierHandle>>, reason: &str) {
        let Some(retired) = retired else {
            return;
        };
        retired.handle.close("retired");
        trace(
            Stage::Quic,
            format!(
                "carrier retired: {} lifetime_ms={}",
                label_str(retired.kind),
                retired.lifetime.as_millis()
            ),
        );
        log::info!(
            "carrier_down kind={} reason=\"{reason}\" lifetime_ms={}",
            label_str(retired.kind),
            retired.lifetime.as_millis()
        );
        self.transitioned();
        if let Some(at) = retired.retry_at {
            self.schedule(at, Trigger::Backoff);
        }
    }

    /// Every carrier transition, up or down: parked relay legs stop serving
    /// once a carrier is live, and parked carrier legs die with their carrier.
    fn transitioned(&self) {
        pool().invalidate();
        self.generation.send_modify(|generation| *generation += 1);
        self.notify();
    }

    fn notify(&self) {
        let (sink, status) = {
            let inner = self.inner.lock();
            (
                inner.sink.clone(),
                CarrierStatus {
                    carrier: inner.state.label(),
                    last_probe: inner.last_probe.clone(),
                },
            )
        };
        if let Some(sink) = sink {
            sink.on_carrier(status);
        }
    }
}

impl Inner {
    fn abort_probe(&mut self) {
        if let Some((_, task)) = self.probe_task.take() {
            task.abort();
        }
    }
}

/// Park a proof leg in the API leg pool, after the transition's
/// invalidation, so the next request rides the new carrier at once.
async fn park(io: LegIo, record: &PairedRecord, epoch: u64) {
    let leg = PooledLeg::fresh(io, BindingKey::from(record), epoch);
    if let Some(orphan) = pool().park_unproven(leg) {
        orphan.io.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::CarrierLabel;

    /// Parked relay legs must stop serving once a carrier is live, and parked
    /// carrier legs must die with their carrier: every transition, up or
    /// down, invalidates the API leg pool and tells the chat supervisor.
    #[test]
    fn every_transition_invalidates_the_pool_and_ticks_the_supervisor() {
        let before = pool().epoch();
        let mut events = hub().subscribe();
        events.mark_unchanged();
        hub().transitioned();
        assert!(pool().epoch() > before);
        assert!(events.has_changed().unwrap());
    }

    /// Pairing and forgetting clear the carrier state infallibly: a new
    /// epoch, so any probe in flight is discarded.
    #[test]
    fn forgetting_the_pairing_clears_carrier_state() {
        let before = hub().inner.lock().state.epoch();
        crate::relay::forget_pairing().expect("forget");
        assert!(hub().inner.lock().state.epoch() > before);
        assert_eq!(hub().inner.lock().state.label(), CarrierLabel::Relay);
    }
}
