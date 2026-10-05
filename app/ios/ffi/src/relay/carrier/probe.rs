//! One probe: P binds its probe sockets, posts its sealed offer through C,
//! opens A's answer, punches and connects to every candidate it may dial —
//! A's mapping too, once C's rendezvous returns it — keeps the best carrier
//! that completes, and proves it with an API leg. Nothing waits on a probe;
//! it runs in the background and the relay carries everything meanwhile.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use carrier::burst::PunchBurst;
use carrier::interfaces;
use carrier::quic::{DIRECT_QUIC_SERVER_NAME, client_config};
use carrier::rendezvous::{OwnAddresses, RegisterOutcome, Registration};
use carrier::socket::{DemuxSocket, ProbeReceiver};
use device_proto::candidates::{CertHash, DeviceOffer, DeviceSealer, OfferId};
use device_proto::noise::StaticKeypair;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use remote_host_protocol::relay::{
    AddressClass, AddressPolicy, DirectOfferRequest, DirectOfferResponse, LegClass,
    MAX_UDP_HOST_CANDIDATES, PEER_WAIT, ProbeDatagram, PunchRole, UdpRendezvous,
};
use std::io::ErrorKind;

use super::network::LocalPath;
use super::quic::{CarrierHandle, dial_leg};
use super::state::{ProbeEnd, ProbeTicket};
use crate::api::{CarrierLabel, TierOutcome, TierReport};
use crate::relay::pairing::PairedRecord;
use crate::relay::tunnel::{LegIo, tunnel_handshake};

/// How long a probe may take to complete a QUIC handshake on any tier.
pub(crate) const PROBE_BUDGET: Duration = Duration::from_secs(10);
/// After the first carrier completes, how long a better-ranked attempt still
/// in flight may take to win instead.
pub(crate) const TIER_GRACE: Duration = Duration::from_millis(300);
/// Punch datagrams P sends in one probe, over every target together.
pub(crate) const MAX_PUNCH_DATAGRAMS_PER_PROBE: usize = 64;
/// The offer's POST: C parks it for up to `DIRECT_ANSWER_TIMEOUT` (3 s)
/// while A answers.
const OFFER_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// A `429` / `503` without a usable `Retry-After`.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(5);

/// The direct tiers, best first. A probe records one outcome for each.
pub(crate) const TIERS: [CarrierLabel; 4] = [
    CarrierLabel::Lan,
    CarrierLabel::Ipv6,
    CarrierLabel::Ipv4,
    CarrierLabel::Ipv4Punched,
];

fn rank(tier: CarrierLabel) -> usize {
    TIERS
        .iter()
        .position(|candidate| *candidate == tier)
        .unwrap_or(TIERS.len())
}

pub(crate) fn label_str(label: CarrierLabel) -> &'static str {
    match label {
        CarrierLabel::Relay => "relay",
        CarrierLabel::Lan => "lan",
        CarrierLabel::Ipv6 => "ipv6",
        CarrierLabel::Ipv4 => "ipv4",
        CarrierLabel::Ipv4Punched => "ipv4_punched",
    }
}

fn outcome_str(outcome: TierOutcome) -> &'static str {
    match outcome {
        TierOutcome::Ok => "ok",
        TierOutcome::Failed => "failed",
        TierOutcome::Timeout => "timeout",
        TierOutcome::NotOffered => "not_offered",
        TierOutcome::Denied => "denied",
        TierOutcome::Skipped => "skipped",
    }
}

/// Each tier's outcome in one probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tiers([TierOutcome; 4]);

impl Default for Tiers {
    fn default() -> Self {
        Self([TierOutcome::NotOffered; 4])
    }
}

impl Tiers {
    fn get(&self, tier: CarrierLabel) -> TierOutcome {
        self.0
            .get(rank(tier))
            .copied()
            .unwrap_or(TierOutcome::Skipped)
    }

    fn set(&mut self, tier: CarrierLabel, outcome: TierOutcome) {
        if let Some(slot) = self.0.get_mut(rank(tier)) {
            *slot = outcome;
        }
    }

    /// Records one attempt's outcome on its tier: the tier keeps the best
    /// outcome any of its attempts reached.
    fn record(&mut self, tier: CarrierLabel, outcome: TierOutcome) {
        let current = self.get(tier);
        if precedence(outcome) > precedence(current) {
            self.set(tier, outcome);
        }
    }

    fn any_denied(&self) -> bool {
        self.0.contains(&TierOutcome::Denied)
    }

    pub(crate) fn reports(&self) -> Vec<TierReport> {
        TIERS
            .iter()
            .map(|tier| TierReport {
                tier: *tier,
                outcome: self.get(*tier),
            })
            .collect()
    }

    /// `lan=… ipv6=… ipv4=… ipv4_punched=…`, for the `direct_probe` line.
    pub(crate) fn summary(&self) -> String {
        TIERS
            .iter()
            .map(|tier| format!("{}={}", label_str(*tier), outcome_str(self.get(*tier))))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Which of an attempt's outcomes a tier reports when it had several.
fn precedence(outcome: TierOutcome) -> u8 {
    match outcome {
        TierOutcome::NotOffered => 0,
        TierOutcome::Skipped => 1,
        TierOutcome::Failed => 2,
        TierOutcome::Timeout => 3,
        TierOutcome::Denied => 4,
        TierOutcome::Ok => 5,
    }
}

/// What a probe produced.
pub(crate) struct ProbeRun {
    pub(crate) end: ProbeEnd<CarrierHandle>,
    pub(crate) tiers: Tiers,
    /// The proof leg, to park in the pool once the carrier is live.
    pub(crate) proof: Option<LegIo>,
}

impl ProbeRun {
    fn ended(end: ProbeEnd<CarrierHandle>, tiers: Tiers) -> Self {
        Self {
            end,
            tiers,
            proof: None,
        }
    }
}

/// One family's probe socket. Its probe queue is taken by the rendezvous
/// registration, the one reader P has for it.
struct ProbeSocket {
    socket: Arc<DemuxSocket>,
    endpoint: quinn::Endpoint,
    probes: Option<ProbeReceiver>,
    port: u16,
}

fn bind(address: SocketAddr) -> Option<ProbeSocket> {
    let (socket, probes) = match DemuxSocket::bind(address) {
        Ok(bound) => bound,
        Err(error) => {
            log::debug!("direct_probe: bind {address} failed: {error}");
            return None;
        }
    };
    let endpoint = match socket.quic_endpoint(None) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            log::debug!("direct_probe: endpoint on {address} failed: {error}");
            return None;
        }
    };
    let port = socket.local_addr().ok()?.port();
    Some(ProbeSocket {
        socket,
        endpoint,
        probes: Some(probes),
        port,
    })
}

/// P's two probe sockets. A family the path does not route is never bound.
struct Sockets {
    v4: Option<ProbeSocket>,
    v6: Option<ProbeSocket>,
}

impl Sockets {
    fn bind(local: &LocalPath) -> Self {
        Self {
            v4: local
                .supports_ipv4
                .then(|| bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))))
                .flatten(),
            v6: local
                .supports_ipv6
                .then(|| bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0))))
                .flatten(),
        }
    }

    fn of(&self, ip: IpAddr) -> Option<&ProbeSocket> {
        match ip {
            IpAddr::V4(_) => self.v4.as_ref(),
            IpAddr::V6(_) => self.v6.as_ref(),
        }
    }

    fn ports(&self) -> FamilyPorts {
        FamilyPorts {
            v4: self.v4.as_ref().map(|socket| socket.port),
            v6: self.v6.as_ref().map(|socket| socket.port),
        }
    }
}

/// The port of each family's bound probe socket.
#[derive(Debug, Clone, Copy)]
struct FamilyPorts {
    v4: Option<u16>,
    v6: Option<u16>,
}

impl FamilyPorts {
    fn of(self, ip: IpAddr) -> Option<u16> {
        match ip {
            IpAddr::V4(_) => self.v4,
            IpAddr::V6(_) => self.v6,
        }
    }
}

/// P's host candidates: the primary interface's usable addresses, each with
/// its family socket's port. `Lan`-class addresses only on Wi-Fi or wired —
/// a private address on cellular is the carrier's CGNAT, and offering it
/// would have A spray datagrams at unrelated hosts on its own LAN. Every GUA
/// is offered, temporary ones too: iOS picks the outbound source itself.
fn host_candidates(
    local: &LocalPath,
    ports: FamilyPorts,
    policy: &AddressPolicy,
) -> Vec<SocketAddr> {
    let mut ranked: Vec<(u8, SocketAddr)> = local
        .addresses
        .iter()
        .filter_map(|address| {
            let ip = AddressPolicy::canonical_ip(address.ip);
            let port = ports.of(ip)?;
            let rank = match (policy.classify(ip)?, ip) {
                (AddressClass::Lan, _) if local.is_local_area() => 0,
                (AddressClass::Lan, _) => return None,
                (AddressClass::Public, IpAddr::V6(_)) => 1,
                (AddressClass::Public, IpAddr::V4(_)) => 2,
            };
            Some((rank, SocketAddr::new(ip, port)))
        })
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    let mut seen = HashSet::new();
    ranked
        .into_iter()
        .map(|(_, candidate)| candidate)
        .filter(|candidate| seen.insert(*candidate))
        .take(MAX_UDP_HOST_CANDIDATES)
        .collect()
}

/// One target P dials, and the tier it counts for.
#[derive(Debug, Clone, Copy)]
struct Target {
    tier: CarrierLabel,
    address: SocketAddr,
}

/// A's candidates P may dial, best first, each address once. `Lan` ones only
/// from a path that may dial them; each family only when the path routes it.
fn dial_targets(
    answer_udp: &[SocketAddr],
    local: &LocalPath,
    ports: FamilyPorts,
    policy: &AddressPolicy,
) -> (Vec<Target>, Tiers) {
    let mut tiers = Tiers::default();
    let mut targets: Vec<Target> = Vec::new();
    for candidate in answer_udp {
        let address = AddressPolicy::canonical_socket_addr(*candidate);
        let ip = address.ip();
        if address.port() == 0 {
            continue;
        }
        let tier = match (policy.classify(ip), ip) {
            (Some(AddressClass::Lan), _) => CarrierLabel::Lan,
            (Some(AddressClass::Public), IpAddr::V6(_)) => CarrierLabel::Ipv6,
            (Some(AddressClass::Public), IpAddr::V4(_)) => CarrierLabel::Ipv4,
            (None, _) => continue,
        };
        let routed = ports.of(ip).is_some()
            && match tier {
                CarrierLabel::Lan => local.may_dial_lan(ip, policy),
                _ => true,
            };
        if !routed || targets.iter().any(|target| target.address == address) {
            continue;
        }
        tiers.set(tier, TierOutcome::Skipped);
        targets.push(Target { tier, address });
    }
    targets.sort_by_key(|target| rank(target.tier));
    (targets, tiers)
}

/// Runs one probe for `ticket`. `record` is the binding's pairing.
pub(crate) async fn run(ticket: &ProbeTicket, record: &PairedRecord) -> ProbeRun {
    let policy = AddressPolicy::active();
    let local = &ticket.local;
    let mut sockets = Sockets::bind(local);
    let mut tiers = Tiers::default();
    if sockets.v4.is_none() && sockets.v6.is_none() {
        return ProbeRun::ended(ProbeEnd::Failed, tiers);
    }

    let sealer = match DeviceSealer::derive(&record.noise_secret, &record.gateway_static_pubkey) {
        Ok(sealer) => sealer,
        Err(error) => {
            log::warn!("direct_probe: candidate keys unavailable: {error}");
            return ProbeRun::ended(ProbeEnd::Failed, tiers);
        }
    };
    let offer = DeviceOffer::new(unix_ms(), host_candidates(local, sockets.ports(), &policy));
    let offered = match exchange(record, &sealer, &offer).await {
        Exchange::Answered(offered) => offered,
        Exchange::RetryAfter(wait) => return ProbeRun::ended(ProbeEnd::RetryAfter(wait), tiers),
        Exchange::Failed => return ProbeRun::ended(ProbeEnd::Failed, tiers),
    };

    let (targets, targeted) = dial_targets(&offered.answer_udp, local, sockets.ports(), &policy);
    tiers = targeted;
    let v4_probes = sockets.v4.as_mut().and_then(|socket| socket.probes.take());
    let rendezvous = offered.rendezvous.zip(v4_probes);
    if rendezvous.is_some() {
        tiers.set(CarrierLabel::Ipv4Punched, TierOutcome::Skipped);
    }
    let context = AttemptContext {
        sealer: &sealer,
        offer_id: offer.offer_id,
        pinned: offered.cert_hash,
        seq: AtomicUsize::new(0),
    };

    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + PROBE_BUDGET;
    let mut in_flight = [0usize; TIERS.len()];
    let mut attempts = FuturesUnordered::new();
    for target in &targets {
        if let Some(socket) = sockets.of(target.address.ip()) {
            in_flight[rank(target.tier)] += 1;
            attempts.push(attempt(&context, socket, *target, local, &policy));
        }
    }
    let mut registration = rendezvous.map(|(rendezvous, replies)| {
        Box::pin(register(
            offered.punch_id,
            rendezvous,
            sockets.v4.as_ref(),
            replies,
            &policy,
        ))
    });

    let mut won: Vec<(Target, quinn::Connection)> = Vec::new();
    let mut grace: Option<tokio::time::Instant> = None;
    while !attempts.is_empty() || registration.is_some() {
        let until = grace.map_or(deadline, |grace| grace.min(deadline));
        tokio::select! {
            () = tokio::time::sleep_until(until) => break,
            Some((target, result, elapsed)) = attempts.next(), if !attempts.is_empty() => {
                in_flight[rank(target.tier)] -= 1;
                let outcome = match &result {
                    Ok(_) => TierOutcome::Ok,
                    Err(AttemptError::Denied) => TierOutcome::Denied,
                    Err(AttemptError::Failed) => TierOutcome::Failed,
                };
                tiers.record(target.tier, outcome);
                log::info!(
                    "direct_attempt tier={} family={} candidate_class={} outcome={} elapsed_ms={}",
                    label_str(target.tier),
                    if target.address.is_ipv4() { "ipv4" } else { "ipv6" },
                    candidate_class(target.address.ip(), &policy),
                    outcome_str(outcome),
                    elapsed.as_millis()
                );
                if let Ok(connection) = result {
                    let best = rank(target.tier);
                    won.push((target, connection));
                    // Nothing outranks the best tier: no reason to wait.
                    if best == 0 {
                        break;
                    }
                    grace.get_or_insert(tokio::time::Instant::now() + TIER_GRACE);
                }
            }
            outcome = async {
                match registration.as_mut() {
                    Some(registering) => registering.await,
                    None => std::future::pending().await,
                }
            }, if registration.is_some() => {
                registration = None;
                match outcome {
                    Registered::Peer(srflx) => {
                        let target = Target {
                            tier: CarrierLabel::Ipv4Punched,
                            address: SocketAddr::V4(srflx),
                        };
                        let known = targets.iter().any(|known| known.address == target.address);
                        match sockets.v4.as_ref() {
                            Some(socket) if !known => {
                                in_flight[rank(target.tier)] += 1;
                                attempts.push(attempt(&context, socket, target, local, &policy));
                            }
                            // A's mapping is one of its host candidates,
                            // already dialed on its own tier.
                            _ => tiers.set(CarrierLabel::Ipv4Punched, TierOutcome::Skipped),
                        }
                    }
                    Registered::NoPeer => {
                        tiers.record(CarrierLabel::Ipv4Punched, TierOutcome::Failed);
                    }
                    Registered::Unresolved => {
                        tiers.set(CarrierLabel::Ipv4Punched, TierOutcome::NotOffered);
                    }
                }
            }
        }
    }
    if registration.is_some() {
        in_flight[rank(CarrierLabel::Ipv4Punched)] += 1;
    }
    drop(registration);
    drop(attempts);
    // What was still in flight lost the race when a carrier won, and timed
    // out when none did.
    let unfinished = if won.is_empty() {
        TierOutcome::Timeout
    } else {
        TierOutcome::Skipped
    };
    for tier in TIERS {
        if in_flight[rank(tier)] > 0 {
            tiers.record(tier, unfinished);
        }
    }

    won.sort_by_key(|(target, _)| rank(target.tier));
    let mut won = won.into_iter();
    let Some((target, connection)) = won.next() else {
        let end = if tiers.any_denied() {
            ProbeEnd::Denied
        } else {
            ProbeEnd::Failed
        };
        log::debug!(
            "direct_probe: no carrier after {}ms",
            started.elapsed().as_millis()
        );
        return ProbeRun::ended(end, tiers);
    };
    for (_, loser) in won {
        loser.close(quinn::VarInt::from_u32(0), b"a better tier won");
    }
    let Some(winner) = sockets.of(target.address.ip()) else {
        return ProbeRun::ended(ProbeEnd::Failed, tiers);
    };
    let handle = CarrierHandle::new(
        connection,
        offered.token,
        winner.endpoint.clone(),
        winner.socket.clone(),
    );
    match prove(&handle, record).await {
        Ok(proof) => {
            let local_ip = handle
                .local_ip()
                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
            ProbeRun {
                end: ProbeEnd::Carrier {
                    handle,
                    kind: target.tier,
                    local_ip,
                },
                tiers,
                proof: Some(proof),
            }
        }
        Err(reason) => {
            log::info!("direct_probe: the carrier did not prove itself: {reason}");
            handle.close("proof failed");
            tiers.set(target.tier, TierOutcome::Failed);
            ProbeRun::ended(ProbeEnd::Failed, tiers)
        }
    }
}

/// Opens one API session on a fresh carrier: a successful handshake proves
/// it, and the leg is parked for the next request.
pub(crate) async fn prove(handle: &CarrierHandle, record: &PairedRecord) -> Result<LegIo, String> {
    let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
    dial_leg(
        handle,
        LegClass::Api,
        || true,
        |socket| tunnel_handshake(socket, record, &local),
    )
    .await
    .map_err(|failure| failure.reason().to_owned())
}

fn candidate_class(ip: IpAddr, policy: &AddressPolicy) -> &'static str {
    match policy.classify(ip) {
        Some(AddressClass::Lan) => "lan",
        Some(AddressClass::Public) => "public",
        None => "excluded",
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// A's opened answer, with what C added to it.
struct Offered {
    punch_id: remote_host_protocol::relay::PunchId,
    token: remote_host_protocol::relay::DirectToken,
    cert_hash: CertHash,
    answer_udp: Vec<SocketAddr>,
    rendezvous: Option<UdpRendezvous>,
}

enum Exchange {
    Answered(Offered),
    RetryAfter(Duration),
    Failed,
}

/// Posts the sealed offer to C and opens A's answer. A `404` or `504`, an
/// answer that does not open, or one that does not echo the offer's id is a
/// failure; `429` and `503` ask to retry later.
async fn exchange(record: &PairedRecord, sealer: &DeviceSealer, offer: &DeviceOffer) -> Exchange {
    let sealed = match sealer.seal_offer(&record.relay_node_id, offer) {
        Ok(sealed) => sealed,
        Err(error) => {
            log::warn!("direct_probe: offer did not seal: {error}");
            return Exchange::Failed;
        }
    };
    let url = remote_host_protocol::relay::direct_offer_url(
        record.relay_url.trim_end_matches('/'),
        &record.relay_node_id,
    );
    let mut request = offer_client()
        .post(&url)
        .timeout(OFFER_REQUEST_TIMEOUT)
        .json(&DirectOfferRequest { offer: sealed });
    if !record.remote_api_key.is_empty() {
        request = request.header(
            remote_host_protocol::REMOTE_API_KEY_HEADER,
            &record.remote_api_key,
        );
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            log::info!("direct_probe: offer POST failed: {error}");
            return Exchange::Failed;
        }
    };
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status == reqwest::StatusCode::SERVICE_UNAVAILABLE
    {
        let wait = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map_or(DEFAULT_RETRY_AFTER, Duration::from_secs);
        return Exchange::RetryAfter(wait);
    }
    if !status.is_success() {
        log::info!(
            "direct_probe: C answered the offer with HTTP {}",
            status.as_u16()
        );
        return Exchange::Failed;
    }
    let body: DirectOfferResponse = match response.json().await {
        Ok(body) => body,
        Err(error) => {
            log::info!("direct_probe: unreadable offer response: {error}");
            return Exchange::Failed;
        }
    };
    let answer = match sealer.open_answer(
        &record.relay_node_id,
        &body.punch_id,
        &offer.offer_id,
        &body.answer,
    ) {
        Ok(answer) => answer,
        Err(error) => {
            log::warn!("direct_probe: the answer was refused: {error}");
            return Exchange::Failed;
        }
    };
    Exchange::Answered(Offered {
        punch_id: body.punch_id,
        token: answer.token,
        cert_hash: answer.quic_cert_sha256,
        answer_udp: answer.udp,
        rendezvous: body.rendezvous,
    })
}

/// One HTTP client for every offer: the relay origin is the same each time.
fn offer_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// What every attempt of one probe shares: the punch keys, the pinned
/// certificate, and the punch counter, which is also the punch budget.
struct AttemptContext<'a> {
    sealer: &'a DeviceSealer,
    offer_id: OfferId,
    pinned: CertHash,
    seq: AtomicUsize,
}

impl AttemptContext<'_> {
    /// The next authenticated punch, or `None` once the probe's budget is
    /// spent.
    fn next_punch(&self) -> Option<ProbeDatagram> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        if seq >= MAX_PUNCH_DATAGRAMS_PER_PROBE {
            return None;
        }
        let seq = u16::try_from(seq).ok()?;
        let tag = self.sealer.punch_tag(&self.offer_id, seq).ok()?;
        Some(ProbeDatagram::Punch { seq, tag })
    }
}

enum AttemptError {
    /// A punch to a `Lan`-class or on-link destination was refused locally:
    /// the iOS Local Network permission is refused or still pending.
    Denied,
    Failed,
}

type AttemptResult = (Target, Result<quinn::Connection, AttemptError>, Duration);

/// One target: an authenticated punch, then a QUIC connect, with the rest of
/// the burst at `PUNCH_INTERVAL` while it connects. An Initial that overtakes
/// its punch is ignored by A and retransmitted by quinn.
async fn attempt(
    context: &AttemptContext<'_>,
    socket: &ProbeSocket,
    target: Target,
    local: &LocalPath,
    policy: &AddressPolicy,
) -> AttemptResult {
    let started = Instant::now();
    let guarded = is_local_network_destination(target.address.ip(), local, policy);
    let punches = async {
        let mut burst = PunchBurst::new();
        while burst.next_round().await.is_some() {
            let Some(punch) = context.next_punch() else {
                return Ok(());
            };
            if let Err(error) = socket.socket.send_probe(&punch, target.address, None).await {
                if guarded && is_permission_refusal(&error) {
                    return Err(AttemptError::Denied);
                }
                log::debug!(
                    "direct_attempt: punch to {} failed: {error}",
                    target.address
                );
            }
        }
        Ok(())
    };
    let connect = async {
        let config = client_config(
            context.pinned,
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .map_err(|error| {
            log::warn!("direct_attempt: client config: {error}");
            AttemptError::Failed
        })?;
        let connecting = socket
            .endpoint
            .connect_with(config, target.address, DIRECT_QUIC_SERVER_NAME)
            .map_err(|error| {
                log::debug!("direct_attempt: connect to {}: {error}", target.address);
                AttemptError::Failed
            })?;
        connecting.await.map_err(|error| {
            log::debug!("direct_attempt: handshake with {}: {error}", target.address);
            AttemptError::Failed
        })
    };
    tokio::pin!(punches, connect);
    let mut punching = true;
    let result = loop {
        // Punches first: the burst's first punch leaves before the Initial.
        tokio::select! {
            biased;
            sent = &mut punches, if punching => {
                punching = false;
                if let Err(denied) = sent {
                    break Err(denied);
                }
            }
            connected = &mut connect => break connected,
        }
    };
    (target, result, started.elapsed())
}

/// A destination the iOS Local Network permission governs: a `Lan`-class
/// address, or one on the primary interface's own IPv6 /64.
fn is_local_network_destination(ip: IpAddr, local: &LocalPath, policy: &AddressPolicy) -> bool {
    if policy.classify(ip) == Some(AddressClass::Lan) {
        return true;
    }
    let IpAddr::V6(target) = ip else {
        return false;
    };
    local.addresses.iter().any(|address| match address.ip {
        IpAddr::V6(own) => own.segments()[..4] == target.segments()[..4],
        IpAddr::V4(_) => false,
    })
}

/// How iOS refuses a send the Local Network permission does not allow:
/// `EHOSTUNREACH` or `EPERM`.
fn is_permission_refusal(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::HostUnreachable | ErrorKind::PermissionDenied
    )
}

enum Registered {
    Peer(std::net::SocketAddrV4),
    NoPeer,
    /// The rendezvous address did not resolve to a `Public` IPv4 address:
    /// the punched tier is not offered.
    Unresolved,
}

/// Registers P's IPv4 socket at C's rendezvous for `punch_id`, exactly as A
/// does, and returns A's mapping once C reports it.
async fn register(
    punch_id: remote_host_protocol::relay::PunchId,
    rendezvous: UdpRendezvous,
    socket: Option<&ProbeSocket>,
    mut replies: ProbeReceiver,
    policy: &AddressPolicy,
) -> Registered {
    let Some(socket) = socket else {
        return Registered::Unresolved;
    };
    let resolve_policy = *policy;
    let resolved = tokio::task::spawn_blocking(move || {
        let address = rendezvous.resolve_public_v4(&resolve_policy);
        (rendezvous.key, address)
    })
    .await;
    let (key, address) = match resolved {
        Ok((key, Ok(address))) => (key, address),
        Ok((_, Err(error))) => {
            log::debug!("direct_probe: rendezvous address refused: {error}");
            return Registered::Unresolved;
        }
        Err(error) => {
            log::warn!("direct_probe: rendezvous lookup task failed: {error}");
            return Registered::Unresolved;
        }
    };
    let own = OwnAddresses::new(
        interfaces::enumerate()
            .into_iter()
            .map(|address| address.ip),
    );
    let Some(mut registration) =
        Registration::new(punch_id, PunchRole::Device, key, address, *policy, own)
    else {
        return Registered::Unresolved;
    };
    // A's punches land on the same queue; the latch ignores them.
    match registration
        .until_peer(
            &socket.socket,
            &mut replies,
            tokio::time::Instant::now() + PEER_WAIT,
        )
        .await
    {
        RegisterOutcome::Peer(srflx) => Registered::Peer(srflx),
        RegisterOutcome::NoPeer { .. } => Registered::NoPeer,
    }
}

#[cfg(test)]
mod tests {
    use carrier::interfaces::InterfaceAddress;

    use super::*;
    use crate::api::{NetworkInterfaceKind, NetworkPath};

    const PORTS: FamilyPorts = FamilyPorts {
        v4: Some(40_004),
        v6: Some(40_006),
    };

    fn local(kind: NetworkInterfaceKind, addresses: &[&str]) -> LocalPath {
        let path = NetworkPath {
            satisfied: true,
            interface_kind: kind,
            interface_name: "if0".into(),
            gateways: vec![],
            supports_ipv4: true,
            supports_ipv6: true,
            available_interfaces: vec!["if0".into()],
            is_expensive: false,
            is_constrained: false,
        };
        let interfaces: Vec<InterfaceAddress> = addresses
            .iter()
            .map(|ip| InterfaceAddress {
                interface: "if0".into(),
                ip: ip.parse().unwrap(),
                up_running: true,
                temporary: false,
                unusable: false,
            })
            .collect();
        LocalPath::new(&path, &interfaces)
    }

    fn socket(address: &str) -> SocketAddr {
        address.parse().unwrap()
    }

    #[test]
    fn cellular_offers_its_guas_but_never_its_cgnat_address() {
        let policy = AddressPolicy::active();
        let cellular = local(
            NetworkInterfaceKind::Cellular,
            &["10.20.30.40", "2606:4700:1::9"],
        );
        assert_eq!(
            host_candidates(&cellular, PORTS, &policy),
            vec![socket("[2606:4700:1::9]:40006")]
        );
    }

    #[test]
    fn wifi_offers_lan_addresses_first_each_on_its_familys_port() {
        let policy = AddressPolicy::active();
        let wifi = local(
            NetworkInterfaceKind::Wifi,
            &["2606:4700:1::9", "192.168.1.20", "fd00::20"],
        );
        assert_eq!(
            host_candidates(&wifi, PORTS, &policy),
            vec![
                socket("192.168.1.20:40004"),
                socket("[fd00::20]:40006"),
                socket("[2606:4700:1::9]:40006"),
            ]
        );
        let v4_only = FamilyPorts {
            v4: Some(40_004),
            v6: None,
        };
        assert_eq!(
            host_candidates(&wifi, v4_only, &policy),
            vec![socket("192.168.1.20:40004")],
            "a family with no socket offers nothing"
        );
    }

    #[test]
    fn the_offer_is_capped() {
        let policy = AddressPolicy::active();
        let many: Vec<String> = (1..=12).map(|n| format!("2606:4700:1::{n}")).collect();
        let many: Vec<&str> = many.iter().map(String::as_str).collect();
        let wifi = local(NetworkInterfaceKind::Wifi, &many);
        assert_eq!(
            host_candidates(&wifi, PORTS, &policy).len(),
            MAX_UDP_HOST_CANDIDATES
        );
    }

    #[test]
    fn a_cellular_path_never_dials_the_gateways_lan_candidates() {
        let policy = AddressPolicy::active();
        let answer = [socket("192.168.1.10:5000"), socket("[2606:4700:2::1]:5000")];
        let cellular = local(NetworkInterfaceKind::Cellular, &["10.20.30.40"]);
        let (targets, tiers) = dial_targets(&answer, &cellular, PORTS, &policy);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].tier, CarrierLabel::Ipv6);
        assert_eq!(tiers.get(CarrierLabel::Lan), TierOutcome::NotOffered);
        assert_eq!(tiers.get(CarrierLabel::Ipv6), TierOutcome::Skipped);
    }

    #[test]
    fn targets_are_ranked_best_first_and_deduplicated() {
        let policy = AddressPolicy::active();
        let answer = [
            socket("8.8.4.4:5000"),
            socket("[2606:4700:2::1]:5000"),
            socket("192.168.1.10:5000"),
            socket("8.8.4.4:5000"),
            socket("127.0.0.1:5000"),
        ];
        let wifi = local(NetworkInterfaceKind::Wifi, &["192.168.1.20"]);
        let (targets, _) = dial_targets(&answer, &wifi, PORTS, &policy);
        let tiers: Vec<CarrierLabel> = targets.iter().map(|target| target.tier).collect();
        assert_eq!(
            tiers,
            vec![CarrierLabel::Lan, CarrierLabel::Ipv6, CarrierLabel::Ipv4],
            "an excluded address is never dialed, and each address once"
        );
    }

    #[test]
    fn a_tier_reports_the_best_outcome_any_attempt_reached() {
        let mut tiers = Tiers::default();
        tiers.record(CarrierLabel::Lan, TierOutcome::Failed);
        tiers.record(CarrierLabel::Lan, TierOutcome::Ok);
        tiers.record(CarrierLabel::Lan, TierOutcome::Timeout);
        tiers.record(CarrierLabel::Ipv6, TierOutcome::Denied);
        assert_eq!(tiers.get(CarrierLabel::Lan), TierOutcome::Ok);
        assert!(tiers.any_denied());
        assert_eq!(
            tiers.summary(),
            "lan=ok ipv6=denied ipv4=not_offered ipv4_punched=not_offered"
        );
    }

    #[test]
    fn the_local_network_permission_governs_lan_and_on_link_destinations() {
        let policy = AddressPolicy::active();
        let wifi = local(
            NetworkInterfaceKind::Wifi,
            &["192.168.1.20", "2606:4700:1::9"],
        );
        let governed = |ip: &str| is_local_network_destination(ip.parse().unwrap(), &wifi, &policy);
        assert!(governed("192.168.1.10"));
        assert!(governed("2606:4700:1::77"), "the same /64 is on-link");
        assert!(!governed("2606:4700:2::1"));
        assert!(!governed("8.8.4.4"));
    }
}
