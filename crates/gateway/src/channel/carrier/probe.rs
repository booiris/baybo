//! A's probe datagrams: the punches it sends for an accepted offer, its
//! on-demand registration with C's rendezvous, and the routing of what
//! arrives on each family socket.
//!
//! A sends punches only to P's sealed host candidates and to the `Peer`
//! mapping it latched for a punch it answered. A source admitted by an
//! authenticated punch is never a target.

use std::net::{IpAddr, SocketAddr, SocketAddrV4};
use std::sync::Arc;

use carrier::burst::{PUNCH_BURST, PunchBurst};
use carrier::rendezvous::{OwnAddresses, RegisterOutcome, Registration};
use carrier::socket::{DemuxSocket, ProbeReceiver, ReceivedProbe};
use remote_host_protocol::relay::{
    PEER_WAIT, PUNCH_TAG_LEN, ProbeDatagram, PunchId, PunchRole, PunchTag, UdpRendezvous,
};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::gather::PunchPair;
use super::punches::PunchAuth;
use super::runtime::RuntimeContext;

/// Punch datagrams A sends for one offer, host candidates and the latched
/// `Peer` together.
pub(crate) const MAX_PUNCH_DATAGRAMS_PER_OFFER: usize = 256;
/// Host-candidate pairs A punches per offer. Every pair gets a whole burst,
/// and one burst's share stays for the `Peer` mapping.
pub(crate) const MAX_HOST_PUNCH_PAIRS: usize = MAX_PUNCH_DATAGRAMS_PER_OFFER / PUNCH_BURST - 1;
/// Rendezvous replies waiting for one punch's registration.
pub(crate) const REPLY_QUEUE_CAPACITY: usize = 16;

/// One pair to punch and the socket of its family.
pub(crate) struct PunchTarget {
    pub(crate) socket: Arc<DemuxSocket>,
    pub(crate) pair: PunchPair,
}

/// Punches each target `PUNCH_BURST` times, `PUNCH_INTERVAL` apart. A's
/// punches carry a random tag: only P can mint a valid one.
pub(crate) async fn punch_hosts(punch_id: PunchId, targets: Vec<PunchTarget>) {
    let mut burst = PunchBurst::new();
    let mut seq: u16 = 0;
    let mut sent = 0usize;
    while burst.next_round().await.is_some() {
        for target in &targets {
            if send_punch(
                &target.socket,
                &mut seq,
                target.pair.target,
                Some(target.pair.source),
            )
            .await
            {
                sent += 1;
            }
        }
    }
    tracing::debug!(
        punch = %punch_id.tag(),
        targets = targets.len(),
        datagrams = sent,
        "punch"
    );
}

async fn send_punch(
    socket: &DemuxSocket,
    seq: &mut u16,
    target: SocketAddr,
    source: Option<IpAddr>,
) -> bool {
    let punch = ProbeDatagram::Punch {
        seq: *seq,
        tag: PunchTag::from_bytes(rand::random::<[u8; PUNCH_TAG_LEN]>()),
    };
    *seq = seq.wrapping_add(1);
    match socket.send_probe(&punch, target, source).await {
        Ok(()) => true,
        Err(error) => {
            tracing::debug!(%error, %target, "carrier: punch send failed");
            false
        }
    }
}

/// Registers A's IPv4 socket for `punch_id` at C's rendezvous, and punches
/// the mapping C returns. The rendezvous address resolves to `Public` IPv4
/// that is none of A's `own` addresses, or registration is skipped. A latched
/// `Peer` joins the punch's allowed set; a later conflicting one is dropped.
pub(crate) async fn register_and_punch(
    context: Arc<RuntimeContext>,
    socket: Arc<DemuxSocket>,
    punch_id: PunchId,
    rendezvous: UdpRendezvous,
    own: OwnAddresses,
    mut replies: mpsc::Receiver<ReceivedProbe>,
) {
    let policy = context.policy;
    let resolved = tokio::task::spawn_blocking(move || {
        let address = rendezvous.resolve_public_v4(&policy);
        (rendezvous.key, address)
    })
    .await;
    let (key, address) = match resolved {
        Ok((key, Ok(address))) => (key, address),
        Ok((_, Err(error))) => {
            tracing::info!(punch = %punch_id.tag(), outcome = "unresolved", "udp_register");
            tracing::debug!(punch = %punch_id.tag(), %error, "udp_register: rendezvous address refused");
            return;
        }
        Err(error) => {
            tracing::warn!(punch = %punch_id.tag(), %error, "udp_register: rendezvous lookup task failed");
            return;
        }
    };
    let deadline = Instant::now() + PEER_WAIT;
    let Some(mut registration) =
        Registration::new(punch_id, PunchRole::Gateway, key, address, policy, own)
    else {
        tracing::info!(punch = %punch_id.tag(), outcome = "own_address", "udp_register");
        return;
    };
    match registration
        .until_peer(&socket, &mut replies, deadline)
        .await
    {
        RegisterOutcome::NoPeer { registered } => {
            let outcome = if registered { "no_peer" } else { "no_reply" };
            tracing::info!(punch = %punch_id.tag(), outcome, "udp_register");
        }
        RegisterOutcome::Peer(srflx) => {
            tracing::info!(punch = %punch_id.tag(), outcome = "peer", "udp_register");
            tracing::debug!(punch = %punch_id.tag(), %srflx, "udp_register: Peer latched");
            let allowed = context.punches.lock().allow_srflx(
                &punch_id,
                IpAddr::V4(*srflx.ip()),
                Instant::now(),
            );
            if allowed {
                tokio::join!(
                    punch_srflx(&socket, punch_id, srflx),
                    registration.watch(&mut replies, deadline)
                );
            }
        }
    }
}

async fn punch_srflx(socket: &DemuxSocket, punch_id: PunchId, srflx: SocketAddrV4) {
    let mut burst = PunchBurst::new();
    let mut seq: u16 = 0;
    let mut sent = 0usize;
    while burst.next_round().await.is_some() {
        if send_punch(socket, &mut seq, SocketAddr::V4(srflx), None).await {
            sent += 1;
        }
    }
    tracing::debug!(punch = %punch_id.tag(), targets = 1, datagrams = sent, "punch");
}

/// Routes every probe datagram a family socket receives: a `Punch` is
/// checked against the live punches, a rendezvous reply goes to its punch's
/// registration, and anything else is dropped.
pub(crate) async fn route_probes(mut probes: ProbeReceiver, context: Arc<RuntimeContext>) {
    while let Some(probe) = probes.recv().await {
        match probe.datagram {
            ProbeDatagram::Punch { seq, tag } => {
                let judged = context.punches.lock().authenticate(
                    probe.source.ip(),
                    seq,
                    &tag,
                    &context.sealer,
                    Instant::now(),
                );
                match judged {
                    PunchAuth::Admitted { punch_id, sources } => {
                        tracing::debug!(punch = %punch_id.tag(), sources, "punch_admit");
                    }
                    PunchAuth::Full { punch_id } => {
                        tracing::debug!(punch = %punch_id.tag(), "punch_admit: source cap reached");
                    }
                    PunchAuth::Replayed | PunchAuth::Rejected => {
                        tracing::trace!(?judged, "carrier: punch not admitted");
                    }
                }
            }
            ProbeDatagram::Registered { punch_id, .. } | ProbeDatagram::Peer { punch_id, .. } => {
                let route = context
                    .punches
                    .lock()
                    .replies_for(&punch_id, Instant::now());
                match route {
                    Some(replies) => {
                        if replies.try_send(probe).is_err() {
                            tracing::trace!(
                                "carrier: rendezvous reply dropped; its queue is gone or full"
                            );
                        }
                    }
                    None => tracing::trace!("carrier: rendezvous reply for no registering punch"),
                }
            }
            ProbeDatagram::Register { .. } => {
                tracing::trace!("carrier: dropped a Register; A registers, it never serves");
            }
        }
    }
}
