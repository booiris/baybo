//! A pool of warm API tunnel legs.
//!
//! Dialing a relay leg costs a WSS upgrade plus a Noise IK handshake — around
//! four phone-side round trips — and the relay client used to pay that on EVERY
//! REST call. The gateway now offers to hold a leg open ([`TunnelReuse`]), so a
//! burst (list → sync → mark-read) and human-paced sequences (list appears → the
//! user taps in → sync) can share one.
//!
//! ## Wait-free, K-deep, serialized by ownership
//!
//! [`ApiLegPool::take`] MOVES a leg out of the pool. Whoever holds it is its only
//! writer, which is what satisfies the one hard rule of the transport: an
//! `ApiTunnelSession` is an ordered, stateful Noise stream, so requests can never
//! be interleaved on one leg. Concurrency comes from having several legs, never
//! from sharing one.
//!
//! And a caller that finds the pool empty **dials its own leg immediately** — it
//! never queues. The app's hot edges are fan-outs, not serial chains: opening a
//! chat fires a sync, a mark-read, and one point lookup per unconfirmed outbox
//! entry, all at once. A single leg behind a contention valve would make most of
//! those wait and then dial anyway, which is strictly worse than dialing at once.
//! A K-deep pool serves what it can and costs the rest nothing.
//!
//! ## Why two clocks
//!
//! On Darwin `Instant` is `mach_absolute_time`, which **does not advance while the
//! device sleeps**. A leg parked before the phone went in a pocket overnight would
//! claim to be three seconds old. A large wall delta next to a small monotonic one
//! is the signature of a suspended process, and on its own is enough to condemn a
//! leg.

use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;

use super::pairing::PairedRecord;
use super::tunnel::{LegIo, NoiseFrames, TunnelFrames, TunnelReuse};

/// How many warm legs to hold. Each one occupies a NON-RESERVED relay connection
/// slot, shared with chat legs, so this is not free: a self-hoster with a small
/// connection cap and several phones would have parked legs competing with chat
/// reconnects.
pub(crate) const MAX_POOLED_LEGS: usize = 3;

/// Retired from the gateway's advertised budget so the CLIENT always closes first.
/// Absorbs the round trip, relay queueing, and clock skew.
const CLIENT_IDLE_MARGIN: Duration = Duration::from_secs(10);
/// A ceiling on how long we will trust any gateway's offer, however generous.
const CLIENT_IDLE_CAP: Duration = Duration::from_secs(120);
/// How long a leg that has been dialed but never used may be held.
///
/// The gateway's budget for the FIRST request head is 20s on every version, old or
/// new, so a leg parked without a request is reclaimed at 20s no matter what the
/// peer supports. **This is the entire reason pre-dialing needs no negotiation.**
/// 12s leaves headroom for the round trip and clock skew.
const UNPROVEN_LEG_TTL: Duration = Duration::from_secs(12);

/// A binding is one device against one gateway through one relay node. A leg from
/// a previous binding must never be reused — re-pairing to a different gateway is
/// the case that matters, and the epoch alone would not catch a rebind to a
/// gateway that happens to reuse the epoch.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct BindingKey {
    relay_node_id: String,
    gateway_static_pubkey: [u8; 32],
    device_id: String,
}

impl From<&PairedRecord> for BindingKey {
    fn from(record: &PairedRecord) -> Self {
        Self {
            relay_node_id: record.relay_node_id.clone(),
            gateway_static_pubkey: record.gateway_static_pubkey,
            device_id: record.device_id.clone(),
        }
    }
}

/// A leg checked out of (or on its way into) the pool.
pub(crate) struct PooledLeg<F: TunnelFrames = NoiseFrames> {
    pub(crate) io: LegIo<F>,
    binding: BindingKey,
    /// The pool generation this leg belongs to. A leg whose epoch is stale was
    /// checked out before something invalidated the pool (backgrounding, logout),
    /// and must be dropped rather than parked.
    epoch: u64,
    /// How long this leg may sit parked. `None` until the gateway has actually
    /// answered on it — see `UNPROVEN_LEG_TTL`.
    ttl: Duration,
    parked_mono: Instant,
    parked_wall: SystemTime,
    /// Whether this leg came from the pool. Only a pooled leg's failure is
    /// retryable: a fresh leg that fails means the network is down, and retrying
    /// there just turns one outage into a dial storm.
    pub(crate) was_pooled: bool,
}

impl<F: TunnelFrames> PooledLeg<F> {
    /// A leg dialed for one caller, not yet trusted for anything.
    pub(crate) fn fresh(io: LegIo<F>, binding: BindingKey, epoch: u64) -> Self {
        Self {
            io,
            binding,
            epoch,
            ttl: UNPROVEN_LEG_TTL,
            parked_mono: Instant::now(),
            parked_wall: SystemTime::now(),
            was_pooled: false,
        }
    }

    /// Re-stamp the leg for parking. `reuse` is the gateway's offer from the
    /// response it just answered; `None` means it never made one (an old gateway,
    /// or a blob leg) and the leg is not poolable at all.
    fn park_stamp(&mut self, reuse: TunnelReuse) {
        self.ttl = ttl_for(reuse);
        self.parked_mono = Instant::now();
        self.parked_wall = SystemTime::now();
    }

    /// Whether this leg is still worth handing to a caller.
    fn usable(&self, key: &BindingKey, epoch: u64) -> bool {
        self.epoch == epoch && self.binding == *key && self.within_ttl()
    }

    /// BOTH clocks must agree the leg is young. See the module docs.
    fn within_ttl(&self) -> bool {
        if self.parked_mono.elapsed() >= self.ttl {
            return false;
        }
        match self.parked_wall.elapsed() {
            Ok(wall) => wall < self.ttl,
            // The user moved the clock backwards. We cannot reason about the leg's
            // age, so we do not.
            Err(_) => false,
        }
    }
}

/// The client's share of the gateway's advertised idle budget.
fn ttl_for(reuse: TunnelReuse) -> Duration {
    Duration::from_millis(u64::from(reuse.idle_ms))
        .min(CLIENT_IDLE_CAP)
        .saturating_sub(CLIENT_IDLE_MARGIN)
}

struct PoolInner<F: TunnelFrames> {
    legs: Vec<PooledLeg<F>>,
    epoch: u64,
}

pub(crate) struct ApiLegPool<F: TunnelFrames = NoiseFrames> {
    /// `parking_lot`, and NEVER held across an await. It guards a `Vec` and a
    /// counter; the serialization that actually matters is ownership (see the
    /// module docs), not this lock.
    inner: Mutex<PoolInner<F>>,
}

impl<F: TunnelFrames> ApiLegPool<F> {
    fn new() -> Self {
        Self {
            inner: Mutex::new(PoolInner {
                legs: Vec::new(),
                epoch: 0,
            }),
        }
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.inner.lock().epoch
    }

    /// Check out a usable leg, or `None` — in which case the caller dials its own
    /// rather than waiting for one.
    pub(crate) fn take(&self, key: &BindingKey) -> Option<PooledLeg<F>> {
        let mut inner = self.inner.lock();
        let epoch = inner.epoch;
        while let Some(leg) = inner.legs.pop() {
            if leg.usable(key, epoch) {
                let mut leg = leg;
                leg.was_pooled = true;
                return Some(leg);
            }
            // Unusable: dropping it closes the socket. Keep looking.
        }
        None
    }

    /// Hand a leg back, if it is still worth holding.
    ///
    /// `reuse` is the gateway's offer on the response this leg just carried. No
    /// offer (an old gateway) means no parking, which is exactly the old
    /// behaviour.
    pub(crate) fn park(
        &self,
        mut leg: PooledLeg<F>,
        reuse: Option<TunnelReuse>,
    ) -> Option<PooledLeg<F>> {
        let Some(reuse) = reuse else {
            return Some(leg);
        };
        let mut inner = self.inner.lock();
        // The pool was invalidated (backgrounded, logged out, re-paired) while this
        // request was in flight. The leg belongs to a world that is gone.
        if leg.epoch != inner.epoch || inner.legs.len() >= MAX_POOLED_LEGS {
            return Some(leg);
        }
        leg.park_stamp(reuse);
        leg.was_pooled = false;
        inner.legs.push(leg);
        None
    }

    /// Park a leg the gateway has never answered on. Only [`warm`] produces one, and
    /// it carries `UNPROVEN_LEG_TTL` rather than an advertised budget.
    fn park_unproven(&self, leg: PooledLeg<F>) -> Option<PooledLeg<F>> {
        let mut inner = self.inner.lock();
        if leg.epoch != inner.epoch || inner.legs.len() >= MAX_POOLED_LEGS {
            return Some(leg);
        }
        inner.legs.push(leg);
        None
    }

    /// Whether a leg is already warm for this binding.
    fn has_usable(&self, key: &BindingKey) -> bool {
        let inner = self.inner.lock();
        let epoch = inner.epoch;
        inner.legs.iter().any(|leg| leg.usable(key, epoch))
    }

    /// Drop every leg and orphan the ones currently checked out.
    ///
    /// Synchronous on purpose: the whole point is that it runs BEFORE anything a
    /// later Task might do with a leg it took a moment ago.
    pub(crate) fn invalidate(&self) {
        let mut inner = self.inner.lock();
        inner.epoch += 1;
        inner.legs.clear();
    }
}

pub(crate) fn pool() -> &'static ApiLegPool {
    static POOL: OnceLock<ApiLegPool> = OnceLock::new();
    POOL.get_or_init(ApiLegPool::new)
}

/// Dial one API leg and park it, without sending anything on it.
///
/// This takes the dial off the critical path of the FIRST call after a foreground
/// — which, parked on the chat list, is the list refresh the user is looking at.
///
/// It needs no capability negotiation at all: the gateway gives EVERY leg 20s to
/// produce its first request head, old version or new, so an unproven leg is safe
/// to hold for `UNPROVEN_LEG_TTL` whatever the peer turns out to support.
///
/// Best-effort. A user who foregrounds and does nothing costs one relay connection
/// slot for twelve seconds.
pub(crate) async fn warm() {
    let Ok(Some(record)) = super::pairing::load_paired_record() else {
        return;
    };
    let key = BindingKey::from(&record);
    // Do not stack a fresh leg on every foreground.
    if pool().has_usable(&key) {
        return;
    }

    let local =
        device_proto::noise::StaticKeypair::from_parts(record.noise_public, record.noise_secret);
    match super::tunnel::dial_tunnel_leg(
        &record,
        &local,
        remote_host_protocol::relay::LegClass::Api,
    )
    .await
    {
        Ok(io) => {
            let leg = PooledLeg::fresh(io, key, pool().epoch());
            if let Some(orphan) = pool().park_unproven(leg) {
                orphan.io.close().await;
            }
        }
        Err(e) => log::debug!("relay api leg warm-up: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::tunnel::testing::ScriptedFrames;

    fn key(node: &str) -> BindingKey {
        BindingKey {
            relay_node_id: node.into(),
            gateway_static_pubkey: [7u8; 32],
            device_id: "device-1".into(),
        }
    }

    fn leg(pool: &ApiLegPool<ScriptedFrames>, key: &BindingKey) -> PooledLeg<ScriptedFrames> {
        PooledLeg::fresh(
            LegIo::new(ScriptedFrames::new(vec![])),
            key.clone(),
            pool.epoch(),
        )
    }

    fn reuse(ms: u32) -> Option<TunnelReuse> {
        Some(TunnelReuse { idle_ms: ms })
    }

    #[test]
    fn the_client_always_closes_before_the_gateway_does() {
        // A 60s gateway budget must leave the client closing at 50s: the margin is
        // what keeps a race with the gateway's own reaper out of the common path.
        assert_eq!(
            ttl_for(TunnelReuse { idle_ms: 60_000 }),
            Duration::from_secs(50)
        );
        // However generous the offer, we cap our trust in it.
        assert_eq!(
            ttl_for(TunnelReuse { idle_ms: 600_000 }),
            CLIENT_IDLE_CAP - CLIENT_IDLE_MARGIN
        );
        // And a budget under the margin means "do not park at all".
        assert_eq!(ttl_for(TunnelReuse { idle_ms: 5_000 }), Duration::ZERO);
    }

    #[test]
    fn a_gateway_that_offers_nothing_is_never_pooled() {
        let pool = ApiLegPool::<ScriptedFrames>::new();
        let k = key("node-a");

        let orphan = pool.park(leg(&pool, &k), None);

        assert!(
            orphan.is_some(),
            "an old gateway's leg is handed back to close"
        );
        assert!(pool.take(&k).is_none());
    }

    #[test]
    fn a_parked_leg_comes_back_and_is_marked_pooled() {
        let pool = ApiLegPool::<ScriptedFrames>::new();
        let k = key("node-a");

        assert!(pool.park(leg(&pool, &k), reuse(60_000)).is_none());

        let taken = pool.take(&k).expect("the parked leg");
        assert!(taken.was_pooled, "so its failure is the retryable kind");
        assert!(pool.take(&k).is_none(), "and it is gone from the pool");
    }

    #[test]
    fn a_leg_from_another_binding_is_never_reused() {
        let pool = ApiLegPool::<ScriptedFrames>::new();
        assert!(
            pool.park(leg(&pool, &key("node-a")), reuse(60_000))
                .is_none()
        );

        // Re-paired to a different gateway.
        assert!(pool.take(&key("node-b")).is_none());
    }

    #[test]
    fn invalidating_empties_the_pool_and_orphans_legs_in_flight() {
        let pool = ApiLegPool::<ScriptedFrames>::new();
        let k = key("node-a");
        // A leg checked out before the app backgrounded.
        let in_flight = leg(&pool, &k);
        assert!(pool.park(leg(&pool, &k), reuse(60_000)).is_none());

        pool.invalidate();

        assert!(pool.take(&k).is_none(), "the parked leg is gone");
        assert!(
            pool.park(in_flight, reuse(60_000)).is_some(),
            "and the one that was in flight cannot come back either"
        );
    }

    #[test]
    fn the_pool_never_holds_more_than_its_cap() {
        let pool = ApiLegPool::<ScriptedFrames>::new();
        let k = key("node-a");

        for _ in 0..MAX_POOLED_LEGS {
            assert!(pool.park(leg(&pool, &k), reuse(60_000)).is_none());
        }
        assert!(
            pool.park(leg(&pool, &k), reuse(60_000)).is_some(),
            "the one over the cap is handed back to close"
        );

        for _ in 0..MAX_POOLED_LEGS {
            assert!(pool.take(&k).is_some());
        }
        assert!(pool.take(&k).is_none());
    }

    /// The iOS suspension nail. `Instant` does not advance while the device sleeps,
    /// so a leg that slept overnight looks brand new by the monotonic clock alone.
    #[test]
    fn a_leg_stale_by_the_wall_clock_is_not_reused() {
        let pool = ApiLegPool::<ScriptedFrames>::new();
        let k = key("node-a");
        let mut parked = leg(&pool, &k);
        parked.ttl = Duration::from_secs(50);
        // What a suspend looks like from inside the process: the monotonic clock
        // barely moved, the wall clock moved an hour.
        parked.parked_mono = Instant::now();
        parked.parked_wall = SystemTime::now() - Duration::from_secs(3600);

        assert!(
            !parked.within_ttl(),
            "a monotonic clock that slept through the gap must not vouch for the leg"
        );
    }

    #[test]
    fn an_unproven_leg_expires_on_the_gateways_first_head_budget() {
        let pool = ApiLegPool::<ScriptedFrames>::new();
        let fresh = leg(&pool, &key("node-a"));

        assert_eq!(
            fresh.ttl, UNPROVEN_LEG_TTL,
            "a warmed leg is reclaimed by EVERY gateway at 20s, old or new"
        );
        assert!(fresh.within_ttl());
    }
}
