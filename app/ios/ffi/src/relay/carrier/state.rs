//! The phone's carrier state: at most one live carrier, at most one probe,
//! and the per-network failure cache. Pure — no I/O, and every clock reading
//! is the caller's `now` — and generic over the live carrier's handle, so the
//! rules below are tested without a socket.
//!
//! **The epoch.** A primary-network change, the background barrier, pairing
//! and forgetting bump it. A probe snapshots it when admitted, and a result
//! from an older epoch is discarded.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use super::network::{LocalPath, NetworkKey, PrimaryInterface};
use crate::api::{CarrierLabel, NetworkPath};
use crate::relay::leg_pool::BindingKey;

/// How long a network's failed probes keep it on the relay: one step per
/// failure, the last step repeating.
pub(crate) const PROBE_BACKOFF: [Duration; 5] = [
    Duration::from_secs(60),
    Duration::from_secs(2 * 60),
    Duration::from_secs(4 * 60),
    Duration::from_secs(8 * 60),
    Duration::from_secs(15 * 60),
];
/// A carrier that dies unsolicited sooner than this counts as a failed probe.
pub(crate) const CARRIER_MIN_LIFETIME: Duration = Duration::from_secs(60);
/// After a stream-level dial failure on a live carrier, new legs dial the
/// relay for this long; the carrier and a chat leg on it stay up.
pub(crate) const CARRIER_DIAL_COOLOFF: Duration = Duration::from_secs(30);
/// One re-probe after a tier was denied by the Local Network permission:
/// the user may just have tapped Allow.
pub(crate) const LOCAL_NETWORK_RETRY: Duration = Duration::from_secs(10);

/// What asked for a probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trigger {
    ChatLive,
    NetworkSettled,
    Foreground,
    Backoff,
    RetryAfter,
    LocalNetworkRetry,
}

impl Trigger {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ChatLive => "chat_live",
            Self::NetworkSettled => "network",
            Self::Foreground => "foreground",
            Self::Backoff => "backoff",
            Self::RetryAfter => "retry_after",
            Self::LocalNetworkRetry => "local_network_retry",
        }
    }
}

/// Why a probe request did not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skip {
    Inactive,
    CarrierLive,
    InFlight,
    RetryAfter,
    NoPath,
    BackingOff,
    LocalNetworkRetry,
}

impl Skip {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Inactive => "inactive",
            Self::CarrierLive => "carrier_live",
            Self::InFlight => "in_flight",
            Self::RetryAfter => "retry_after",
            Self::NoPath => "no_path",
            Self::BackingOff => "backing_off",
            Self::LocalNetworkRetry => "local_network_retry",
        }
    }
}

/// The current path, as the last satisfied delivery left it.
#[derive(Debug, Clone)]
pub(crate) struct PathState {
    primary: PrimaryInterface,
    path: NetworkPath,
    pub(crate) network: NetworkKey,
    pub(crate) local: LocalPath,
}

impl PathState {
    pub(crate) fn new(path: NetworkPath, network: NetworkKey, local: LocalPath) -> Self {
        Self {
            primary: PrimaryInterface::of(&path),
            path,
            network,
            local,
        }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &NetworkPath {
        &self.path
    }
}

/// An admitted probe.
#[derive(Debug, Clone)]
pub(crate) struct ProbeTicket {
    pub(crate) id: u64,
    pub(crate) epoch: u64,
    pub(crate) binding: BindingKey,
    pub(crate) network: NetworkKey,
    pub(crate) local: LocalPath,
    pub(crate) trigger: Trigger,
}

/// How a probe ended.
pub(crate) enum ProbeEnd<H> {
    /// A proven carrier.
    Carrier {
        handle: H,
        kind: CarrierLabel,
        local_ip: IpAddr,
    },
    /// No carrier: a `404` or `504`, an AEAD or `offer_id` failure, or no
    /// tier that connected.
    Failed,
    /// `429` / `503`: retry after this long, cache untouched.
    RetryAfter(Duration),
    /// No carrier, and a tier was denied by the Local Network permission.
    Denied,
}

/// What [`CarrierState::finish`] decided.
pub(crate) enum Finished<H> {
    /// The carrier is live.
    Up,
    /// The probe belongs to an epoch that is gone; close what it found.
    Stale(Option<H>),
    /// No carrier; ask again at this instant.
    RetryAt(Instant, Trigger),
}

/// What a path delivery did.
pub(crate) enum PathChange<H> {
    /// An identical delivery: nothing to do.
    Duplicate,
    /// A change confined to secondary interfaces: the carrier stays.
    Kept,
    /// The primary interface changed, or the carrier's local address left
    /// the path. The epoch moved; a live carrier is returned to close.
    Moved(Option<Retired<H>>),
}

/// A carrier that just stopped being live.
pub(crate) struct Retired<H> {
    pub(crate) handle: H,
    pub(crate) kind: CarrierLabel,
    pub(crate) lifetime: Duration,
    pub(crate) retry_at: Option<Instant>,
}

pub(crate) struct Reproof<H> {
    pub(crate) lease: Lease<H>,
    epoch: u64,
}

pub(crate) enum Reproved<H> {
    Current,
    Stale,
    Retired(Retired<H>),
}

/// A live carrier a leg may dial.
pub(crate) struct Lease<H> {
    pub(crate) id: u64,
    pub(crate) handle: H,
}

struct Live<H> {
    id: u64,
    handle: H,
    kind: CarrierLabel,
    binding: BindingKey,
    network: NetworkKey,
    local_ip: IpAddr,
    established: Instant,
    suspended: bool,
    cooloff_until: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct Backoff {
    step: usize,
    until: Instant,
}

type CacheKey = (BindingKey, NetworkKey);

pub(crate) struct CarrierState<H> {
    epoch: u64,
    app_active: bool,
    path: Option<PathState>,
    live: Option<Live<H>>,
    probe: Option<u64>,
    retry_after: Option<Instant>,
    failures: HashMap<CacheKey, Backoff>,
    /// Networks whose last probe was denied by the Local Network permission.
    /// A second denied probe in a row counts as a failure.
    denied: HashMap<CacheKey, Instant>,
    next_id: u64,
}

impl<H> Default for CarrierState<H> {
    fn default() -> Self {
        Self {
            epoch: 0,
            app_active: true,
            path: None,
            live: None,
            probe: None,
            retry_after: None,
            failures: HashMap::new(),
            denied: HashMap::new(),
            next_id: 0,
        }
    }
}

impl<H: Clone> CarrierState<H> {
    #[cfg(test)]
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn probe_in_flight(&self) -> Option<u64> {
        self.probe
    }

    pub(crate) fn live_id(&self) -> Option<u64> {
        self.live.as_ref().map(|live| live.id)
    }

    pub(crate) fn cooloff_until(&self, id: u64) -> Option<Instant> {
        self.live
            .as_ref()
            .filter(|live| live.id == id)
            .and_then(|live| live.cooloff_until)
    }

    pub(crate) fn label(&self) -> CarrierLabel {
        self.live
            .as_ref()
            .map_or(CarrierLabel::Relay, |live| live.kind)
    }

    /// Admit a probe for `binding`, or say why not. The caller has already
    /// checked that the binding is a relay binding.
    pub(crate) fn admit(
        &mut self,
        binding: BindingKey,
        trigger: Trigger,
        now: Instant,
    ) -> Result<ProbeTicket, Skip> {
        if !self.app_active {
            return Err(Skip::Inactive);
        }
        if self.live.is_some() {
            return Err(Skip::CarrierLive);
        }
        if self.probe.is_some() {
            return Err(Skip::InFlight);
        }
        if self.retry_after.is_some_and(|at| at > now) {
            return Err(Skip::RetryAfter);
        }
        let Some(path) = self.path.as_ref() else {
            return Err(Skip::NoPath);
        };
        let key = (binding.clone(), path.network);
        if self.denied.get(&key).is_some_and(|until| *until > now) {
            return Err(Skip::LocalNetworkRetry);
        }
        if self
            .failures
            .get(&key)
            .is_some_and(|backoff| backoff.until > now)
        {
            return Err(Skip::BackingOff);
        }
        self.next_id += 1;
        let ticket = ProbeTicket {
            id: self.next_id,
            epoch: self.epoch,
            binding,
            network: path.network,
            local: path.local.clone(),
            trigger,
        };
        self.probe = Some(ticket.id);
        self.retry_after = None;
        Ok(ticket)
    }

    pub(crate) fn finish(
        &mut self,
        ticket: &ProbeTicket,
        end: ProbeEnd<H>,
        now: Instant,
    ) -> Finished<H> {
        if self.probe == Some(ticket.id) {
            self.probe = None;
        }
        if ticket.epoch != self.epoch || self.live.is_some() {
            return Finished::Stale(match end {
                ProbeEnd::Carrier { handle, .. } => Some(handle),
                _ => None,
            });
        }
        let key = (ticket.binding.clone(), ticket.network);
        match end {
            ProbeEnd::Carrier {
                handle,
                kind,
                local_ip,
            } => {
                self.failures.remove(&key);
                self.denied.remove(&key);
                self.next_id += 1;
                self.live = Some(Live {
                    id: self.next_id,
                    handle,
                    kind,
                    binding: ticket.binding.clone(),
                    network: ticket.network,
                    local_ip,
                    established: now,
                    suspended: false,
                    cooloff_until: None,
                });
                Finished::Up
            }
            ProbeEnd::Failed => {
                self.denied.remove(&key);
                Finished::RetryAt(self.fail(key, now), Trigger::Backoff)
            }
            ProbeEnd::RetryAfter(wait) => {
                let at = now
                    .checked_add(wait)
                    .unwrap_or_else(|| now + PROBE_BACKOFF[PROBE_BACKOFF.len() - 1]);
                self.retry_after = Some(at);
                Finished::RetryAt(at, Trigger::RetryAfter)
            }
            ProbeEnd::Denied => {
                if self.denied.remove(&key).is_some() {
                    Finished::RetryAt(self.fail(key, now), Trigger::Backoff)
                } else {
                    let at = now + LOCAL_NETWORK_RETRY;
                    self.denied.insert(key, at);
                    Finished::RetryAt(at, Trigger::LocalNetworkRetry)
                }
            }
        }
    }

    /// Advance `key`'s backoff and return when it ends.
    fn fail(&mut self, key: CacheKey, now: Instant) -> Instant {
        let step = self
            .failures
            .get(&key)
            .map_or(0, |backoff| (backoff.step + 1).min(PROBE_BACKOFF.len() - 1));
        let until = now + PROBE_BACKOFF[step];
        self.failures.insert(key, Backoff { step, until });
        until
    }

    /// A path delivery. An unsatisfied path counts as a primary change to no
    /// network, and leaves no path to probe on.
    pub(crate) fn network_changed(
        &mut self,
        next: Option<PathState>,
        now: Instant,
    ) -> PathChange<H> {
        let previous = self.path.take();
        let duplicate = match (&previous, &next) {
            (Some(previous), Some(next)) => {
                previous.path == next.path && previous.network == next.network
            }
            (None, None) => true,
            _ => false,
        };
        let same_primary = match (&previous, &next) {
            (Some(previous), Some(next)) => {
                previous.primary == next.primary && previous.network == next.network
            }
            _ => duplicate,
        };
        let address_left = match (&self.live, &next) {
            (Some(live), Some(next)) => !next.local.holds(live.local_ip),
            _ => false,
        };
        self.path = next;
        if duplicate && !address_left {
            return PathChange::Duplicate;
        }
        if same_primary && !address_left {
            return PathChange::Kept;
        }
        self.bump_epoch();
        PathChange::Moved(self.take_live(now).map(|(retired, _)| retired))
    }

    /// The `.background` barrier: abort any probe and suspend the carrier,
    /// leaving its connection and open streams alone.
    pub(crate) fn background(&mut self) {
        self.app_active = false;
        self.bump_epoch();
        if let Some(live) = self.live.as_mut() {
            live.suspended = true;
        }
    }

    /// `.active`: a suspended carrier, to re-prove before legs dial it again.
    pub(crate) fn foreground(&mut self) -> Option<Reproof<H>> {
        self.app_active = true;
        self.live
            .as_ref()
            .filter(|live| live.suspended)
            .map(|live| Reproof {
                lease: Lease {
                    id: live.id,
                    handle: live.handle.clone(),
                },
                epoch: self.epoch,
            })
    }

    /// The re-proof's verdict. A failed re-proof is P's own retirement and
    /// never touches the cache.
    pub(crate) fn reproved(&mut self, proof: &Reproof<H>, ok: bool, now: Instant) -> Reproved<H> {
        if proof.epoch != self.epoch
            || !self.app_active
            || !self
                .live
                .as_ref()
                .is_some_and(|live| live.id == proof.lease.id && live.suspended)
        {
            return Reproved::Stale;
        }
        if ok {
            if let Some(live) = self.live.as_mut() {
                live.suspended = false;
            }
            Reproved::Current
        } else {
            match self.retire(proof.lease.id, now) {
                Some(retired) => Reproved::Retired(retired),
                None => Reproved::Stale,
            }
        }
    }

    /// The live carrier, when a leg may dial it: not suspended, not cooling
    /// off, and of `binding`.
    pub(crate) fn lease(&self, binding: &BindingKey, now: Instant) -> Option<Lease<H>> {
        let live = self.live.as_ref()?;
        if live.suspended
            || live.binding != *binding
            || live.cooloff_until.is_some_and(|until| until > now)
        {
            return None;
        }
        Some(Lease {
            id: live.id,
            handle: live.handle.clone(),
        })
    }

    /// A leg dial on carrier `id` failed. Connection-level evidence retires
    /// the carrier, as an unsolicited death; anything else belongs to the
    /// stream, and only pauses new carrier legs.
    pub(crate) fn dial_failed(
        &mut self,
        id: u64,
        connection_level: bool,
        now: Instant,
    ) -> Option<Retired<H>> {
        if !self.is_live(id) {
            return None;
        }
        if connection_level {
            return self.died(id, now);
        }
        if let Some(live) = self.live.as_mut() {
            live.cooloff_until = Some(now + CARRIER_DIAL_COOLOFF);
        }
        None
    }

    /// Carrier `id`'s connection closed without P asking: a `CONNECTION_CLOSE`
    /// from A, or a QUIC idle timeout. An early death counts as a failure.
    pub(crate) fn died(&mut self, id: u64, now: Instant) -> Option<Retired<H>> {
        if !self.is_live(id) {
            return None;
        }
        let (mut retired, key) = self.take_live(now)?;
        retired.retry_at = Some(if retired.lifetime < CARRIER_MIN_LIFETIME {
            self.fail(key, now)
        } else {
            now
        });
        Some(retired)
    }

    /// P retires carrier `id` itself: never a failure.
    pub(crate) fn retire(&mut self, id: u64, now: Instant) -> Option<Retired<H>> {
        if !self.is_live(id) {
            return None;
        }
        self.take_live(now).map(|(retired, _)| retired)
    }

    /// Pairing or forgetting: every piece of carrier state goes.
    pub(crate) fn clear(&mut self, now: Instant) -> Option<Retired<H>> {
        self.bump_epoch();
        self.failures.clear();
        self.denied.clear();
        self.retry_after = None;
        self.take_live(now).map(|(retired, _)| retired)
    }

    fn is_live(&self, id: u64) -> bool {
        self.live.as_ref().is_some_and(|live| live.id == id)
    }

    fn bump_epoch(&mut self) {
        self.epoch += 1;
        self.probe = None;
    }

    fn take_live(&mut self, now: Instant) -> Option<(Retired<H>, CacheKey)> {
        let live = self.live.take()?;
        Some((
            Retired {
                handle: live.handle,
                kind: live.kind,
                lifetime: now.saturating_duration_since(live.established),
                retry_at: None,
            },
            (live.binding, live.network),
        ))
    }
}

#[cfg(test)]
mod tests {
    use carrier::interfaces::InterfaceAddress;

    use super::*;
    use crate::api::NetworkInterfaceKind;

    /// The handle a test carrier stands for.
    type State = CarrierState<&'static str>;

    fn binding() -> BindingKey {
        BindingKey::for_tests("node-1")
    }

    fn address(interface: &str, ip: &str) -> InterfaceAddress {
        InterfaceAddress {
            interface: interface.to_owned(),
            ip: ip.parse().unwrap(),
            up_running: true,
            temporary: false,
            unusable: false,
        }
    }

    fn path_on(kind: NetworkInterfaceKind, name: &str, ip: &str, gateway: &str) -> PathState {
        let path = NetworkPath {
            satisfied: true,
            interface_kind: kind,
            interface_name: name.to_owned(),
            gateways: vec![gateway.to_owned()],
            supports_ipv4: true,
            supports_ipv6: true,
            available_interfaces: vec![name.to_owned()],
            is_expensive: false,
            is_constrained: false,
        };
        let interfaces = [address(name, ip)];
        PathState::new(
            path.clone(),
            NetworkKey::of(&path, &interfaces),
            LocalPath::new(&path, &interfaces),
        )
    }

    fn home() -> PathState {
        path_on(
            NetworkInterfaceKind::Wifi,
            "en0",
            "192.168.1.20",
            "192.168.1.1",
        )
    }

    fn cellular() -> PathState {
        path_on(
            NetworkInterfaceKind::Cellular,
            "pdp_ip0",
            "10.20.30.40",
            "fe80::1",
        )
    }

    fn on(path: PathState) -> State {
        let mut state = State::default();
        let _ = state.network_changed(Some(path), Instant::now());
        state
    }

    fn up(state: &mut State, now: Instant) -> u64 {
        let ticket = state
            .admit(binding(), Trigger::ChatLive, now)
            .expect("admitted");
        let end = ProbeEnd::Carrier {
            handle: "carrier",
            kind: CarrierLabel::Lan,
            local_ip: "192.168.1.20".parse().unwrap(),
        };
        assert!(matches!(state.finish(&ticket, end, now), Finished::Up));
        state.lease(&binding(), now).expect("a lease").id
    }

    fn fail(state: &mut State, now: Instant) -> Instant {
        let ticket = state
            .admit(binding(), Trigger::Backoff, now)
            .expect("admitted");
        match state.finish(&ticket, ProbeEnd::Failed, now) {
            Finished::RetryAt(at, Trigger::Backoff) => at,
            _ => panic!("a failure backs off"),
        }
    }

    #[test]
    fn probes_are_single_flight() {
        let now = Instant::now();
        let mut state = on(home());
        let _ticket = state.admit(binding(), Trigger::ChatLive, now).unwrap();
        assert_eq!(
            state
                .admit(binding(), Trigger::Foreground, now)
                .unwrap_err(),
            Skip::InFlight
        );
    }

    #[test]
    fn a_probe_needs_a_path_and_an_active_app() {
        let now = Instant::now();
        let mut state = State::default();
        assert_eq!(
            state.admit(binding(), Trigger::ChatLive, now).unwrap_err(),
            Skip::NoPath
        );
        let mut state = on(home());
        state.background();
        assert_eq!(
            state.admit(binding(), Trigger::ChatLive, now).unwrap_err(),
            Skip::Inactive
        );
    }

    #[test]
    fn each_failure_advances_the_backoff_and_the_last_step_repeats() {
        let mut now = Instant::now();
        let mut state = on(home());
        for expected in PROBE_BACKOFF.iter().chain([&PROBE_BACKOFF[4]]) {
            let until = fail(&mut state, now);
            assert_eq!(until - now, *expected);
            assert_eq!(
                state
                    .admit(binding(), Trigger::Backoff, until - Duration::from_secs(1))
                    .unwrap_err(),
                Skip::BackingOff
            );
            now = until;
        }
    }

    #[test]
    fn a_new_network_probes_at_once_and_a_success_clears_the_cache() {
        let now = Instant::now();
        let mut state = on(home());
        fail(&mut state, now);
        assert_eq!(
            state.admit(binding(), Trigger::ChatLive, now).unwrap_err(),
            Skip::BackingOff
        );
        let _ = state.network_changed(Some(cellular()), now);
        assert!(
            state.admit(binding(), Trigger::NetworkSettled, now).is_ok(),
            "the cellular network has no entry"
        );
    }

    #[test]
    fn a_result_from_an_older_epoch_is_discarded() {
        let now = Instant::now();
        let mut state = on(home());
        let ticket = state.admit(binding(), Trigger::ChatLive, now).unwrap();
        state.background();
        let end = ProbeEnd::Carrier {
            handle: "late",
            kind: CarrierLabel::Lan,
            local_ip: "192.168.1.20".parse().unwrap(),
        };
        assert!(matches!(
            state.finish(&ticket, end, now),
            Finished::Stale(Some("late"))
        ));
        assert_eq!(state.label(), CarrierLabel::Relay);
    }

    #[test]
    fn retry_after_waits_and_leaves_the_cache_alone() {
        let now = Instant::now();
        let mut state = on(home());
        let ticket = state.admit(binding(), Trigger::ChatLive, now).unwrap();
        let wait = Duration::from_secs(7);
        assert!(matches!(
            state.finish(&ticket, ProbeEnd::RetryAfter(wait), now),
            Finished::RetryAt(at, Trigger::RetryAfter) if at == now + wait
        ));
        assert_eq!(
            state.admit(binding(), Trigger::ChatLive, now).unwrap_err(),
            Skip::RetryAfter
        );
        assert!(
            state
                .admit(binding(), Trigger::RetryAfter, now + wait)
                .is_ok(),
            "no backoff entry was written"
        );
    }

    #[test]
    fn an_unrepresentable_retry_after_does_not_panic_or_wedge_the_probe() {
        let now = Instant::now();
        let mut state = on(home());
        let ticket = state.admit(binding(), Trigger::ChatLive, now).unwrap();
        let Finished::RetryAt(at, Trigger::RetryAfter) =
            state.finish(&ticket, ProbeEnd::RetryAfter(Duration::MAX), now)
        else {
            panic!("a bounded retry");
        };
        assert!(at > now);
        assert!(state.admit(binding(), Trigger::RetryAfter, at).is_ok());
    }

    #[test]
    fn a_denied_probe_re_probes_once_and_a_second_counts_as_a_failure() {
        let now = Instant::now();
        let mut state = on(home());
        let ticket = state.admit(binding(), Trigger::ChatLive, now).unwrap();
        assert!(matches!(
            state.finish(&ticket, ProbeEnd::Denied, now),
            Finished::RetryAt(at, Trigger::LocalNetworkRetry) if at == now + LOCAL_NETWORK_RETRY
        ));
        let later = now + LOCAL_NETWORK_RETRY;
        assert_eq!(
            state.admit(binding(), Trigger::ChatLive, now).unwrap_err(),
            Skip::LocalNetworkRetry
        );
        let ticket = state
            .admit(binding(), Trigger::LocalNetworkRetry, later)
            .expect("the denial wrote no backoff");
        assert!(matches!(
            state.finish(&ticket, ProbeEnd::Denied, later),
            Finished::RetryAt(at, Trigger::Backoff) if at == later + PROBE_BACKOFF[0]
        ));
    }

    #[test]
    fn a_local_network_retry_does_not_delay_a_different_network() {
        let now = Instant::now();
        let mut state = on(home());
        let ticket = state.admit(binding(), Trigger::ChatLive, now).unwrap();
        state.finish(&ticket, ProbeEnd::Denied, now);
        state.network_changed(Some(cellular()), now);
        assert!(state.admit(binding(), Trigger::NetworkSettled, now).is_ok());
    }

    #[test]
    fn a_new_subnet_on_the_same_interface_invalidates_the_old_probe() {
        let now = Instant::now();
        let mut state = on(home());
        let old = state.admit(binding(), Trigger::ChatLive, now).unwrap();
        let renumbered = path_on(
            NetworkInterfaceKind::Wifi,
            "en0",
            "192.168.2.20",
            "192.168.1.1",
        );
        assert!(matches!(
            state.network_changed(Some(renumbered), now),
            PathChange::Moved(None)
        ));
        assert!(matches!(
            state.finish(&old, ProbeEnd::Failed, now),
            Finished::Stale(None)
        ));
        assert!(state.admit(binding(), Trigger::NetworkSettled, now).is_ok());
    }

    #[test]
    fn a_retirement_p_starts_never_touches_the_cache() {
        let now = Instant::now();
        let mut state = on(home());
        let id = up(&mut state, now);
        assert!(state.retire(id, now).is_some());
        assert!(state.admit(binding(), Trigger::ChatLive, now).is_ok());

        let mut state = on(home());
        let id = up(&mut state, now);
        state.background();
        let suspended = state.foreground().expect("a carrier to re-prove");
        assert_eq!(suspended.lease.id, id);
        assert!(matches!(
            state.reproved(&suspended, false, now),
            Reproved::Retired(Retired { retry_at: None, .. })
        ));
        assert!(state.admit(binding(), Trigger::Foreground, now).is_ok());
    }

    #[test]
    fn an_early_unsolicited_death_counts_as_a_failure_and_a_late_one_does_not() {
        let now = Instant::now();
        let mut state = on(home());
        let id = up(&mut state, now);
        let early = now + Duration::from_secs(5);
        let retired = state.died(id, early).expect("retired");
        assert_eq!(retired.retry_at, Some(early + PROBE_BACKOFF[0]));
        assert_eq!(
            state
                .admit(binding(), Trigger::ChatLive, now + Duration::from_secs(5))
                .unwrap_err(),
            Skip::BackingOff
        );

        let mut state = on(home());
        let id = up(&mut state, now);
        let late = now + CARRIER_MIN_LIFETIME;
        assert_eq!(state.died(id, late).expect("retired").retry_at, Some(late));
        assert!(state.admit(binding(), Trigger::ChatLive, late).is_ok());
    }

    #[test]
    fn a_stream_level_failure_keeps_the_carrier_and_pauses_new_legs() {
        let now = Instant::now();
        let mut state = on(home());
        let id = up(&mut state, now);
        assert!(state.dial_failed(id, false, now).is_none());
        assert_eq!(state.label(), CarrierLabel::Lan, "the carrier stays");
        assert_eq!(state.cooloff_until(id), Some(now + CARRIER_DIAL_COOLOFF));
        assert!(state.lease(&binding(), now).is_none());
        assert!(
            state
                .lease(&binding(), now + CARRIER_DIAL_COOLOFF)
                .is_some()
        );

        assert!(state.dial_failed(id, true, now).is_some());
        assert_eq!(state.label(), CarrierLabel::Relay);
    }

    #[test]
    fn a_duplicate_path_and_a_secondary_change_keep_the_carrier() {
        let now = Instant::now();
        let mut state = on(home());
        let _id = up(&mut state, now);
        assert!(matches!(
            state.network_changed(Some(home()), now),
            PathChange::Duplicate
        ));
        let mut with_cellular = home();
        let mut path = with_cellular.path().clone();
        path.available_interfaces.push("pdp_ip0".into());
        path.is_expensive = true;
        with_cellular = PathState::new(path, with_cellular.network, with_cellular.local);
        assert!(matches!(
            state.network_changed(Some(with_cellular.clone()), now),
            PathChange::Kept
        ));
        assert!(matches!(
            state.network_changed(Some(home()), now),
            PathChange::Kept
        ));
        assert_eq!(state.label(), CarrierLabel::Lan);
    }

    #[test]
    fn wifi_to_cellular_retires_the_carrier_exactly_once() {
        let now = Instant::now();
        let mut state = on(home());
        let _id = up(&mut state, now);
        let epoch = state.epoch();
        assert!(matches!(
            state.network_changed(Some(cellular()), now),
            PathChange::Moved(Some(_))
        ));
        assert!(state.epoch() > epoch);
        assert!(matches!(
            state.network_changed(Some(cellular()), now),
            PathChange::Duplicate
        ));
    }

    #[test]
    fn a_carrier_whose_local_address_left_the_path_is_retired() {
        let now = Instant::now();
        let mut state = on(home());
        let _id = up(&mut state, now);
        let renumbered = path_on(
            NetworkInterfaceKind::Wifi,
            "en0",
            "192.168.1.99",
            "192.168.1.1",
        );
        assert!(matches!(
            state.network_changed(Some(renumbered), now),
            PathChange::Moved(Some(_))
        ));
    }

    #[test]
    fn the_background_barrier_suspends_and_aborts_the_probe() {
        let now = Instant::now();
        let mut state = on(home());
        let id = up(&mut state, now);
        state.background();
        assert!(state.lease(&binding(), now).is_none(), "suspended");
        assert_eq!(state.label(), CarrierLabel::Lan, "but not closed");
        let suspended = state.foreground().expect("re-prove");
        assert!(matches!(
            state.reproved(&suspended, true, now),
            Reproved::Current
        ));
        assert_eq!(state.lease(&binding(), now).map(|l| l.id), Some(id));
    }

    #[test]
    fn a_reproof_cannot_cross_a_background_barrier_even_after_another_foreground() {
        for ok in [true, false] {
            let now = Instant::now();
            let mut state = on(home());
            let id = up(&mut state, now);
            state.background();
            let old = state.foreground().expect("first re-proof");
            state.background();
            assert!(matches!(state.reproved(&old, ok, now), Reproved::Stale));
            assert_eq!(state.label(), CarrierLabel::Lan);
            assert!(state.lease(&binding(), now).is_none());

            let current = state.foreground().expect("second re-proof");
            assert!(matches!(state.reproved(&old, ok, now), Reproved::Stale));
            assert!(state.lease(&binding(), now).is_none());
            assert!(matches!(
                state.reproved(&current, true, now),
                Reproved::Current
            ));
            assert_eq!(state.lease(&binding(), now).map(|lease| lease.id), Some(id));
        }
    }

    #[test]
    fn a_reproof_after_a_network_change_cannot_retire_the_replacement() {
        let now = Instant::now();
        let mut state = on(home());
        up(&mut state, now);
        state.background();
        let old = state.foreground().expect("re-proof");
        state.network_changed(Some(cellular()), now);
        let replacement = up(&mut state, now);
        assert!(matches!(state.reproved(&old, false, now), Reproved::Stale));
        assert_eq!(
            state.lease(&binding(), now).map(|lease| lease.id),
            Some(replacement)
        );
    }

    #[test]
    fn a_late_duplicate_reproof_cannot_undo_a_successful_one() {
        let now = Instant::now();
        let mut state = on(home());
        let id = up(&mut state, now);
        state.background();
        let first = state.foreground().expect("first re-proof");
        let second = state.foreground().expect("concurrent re-proof");
        assert!(matches!(
            state.reproved(&first, true, now),
            Reproved::Current
        ));
        assert!(matches!(
            state.reproved(&second, false, now),
            Reproved::Stale
        ));
        assert_eq!(state.lease(&binding(), now).map(|lease| lease.id), Some(id));
    }

    #[test]
    fn forgetting_clears_every_piece_of_carrier_state() {
        let now = Instant::now();
        let mut state = on(home());
        fail(&mut state, now);
        let epoch = state.epoch();
        assert!(state.clear(now).is_none());
        assert!(state.epoch() > epoch);
        assert!(state.admit(binding(), Trigger::ChatLive, now).is_ok());
    }

    #[test]
    fn a_lease_is_scoped_to_its_binding() {
        let now = Instant::now();
        let mut state = on(home());
        up(&mut state, now);
        assert!(state.lease(&BindingKey::for_tests("node-2"), now).is_none());
    }
}
