//! The opt-in TCP carrier (`gateway.direct_tcp`): one listener per configured
//! family, bound when the runtime starts and bound again
//! [`REBIND_DELAY`](super::runtime::REBIND_DELAY) after a failed bind, or
//! after an accept error that breaks the listener itself. Each connection is
//! one carrier session.
//!
//! - Before its preface, a connection holds a pre-authentication permit,
//!   taken without waiting: at most [`MAX_TCP_PREAUTH`] in all and
//!   [`MAX_TCP_PREAUTH_PER_SOURCE`] per `source_key`. A **known** source, one
//!   in a live punch's allowed set or one that has authenticated in this
//!   runtime, is exempt from the per-source cap and may also use the
//!   [`TCP_PREAUTH_RESERVED`] permits no other source can take. A
//!   connection that gets no permit is closed unread.
//! - The permit is released when Noise IK completes, the initiator's
//!   confirmation included, or with the session when it fails; an
//!   authenticated session holds one of its device's
//!   [`MAX_TCP_SESSIONS_PER_DEVICE`] permits instead.
//! - `DirectOpen` must arrive within `DIRECT_OPEN_DEADLINE`, and then the
//!   session runs exactly as a QUIC stream's does.
//! - A session whose peer vanishes without a FIN ends after
//!   `DIRECT_QUIC_IDLE_TIMEOUT`, as a QUIC carrier does: keepalive probes
//!   cover a silent session, and on Linux `TCP_USER_TIMEOUT` one whose data
//!   goes unacknowledged.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use carrier::framing::{FrameReader, write_frame};
use carrier::kind::CarrierKind;
use carrier::quic::{DIRECT_QUIC_IDLE_TIMEOUT, DIRECT_QUIC_KEEP_ALIVE};
use parking_lot::Mutex;
use remote_host_protocol::relay::{AddressPolicy, SourceKey};
use socket2::{Domain, Protocol, SockRef, Socket, TcpKeepalive, Type};
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::runtime::RuntimeContext;
use super::session::{FramedSource, handle_authenticated_transport, log_session_end, read_open};
use crate::channel::device_content::{AuthenticatedDevice, BinarySink, RelaySessionError};

/// Pre-authentication permits per runtime.
pub(crate) const MAX_TCP_PREAUTH: usize = 48;
/// Pre-authentication permits one unknown source (an IPv4 address or an IPv6
/// /64) may hold.
pub(crate) const MAX_TCP_PREAUTH_PER_SOURCE: usize = 4;
/// Pre-authentication permits only known sources may take: above the app's
/// burst of 1 chat + 12 concurrent API dials + 3 pooled legs + blob.
pub(crate) const TCP_PREAUTH_RESERVED: usize = 24;
/// Authenticated TCP sessions one device may hold.
pub(crate) const MAX_TCP_SESSIONS_PER_DEVICE: usize = 32;
/// Sources remembered as having authenticated in this runtime; the oldest
/// is forgotten first. Only the paired device can add one: a session counts
/// only once the initiator has confirmed the Noise handshake, which a replay
/// of its first message cannot.
pub(crate) const MAX_KNOWN_TCP_SOURCES: usize = 8;
/// The gap between keepalive probes once a session has been silent for
/// `DIRECT_QUIC_KEEP_ALIVE`.
pub(crate) const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);
/// Unanswered keepalive probes that end a session. With the interval, a
/// silent peer is dropped after `DIRECT_QUIC_IDLE_TIMEOUT`, as a QUIC carrier
/// would be.
pub(crate) const TCP_KEEPALIVE_PROBES: u32 = 7;

/// The pre-authentication permits unknown sources share.
const SHARED_TCP_PREAUTH: usize = MAX_TCP_PREAUTH - TCP_PREAUTH_RESERVED;
const TCP_LISTEN_BACKLOG: i32 = 128;

const _: () = assert!(TCP_PREAUTH_RESERVED < MAX_TCP_PREAUTH);
const _: () = assert!(
    DIRECT_QUIC_KEEP_ALIVE.as_secs()
        + TCP_KEEPALIVE_INTERVAL.as_secs() * TCP_KEEPALIVE_PROBES as u64
        == DIRECT_QUIC_IDLE_TIMEOUT.as_secs()
);
const _: () = assert!(MAX_TCP_PREAUTH_PER_SOURCE <= SHARED_TCP_PREAUTH);

/// The TCP carrier's caps and known sources, shared by a runtime's listeners.
#[derive(Default)]
pub(crate) struct TcpAdmission(Mutex<TcpCounts>);

#[derive(Default)]
struct TcpCounts {
    preauth: usize,
    /// Pre-authentication permits held by unknown sources, in all and per
    /// source.
    unknown: usize,
    unknown_per_source: HashMap<SourceKey, usize>,
    /// Sources that authenticated in this runtime, oldest first.
    authenticated: VecDeque<SourceKey>,
    sessions_per_device: HashMap<String, usize>,
}

impl TcpCounts {
    fn remember(&mut self, source: SourceKey) {
        self.authenticated.retain(|known| *known != source);
        self.authenticated.push_back(source);
        if self.authenticated.len() > MAX_KNOWN_TCP_SOURCES {
            self.authenticated.pop_front();
        }
    }
}

/// A connection's pre-authentication permit, released on drop.
pub(crate) struct PreauthPermit {
    admission: Arc<TcpAdmission>,
    source: SourceKey,
    /// Whether it counts toward the unknown sources' share and the
    /// per-source cap.
    unknown: bool,
}

/// An authenticated session's share of its device's permits, released on
/// drop.
pub(crate) struct DeviceSessionPermit {
    admission: Arc<TcpAdmission>,
    device_id: String,
}

impl TcpAdmission {
    /// A pre-authentication permit for a connection from `ip`, or `None`
    /// when a cap is full. `allowed` says whether `ip` is in a live punch's
    /// allowed set; that, or an earlier authentication from its source, makes
    /// the source known.
    pub(crate) fn try_preauth(
        self: &Arc<Self>,
        ip: IpAddr,
        allowed: bool,
    ) -> Option<PreauthPermit> {
        let source = AddressPolicy::source_key(ip);
        let mut counts = self.0.lock();
        if counts.preauth >= MAX_TCP_PREAUTH {
            return None;
        }
        let unknown = !allowed && !counts.authenticated.contains(&source);
        if unknown {
            let from_source = counts.unknown_per_source.get(&source).copied().unwrap_or(0);
            if counts.unknown >= SHARED_TCP_PREAUTH || from_source >= MAX_TCP_PREAUTH_PER_SOURCE {
                return None;
            }
            counts.unknown += 1;
            counts.unknown_per_source.insert(source, from_source + 1);
        }
        counts.preauth += 1;
        Some(PreauthPermit {
            admission: Arc::clone(self),
            source,
            unknown,
        })
    }

    /// Records that `source` authenticated as `device_id`, and takes one of
    /// the device's session permits: `None` when it holds them all.
    fn authenticate(
        self: &Arc<Self>,
        source: SourceKey,
        device_id: &str,
    ) -> Option<DeviceSessionPermit> {
        let mut counts = self.0.lock();
        counts.remember(source);
        let sessions = counts
            .sessions_per_device
            .get(device_id)
            .copied()
            .unwrap_or(0);
        if sessions >= MAX_TCP_SESSIONS_PER_DEVICE {
            return None;
        }
        counts
            .sessions_per_device
            .insert(device_id.to_owned(), sessions + 1);
        Some(DeviceSessionPermit {
            admission: Arc::clone(self),
            device_id: device_id.to_owned(),
        })
    }

    #[cfg(test)]
    pub(crate) fn preauth_held(&self) -> usize {
        self.0.lock().preauth
    }

    /// Whether `ip`'s source has authenticated in this runtime.
    #[cfg(test)]
    pub(crate) fn knows(&self, ip: IpAddr) -> bool {
        self.0
            .lock()
            .authenticated
            .contains(&AddressPolicy::source_key(ip))
    }

    #[cfg(test)]
    pub(crate) fn sessions_held(&self, device_id: &str) -> usize {
        self.0
            .lock()
            .sessions_per_device
            .get(device_id)
            .copied()
            .unwrap_or(0)
    }
}

impl Drop for PreauthPermit {
    fn drop(&mut self) {
        let mut counts = self.admission.0.lock();
        counts.preauth = counts.preauth.saturating_sub(1);
        if self.unknown {
            counts.unknown = counts.unknown.saturating_sub(1);
            if let Some(count) = counts.unknown_per_source.get_mut(&self.source) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    counts.unknown_per_source.remove(&self.source);
                }
            }
        }
    }
}

impl Drop for DeviceSessionPermit {
    fn drop(&mut self) {
        let mut counts = self.admission.0.lock();
        if let Some(count) = counts.sessions_per_device.get_mut(&self.device_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.sessions_per_device.remove(&self.device_id);
            }
        }
    }
}

/// One family's listener: its configured bind address, and the address it
/// listens on while bound.
pub(crate) struct TcpFamily {
    bind: SocketAddr,
    local_addr: Mutex<Option<SocketAddr>>,
}

impl TcpFamily {
    /// Binds the family's listener. A failed bind leaves the family unbound,
    /// and [`serve`] binds it again after the rebind delay.
    pub(crate) fn bind(bind: SocketAddr) -> (Arc<Self>, Option<TcpListener>) {
        let family = Arc::new(Self {
            bind,
            local_addr: Mutex::new(None),
        });
        let listener = match family.listen() {
            Ok(listener) => Some(listener),
            Err(error) => {
                tracing::warn!(
                    bind = %bind,
                    %error,
                    "carrier: TCP listener unavailable; retrying"
                );
                None
            }
        };
        (family, listener)
    }

    /// The address the listener accepts on, while it is bound.
    pub(crate) fn local_addr(&self) -> Option<SocketAddr> {
        *self.local_addr.lock()
    }

    fn listen(&self) -> io::Result<TcpListener> {
        let listener = listen(self.bind)?;
        *self.local_addr.lock() = Some(listener.local_addr()?);
        Ok(listener)
    }

    fn unbind(&self) {
        *self.local_addr.lock() = None;
    }
}

/// A listening socket on `bind`. An IPv6 one is IPv6-only, so an IPv4
/// listener can take the same port.
fn listen(bind: SocketAddr) -> io::Result<TcpListener> {
    tokio::runtime::Handle::try_current().map_err(io::Error::other)?;
    let socket = Socket::new(Domain::for_address(bind), Type::STREAM, Some(Protocol::TCP))?;
    if bind.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&bind.into())?;
    socket.listen(TCP_LISTEN_BACKLOG)?;
    TcpListener::from_std(socket.into())
}

/// Accepts on `family` until `cancel` fires, binding it again after the
/// rebind delay whenever it is unbound, then drops every session it
/// accepted before it returns.
pub(crate) async fn serve(
    family: Arc<TcpFamily>,
    mut listener: Option<TcpListener>,
    context: Arc<RuntimeContext>,
    cancel: CancellationToken,
) {
    let delay = context.timing.rebind_delay;
    let mut sessions = JoinSet::new();
    // A bound listener accepts from `resume_at` on; an unbound one is bound
    // again at `resume_at`.
    let mut resume_at = Instant::now();
    if listener.is_none() {
        resume_at += delay;
    }
    loop {
        match listener.as_ref() {
            Some(bound) if Instant::now() >= resume_at => tokio::select! {
                accepted = bound.accept() => match accepted {
                    Ok((stream, peer)) => admit(stream, peer, &context, &mut sessions),
                    Err(error) => {
                        let failure = AcceptFailure::of(&error);
                        failure.log(family.bind, &error, delay);
                        let recovery = failure.recovery(Instant::now(), delay);
                        resume_at = recovery.resume_at;
                        if !recovery.keep_listener {
                            family.unbind();
                            listener = None;
                        }
                    }
                },
                Some(_) = sessions.join_next(), if !sessions.is_empty() => {}
                () = cancel.cancelled() => break,
            },
            _ => tokio::select! {
                () = tokio::time::sleep_until(resume_at) => if listener.is_none() {
                    match family.listen() {
                        Ok(bound) => {
                            tracing::info!(bind = %family.bind, "carrier: TCP listener bound");
                            listener = Some(bound);
                        }
                        Err(error) => {
                            tracing::debug!(bind = %family.bind, %error, "carrier: TCP rebind failed; retrying");
                            resume_at = Instant::now() + delay;
                        }
                    }
                },
                Some(_) = sessions.join_next(), if !sessions.is_empty() => {}
                () = cancel.cancelled() => break,
            },
        }
    }
    family.unbind();
    drop(listener);
    sessions.shutdown().await;
}

/// What a failed `accept` says about the listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AcceptFailure {
    /// Only the connection being accepted failed: its peer aborted or reset
    /// it while it was queued, or `accept` passed on one of its network
    /// errors. Any peer can cause one, so the listener takes the next
    /// connection at once.
    Connection,
    /// The process is out of descriptors or memory. The listener keeps its
    /// port and stops accepting for the rebind delay.
    Resources,
    /// The listening socket itself is broken: it is dropped and bound again
    /// after the rebind delay.
    Listener,
}

/// The errors `accept` returns for the connection it was accepting, not for
/// the listener: Linux passes that connection's pending network errors on
/// (`accept(2)`), and macOS reports one reset while it was queued as
/// `ECONNABORTED`.
const CONNECTION_ACCEPT_ERRORS: [i32; 14] = [
    libc::ECONNABORTED,
    libc::ECONNRESET,
    libc::ECONNREFUSED,
    libc::EINTR,
    libc::EAGAIN,
    libc::EPERM,
    libc::ETIMEDOUT,
    libc::EPROTO,
    libc::ENOPROTOOPT,
    libc::EOPNOTSUPP,
    libc::ENETDOWN,
    libc::ENETUNREACH,
    libc::EHOSTDOWN,
    libc::EHOSTUNREACH,
];
const RESOURCE_ACCEPT_ERRORS: [i32; 4] = [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM];

/// What the accept loop does after a failed `accept`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AcceptRecovery {
    /// Whether the listening socket is kept, and with it the port.
    keep_listener: bool,
    /// When the loop accepts again, or binds the listener again.
    resume_at: Instant,
}

impl AcceptFailure {
    fn of(error: &io::Error) -> Self {
        match error.raw_os_error() {
            Some(code) if CONNECTION_ACCEPT_ERRORS.contains(&code) => Self::Connection,
            Some(code) if RESOURCE_ACCEPT_ERRORS.contains(&code) => Self::Resources,
            _ => Self::Listener,
        }
    }

    fn recovery(self, now: Instant, delay: Duration) -> AcceptRecovery {
        match self {
            Self::Connection => AcceptRecovery {
                keep_listener: true,
                resume_at: now,
            },
            Self::Resources => AcceptRecovery {
                keep_listener: true,
                resume_at: now + delay,
            },
            Self::Listener => AcceptRecovery {
                keep_listener: false,
                resume_at: now + delay,
            },
        }
    }

    fn log(self, bind: SocketAddr, error: &io::Error, delay: Duration) {
        match self {
            Self::Connection => {
                tracing::trace!(%error, outcome = "accept_failed", "tcp_incoming");
            }
            Self::Resources => tracing::warn!(
                %bind,
                %error,
                resume_in = ?delay,
                "carrier: TCP accept is out of resources; pausing the listener"
            ),
            Self::Listener => tracing::warn!(
                %bind,
                %error,
                retry_in = ?delay,
                "carrier: TCP accept failed; rebinding the listener"
            ),
        }
    }
}

/// Takes a pre-authentication permit for a new connection and starts its
/// session, or closes it unread.
fn admit(
    stream: TcpStream,
    peer: SocketAddr,
    context: &Arc<RuntimeContext>,
    sessions: &mut JoinSet<()>,
) {
    let ip = AddressPolicy::canonical_ip(peer.ip());
    let allowed = context.punches.lock().admits(ip, Instant::now());
    let Some(preauth) = context.tcp.try_preauth(ip, allowed) else {
        tracing::trace!(outcome = "refused:preauth_cap", "tcp_incoming");
        return;
    };
    if let Err(error) = configure_session_socket(&stream) {
        tracing::debug!(%error, "carrier: TCP session options failed; closing the connection");
        return;
    }
    let (abort_tx, abort_rx) = oneshot::channel();
    let abort = sessions.spawn(serve_session(
        stream,
        preauth,
        Arc::clone(context),
        abort_rx,
    ));
    let _ = abort_tx.send(abort);
}

/// Sets `TCP_NODELAY`, and bounds how long a session outlives a peer that
/// vanished without a FIN by `DIRECT_QUIC_IDLE_TIMEOUT`, as a QUIC carrier's
/// idle timeout does: keepalive probes cover a silent session, and on Linux
/// `TCP_USER_TIMEOUT` covers one whose data goes unacknowledged, such as the
/// chat pump's pings or a blob download.
fn configure_session_socket(stream: &TcpStream) -> io::Result<()> {
    stream.set_nodelay(true)?;
    let socket = SockRef::from(stream);
    socket.set_tcp_keepalive(
        &TcpKeepalive::new()
            .with_time(DIRECT_QUIC_KEEP_ALIVE)
            .with_interval(TCP_KEEPALIVE_INTERVAL)
            .with_retries(TCP_KEEPALIVE_PROBES),
    )?;
    #[cfg(target_os = "linux")]
    socket.set_tcp_user_timeout(Some(DIRECT_QUIC_IDLE_TIMEOUT))?;
    Ok(())
}

/// Runs one connection's session: the `DirectOpen` gate, then the responder
/// of its class.
async fn serve_session(
    stream: TcpStream,
    preauth: PreauthPermit,
    context: Arc<RuntimeContext>,
    abort: oneshot::Receiver<AbortHandle>,
) {
    let (read, write) = stream.into_split();
    let mut frames = FrameReader::new(read);
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
    tracing::info!(
        class = open.class.as_str(),
        kind = CarrierKind::Tcp.as_str(),
        device = %crate::channel::short_hash(&context.device_id),
        "carrier_session"
    );
    let sink = TcpBinarySink {
        write,
        admission: Arc::clone(&context.tcp),
        source: preauth.source,
        preauth: Some(preauth),
        session: None,
    };
    if let Err(error) = handle_authenticated_transport(
        sink,
        FramedSource(frames),
        open.class,
        Some(CarrierKind::Tcp),
        &context.state,
        abort,
    )
    .await
    {
        log_session_end(&error);
    }
}

/// The send half of one TCP carrier session, framed, and the permit the
/// session holds: its pre-authentication permit until Noise IK completes,
/// then its device's.
struct TcpBinarySink {
    write: OwnedWriteHalf,
    admission: Arc<TcpAdmission>,
    source: SourceKey,
    preauth: Option<PreauthPermit>,
    session: Option<DeviceSessionPermit>,
}

#[async_trait::async_trait]
impl BinarySink for TcpBinarySink {
    const CONFIRMS_HANDSHAKE: bool = true;

    async fn send_bytes(&mut self, bytes: Vec<u8>) -> Result<(), ()> {
        write_frame(&mut self.write, &bytes)
            .await
            .map_err(|error| tracing::debug!(%error, "carrier TCP write failed"))
    }

    async fn close(&mut self) {
        let _ = self.write.shutdown().await;
    }

    fn authenticated(&mut self, device: &AuthenticatedDevice) -> Result<(), RelaySessionError> {
        let Some(session) = self.admission.authenticate(self.source, &device.device_id) else {
            tracing::info!(
                device = %crate::channel::short_hash(&device.device_id),
                cap = MAX_TCP_SESSIONS_PER_DEVICE,
                "carrier: the device holds every direct TCP session permit; refusing another"
            );
            return Err(RelaySessionError::Ended(
                "the device holds every direct TCP session permit".to_owned(),
            ));
        };
        self.session = Some(session);
        self.preauth = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    const DEVICE: &str = "device-1";

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    fn v4(last: usize) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(1, 1, 1, u8::try_from(last).unwrap()))
    }

    fn admission() -> Arc<TcpAdmission> {
        Arc::new(TcpAdmission::default())
    }

    #[test]
    fn an_unknown_source_is_capped_and_its_permits_release_on_drop() {
        let admission = admission();
        let source = ip("1.1.1.1");
        let held: Vec<_> = (0..MAX_TCP_PREAUTH_PER_SOURCE)
            .map(|_| admission.try_preauth(source, false).unwrap())
            .collect();
        assert!(admission.try_preauth(source, false).is_none());
        assert!(
            admission.try_preauth(ip("::ffff:1.1.1.1"), false).is_none(),
            "the mapped form is the same source"
        );
        assert!(admission.try_preauth(ip("1.1.1.2"), false).is_some());
        drop(held);
        assert_eq!(admission.preauth_held(), 0);
        assert!(admission.try_preauth(source, false).is_some());
    }

    #[test]
    fn an_ipv6_slash_64_is_one_source() {
        let admission = admission();
        let _held: Vec<_> = (1..=MAX_TCP_PREAUTH_PER_SOURCE)
            .map(|host| {
                admission
                    .try_preauth(ip(&format!("2606:4700:1::{host}")), false)
                    .unwrap()
            })
            .collect();
        assert!(
            admission
                .try_preauth(ip("2606:4700:1:0:ffff::9"), false)
                .is_none()
        );
        assert!(admission.try_preauth(ip("2606:4700:2::1"), false).is_some());
    }

    #[test]
    fn a_preauth_flood_leaves_known_sources_their_reserved_permits() {
        let admission = admission();
        let flood: Vec<_> = (0..)
            .map_while(|index| admission.try_preauth(v4(index + 1), false))
            .collect();
        assert_eq!(flood.len(), MAX_TCP_PREAUTH - TCP_PREAUTH_RESERVED);
        assert!(admission.try_preauth(ip("2.2.2.2"), false).is_none());

        let device = ip("10.0.0.9");
        let known: Vec<_> = (0..TCP_PREAUTH_RESERVED)
            .map(|_| {
                admission
                    .try_preauth(device, true)
                    .expect("a known source keeps its reserved share")
            })
            .collect();
        assert!(known.len() > MAX_TCP_PREAUTH_PER_SOURCE);
        assert!(
            admission.try_preauth(device, true).is_none(),
            "the global cap binds known sources too"
        );
        drop(flood);
        assert!(admission.try_preauth(device, true).is_some());
    }

    #[test]
    fn a_source_that_authenticated_stays_known_until_it_is_the_oldest_of_too_many() {
        let admission = admission();
        let first = ip("2606:4700:1::1");
        let session = admission
            .authenticate(AddressPolicy::source_key(first), DEVICE)
            .unwrap();
        let from_its_slash_64: Vec<_> = (0..=MAX_TCP_PREAUTH_PER_SOURCE)
            .map(|_| {
                admission
                    .try_preauth(ip("2606:4700:1::77"), false)
                    .expect("its /64 is known")
            })
            .collect();
        drop(from_its_slash_64);

        for index in 0..MAX_KNOWN_TCP_SOURCES {
            let other = AddressPolicy::source_key(v4(index + 1));
            drop(admission.authenticate(other, DEVICE));
        }
        let _unknown_again: Vec<_> = (0..MAX_TCP_PREAUTH_PER_SOURCE)
            .map(|_| admission.try_preauth(first, false).unwrap())
            .collect();
        assert!(
            admission.try_preauth(first, false).is_none(),
            "the oldest known source is forgotten"
        );
        drop(session);
    }

    #[test]
    fn only_an_accept_error_of_the_listener_itself_rebinds_it() {
        use AcceptFailure::{Connection, Listener, Resources};
        let cases = [
            (libc::ECONNABORTED, Connection),
            (libc::ECONNRESET, Connection),
            (libc::EPROTO, Connection),
            (libc::ENETUNREACH, Connection),
            (libc::EHOSTUNREACH, Connection),
            (libc::EPERM, Connection),
            (libc::EMFILE, Resources),
            (libc::ENFILE, Resources),
            (libc::ENOBUFS, Resources),
            (libc::ENOMEM, Resources),
            (libc::EBADF, Listener),
            (libc::EINVAL, Listener),
            (libc::ENOTSOCK, Listener),
        ];
        for (code, failure) in cases {
            assert_eq!(
                AcceptFailure::of(&io::Error::from_raw_os_error(code)),
                failure,
                "errno {code}"
            );
        }
        assert_eq!(
            AcceptFailure::of(&io::Error::other("no errno")),
            Listener,
            "an error of unknown cause rebinds"
        );
    }

    #[test]
    fn a_connection_error_keeps_accepting_a_resource_error_pauses_and_a_listener_error_rebinds() {
        use AcceptFailure::{Connection, Listener, Resources};
        let now = Instant::now();
        let delay = crate::channel::carrier::runtime::REBIND_DELAY;
        assert_eq!(
            Connection.recovery(now, delay),
            AcceptRecovery {
                keep_listener: true,
                resume_at: now,
            },
            "the next connection is taken at once"
        );
        assert_eq!(
            Resources.recovery(now, delay),
            AcceptRecovery {
                keep_listener: true,
                resume_at: now + delay,
            },
            "the port is kept through the pause"
        );
        assert_eq!(
            Listener.recovery(now, delay),
            AcceptRecovery {
                keep_listener: false,
                resume_at: now + delay,
            },
            "only a broken listener is dropped and bound again"
        );
    }

    #[tokio::test]
    async fn a_session_socket_drops_a_vanished_peer_after_the_quic_idle_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (accepted, _) = listener.accept().await.unwrap();
        configure_session_socket(&accepted).unwrap();

        let socket = SockRef::from(&accepted);
        assert!(accepted.nodelay().unwrap());
        assert!(socket.keepalive().unwrap());
        assert_eq!(socket.tcp_keepalive_time().unwrap(), DIRECT_QUIC_KEEP_ALIVE);
        assert_eq!(
            socket.tcp_keepalive_interval().unwrap(),
            TCP_KEEPALIVE_INTERVAL
        );
        assert_eq!(
            socket.tcp_keepalive_retries().unwrap(),
            TCP_KEEPALIVE_PROBES
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            socket.tcp_user_timeout().unwrap(),
            Some(DIRECT_QUIC_IDLE_TIMEOUT)
        );
        drop(client);
    }

    #[test]
    fn a_device_holds_at_most_its_session_permits() {
        let admission = admission();
        let source = AddressPolicy::source_key(ip("10.0.0.9"));
        let held: Vec<_> = (0..MAX_TCP_SESSIONS_PER_DEVICE)
            .map(|_| admission.authenticate(source, DEVICE).unwrap())
            .collect();
        assert!(admission.authenticate(source, DEVICE).is_none());
        assert!(admission.authenticate(source, "device-2").is_some());
        drop(held);
        assert_eq!(admission.sessions_held(DEVICE), 0);
        assert!(admission.authenticate(source, DEVICE).is_some());
    }
}
