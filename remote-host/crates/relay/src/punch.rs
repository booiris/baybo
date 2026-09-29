//! C's per-punch direct-carrier state. A punch exists from the moment C
//! forwards P's offer to A: it holds the wait for A's report, the offer
//! budgets that admitted it and, when C runs a UDP rendezvous, both roles'
//! tickets and observed mappings. C keeps no direct state outside a punch.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddrV4;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use remote_host_protocol::key_tag;
use remote_host_protocol::relay::{
    ControlReport, DirectOfferResponse, MAX_INFLIGHT_PUNCHES_PER_NODE, PUNCH_TTL, ProbeDatagram,
    PunchId, PunchRole, RendezvousTicket, SealedCandidates, SourceKey, UdpRendezvous,
};
use tokio::sync::oneshot;
use tokio::time::Instant;

/// How long C holds P's `POST /direct` open for A's report.
pub(crate) const DIRECT_ANSWER_TIMEOUT: Duration = Duration::from_secs(3);
/// `Register` datagrams C considers per punch; later ones are dropped.
pub(crate) const MAX_DATAGRAMS_PER_PUNCH: u32 = 64;
/// Offers per minute to one gateway node from one client source.
pub(crate) const DIRECT_OFFERS_PER_SOURCE_PER_MINUTE: usize = 6;
/// Offers per minute to one gateway node from all sources together.
pub(crate) const DIRECT_OFFERS_PER_NODE_PER_MINUTE: usize = 30;
/// Punches C holds across all nodes.
pub(crate) const MAX_PENDING_PUNCHES: usize = 4096;
/// How often the rendezvous loop drops expired punches. Every lookup already
/// ignores an expired punch; the sweep reclaims its memory and logs its end.
pub(crate) const PUNCH_SWEEP_INTERVAL: Duration = Duration::from_secs(5);
const OFFER_BUDGET_WINDOW: Duration = Duration::from_secs(60);

/// What A reported for a punch over its control connection.
#[derive(Debug)]
pub(crate) enum DirectReply {
    Answer(SealedCandidates),
    Declined,
}

/// The per-role registration credentials C mints for a punch when it runs a
/// UDP rendezvous. Each role only ever receives its own.
struct RoleTickets {
    gateway: RendezvousTicket,
    device: RendezvousTicket,
}

impl RoleTickets {
    fn generate() -> Self {
        Self {
            gateway: RendezvousTicket::generate(),
            device: RendezvousTicket::generate(),
        }
    }

    fn for_role(&self, role: PunchRole) -> &RendezvousTicket {
        match role {
            PunchRole::Gateway => &self.gateway,
            PunchRole::Device => &self.device,
        }
    }
}

fn other_role(role: PunchRole) -> PunchRole {
    match role {
        PunchRole::Gateway => PunchRole::Device,
        PunchRole::Device => PunchRole::Gateway,
    }
}

/// One role's view at the rendezvous: the first source that registered with a
/// valid ticket (its IPv4 mapping), and whether C has sent it a `Peer`.
#[derive(Default)]
struct RoleState {
    source: Option<SocketAddrV4>,
    peer_sent: bool,
}

struct Punch {
    node: String,
    remote_api_key: String,
    control_token: u64,
    answer: Option<oneshot::Sender<DirectReply>>,
    tickets: Option<RoleTickets>,
    gateway: RoleState,
    device: RoleState,
    datagrams: u32,
    created: Instant,
    paired_at: Option<Instant>,
}

impl Punch {
    fn expires_at(&self) -> Instant {
        self.created + PUNCH_TTL
    }

    fn is_live(&self, now: Instant) -> bool {
        now < self.expires_at()
    }

    /// A punch counts toward [`MAX_INFLIGHT_PUNCHES_PER_NODE`] until C has sent
    /// `Peer` to both roles. A punch whose POST ended without a rendezvous is
    /// already gone from the table.
    fn in_flight(&self) -> bool {
        !(self.gateway.peer_sent && self.device.peer_sent)
    }

    fn role(&self, role: PunchRole) -> &RoleState {
        match role {
            PunchRole::Gateway => &self.gateway,
            PunchRole::Device => &self.device,
        }
    }

    fn role_mut(&mut self, role: PunchRole) -> &mut RoleState {
        match role {
            PunchRole::Gateway => &mut self.gateway,
            PunchRole::Device => &mut self.device,
        }
    }

    fn ended(self, punch_id: PunchId) -> EndedPunch {
        EndedPunch {
            punch_id,
            node: self.node,
            remote_api_key: self.remote_api_key,
            rendezvous: self.tickets.is_some(),
            gateway_registered: self.gateway.source.is_some(),
            device_registered: self.device.source.is_some(),
            paired_after: self.paired_at.map(|at| at.duration_since(self.created)),
        }
    }
}

/// A removed punch, logged once the table lock is released.
struct EndedPunch {
    punch_id: PunchId,
    node: String,
    remote_api_key: String,
    rendezvous: bool,
    gateway_registered: bool,
    device_registered: bool,
    paired_after: Option<Duration>,
}

#[derive(Clone, Copy)]
enum PunchEnd {
    Expired,
    ControlClosed,
    Settled,
}

impl PunchEnd {
    fn as_str(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::ControlClosed => "control_closed",
            Self::Settled => "settled",
        }
    }
}

/// Logs the rendezvous outcome of each ended punch that had one. A punch
/// without a rendezvous ends with its POST, whose own log line covers it.
fn log_ended(ended: &[EndedPunch], end: PunchEnd) {
    for punch in ended.iter().filter(|punch| punch.rendezvous) {
        tracing::info!(
            punch = %punch.punch_id.tag(),
            node = %punch.node,
            key_tag = %key_tag(&punch.remote_api_key),
            end = end.as_str(),
            gateway_registered = punch.gateway_registered,
            device_registered = punch.device_registered,
            paired_after_ms = ?punch
                .paired_after
                .map(|after| u64::try_from(after.as_millis()).unwrap_or(u64::MAX)),
            "direct: punch ended"
        );
    }
}

type SourceBudgetKey = (String, Option<SourceKey>);

/// The per-(node, source) and per-node offer windows. Each window holds the
/// admission times of the last [`OFFER_BUDGET_WINDOW`], at most its cap.
#[derive(Default)]
struct OfferBudget {
    per_node: HashMap<String, VecDeque<Instant>>,
    per_source: HashMap<SourceBudgetKey, VecDeque<Instant>>,
}

impl OfferBudget {
    fn node_wait(&self, node: &str, now: Instant) -> Option<Duration> {
        Self::wait(
            self.per_node.get(node),
            DIRECT_OFFERS_PER_NODE_PER_MINUTE,
            now,
        )
    }

    fn source_wait(&self, key: &SourceBudgetKey, now: Instant) -> Option<Duration> {
        Self::wait(
            self.per_source.get(key),
            DIRECT_OFFERS_PER_SOURCE_PER_MINUTE,
            now,
        )
    }

    /// `None` when the window has room now, else how long until it does.
    fn wait(window: Option<&VecDeque<Instant>>, cap: usize, now: Instant) -> Option<Duration> {
        let window = window.filter(|window| window.len() >= cap)?;
        window
            .front()
            .map(|oldest| (*oldest + OFFER_BUDGET_WINDOW).saturating_duration_since(now))
    }

    fn record(&mut self, source: SourceBudgetKey, now: Instant) {
        self.per_node
            .entry(source.0.clone())
            .or_default()
            .push_back(now);
        self.per_source.entry(source).or_default().push_back(now);
    }

    fn prune(&mut self, now: Instant) {
        fn prune_window(window: &mut VecDeque<Instant>, now: Instant) -> bool {
            while window
                .front()
                .is_some_and(|oldest| *oldest + OFFER_BUDGET_WINDOW <= now)
            {
                window.pop_front();
            }
            !window.is_empty()
        }
        self.per_node.retain(|_, window| prune_window(window, now));
        self.per_source
            .retain(|_, window| prune_window(window, now));
    }
}

#[derive(Default)]
struct PunchTable {
    punches: HashMap<PunchId, Punch>,
    budget: OfferBudget,
}

impl PunchTable {
    fn sweep(&mut self, now: Instant) -> Vec<EndedPunch> {
        self.budget.prune(now);
        self.punches
            .extract_if(|_, punch| !punch.is_live(now))
            .map(|(punch_id, punch)| punch.ended(punch_id))
            .collect()
    }

    fn in_flight_wait(&self, node: &str, now: Instant) -> Option<Duration> {
        let mut in_flight = 0;
        let mut soonest: Option<Instant> = None;
        for punch in self
            .punches
            .values()
            .filter(|punch| punch.node == node && punch.in_flight())
        {
            in_flight += 1;
            let expires_at = punch.expires_at();
            soonest = Some(soonest.map_or(expires_at, |soonest| soonest.min(expires_at)));
        }
        if in_flight < MAX_INFLIGHT_PUNCHES_PER_NODE {
            return None;
        }
        soonest.map(|expires_at| expires_at.saturating_duration_since(now))
    }

    fn capacity_wait(&self, now: Instant) -> Option<Duration> {
        if self.punches.len() < MAX_PENDING_PUNCHES {
            return None;
        }
        self.punches
            .values()
            .map(|punch| punch.expires_at().saturating_duration_since(now))
            .min()
    }

    fn admit(
        &mut self,
        request: &PunchRequest<'_>,
        now: Instant,
    ) -> Result<NewPunch, PunchRefusal> {
        let source: SourceBudgetKey = (request.relay_node_id.to_owned(), request.source);
        let waits = [
            (
                OfferLimit::SourceRate,
                self.budget.source_wait(&source, now),
            ),
            (
                OfferLimit::NodeRate,
                self.budget.node_wait(request.relay_node_id, now),
            ),
            (
                OfferLimit::InFlight,
                self.in_flight_wait(request.relay_node_id, now),
            ),
        ];
        let mut refusals = waits
            .into_iter()
            .filter_map(|(limit, wait)| wait.map(|wait| (limit, wait)));
        if let Some((limit, wait)) = refusals.next() {
            let retry_after = refusals.fold(wait, |longest, (_, wait)| longest.max(wait));
            return Err(PunchRefusal::Limited { limit, retry_after });
        }
        if let Some(retry_after) = self.capacity_wait(now) {
            return Err(PunchRefusal::AtCapacity { retry_after });
        }
        self.budget.record(source, now);
        let punch_id = loop {
            let candidate = PunchId::generate();
            if !self.punches.contains_key(&candidate) {
                break candidate;
            }
        };
        let (answer_tx, answer) = oneshot::channel();
        let tickets = request.rendezvous.map(|address| {
            let tickets = RoleTickets::generate();
            let rendezvous_for = |role| UdpRendezvous {
                address: address.to_owned(),
                ticket: tickets.for_role(role).clone(),
            };
            let issued = (
                rendezvous_for(PunchRole::Gateway),
                rendezvous_for(PunchRole::Device),
            );
            (tickets, issued)
        });
        let (tickets, issued) = tickets.unzip();
        self.punches.insert(
            punch_id,
            Punch {
                node: request.relay_node_id.to_owned(),
                remote_api_key: request.remote_api_key.to_owned(),
                control_token: request.control_token,
                answer: Some(answer_tx),
                tickets,
                gateway: RoleState::default(),
                device: RoleState::default(),
                datagrams: 0,
                created: now,
                paired_at: None,
            },
        );
        let (register, device_rendezvous) = issued.unzip();
        Ok(NewPunch {
            punch_id,
            answer,
            register,
            device_rendezvous,
        })
    }
}

struct NewPunch {
    punch_id: PunchId,
    answer: oneshot::Receiver<DirectReply>,
    register: Option<UdpRendezvous>,
    device_rendezvous: Option<UdpRendezvous>,
}

/// What C knows about an offer when it asks for a punch.
pub(crate) struct PunchRequest<'a> {
    pub(crate) relay_node_id: &'a str,
    pub(crate) remote_api_key: &'a str,
    /// The admitted control connection the offer is forwarded on. Only a
    /// report arriving on it settles the punch.
    pub(crate) control_token: u64,
    /// The client source the offer budget is keyed on.
    pub(crate) source: Option<SourceKey>,
    /// The rendezvous address both roles register at, when C runs one and the
    /// gateway registers on demand.
    pub(crate) rendezvous: Option<&'a str>,
    pub(crate) answer_timeout: Duration,
}

/// A newly minted punch: the POST's side, and the gateway role's rendezvous
/// for the `DirectOffer`.
pub(crate) struct AdmittedPunch {
    pub(crate) pending: PendingOffer,
    pub(crate) register: Option<UdpRendezvous>,
}

/// The limit that refused an offer with `429`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OfferLimit {
    SourceRate,
    NodeRate,
    InFlight,
}

impl OfferLimit {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::SourceRate => "source_rate",
            Self::NodeRate => "node_rate",
            Self::InFlight => "in_flight",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PunchRefusal {
    /// A per-source rate, the per-node ceiling or the in-flight cap. When
    /// several refuse, `limit` is the first of them and `retry_after` the
    /// longest wait.
    Limited {
        limit: OfferLimit,
        retry_after: Duration,
    },
    /// [`MAX_PENDING_PUNCHES`]; `retry_after` is when the oldest punch expires.
    AtCapacity { retry_after: Duration },
}

/// What became of a [`ControlReport`] the gateway sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportOutcome {
    Delivered,
    UnknownPunch,
    /// The punch belongs to another node or another control connection.
    Foreign,
    AlreadySettled,
    OversizedAnswer,
}

impl ReportOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::UnknownPunch => "unknown_punch",
            Self::Foreign => "foreign",
            Self::AlreadySettled => "already_settled",
            Self::OversizedAnswer => "oversized_answer",
        }
    }
}

/// Every live punch, keyed by [`PunchId`], with the offer budgets.
#[derive(Default)]
pub(crate) struct PunchRegistry {
    table: Mutex<PunchTable>,
}

impl PunchRegistry {
    /// Mints a punch for an offer that passed the route checks, if the offer
    /// budgets, the node's in-flight cap and [`MAX_PENDING_PUNCHES`] allow it.
    /// A refused offer consumes no budget.
    pub(crate) fn admit(
        self: &Arc<Self>,
        request: PunchRequest<'_>,
    ) -> Result<AdmittedPunch, PunchRefusal> {
        let now = Instant::now();
        let (admitted, ended) = {
            let mut table = self.table.lock();
            let ended = table.sweep(now);
            (table.admit(&request, now), ended)
        };
        log_ended(&ended, PunchEnd::Expired);
        let NewPunch {
            punch_id,
            answer,
            register,
            device_rendezvous,
        } = admitted?;
        Ok(AdmittedPunch {
            pending: PendingOffer {
                punches: Arc::clone(self),
                punch_id,
                answer,
                device_rendezvous,
                answer_timeout: request.answer_timeout,
                keep: false,
            },
            register,
        })
    }

    /// Hands A's report to the waiting POST. Honoured only for a punch of
    /// `relay_node_id` forwarded on the control connection `control_token`.
    pub(crate) fn resolve(
        &self,
        relay_node_id: &str,
        control_token: u64,
        report: ControlReport,
    ) -> ReportOutcome {
        let (punch_id, reply) = match report {
            ControlReport::DirectAnswer { punch_id, answer } => {
                if !answer.is_within_bounds() {
                    return ReportOutcome::OversizedAnswer;
                }
                (punch_id, DirectReply::Answer(answer))
            }
            ControlReport::DirectDeclined { punch_id } => (punch_id, DirectReply::Declined),
        };
        let sender = {
            let mut table = self.table.lock();
            let Some(punch) = table.punches.get_mut(&punch_id) else {
                return ReportOutcome::UnknownPunch;
            };
            if punch.node != relay_node_id || punch.control_token != control_token {
                return ReportOutcome::Foreign;
            }
            punch.answer.take()
        };
        match sender.map(|sender| sender.send(reply)) {
            Some(Ok(())) => ReportOutcome::Delivered,
            Some(Err(_)) | None => ReportOutcome::AlreadySettled,
        }
    }

    /// Drops every punch forwarded on one control connection, which ends a
    /// POST still waiting on it with `504`.
    pub(crate) fn drop_control(&self, relay_node_id: &str, control_token: u64) {
        let ended: Vec<EndedPunch> = self
            .table
            .lock()
            .punches
            .extract_if(|_, punch| {
                punch.control_token == control_token && punch.node == relay_node_id
            })
            .map(|(punch_id, punch)| punch.ended(punch_id))
            .collect();
        log_ended(&ended, PunchEnd::ControlClosed);
    }

    /// C's reply to a `Register` from `source`, or `None` to drop it
    /// silently: an unknown or expired punch, a spent datagram budget, a
    /// wrong ticket, or a second source for a role. The first valid source of
    /// each role is latched; the reply is `Peer` with the other role's
    /// mapping once both are latched, `Registered` before that.
    pub(crate) fn register_datagram(
        &self,
        punch_id: PunchId,
        role: PunchRole,
        ticket: &RendezvousTicket,
        source: SocketAddrV4,
    ) -> Option<ProbeDatagram> {
        let now = Instant::now();
        let mut table = self.table.lock();
        let punch = table
            .punches
            .get_mut(&punch_id)
            .filter(|punch| punch.is_live(now))?;
        if punch.datagrams >= MAX_DATAGRAMS_PER_PUNCH {
            return None;
        }
        punch.datagrams += 1;
        if punch.tickets.as_ref()?.for_role(role) != ticket {
            return None;
        }
        let own = punch.role_mut(role);
        match own.source {
            Some(latched) if latched != source => return None,
            Some(_) => {}
            None => own.source = Some(source),
        }
        let Some(srflx) = punch.role(other_role(role)).source else {
            return Some(ProbeDatagram::Registered { punch_id });
        };
        punch.role_mut(role).peer_sent = true;
        punch.paired_at.get_or_insert(now);
        Some(ProbeDatagram::Peer { punch_id, srflx })
    }

    /// Drops every expired punch and every offer window that has emptied.
    pub(crate) fn sweep(&self) {
        let ended = self.table.lock().sweep(Instant::now());
        log_ended(&ended, PunchEnd::Expired);
    }

    fn remove(&self, punch_id: PunchId) {
        let ended = self
            .table
            .lock()
            .punches
            .remove(&punch_id)
            .map(|punch| punch.ended(punch_id));
        if let Some(ended) = ended {
            log_ended(std::slice::from_ref(&ended), PunchEnd::Settled);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.table.lock().punches.len()
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, punch_id: PunchId) -> bool {
        self.table.lock().punches.contains_key(&punch_id)
    }
}

/// How P's POST ended.
#[derive(Debug)]
pub(crate) enum OfferOutcome {
    Answered(DirectOfferResponse),
    Declined,
    NoAnswer,
}

/// The POST's hold on its punch. Dropping it drops the punch, unless the POST
/// ended `200` with a rendezvous: that punch stays for the roles' `Register`s
/// until it is paired and then until [`PUNCH_TTL`].
pub(crate) struct PendingOffer {
    punches: Arc<PunchRegistry>,
    punch_id: PunchId,
    answer: oneshot::Receiver<DirectReply>,
    device_rendezvous: Option<UdpRendezvous>,
    answer_timeout: Duration,
    keep: bool,
}

impl PendingOffer {
    pub(crate) fn punch_id(&self) -> PunchId {
        self.punch_id
    }

    /// Waits up to the answer timeout for A's report. A dropped punch (its
    /// control connection ended) reads as no answer.
    pub(crate) async fn outcome(mut self) -> OfferOutcome {
        match tokio::time::timeout(self.answer_timeout, &mut self.answer).await {
            Ok(Ok(DirectReply::Answer(answer))) => {
                let rendezvous = self.device_rendezvous.take();
                self.keep = rendezvous.is_some();
                OfferOutcome::Answered(DirectOfferResponse {
                    punch_id: self.punch_id,
                    answer,
                    rendezvous,
                })
            }
            Ok(Ok(DirectReply::Declined)) => OfferOutcome::Declined,
            Ok(Err(_)) | Err(_) => OfferOutcome::NoAnswer,
        }
    }
}

impl Drop for PendingOffer {
    fn drop(&mut self) {
        if !self.keep {
            self.punches.remove(self.punch_id);
        }
    }
}

#[cfg(test)]
mod tests;
