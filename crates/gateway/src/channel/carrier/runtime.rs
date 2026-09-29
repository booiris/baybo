//! The carrier runtime of one relay binding scope (`relay_content::run`). It is
//! started before the scope's first control connection, so the capability in
//! every hello describes the families it serves, and it is stopped when the
//! scope ends; a control redial inside the scope leaves it running.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use carrier::quic::{ServerIdentity, server_config};
use device_proto::aead::KEY_LEN;
use device_proto::candidates::{
    CANDIDATE_SET_VERSION, CertHash, DeviceOffer, GatewayAnswer, GatewaySealer,
};
use futures::future::join_all;
use parking_lot::Mutex;
use remote_host_protocol::relay::{
    AddressPolicy, ControlReport, DIRECT_PROTOCOL_VERSION, DirectCapability, DirectToken,
    MAX_TCP_CANDIDATES, PunchId, SealedCandidates, UdpRendezvous,
};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use super::error::CarrierBindingError;
use super::gather::{Unoffered, advertised_candidates, gather, is_candidate, punch_pairs};
use super::interfaces;
use super::offer::{ACCEPTED, Decline, OfferGate};
use super::probe::{
    MAX_HOST_PUNCH_PAIRS, PunchTarget, REPLY_QUEUE_CAPACITY, punch_hosts, register_and_punch,
};
use super::punches::{NewPunch, PunchTable};
use super::quic::{
    CARRIER_REVOKED, CARRIER_REVOKED_REASON, ConnectionSlots, FIRST_STREAM_DEADLINE, IncomingCounts,
};
use super::session::DIRECT_OPEN_DEADLINE;
use super::tcp::{TcpAdmission, TcpFamily};
use super::udp::{BoundSocket, UdpFamily, supervise};
use crate::channel::state::WsChannelState;
use crate::config::RuntimeCarrierConfig;
use crate::device::load_or_create_static_keypair;

/// The wait before a family whose bind failed is bound again, UDP or TCP,
/// and before a broken TCP listener is; also the spacing of retries after a
/// UDP family's persistent receive errors (its first rebind is immediate)
/// and the pause of a TCP listener out of resources.
pub(crate) const REBIND_DELAY: Duration = Duration::from_secs(2);
/// How long a stopping runtime waits for its closed QUIC endpoints to drain
/// and for quinn to release their sockets. It covers the 3×PTO drain of a
/// connection closed mid-handshake (about 3 s at quinn's 333 ms initial RTT),
/// so a fixed-port family is free again when the next runtime binds it.
pub(crate) const CARRIER_DRAIN_GRACE: Duration = Duration::from_secs(4);

/// quinn signals nothing when it drops its last reference to a socket, so a
/// stopping runtime polls for that at this interval.
const SOCKET_RELEASE_POLL: Duration = Duration::from_millis(10);

/// The deadlines and delays a carrier runtime runs under:
/// [`CarrierTiming::PRODUCTION`] outside tests, which shrink them to
/// milliseconds.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CarrierTiming {
    pub(crate) direct_open_deadline: Duration,
    pub(crate) first_stream_deadline: Duration,
    pub(crate) rebind_delay: Duration,
}

impl CarrierTiming {
    pub(crate) const PRODUCTION: Self = Self {
        direct_open_deadline: DIRECT_OPEN_DEADLINE,
        first_stream_deadline: FIRST_STREAM_DEADLINE,
        rebind_delay: REBIND_DELAY,
    };
}

/// The approved device a binding scope is entered for.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BindingDevice<'a> {
    pub(crate) device_id: &'a str,
    pub(crate) device_pubkey: &'a [u8],
}

/// What a binding's carrier runtime serves: the approved device, the
/// candidate keys of its pairing, and the gateway state its sessions run on.
pub(crate) struct CarrierBinding {
    pub(crate) state: WsChannelState,
    pub(crate) relay_node_id: String,
    pub(crate) device_id: String,
    pub(crate) sealer: GatewaySealer,
    pub(crate) timing: CarrierTiming,
}

impl CarrierBinding {
    /// Derives the binding's [`GatewaySealer`] from the gateway's static key
    /// and the device's, with production timing.
    pub(crate) async fn derive(
        state: &WsChannelState,
        relay_node_id: &str,
        device_id: &str,
        device_pubkey: &[u8],
    ) -> Result<Self, CarrierBindingError> {
        let gateway = load_or_create_static_keypair(&state.secret_vault)
            .await
            .map_err(|error| CarrierBindingError::StaticKey {
                reason: error.to_string(),
            })?;
        let secret = Zeroizing::new(gateway.secret());
        let device_public: [u8; KEY_LEN] =
            device_pubkey
                .try_into()
                .map_err(|_| CarrierBindingError::DevicePublicKey {
                    len: device_pubkey.len(),
                    expected: KEY_LEN,
                })?;
        Ok(Self {
            state: state.clone(),
            relay_node_id: relay_node_id.to_owned(),
            device_id: device_id.to_owned(),
            sealer: GatewaySealer::derive(&secret, &device_public)?,
            timing: CarrierTiming::PRODUCTION,
        })
    }
}

/// What a gateway process keeps across the carrier runtimes of its binding
/// scopes: A's QUIC certificate, generated the first time a runtime has
/// something to bind, whose hash every answer carries; and the offer gate,
/// whose replay cache a Reconfigure, which starts a new runtime, must not
/// empty.
pub(crate) struct CarrierProcess {
    identity: Option<ServerIdentity>,
    offers: OfferGate,
}

impl CarrierProcess {
    pub(crate) fn new() -> Self {
        Self {
            identity: None,
            offers: OfferGate::new(unix_now_ms()),
        }
    }

    #[cfg(test)]
    pub(crate) fn identity(&self) -> Option<&ServerIdentity> {
        self.identity.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn offers(&self) -> &OfferGate {
        &self.offers
    }
}

/// What every task of one runtime shares.
pub(crate) struct RuntimeContext {
    pub(crate) state: WsChannelState,
    pub(crate) device_id: String,
    /// Minted per runtime from a CSPRNG; zeroized on drop and never printed.
    pub(crate) token: DirectToken,
    pub(crate) sealer: GatewaySealer,
    pub(crate) timing: CarrierTiming,
    pub(crate) policy: AddressPolicy,
    pub(crate) punches: Mutex<PunchTable>,
    pub(crate) connections: Arc<ConnectionSlots>,
    pub(crate) incoming: IncomingCounts,
    pub(crate) tcp: Arc<TcpAdmission>,
}

/// A binding's carrier runtime. It is inactive, with nothing bound and no
/// capability, when the binding has no candidate keys, nothing is configured
/// or no QUIC certificate can be generated, or when the QUIC server
/// configuration cannot be built and no TCP listener is configured. A
/// configured family whose bind fails keeps it active.
pub(crate) struct CarrierRuntime {
    active: Option<ActiveRuntime>,
}

struct ActiveRuntime {
    context: Arc<RuntimeContext>,
    relay_node_id: String,
    cert_hash: CertHash,
    /// The process's, shared with every other runtime of it.
    gate: OfferGate,
    /// Each UDP and TCP family is supervised for the runtime's life: one
    /// whose bind fails is bound again, so it stays here unbound until then.
    udp: Vec<Arc<UdpFamily>>,
    tcp: Vec<Arc<TcpFamily>>,
    /// The `gateway.direct_tcp.advertised_addresses` A offers while a
    /// listener is bound ([`advertised_candidates`]).
    advertised: Vec<SocketAddr>,
    /// Every task the runtime spawns. Each ends when `cancel` fires, a
    /// family's task only after the connections, streams or TCP sessions
    /// under it have ended, so stopping drops every session before any
    /// endpoint closes.
    tasks: JoinSet<()>,
    cancel: CancellationToken,
}

impl CarrierRuntime {
    pub(crate) fn inactive() -> Self {
        Self { active: None }
    }

    /// Starts the runtime of a binding scope: derives the binding's
    /// candidate keys, then [`bind`](Self::bind)s. With nothing configured it
    /// derives nothing and stays inactive; a binding whose device key cannot
    /// yield candidate keys stays inactive too, so its hellos carry no
    /// capability. A failed read of the gateway's static key may pass, so it
    /// is returned for the caller to retry.
    pub(crate) async fn start(
        config: &RuntimeCarrierConfig,
        process: &mut CarrierProcess,
        state: &WsChannelState,
        relay_node_id: &str,
        device: BindingDevice<'_>,
    ) -> Result<Self, CarrierBindingError> {
        if !config.binds_anything() {
            return Ok(Self::inactive());
        }
        match CarrierBinding::derive(state, relay_node_id, device.device_id, device.device_pubkey)
            .await
        {
            Ok(binding) => Ok(Self::bind(config, process, binding)),
            Err(error) if error.is_transient() => Err(error),
            Err(error) => {
                tracing::warn!(
                    device = %crate::channel::short_hash(device.device_id),
                    %error,
                    "carrier: no candidate keys for this binding; direct carriers stay off"
                );
                Ok(Self::inactive())
            }
        }
    }

    /// Binds the configured UDP sockets, one QUIC endpoint on each, and the
    /// configured TCP listeners, and starts serving them. A family whose
    /// first bind fails, UDP or TCP, is bound again every [`REBIND_DELAY`]
    /// until it succeeds. With no family served, the runtime is inactive.
    /// The certificate and the offer gate are the process's.
    pub(crate) fn bind(
        config: &RuntimeCarrierConfig,
        process: &mut CarrierProcess,
        binding: CarrierBinding,
    ) -> Self {
        if !config.binds_anything() {
            return Self::inactive();
        }
        if process.identity.is_none() {
            match ServerIdentity::generate() {
                Ok(generated) => process.identity = Some(generated),
                Err(error) => {
                    tracing::warn!(error = %error, "carrier: no QUIC certificate; direct carriers stay off");
                    return Self::inactive();
                }
            }
        }
        let Some(identity) = process.identity.as_ref() else {
            return Self::inactive();
        };
        let context = Arc::new(RuntimeContext {
            state: binding.state,
            device_id: binding.device_id,
            token: DirectToken::generate(),
            sealer: binding.sealer,
            timing: binding.timing,
            policy: AddressPolicy::active(),
            punches: Mutex::new(PunchTable::default()),
            connections: Arc::new(ConnectionSlots::default()),
            incoming: IncomingCounts::default(),
            tcp: Arc::new(TcpAdmission::default()),
        });
        let cancel = CancellationToken::new();
        let mut tasks = JoinSet::new();
        let udp = bind_udp(&config.udp_binds(), identity, &context, &cancel, &mut tasks);
        let tcp: Vec<Arc<TcpFamily>> = config
            .tcp_binds()
            .into_iter()
            .map(|bind| {
                let (family, listener) = TcpFamily::bind(bind);
                tasks.spawn(super::tcp::serve(
                    Arc::clone(&family),
                    listener,
                    Arc::clone(&context),
                    cancel.clone(),
                ));
                family
            })
            .collect();
        if udp.is_empty() && tcp.is_empty() {
            return Self::inactive();
        }
        let configured = config
            .tcp
            .as_ref()
            .map_or(&[][..], |tcp| &tcp.advertised_addresses[..]);
        let (advertised, unoffered) = advertised_candidates(configured, &context.policy);
        for (index, reason) in unoffered {
            match reason {
                Unoffered::NotACandidate => tracing::warn!(
                    index,
                    "carrier: a gateway.direct_tcp.advertised_addresses entry has no address class; it is never offered"
                ),
                Unoffered::OverCap => tracing::warn!(
                    index,
                    cap = MAX_TCP_CANDIDATES,
                    "carrier: a gateway.direct_tcp.advertised_addresses entry comes after the cap; it is never offered"
                ),
            }
        }
        let runtime = ActiveRuntime {
            context,
            relay_node_id: binding.relay_node_id,
            cert_hash: identity.cert_hash(),
            gate: process.offers.clone(),
            udp,
            tcp,
            advertised,
            tasks,
            cancel,
        };
        tracing::debug!(
            udp = ?runtime.bound_addresses(),
            tcp = ?runtime.listening(),
            "carrier: runtime started"
        );
        Self {
            active: Some(runtime),
        }
    }

    /// The capability the scope's hellos carry: present while the runtime is
    /// active, with `udp` when it serves an IPv4 UDP family. It names the
    /// families the runtime serves, not what is bound this instant, so it
    /// is the same for the runtime's life and no hello goes stale: a family
    /// being bound again declines or leaves out its part of each offer
    /// meanwhile.
    pub(crate) fn capability(&self) -> Option<DirectCapability> {
        let active = self.active.as_ref()?;
        Some(DirectCapability {
            version: DIRECT_PROTOCOL_VERSION,
            udp: active.udp.iter().any(|family| family.serves_ipv4()),
        })
    }

    /// The one port a delivered `DirectOffer` enters through; the caller
    /// writes the report back on the control connection that delivered the
    /// offer. It never waits: the punches, the registration and the
    /// rendezvous lookup run as runtime tasks. Any failed rule declines the
    /// offer, with no punch. An active runtime records the outcome as the
    /// binding device's last offer in the link table; an inactive one has
    /// sent no capability, so no offer is meant to reach it.
    pub(crate) fn handle_offer(
        &mut self,
        punch_id: PunchId,
        offer: SealedCandidates,
        register: Option<UdpRendezvous>,
    ) -> ControlReport {
        let answered = match self.active.as_mut() {
            Some(runtime) => {
                let answered = runtime.answer(punch_id, &offer, register);
                runtime.context.state.device_links.record_offer(
                    &runtime.context.device_id,
                    answered.as_ref().map(|_| ()).map_err(|decline| *decline),
                );
                answered
            }
            None => Err(Decline::Unbound),
        };
        match answered {
            Ok(answer) => ControlReport::DirectAnswer { punch_id, answer },
            Err(decline) => {
                tracing::info!(
                    punch = %punch_id.tag(),
                    device = %self.device_tag(),
                    outcome = decline.outcome(),
                    "direct_offer"
                );
                ControlReport::DirectDeclined { punch_id }
            }
        }
    }

    fn device_tag(&self) -> String {
        self.active.as_ref().map_or_else(String::new, |runtime| {
            crate::channel::short_hash(&runtime.context.device_id)
        })
    }

    /// Ends every task first, each QUIC session dropped before its
    /// connection closes with `CARRIER_REVOKED` and each TCP session with its
    /// listener, then closes each endpoint and waits, bounded by
    /// [`CARRIER_DRAIN_GRACE`], until every UDP socket is closed.
    pub(crate) async fn stop(self) {
        let Some(runtime) = self.active else {
            return;
        };
        let ActiveRuntime {
            udp,
            mut tasks,
            cancel,
            ..
        } = runtime;
        cancel.cancel();
        while tasks.join_next().await.is_some() {}
        let bound: Vec<BoundSocket> = udp.iter().filter_map(|family| family.take()).collect();
        for socket in &bound {
            socket
                .endpoint
                .close(CARRIER_REVOKED, CARRIER_REVOKED_REASON);
        }
        let addresses: Vec<SocketAddr> = bound.iter().map(|socket| socket.local_addr).collect();
        if tokio::time::timeout(CARRIER_DRAIN_GRACE, release(bound))
            .await
            .is_err()
        {
            tracing::debug!(
                udp = ?addresses,
                "carrier: a socket is still held by a draining connection at the grace deadline; it closes when the drain ends"
            );
        }
    }
}

impl ActiveRuntime {
    fn bound(&self) -> Vec<BoundSocket> {
        self.udp
            .iter()
            .filter_map(|family| family.current())
            .collect()
    }

    fn bound_addresses(&self) -> Vec<SocketAddr> {
        self.bound()
            .iter()
            .map(|socket| socket.local_addr)
            .collect()
    }

    /// The addresses the TCP listeners accept on, while bound.
    fn listening(&self) -> Vec<SocketAddr> {
        self.tcp
            .iter()
            .filter_map(|family| family.local_addr())
            .collect()
    }

    /// Spawns a task that ends when `cancel` fires, and reaps finished ones.
    fn spawn_cancellable(
        &mut self,
        cancel: CancellationToken,
        task: impl Future<Output = ()> + Send + 'static,
    ) {
        while self.tasks.try_join_next().is_some() {}
        self.tasks.spawn(async move {
            cancel.run_until_cancelled(task).await;
        });
    }

    /// Opens and checks the offer, seals the answer, registers the punch,
    /// and starts its punches and registration.
    fn answer(
        &mut self,
        punch_id: PunchId,
        sealed: &SealedCandidates,
        register: Option<UdpRendezvous>,
    ) -> Result<SealedCandidates, Decline> {
        let bound = self.bound();
        let listening = self.listening();
        if bound.is_empty() && listening.is_empty() {
            return Err(Decline::Unbound);
        }
        let context = Arc::clone(&self.context);
        let offer = context
            .sealer
            .open_offer(&self.relay_node_id, sealed)
            .map_err(|error| {
                tracing::debug!(punch = %punch_id.tag(), %error, "direct_offer: the offer did not open");
                Decline::Auth
            })?;
        let now_ms = unix_now_ms();
        self.gate.admit(&offer, now_ms).inspect_err(|decline| {
            if *decline == Decline::Stale {
                tracing::info!(
                    punch = %punch_id.tag(),
                    skew_ms = i128::from(now_ms) - i128::from(offer.issued_at_ms),
                    "direct_offer: stale offer; the phone's and gateway's clocks may disagree"
                );
            }
        })?;

        let local: Vec<SocketAddr> = bound.iter().map(|socket| socket.local_addr).collect();
        let own = gather(
            &local,
            &listening,
            &self.advertised,
            &context.policy,
            interfaces::enumerate,
        );
        let answer = GatewayAnswer {
            v: CANDIDATE_SET_VERSION,
            issued_at_ms: now_ms,
            offer_id: offer.offer_id,
            token: context.token.clone(),
            quic_cert_sha256: self.cert_hash,
            udp: own.udp.clone(),
            tcp: own.tcp,
        };
        let sealed_answer = context
            .sealer
            .seal_answer(&self.relay_node_id, &punch_id, &answer)
            .map_err(|error| {
                tracing::warn!(punch = %punch_id.tag(), %error, "direct_offer: the answer did not seal");
                Decline::Auth
            })?;

        let targets = device_candidates(&offer, &context.policy, punch_id);
        let ipv4 = bound.iter().find(|socket| socket.local_addr.is_ipv4());
        let registration = register.zip(ipv4.map(|socket| Arc::clone(&socket.socket)));
        let (replies, replies_rx) = match registration {
            Some(_) => {
                let (replies, replies_rx) = mpsc::channel(REPLY_QUEUE_CAPACITY);
                (Some(replies), Some(replies_rx))
            }
            None => (None, None),
        };
        let punch_tasks = self.cancel.child_token();
        let superseded = context.punches.lock().insert(
            NewPunch {
                punch_id,
                offer_id: offer.offer_id,
                hosts: targets.iter().map(SocketAddr::ip).collect(),
                replies,
                tasks: punch_tasks.clone().drop_guard(),
            },
            Instant::now(),
        );

        let punches: Vec<PunchTarget> =
            punch_pairs(&own.udp, &targets, &context.policy, MAX_HOST_PUNCH_PAIRS)
                .into_iter()
                .filter_map(|pair| {
                    let socket = bound
                        .iter()
                        .find(|socket| socket.local_addr.is_ipv4() == pair.target.is_ipv4())?;
                    Some(PunchTarget {
                        socket: Arc::clone(&socket.socket),
                        pair,
                    })
                })
                .collect();
        self.spawn_cancellable(punch_tasks.clone(), punch_hosts(punch_id, punches));
        if let (Some((rendezvous, socket)), Some(replies_rx)) = (registration, replies_rx) {
            self.spawn_cancellable(
                punch_tasks,
                register_and_punch(
                    Arc::clone(&context),
                    socket,
                    punch_id,
                    rendezvous,
                    replies_rx,
                ),
            );
        }
        tracing::info!(
            punch = %punch_id.tag(),
            device = %crate::channel::short_hash(&context.device_id),
            outcome = ACCEPTED,
            superseded = superseded.map(|superseded| superseded.tag()),
            "direct_offer"
        );
        Ok(sealed_answer)
    }
}

/// P's host candidates that have an address class. An authenticated entry
/// without one came from the paired peer, so it signals a bug: it is dropped
/// and logged.
fn device_candidates(
    offer: &DeviceOffer,
    policy: &AddressPolicy,
    punch_id: PunchId,
) -> Vec<SocketAddr> {
    offer
        .udp
        .iter()
        .map(|candidate| AddressPolicy::canonical_socket_addr(*candidate))
        .filter(|candidate| {
            let usable = is_candidate(*candidate, policy);
            if !usable {
                tracing::debug!(
                    punch = %punch_id.tag(),
                    %candidate,
                    "direct_offer: dropped an excluded device candidate"
                );
            }
            usable
        })
        .collect()
}

/// Binds each UDP family, with one QUIC endpoint on its socket, and starts
/// serving it. A family whose bind fails is served unbound, and
/// [`supervise`] binds it again after the rebind delay.
fn bind_udp(
    binds: &[SocketAddr],
    identity: &ServerIdentity,
    context: &Arc<RuntimeContext>,
    cancel: &CancellationToken,
    tasks: &mut JoinSet<()>,
) -> Vec<Arc<UdpFamily>> {
    if binds.is_empty() {
        return Vec::new();
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let server = match server_config(identity, provider) {
        Ok(server) => server,
        Err(error) => {
            tracing::warn!(error = %error, "carrier: no QUIC server configuration; direct UDP stays off");
            return Vec::new();
        }
    };
    binds
        .iter()
        .map(|bind| {
            let (family, probes) = UdpFamily::bind(*bind, server.clone());
            tasks.spawn(supervise(
                Arc::clone(&family),
                probes,
                Arc::clone(context),
                cancel.clone(),
            ));
            family
        })
        .collect()
}

/// Resolves once every connection has drained and quinn holds no reference to
/// any socket; the runtime's own reference, the last one, then closes it.
async fn release(bound: Vec<BoundSocket>) {
    join_all(bound.iter().map(|socket| socket.endpoint.wait_idle())).await;
    let sockets: Vec<Arc<_>> = bound.into_iter().map(|bound| bound.socket).collect();
    while sockets.iter().any(|socket| Arc::strong_count(socket) > 1) {
        tokio::time::sleep(SOCKET_RELEASE_POLL).await;
    }
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests;
