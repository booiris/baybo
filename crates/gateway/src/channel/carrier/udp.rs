//! One address family's stable UDP socket: bound when the runtime starts,
//! served for the runtime's life, and rebound only when receive errors
//! persist. A receive error backs off inside the socket without reaching
//! quinn, so a transient one never costs the family its connections.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use carrier::error::CarrierError;
use carrier::socket::{DemuxSocket, ProbeReceiver};
use parking_lot::Mutex;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::probe::route_probes;
use super::quic::{CARRIER_REVOKED, serve_endpoint};
use super::runtime::RuntimeContext;

/// Receive errors in a row that make an error persistent. The socket backs
/// off between them (`SOCKET_RECV_BACKOFF`), so this is about 8 s of
/// failures.
pub(crate) const REBIND_AFTER_RECV_ERRORS: u32 = 6;
/// How often the family checks its socket's receive-error streak.
const SOCKET_HEALTH_POLL: Duration = Duration::from_secs(1);
/// The wait before a failed rebind is tried again.
const SOCKET_REBIND_DELAY: Duration = Duration::from_secs(2);
const SOCKET_REBOUND_REASON: &[u8] = b"socket rebound";

/// A family's socket and the QUIC endpoint on it.
#[derive(Clone)]
pub(crate) struct BoundSocket {
    pub(crate) local_addr: SocketAddr,
    pub(crate) socket: Arc<DemuxSocket>,
    pub(crate) endpoint: quinn::Endpoint,
}

impl BoundSocket {
    fn bind(
        address: SocketAddr,
        server: quinn::ServerConfig,
    ) -> Result<(Self, ProbeReceiver), CarrierError> {
        let (socket, probes) = DemuxSocket::bind(address)?;
        let local_addr = socket.local_addr().map_err(|error| CarrierError::Bind {
            address,
            reason: error.to_string(),
        })?;
        let endpoint = socket.quic_endpoint(Some(server))?;
        Ok((
            Self {
                local_addr,
                socket,
                endpoint,
            },
            probes,
        ))
    }
}

/// One family: its configured bind address and its current socket, which a
/// rebind replaces. The runtime holds the last reference to the socket once
/// its tasks are gone and quinn has let go of it.
pub(crate) struct UdpFamily {
    bind: SocketAddr,
    server: quinn::ServerConfig,
    bound: Mutex<Option<BoundSocket>>,
}

impl UdpFamily {
    pub(crate) fn bind(
        bind: SocketAddr,
        server: quinn::ServerConfig,
    ) -> Result<(Arc<Self>, ProbeReceiver), CarrierError> {
        let (bound, probes) = BoundSocket::bind(bind, server.clone())?;
        let family = Self {
            bind,
            server,
            bound: Mutex::new(Some(bound)),
        };
        Ok((Arc::new(family), probes))
    }

    /// Whether this is the IPv4 family.
    pub(crate) fn serves_ipv4(&self) -> bool {
        self.bind.is_ipv4()
    }

    /// The family's socket, unless a rebind has not yet found one.
    pub(crate) fn current(&self) -> Option<BoundSocket> {
        self.bound.lock().clone()
    }

    /// Takes the socket out for good: the runtime is stopping.
    pub(crate) fn take(&self) -> Option<BoundSocket> {
        self.bound.lock().take()
    }
}

/// Serves `family` until `cancel` fires: its endpoint's connections and its
/// probe datagrams, each in a task that sees a per-socket child of `cancel`
/// and ends when it fires. Returns only after they have all ended. A
/// persistent receive error closes the endpoint and binds the family again.
pub(crate) async fn supervise(
    family: Arc<UdpFamily>,
    mut probes: ProbeReceiver,
    context: Arc<RuntimeContext>,
    cancel: CancellationToken,
) {
    while let Some(bound) = family.current() {
        let socket_cancel = cancel.child_token();
        let mut serving = JoinSet::new();
        serving.spawn(serve_endpoint(
            bound.endpoint.clone(),
            bound.local_addr,
            Arc::clone(&context),
            socket_cancel.clone(),
        ));
        serving.spawn({
            let socket_cancel = socket_cancel.clone();
            let context = Arc::clone(&context);
            async move {
                socket_cancel
                    .run_until_cancelled(route_probes(probes, context))
                    .await;
            }
        });
        let persistent_error = tokio::select! {
            () = cancel.cancelled() => false,
            () = persistent_recv_errors(&bound.socket) => true,
        };
        socket_cancel.cancel();
        while serving.join_next().await.is_some() {}
        if !persistent_error {
            return;
        }
        tracing::warn!(
            bind = %family.bind,
            consecutive = bound.socket.consecutive_recv_errors(),
            "carrier: UDP receive keeps failing; rebinding the socket"
        );
        bound.endpoint.close(CARRIER_REVOKED, SOCKET_REBOUND_REASON);
        *family.bound.lock() = None;
        drop(bound);
        probes = match rebind(&family, &cancel).await {
            Some(probes) => probes,
            None => return,
        };
    }
}

/// Binds `family` again, retrying every [`SOCKET_REBIND_DELAY`] until it
/// succeeds or `cancel` fires.
async fn rebind(family: &UdpFamily, cancel: &CancellationToken) -> Option<ProbeReceiver> {
    loop {
        match BoundSocket::bind(family.bind, family.server.clone()) {
            Ok((bound, probes)) => {
                tracing::info!(bind = %family.bind, "carrier: UDP socket rebound");
                tracing::debug!(local = %bound.local_addr, "carrier: rebound UDP socket address");
                *family.bound.lock() = Some(bound);
                return Some(probes);
            }
            Err(error) => {
                tracing::warn!(%error, retry_in = ?SOCKET_REBIND_DELAY, "carrier: UDP rebind failed");
                tokio::select! {
                    () = tokio::time::sleep(SOCKET_REBIND_DELAY) => {}
                    () = cancel.cancelled() => return None,
                }
            }
        }
    }
}

/// Resolves when the socket's receive errors have persisted.
async fn persistent_recv_errors(socket: &DemuxSocket) {
    let mut poll = tokio::time::interval(SOCKET_HEALTH_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        poll.tick().await;
        if is_persistent(socket.consecutive_recv_errors()) {
            return;
        }
    }
}

fn is_persistent(consecutive_recv_errors: u32) -> bool {
    consecutive_recv_errors >= REBIND_AFTER_RECV_ERRORS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_streak_of_receive_errors_is_persistent() {
        assert!(!is_persistent(0));
        assert!(!is_persistent(REBIND_AFTER_RECV_ERRORS - 1));
        assert!(is_persistent(REBIND_AFTER_RECV_ERRORS));
    }
}
