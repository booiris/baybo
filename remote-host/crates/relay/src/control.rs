//! The C-side of the A↔C control connection.
//!
//! A NAT'd gateway (A) can't be dialed by C, so each A instead holds a
//! **persistent outbound control connection** to C. When a phone arrives at the
//! relay for A's `relay_node_id`, C signals A over that control connection to
//! open a **data leg**, which then joins the blind byte-pipe
//! [`super::RelayBroker`] under a shared key and meets the phone's leg. The same
//! connection carries direct-carrier signalling: C forwards P's sealed offer to
//! a direct-capable gateway as a punch ([`crate::punch`]) and routes A's report
//! back to the waiting POST.
//!
//! This is the C-side registry + signaling core (keyed by `relay_node_id`); the
//! production WebSocket transport — A dialing out, C accepting and pumping the
//! signal stream — layers on top. Host-testable over `mpsc`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use remote_host_protocol::key_tag;
use remote_host_protocol::relay::{
    ControlReport, DirectCapability, MAX_RELAY_NODE_ID_BYTES, SealedCandidates, SourceKey,
};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::punch::{
    DIRECT_ANSWER_TIMEOUT, OfferLimit, PendingOffer, PunchRefusal, PunchRegistry, PunchRequest,
    ReportOutcome,
};
use crate::udp::RendezvousAddress;

/// Bounded backlog of control signals toward one gateway.
const CONTROL_CHANNEL_CAP: usize = 32;

/// The control-plane wire types ([`ControlHello`] in, [`ControlSignal`] out)
/// live in the shared protocol crate, so the gateway encodes/decodes the exact
/// same definitions.
pub use remote_host_protocol::relay::{ControlHello, ControlSignal, LegClass};

/// A gateway's live control connection: the signal channel plus its admitted
/// `remote_api_key`, so the relay can attribute the anonymous phone-side content
/// leg (which names only the `relay_node_id`) back to the owning gateway.
struct ControlEntry {
    tx: mpsc::Sender<ControlSignal>,
    remote_api_key: String,
    /// Unique per accepted control connection. A stale connection's cleanup
    /// removes its slot only when this still matches, so it can't evict the
    /// entry a faster reconnect already installed under the same
    /// `relay_node_id` + `remote_api_key`.
    token: u64,
    /// The supported direct capability from the gateway's hello; `None` means
    /// C never forwards it an offer.
    direct: Option<DirectCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlRegisterError {
    /// The `relay_node_id` is already registered to a different key; `owner` is
    /// the registered entry's `remote_api_key` (log it only via [`key_tag`]).
    OwnerMismatch { owner: String },
    /// The `relay_node_id` is longer than [`MAX_RELAY_NODE_ID_BYTES`].
    NodeIdTooLong { len: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalOpenError {
    NotConnected,
    OwnerMismatch,
    Backpressure,
}

/// Whether `relay_node_id` may become a key of any map C keeps: no longer than
/// [`MAX_RELAY_NODE_ID_BYTES`]. A longer id can never be registered, so every
/// route naming one is answered as if its gateway were not connected.
pub(crate) fn node_id_within_bound(relay_node_id: &str) -> bool {
    relay_node_id.len() <= MAX_RELAY_NODE_ID_BYTES
}

/// Who is offering: the route's node, the admitted key presented on the POST,
/// and the client source its offer budget is keyed on.
pub(crate) struct OfferSource<'a> {
    pub(crate) relay_node_id: &'a str,
    pub(crate) remote_api_key: &'a str,
    pub(crate) client: Option<SourceKey>,
}

/// Why C found no direct route. The POST answers every one with the same
/// opaque `404`; the reason is for C's own log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoRoute {
    NodeIdTooLong,
    NotConnected,
    OwnerMismatch,
    NotCapable,
}

impl NoRoute {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NodeIdTooLong => "node_id_too_long",
            Self::NotConnected => "not_connected",
            Self::OwnerMismatch => "owner_mismatch",
            Self::NotCapable => "not_capable",
        }
    }
}

/// Why an offer was not forwarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OfferRefusal {
    NoRoute(NoRoute),
    Limited {
        limit: OfferLimit,
        retry_after: Duration,
    },
    AtCapacity {
        retry_after: Duration,
    },
    /// The gateway's control channel is full.
    ControlBusy,
    /// The gateway's control connection ended as C forwarded the offer.
    ControlGone,
}

impl OfferRefusal {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::NoRoute(reason) => reason.as_str(),
            Self::Limited { limit, .. } => limit.as_str(),
            Self::AtCapacity { .. } => "at_capacity",
            Self::ControlBusy => "control_busy",
            Self::ControlGone => "control_gone",
        }
    }
}

impl From<PunchRefusal> for OfferRefusal {
    fn from(refusal: PunchRefusal) -> Self {
        match refusal {
            PunchRefusal::Limited { limit, retry_after } => Self::Limited { limit, retry_after },
            PunchRefusal::AtCapacity { retry_after } => Self::AtCapacity { retry_after },
        }
    }
}

/// Registry of gateways' live control connections, keyed by `relay_node_id`,
/// and of the punches forwarded on them.
pub struct ControlRegistry {
    instances: Mutex<HashMap<String, ControlEntry>>,
    next_token: AtomicU64,
    punches: Arc<PunchRegistry>,
    rendezvous: Option<RendezvousAddress>,
    answer_timeout: Duration,
}

impl Default for ControlRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlRegistry {
    pub fn new() -> Self {
        Self {
            instances: Mutex::new(HashMap::new()),
            next_token: AtomicU64::new(0),
            punches: Arc::new(PunchRegistry::default()),
            rendezvous: None,
            answer_timeout: DIRECT_ANSWER_TIMEOUT,
        }
    }

    /// Hand every forwarded offer a UDP rendezvous at `address`, when the
    /// gateway's capability says it registers. Set only once C's rendezvous
    /// socket is bound ([`crate::udp::RendezvousServer`]).
    pub fn with_udp_rendezvous(mut self, address: RendezvousAddress) -> Self {
        self.rendezvous = Some(address);
        self
    }

    /// Shortens how long a POST waits for the gateway's report, so tests of
    /// the no-answer path do not sleep for the real timeout.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_direct_answer_timeout(mut self, timeout: Duration) -> Self {
        self.answer_timeout = timeout;
        self
    }

    /// A gateway registers its control connection under `relay_node_id` (with its
    /// admitted `remote_api_key` and the supported direct capability from its
    /// hello) and gets the receiver its control loop acts on plus a connection
    /// token to pass to [`Self::unregister_if_owned`]. A re-register supersedes
    /// a stale connection (reconnect wins), and the superseded connection's
    /// punches drop with it.
    pub fn register(
        &self,
        relay_node_id: &str,
        remote_api_key: &str,
        direct: Option<DirectCapability>,
    ) -> Result<(mpsc::Receiver<ControlSignal>, u64), ControlRegisterError> {
        if !node_id_within_bound(relay_node_id) {
            return Err(ControlRegisterError::NodeIdTooLong {
                len: relay_node_id.len(),
            });
        }
        let (tx, rx) = mpsc::channel(CONTROL_CHANNEL_CAP);
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let superseded = {
            let mut instances = self.instances.lock();
            if let Some(existing) = instances.get(relay_node_id) {
                if existing.remote_api_key != remote_api_key {
                    return Err(ControlRegisterError::OwnerMismatch {
                        owner: existing.remote_api_key.clone(),
                    });
                }
                tracing::debug!(
                    relay_node_id = %relay_node_id,
                    key_tag = %key_tag(remote_api_key),
                    "control: new registration superseded a live control connection for this relay_node_id"
                );
            }
            instances
                .insert(
                    relay_node_id.to_string(),
                    ControlEntry {
                        tx,
                        remote_api_key: remote_api_key.to_string(),
                        token,
                        direct,
                    },
                )
                .map(|previous| previous.token)
        };
        if let Some(previous) = superseded {
            self.punches.drop_control(relay_node_id, previous);
        }
        Ok((rx, token))
    }

    /// Signal a registered gateway to open a data leg under `relay_key` of the
    /// given `class` (so the gateway runs the chat loop or API tunnel and meters
    /// accordingly). Returns the gateway's `remote_api_key` on success
    /// (so the caller can meter the resulting leg against it), or `None` if the
    /// gateway isn't connected (the phone's relay attempt then fails fast rather
    /// than hanging) or its control channel is closed.
    pub fn signal_open(
        &self,
        relay_node_id: &str,
        expected_remote_api_key: &str,
        relay_key: &str,
        class: LegClass,
    ) -> Result<String, SignalOpenError> {
        let entry = self
            .instances
            .lock()
            .get(relay_node_id)
            .map(|e| (e.tx.clone(), e.remote_api_key.clone()));
        let Some((tx, remote_api_key)) = entry else {
            return Err(SignalOpenError::NotConnected);
        };
        if remote_api_key != expected_remote_api_key {
            return Err(SignalOpenError::OwnerMismatch);
        }
        let signal = ControlSignal::OpenDataLeg {
            relay_key: relay_key.to_string(),
            class,
        };
        match tx.try_send(signal) {
            Ok(()) => Ok(remote_api_key),
            Err(TrySendError::Closed(_)) => Err(SignalOpenError::NotConnected),
            Err(TrySendError::Full(_)) => Err(SignalOpenError::Backpressure),
        }
    }

    /// Forward P's sealed offer to the node's gateway as a new punch. The
    /// route exists only for a connected, owner-matched, direct-capable
    /// gateway; the punch then needs the offer budgets and caps
    /// ([`PunchRegistry::admit`]), and the signal a free slot on the control
    /// channel. The slot is reserved before the punch is admitted, so an offer
    /// the channel cannot take consumes no budget. The returned
    /// [`PendingOffer`] waits for the gateway's report.
    pub(crate) fn offer_direct(
        &self,
        from: OfferSource<'_>,
        offer: SealedCandidates,
    ) -> Result<PendingOffer, OfferRefusal> {
        if !node_id_within_bound(from.relay_node_id) {
            return Err(OfferRefusal::NoRoute(NoRoute::NodeIdTooLong));
        }
        let instances = self.instances.lock();
        let entry = instances
            .get(from.relay_node_id)
            .ok_or(OfferRefusal::NoRoute(NoRoute::NotConnected))?;
        if entry.remote_api_key != from.remote_api_key {
            return Err(OfferRefusal::NoRoute(NoRoute::OwnerMismatch));
        }
        let capability = entry
            .direct
            .ok_or(OfferRefusal::NoRoute(NoRoute::NotCapable))?;
        let rendezvous = self
            .rendezvous
            .as_ref()
            .filter(|_| capability.udp)
            .map(RendezvousAddress::as_str);
        let slot = entry.tx.try_reserve().map_err(|refused| match refused {
            TrySendError::Full(()) => OfferRefusal::ControlBusy,
            TrySendError::Closed(()) => OfferRefusal::ControlGone,
        })?;
        let admitted = self.punches.admit(PunchRequest {
            relay_node_id: from.relay_node_id,
            remote_api_key: from.remote_api_key,
            control_token: entry.token,
            source: from.client,
            rendezvous,
            answer_timeout: self.answer_timeout,
        })?;
        slot.send(ControlSignal::DirectOffer {
            punch_id: admitted.pending.punch_id(),
            offer,
            register: admitted.register,
        });
        Ok(admitted.pending)
    }

    /// Route a [`ControlReport`] that arrived on the control connection
    /// `control_token` of `relay_node_id` to the POST waiting on its punch.
    pub(crate) fn report_direct(
        &self,
        relay_node_id: &str,
        control_token: u64,
        report: ControlReport,
    ) -> ReportOutcome {
        self.punches.resolve(relay_node_id, control_token, report)
    }

    pub(crate) fn punches(&self) -> &PunchRegistry {
        &self.punches
    }

    /// Drop a gateway's control connection on disconnect — but only if `token`
    /// still owns the slot. A stale connection's cleanup firing *after* a faster
    /// reconnect already replaced the entry (both share the same `relay_node_id` +
    /// `remote_api_key`, so the key can't tell them apart) must not evict the live
    /// owner, which would leave the gateway connected but unroutable until its next
    /// control redial. The connection token is the identity that distinguishes them.
    /// The connection's punches drop either way.
    pub fn unregister_if_owned(&self, relay_node_id: &str, token: u64) {
        {
            let mut instances = self.instances.lock();
            if instances
                .get(relay_node_id)
                .is_some_and(|entry| entry.token == token)
            {
                instances.remove(relay_node_id);
            }
        }
        self.punches.drop_control(relay_node_id, token);
    }

    /// Number of gateways with a live control connection.
    pub fn connected(&self) -> usize {
        self.instances.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RelayBroker;
    use crate::punch::{DIRECT_OFFERS_PER_SOURCE_PER_MINUTE, OfferOutcome};
    use remote_host_protocol::relay::{
        DIRECT_PROTOCOL_VERSION, MAX_SEALED_CANDIDATES_BYTES, PunchId,
    };

    const CAPABLE: Option<DirectCapability> = Some(DirectCapability {
        version: DIRECT_PROTOCOL_VERSION,
        udp: true,
    });

    fn sealed() -> SealedCandidates {
        SealedCandidates {
            n: "nonce".into(),
            enc: "ciphertext".into(),
        }
    }

    fn from<'a>(node: &'a str, key: &'a str) -> OfferSource<'a> {
        OfferSource {
            relay_node_id: node,
            remote_api_key: key,
            client: None,
        }
    }

    fn rendezvous() -> RendezvousAddress {
        RendezvousAddress::parse("rendezvous.example.com:7777").unwrap()
    }

    #[tokio::test]
    async fn signals_a_registered_gateway_to_open_a_leg() {
        let reg = ControlRegistry::new();
        let (mut rx, _token) = reg.register("node-1", "inst-A", None).unwrap();
        assert_eq!(reg.connected(), 1);

        // The signal succeeds and reports the gateway's owning remote_api_key.
        assert_eq!(
            reg.signal_open("node-1", "inst-A", "leg-abc", LegClass::Chat),
            Ok("inst-A".to_string())
        );
        assert_eq!(
            rx.recv().await.unwrap(),
            ControlSignal::OpenDataLeg {
                relay_key: "leg-abc".into(),
                class: LegClass::Chat,
            },
        );
    }

    #[test]
    fn open_data_leg_serializes_to_gateway_wire_shape() {
        // Must match the gateway's `ControlServerMsg` JSON byte-for-byte. A Chat
        // leg omits no field the gateway requires; `class` rides as a snake_case
        // string (and a pre-class gateway tolerates it via `#[serde(default)]`).
        let v = serde_json::to_value(ControlSignal::OpenDataLeg {
            relay_key: "k".into(),
            class: LegClass::Blob,
        })
        .unwrap();
        assert_eq!(v["t"], "open_data_leg");
        assert_eq!(v["relay_key"], "k");
        assert_eq!(v["class"], "blob");

        let v = serde_json::to_value(ControlSignal::OpenDataLeg {
            relay_key: "k".into(),
            class: LegClass::Api,
        })
        .unwrap();
        assert_eq!(v["class"], "api");
    }

    #[tokio::test]
    async fn signal_to_unknown_gateway_fails_fast() {
        let reg = ControlRegistry::new();
        assert!(matches!(
            reg.signal_open("ghost", "inst-A", "k", LegClass::Chat),
            Err(SignalOpenError::NotConnected)
        ));
    }

    #[tokio::test]
    async fn unregister_drops_the_connection() {
        let reg = ControlRegistry::new();
        let (_rx, token) = reg.register("n", "inst-A", None).unwrap();
        reg.unregister_if_owned("n", token);
        assert_eq!(reg.connected(), 0);
        assert!(matches!(
            reg.signal_open("n", "inst-A", "k", LegClass::Chat),
            Err(SignalOpenError::NotConnected)
        ));
    }

    #[tokio::test]
    async fn stale_unregister_does_not_evict_a_reconnected_owner() {
        // A fast gateway reconnect (same node + same remote_api_key) supersedes the
        // old slot. The old connection's cleanup then fires with its stale token —
        // it must NOT evict the live (reconnected) owner, or the gateway would be
        // connected yet unroutable until its next control redial. This is the exact
        // race the connection token was added to close (mirrors the gateway-side
        // `ChannelControlRegistry::unregister_if_owned`).
        let reg = ControlRegistry::new();
        let (_rx_old, old_token) = reg.register("node-1", "inst-A", None).unwrap();
        let (_rx_new, _new_token) = reg.register("node-1", "inst-A", None).unwrap();
        assert_eq!(reg.connected(), 1, "reconnect supersedes, not duplicates");

        // Stale cleanup with the old token is a no-op against the new owner.
        reg.unregister_if_owned("node-1", old_token);
        assert_eq!(reg.connected(), 1, "live owner survives stale cleanup");
        assert!(matches!(
            reg.signal_open("node-1", "inst-A", "k", LegClass::Chat),
            Ok(key) if key == "inst-A"
        ));
    }

    #[test]
    fn different_key_cannot_replace_registered_node() {
        let reg = ControlRegistry::new();
        let (_rx, _token) = reg.register("node-1", "inst-A", None).unwrap();
        assert!(matches!(
            reg.register("node-1", "inst-B", None),
            Err(ControlRegisterError::OwnerMismatch { owner }) if owner == "inst-A"
        ));
        assert_eq!(
            reg.signal_open("node-1", "inst-B", "leg-abc", LegClass::Chat),
            Err(SignalOpenError::OwnerMismatch)
        );
    }

    #[test]
    fn a_node_id_past_the_protocol_bound_never_becomes_a_key() {
        let reg = ControlRegistry::new();
        let longest = "n".repeat(MAX_RELAY_NODE_ID_BYTES);
        let too_long = "n".repeat(MAX_RELAY_NODE_ID_BYTES + 1);
        assert!(reg.register(&longest, "inst-A", CAPABLE).is_ok());
        assert_eq!(
            reg.register(&too_long, "inst-A", CAPABLE).err(),
            Some(ControlRegisterError::NodeIdTooLong {
                len: MAX_RELAY_NODE_ID_BYTES + 1
            })
        );
        assert_eq!(reg.connected(), 1);
        assert_eq!(
            reg.offer_direct(from(&too_long, "inst-A"), sealed()).err(),
            Some(OfferRefusal::NoRoute(NoRoute::NodeIdTooLong))
        );
    }

    /// The control plane + the byte-pipe compose: C signals A to open a leg
    /// under a key, both legs join the broker under that key, and bytes flow.
    #[tokio::test]
    async fn control_signal_drives_a_relay_match() {
        let reg = ControlRegistry::new();
        let broker = RelayBroker::new();

        // Gateway A registers its control connection.
        let (mut a_control, _token) = reg.register("node-1", "inst-A", None).unwrap();

        // A phone arrives at the relay for node-1: it joins the broker and C
        // signals A to open the matching data leg.
        let phone = broker.join("leg-xyz").expect("phone leg parks");
        assert_eq!(
            reg.signal_open("node-1", "inst-A", "leg-xyz", LegClass::Chat),
            Ok("inst-A".to_string())
        );

        // A acts on the signal: opens a data leg under the same key.
        let Some(ControlSignal::OpenDataLeg { relay_key, .. }) = a_control.recv().await else {
            panic!("expected an OpenDataLeg signal");
        };
        let mut gateway = broker.join(&relay_key).expect("gateway leg matches");

        // The two legs are matched; opaque frames flow blind.
        phone.to_peer.send(b"noise-frame".to_vec()).await.unwrap();
        assert_eq!(gateway.from_peer.recv().await.unwrap(), b"noise-frame");
    }

    #[tokio::test]
    async fn offers_route_only_to_the_owners_capable_gateway() {
        let reg = ControlRegistry::new();
        assert_eq!(
            reg.offer_direct(from("node-1", "inst-A"), sealed()).err(),
            Some(OfferRefusal::NoRoute(NoRoute::NotConnected))
        );
        let (_legacy_rx, _) = reg.register("legacy", "inst-A", None).unwrap();
        assert_eq!(
            reg.offer_direct(from("legacy", "inst-A"), sealed()).err(),
            Some(OfferRefusal::NoRoute(NoRoute::NotCapable))
        );
        let (mut rx, _) = reg.register("node-1", "inst-A", CAPABLE).unwrap();
        assert_eq!(
            reg.offer_direct(from("node-1", "inst-B"), sealed()).err(),
            Some(OfferRefusal::NoRoute(NoRoute::OwnerMismatch))
        );

        let pending = reg
            .offer_direct(from("node-1", "inst-A"), sealed())
            .unwrap();
        let Some(ControlSignal::DirectOffer {
            punch_id,
            offer,
            register,
        }) = rx.recv().await
        else {
            panic!("expected a DirectOffer signal");
        };
        assert_eq!(punch_id, pending.punch_id());
        assert_eq!(offer, sealed(), "the sealed offer is forwarded untouched");
        assert_eq!(register, None, "C runs no rendezvous");
    }

    #[tokio::test]
    async fn the_rendezvous_is_offered_only_to_a_gateway_that_registers() {
        let reg = ControlRegistry::new().with_udp_rendezvous(rendezvous());
        let (mut udp_rx, _) = reg.register("udp", "inst-A", CAPABLE).unwrap();
        let (mut host_rx, _) = reg
            .register(
                "host-only",
                "inst-A",
                Some(DirectCapability {
                    version: DIRECT_PROTOCOL_VERSION,
                    udp: false,
                }),
            )
            .unwrap();

        let _udp = reg.offer_direct(from("udp", "inst-A"), sealed()).unwrap();
        let Some(ControlSignal::DirectOffer {
            register: Some(register),
            ..
        }) = udp_rx.recv().await
        else {
            panic!("expected a DirectOffer with a rendezvous");
        };
        assert_eq!(register.address, "rendezvous.example.com:7777");

        let _host = reg
            .offer_direct(from("host-only", "inst-A"), sealed())
            .unwrap();
        let Some(ControlSignal::DirectOffer { register, .. }) = host_rx.recv().await else {
            panic!("expected a DirectOffer");
        };
        assert_eq!(register, None);
    }

    /// The source still has its whole offer budget: exactly
    /// `DIRECT_OFFERS_PER_SOURCE_PER_MINUTE` offers pass, then the source rate
    /// refuses.
    fn assert_whole_source_budget(reg: &ControlRegistry, rx: &mut mpsc::Receiver<ControlSignal>) {
        while rx.try_recv().is_ok() {}
        for _ in 0..DIRECT_OFFERS_PER_SOURCE_PER_MINUTE {
            drop(
                reg.offer_direct(from("node-1", "inst-A"), sealed())
                    .unwrap(),
            );
            assert!(matches!(
                rx.try_recv(),
                Ok(ControlSignal::DirectOffer { .. })
            ));
        }
        assert!(matches!(
            reg.offer_direct(from("node-1", "inst-A"), sealed()).err(),
            Some(OfferRefusal::Limited {
                limit: OfferLimit::SourceRate,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_full_control_channel_refuses_the_offer_and_keeps_no_punch() {
        let reg = ControlRegistry::new();
        let (mut rx, _) = reg.register("node-1", "inst-A", CAPABLE).unwrap();
        for i in 0..CONTROL_CHANNEL_CAP {
            reg.signal_open("node-1", "inst-A", &format!("k{i}"), LegClass::Chat)
                .unwrap();
        }
        for _ in 0..DIRECT_OFFERS_PER_SOURCE_PER_MINUTE {
            assert_eq!(
                reg.offer_direct(from("node-1", "inst-A"), sealed()).err(),
                Some(OfferRefusal::ControlBusy)
            );
        }
        assert_eq!(reg.punches().len(), 0);
        assert_whole_source_budget(&reg, &mut rx);
    }

    #[tokio::test]
    async fn a_closed_control_channel_refuses_the_offer_and_consumes_no_budget() {
        let reg = ControlRegistry::new();
        let (rx, _) = reg.register("node-1", "inst-A", CAPABLE).unwrap();
        drop(rx);
        for _ in 0..DIRECT_OFFERS_PER_SOURCE_PER_MINUTE {
            assert_eq!(
                reg.offer_direct(from("node-1", "inst-A"), sealed()).err(),
                Some(OfferRefusal::ControlGone)
            );
        }
        assert_eq!(reg.punches().len(), 0);
        let (mut rx, _) = reg.register("node-1", "inst-A", CAPABLE).unwrap();
        assert_whole_source_budget(&reg, &mut rx);
    }

    #[tokio::test]
    async fn a_report_settles_only_its_own_connections_punch() {
        let reg = ControlRegistry::new();
        let (_rx, token) = reg.register("node-1", "inst-A", CAPABLE).unwrap();
        let (_other_rx, other_token) = reg.register("node-2", "inst-A", CAPABLE).unwrap();
        let pending = reg
            .offer_direct(from("node-1", "inst-A"), sealed())
            .unwrap();
        let punch_id = pending.punch_id();
        let declined = || ControlReport::DirectDeclined { punch_id };

        assert_eq!(
            reg.report_direct("node-2", other_token, declined()),
            ReportOutcome::Foreign
        );
        assert_eq!(
            reg.report_direct("node-1", other_token, declined()),
            ReportOutcome::Foreign
        );
        assert_eq!(
            reg.report_direct(
                "node-1",
                token,
                ControlReport::DirectDeclined {
                    punch_id: PunchId::generate()
                }
            ),
            ReportOutcome::UnknownPunch
        );
        assert_eq!(
            reg.report_direct(
                "node-1",
                token,
                ControlReport::DirectAnswer {
                    punch_id,
                    answer: SealedCandidates {
                        n: String::new(),
                        enc: "x".repeat(MAX_SEALED_CANDIDATES_BYTES + 1),
                    },
                }
            ),
            ReportOutcome::OversizedAnswer
        );
        assert_eq!(
            reg.report_direct("node-1", token, declined()),
            ReportOutcome::Delivered
        );
        assert_eq!(
            reg.report_direct("node-1", token, declined()),
            ReportOutcome::AlreadySettled
        );
        assert!(matches!(pending.outcome().await, OfferOutcome::Declined));
        assert_eq!(reg.punches().len(), 0, "a declined punch drops at once");
    }

    #[tokio::test]
    async fn a_control_disconnect_drops_its_punches_and_ends_the_wait() {
        let reg = ControlRegistry::new().with_udp_rendezvous(rendezvous());
        let (_rx, token) = reg.register("node-1", "inst-A", CAPABLE).unwrap();
        let (_other_rx, _) = reg.register("node-2", "inst-A", CAPABLE).unwrap();
        let waiting = reg
            .offer_direct(from("node-1", "inst-A"), sealed())
            .unwrap();
        let answered = reg
            .offer_direct(from("node-1", "inst-A"), sealed())
            .unwrap();
        let other = reg
            .offer_direct(from("node-2", "inst-A"), sealed())
            .unwrap();
        let answered_id = answered.punch_id();
        assert_eq!(
            reg.report_direct(
                "node-1",
                token,
                ControlReport::DirectAnswer {
                    punch_id: answered_id,
                    answer: sealed(),
                }
            ),
            ReportOutcome::Delivered
        );
        assert!(matches!(
            answered.outcome().await,
            OfferOutcome::Answered(_)
        ));
        assert!(
            reg.punches().contains(answered_id),
            "a 200 with a rendezvous keeps its punch"
        );

        reg.unregister_if_owned("node-1", token);
        assert!(!reg.punches().contains(answered_id));
        assert!(
            matches!(waiting.outcome().await, OfferOutcome::NoAnswer),
            "a POST still waiting ends at once"
        );
        assert!(
            reg.punches().contains(other.punch_id()),
            "another node's punch is untouched"
        );
    }

    #[tokio::test]
    async fn a_reconnect_drops_the_superseded_connections_punches() {
        let reg = ControlRegistry::new();
        let (_old_rx, _) = reg.register("node-1", "inst-A", CAPABLE).unwrap();
        let stale = reg
            .offer_direct(from("node-1", "inst-A"), sealed())
            .unwrap();
        let stale_id = stale.punch_id();
        let (_new_rx, _) = reg.register("node-1", "inst-A", CAPABLE).unwrap();
        assert!(!reg.punches().contains(stale_id));
        assert!(matches!(stale.outcome().await, OfferOutcome::NoAnswer));
    }
}
