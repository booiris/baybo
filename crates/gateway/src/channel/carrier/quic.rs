//! A's QUIC endpoint: admission of each `Incoming`, and the connections and
//! streams it admits.
//!
//! - An `Incoming` proceeds only when its source IP is in a live punch's
//!   allowed set and the connection caps have room. Everything else is
//!   `ignore()`d: no response, no Retry and no TLS work. An admitted but
//!   unvalidated address gets a Retry first.
//! - [`FIRST_STREAM_DEADLINE`] starts at `Incoming::accept()`. A connection
//!   that has no stream authenticated by Noise IK by then, or whose first
//!   stream to settle fails, is closed with [`CARRIER_UNAUTHENTICATED`].
//! - Each stream's `DirectOpen` and Noise IK run in their own task in the
//!   connection's `JoinSet`, bounded by a semaphore of
//!   `MAX_STREAMS_PER_CONNECTION`, so a stalled preface never blocks the
//!   next stream.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use carrier::framing::FrameReader;
use carrier::kind::CarrierKind;
use carrier::quic::MAX_STREAMS_PER_CONNECTION;
use parking_lot::Mutex;
use remote_host_protocol::relay::{AddressPolicy, LegClass, SourceKey};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::runtime::RuntimeContext;
use super::session::{
    AuthReport, FramedSource, QuicBinarySink, handle_authenticated_transport, log_session_end,
    read_open,
};

/// A connection must have an authenticated stream this long after
/// `Incoming::accept()`, so the deadline covers the handshake.
pub(crate) const FIRST_STREAM_DEADLINE: Duration = Duration::from_secs(3);
/// Live QUIC connections per runtime: one carrier plus the losing probe
/// attempts.
pub(crate) const MAX_QUIC_CONNECTIONS: usize = 8;
/// Live QUIC connections per source (an IPv4 address or an IPv6 /64).
pub(crate) const MAX_QUIC_CONNECTIONS_PER_SOURCE: usize = 4;

/// The QUIC application error code A closes a carrier with when its runtime
/// stops: the binding ended or its socket is rebound.
pub(crate) const CARRIER_REVOKED: quinn::VarInt = quinn::VarInt::from_u32(1);
pub(crate) const CARRIER_REVOKED_REASON: &[u8] = b"carrier stopped";
/// The code a connection closes with when no stream authenticated in time,
/// or its first stream failed authentication.
pub(crate) const CARRIER_UNAUTHENTICATED: quinn::VarInt = quinn::VarInt::from_u32(2);

/// Blob streams yield to chat and API streams; quinn sends higher values
/// first and defaults to 0.
const BLOB_STREAM_PRIORITY: i32 = -1;
/// How often at most the endpoint logs its admission counters.
const INCOMING_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// The connection caps, keyed by `source_key`.
#[derive(Default)]
pub(crate) struct ConnectionSlots(Mutex<SlotCounts>);

#[derive(Default)]
struct SlotCounts {
    total: usize,
    per_source: HashMap<SourceKey, usize>,
}

/// One live connection's share of the caps, released on drop.
pub(crate) struct ConnectionSlot {
    slots: Arc<ConnectionSlots>,
    source: SourceKey,
}

impl SlotCounts {
    /// The connections `source` holds, or `None` when the global or its
    /// per-source cap is full.
    fn room_for(&self, source: SourceKey) -> Option<usize> {
        let from_source = self.per_source.get(&source).copied().unwrap_or(0);
        (self.total < MAX_QUIC_CONNECTIONS && from_source < MAX_QUIC_CONNECTIONS_PER_SOURCE)
            .then_some(from_source)
    }
}

impl ConnectionSlots {
    /// Whether a connection from `ip` would get a slot now, without taking one.
    pub(crate) fn has_room(&self, ip: IpAddr) -> bool {
        self.0
            .lock()
            .room_for(AddressPolicy::source_key(ip))
            .is_some()
    }

    /// A slot for a connection from `ip`, or `None` when the global or the
    /// per-source cap is full.
    pub(crate) fn try_acquire(self: &Arc<Self>, ip: IpAddr) -> Option<ConnectionSlot> {
        let source = AddressPolicy::source_key(ip);
        let mut counts = self.0.lock();
        let from_source = counts.room_for(source)?;
        counts.total += 1;
        counts.per_source.insert(source, from_source + 1);
        Some(ConnectionSlot {
            slots: Arc::clone(self),
            source,
        })
    }

    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.0.lock().total
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        let mut counts = self.slots.0.lock();
        counts.total = counts.total.saturating_sub(1);
        if let Some(count) = counts.per_source.get_mut(&self.source) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.per_source.remove(&self.source);
            }
        }
    }
}

/// What the runtime's endpoints did with each `Incoming`, for the
/// `quic_incoming` log line, which the runtime writes at most once per
/// [`INCOMING_LOG_INTERVAL`] whatever its families and rebinds.
#[derive(Default)]
pub(crate) struct IncomingCounts {
    pub(crate) accepted: AtomicU64,
    pub(crate) retried: AtomicU64,
    pub(crate) ignored_not_in_punch: AtomicU64,
    pub(crate) ignored_cap: AtomicU64,
    /// When the current interval started: at the first `Incoming`, then at
    /// each line.
    interval_start: Mutex<Option<Instant>>,
}

impl IncomingCounts {
    /// Logs the counters when an interval has passed since the last line.
    fn note(&self) {
        if self.log_due(Instant::now()) {
            self.log();
        }
    }

    /// Whether a line is due at `now`, starting the next interval if so.
    fn log_due(&self, now: Instant) -> bool {
        let mut interval_start = self.interval_start.lock();
        match *interval_start {
            Some(start) if now.duration_since(start) < INCOMING_LOG_INTERVAL => false,
            Some(_) => {
                *interval_start = Some(now);
                true
            }
            None => {
                *interval_start = Some(now);
                false
            }
        }
    }

    fn log(&self) {
        tracing::debug!(
            accepted = self.accepted.load(Ordering::Relaxed),
            retried = self.retried.load(Ordering::Relaxed),
            ignored_not_in_punch = self.ignored_not_in_punch.load(Ordering::Relaxed),
            ignored_cap = self.ignored_cap.load(Ordering::Relaxed),
            "quic_incoming"
        );
    }
}

/// Accepts on one family's endpoint until it closes or `cancel` fires, then
/// waits for its connections, which see the same `cancel`, to end.
pub(crate) async fn serve_endpoint(
    endpoint: quinn::Endpoint,
    local_addr: SocketAddr,
    context: Arc<RuntimeContext>,
    cancel: CancellationToken,
) {
    let bind_ip = (!local_addr.ip().is_unspecified()).then(|| local_addr.ip());
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else {
                    break;
                };
                if let Some((connecting, slot)) = admit(incoming, &context) {
                    connections.spawn(serve_connection(
                        connecting,
                        slot,
                        bind_ip,
                        Arc::clone(&context),
                        cancel.clone(),
                    ));
                }
                context.incoming.note();
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            () = cancel.cancelled() => break,
        }
    }
    while connections.join_next().await.is_some() {}
}

fn admit(
    incoming: quinn::Incoming,
    context: &RuntimeContext,
) -> Option<(quinn::Connecting, ConnectionSlot)> {
    let counts = &context.incoming;
    let source = AddressPolicy::canonical_ip(incoming.remote_address().ip());
    if !context.punches.lock().admits(source, Instant::now()) {
        counts.ignored_not_in_punch.fetch_add(1, Ordering::Relaxed);
        tracing::trace!(outcome = "ignored:not_in_punch", "quic_incoming");
        incoming.ignore();
        return None;
    }
    if !context.connections.has_room(source) {
        return ignore_over_cap(incoming, counts);
    }
    if !incoming.remote_address_validated() {
        match incoming.retry() {
            Ok(()) => {
                counts.retried.fetch_add(1, Ordering::Relaxed);
                tracing::trace!(outcome = "retried", "quic_incoming");
            }
            Err(unretryable) => unretryable.into_incoming().ignore(),
        }
        return None;
    }
    let Some(slot) = context.connections.try_acquire(source) else {
        return ignore_over_cap(incoming, counts);
    };
    match incoming.accept() {
        Ok(connecting) => {
            counts.accepted.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(outcome = "accepted", "quic_incoming");
            Some((connecting, slot))
        }
        Err(error) => {
            tracing::debug!(%error, "carrier: an admitted QUIC connection failed to start");
            None
        }
    }
}

fn ignore_over_cap(
    incoming: quinn::Incoming,
    counts: &IncomingCounts,
) -> Option<(quinn::Connecting, ConnectionSlot)> {
    counts.ignored_cap.fetch_add(1, Ordering::Relaxed);
    tracing::trace!(outcome = "ignored:cap", "quic_incoming");
    incoming.ignore();
    None
}

/// Serves one admitted connection: its handshake and its streams, under
/// [`FIRST_STREAM_DEADLINE`]. When `cancel` fires, every stream task is
/// dropped before the connection closes with [`CARRIER_REVOKED`].
async fn serve_connection(
    connecting: quinn::Connecting,
    slot: ConnectionSlot,
    bind_ip: Option<IpAddr>,
    context: Arc<RuntimeContext>,
    cancel: CancellationToken,
) {
    let _slot = slot;
    let deadline = tokio::time::sleep(context.timing.first_stream_deadline);
    tokio::pin!(deadline);
    let connection = tokio::select! {
        connected = connecting => match connected {
            Ok(connection) => connection,
            Err(error) => {
                tracing::debug!(%error, "carrier: QUIC handshake failed");
                return;
            }
        },
        () = &mut deadline => {
            tracing::debug!("carrier: QUIC handshake outlived the first-stream deadline");
            return;
        }
        () = cancel.cancelled() => return,
    };
    let kind = connection.local_ip().or(bind_ip).and_then(|local| {
        CarrierKind::of_quic_path(local, connection.remote_address().ip(), &context.policy)
    });
    let permits = Arc::new(Semaphore::new(
        usize::try_from(MAX_STREAMS_PER_CONNECTION).unwrap_or(usize::MAX),
    ));
    let (authenticated_tx, mut authenticated_rx) = mpsc::unbounded_channel::<bool>();
    let mut streams = JoinSet::new();
    let mut authenticated = false;
    loop {
        tokio::select! {
            accepted = connection.accept_bi() => {
                let Ok((send, recv)) = accepted else {
                    break;
                };
                let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                    continue;
                };
                let (abort_tx, abort_rx) = oneshot::channel();
                let abort = streams.spawn(serve_stream(
                    Stream { send, recv, permit },
                    kind,
                    Arc::clone(&context),
                    AuthReport::new(authenticated_tx.clone()),
                    abort_rx,
                ));
                let _ = abort_tx.send(abort);
            }
            Some(stream_authenticated) = authenticated_rx.recv() => {
                if stream_authenticated {
                    authenticated = true;
                } else if !authenticated {
                    connection.close(CARRIER_UNAUTHENTICATED, b"first stream failed authentication");
                    break;
                }
            }
            Some(_) = streams.join_next(), if !streams.is_empty() => {}
            () = &mut deadline, if !authenticated => {
                connection.close(CARRIER_UNAUTHENTICATED, b"no authenticated stream");
                break;
            }
            () = cancel.cancelled() => {
                streams.shutdown().await;
                connection.close(CARRIER_REVOKED, CARRIER_REVOKED_REASON);
                return;
            }
        }
    }
    streams.shutdown().await;
}

/// One accepted bidirectional stream and its share of the connection's
/// stream permits.
struct Stream {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    permit: OwnedSemaphorePermit,
}

/// Runs one stream's session: the `DirectOpen` gate, then the responder of
/// its class. `report` tells the connection whether Noise IK authenticated
/// the device.
async fn serve_stream(
    stream: Stream,
    kind: Option<CarrierKind>,
    context: Arc<RuntimeContext>,
    report: AuthReport,
    abort: oneshot::Receiver<tokio::task::AbortHandle>,
) {
    let Stream { send, recv, permit } = stream;
    let _permit = permit;
    let mut frames = FrameReader::new(recv);
    let open = match read_open(
        &mut frames,
        &context.token,
        context.timing.direct_open_deadline,
    )
    .await
    {
        Ok(open) => open,
        Err(error) => {
            log_session_end(&error);
            return;
        }
    };
    if open.class == LegClass::Blob {
        let _ = send.set_priority(BLOB_STREAM_PRIORITY);
    }
    tracing::info!(
        class = open.class.as_str(),
        kind = kind.map_or("unknown", CarrierKind::as_str),
        device = %crate::channel::short_hash(&context.device_id),
        "carrier_session"
    );
    if let Err(error) = handle_authenticated_transport(
        QuicBinarySink {
            stream: send,
            report,
        },
        FramedSource(frames),
        open.class,
        kind,
        &context.state,
        abort,
    )
    .await
    {
        log_session_end(&error);
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    /// One set of counters serves every endpoint of the runtime, so the
    /// line is due once per interval however many families note it.
    #[test]
    fn the_incoming_line_is_due_once_per_interval_across_every_endpoint() {
        let counts = IncomingCounts::default();
        let start = Instant::now();
        assert!(
            !counts.log_due(start),
            "the first Incoming starts the interval"
        );
        let almost = start + INCOMING_LOG_INTERVAL - Duration::from_millis(1);
        assert!(!counts.log_due(almost));
        let due = start + INCOMING_LOG_INTERVAL;
        assert!(counts.log_due(due));
        assert!(!counts.log_due(due), "another family noting at once");
        assert!(!counts.log_due(due + INCOMING_LOG_INTERVAL - Duration::from_millis(1)));
        assert!(counts.log_due(due + INCOMING_LOG_INTERVAL));
    }

    #[test]
    fn connections_are_capped_per_source_and_in_total_and_release_on_drop() {
        let slots = Arc::new(ConnectionSlots::default());
        let first = ip("1.1.1.1");
        let held: Vec<_> = (0..MAX_QUIC_CONNECTIONS_PER_SOURCE)
            .map(|_| slots.try_acquire(first).unwrap())
            .collect();
        assert!(slots.try_acquire(first).is_none());
        assert!(slots.try_acquire(ip("::ffff:1.1.1.1")).is_none());

        let others: Vec<_> = (0..MAX_QUIC_CONNECTIONS - MAX_QUIC_CONNECTIONS_PER_SOURCE)
            .map(|index| {
                let last = u8::try_from(index + 2).unwrap();
                slots
                    .try_acquire(IpAddr::V4(Ipv4Addr::new(1, 1, 1, last)))
                    .unwrap()
            })
            .collect();
        assert!(slots.try_acquire(ip("9.9.9.9")).is_none(), "the global cap");

        drop(held);
        assert_eq!(slots.live(), others.len());
        assert!(slots.try_acquire(first).is_some());
    }

    #[test]
    fn an_ipv6_slash_64_is_one_source() {
        let slots = Arc::new(ConnectionSlots::default());
        let _held: Vec<_> = (1..=MAX_QUIC_CONNECTIONS_PER_SOURCE)
            .map(|host| {
                slots
                    .try_acquire(ip(&format!("2606:4700:1::{host}")))
                    .unwrap()
            })
            .collect();
        assert!(slots.try_acquire(ip("2606:4700:1::ff")).is_none());
        assert!(slots.try_acquire(ip("2606:4700:2::1")).is_some());
    }
}
