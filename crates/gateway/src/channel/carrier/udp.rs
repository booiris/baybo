//! One address family's stable UDP socket: bound when the runtime starts,
//! served for the runtime's life, and bound again when its first bind fails
//! or when receive errors persist. A receive error backs off inside the
//! socket without reaching quinn, so a transient one never costs the family
//! its connections.

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
    /// Binds the family's socket. A failed bind leaves the family unbound,
    /// and [`supervise`] binds it again after the rebind delay.
    pub(crate) fn bind(
        bind: SocketAddr,
        server: quinn::ServerConfig,
    ) -> (Arc<Self>, Option<ProbeReceiver>) {
        let (bound, probes) = match BoundSocket::bind(bind, server.clone()) {
            Ok((bound, probes)) => (Some(bound), Some(probes)),
            Err(error) => {
                tracing::warn!(
                    bind = %bind,
                    %error,
                    "carrier: UDP socket unavailable; retrying"
                );
                (None, None)
            }
        };
        let family = Self {
            bind,
            server,
            bound: Mutex::new(bound),
        };
        (Arc::new(family), probes)
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
/// and ends when it fires. Returns only after they have all ended. An
/// unbound family (`probes` is `None`) is bound again after the rebind
/// delay; a persistent receive error closes the endpoint and binds the
/// family again at once.
pub(crate) async fn supervise(
    family: Arc<UdpFamily>,
    probes: Option<ProbeReceiver>,
    context: Arc<RuntimeContext>,
    cancel: CancellationToken,
) {
    let delay = context.timing.rebind_delay;
    let mut probes = match probes {
        Some(probes) => probes,
        None => match rebind(&family, delay, delay, &cancel).await {
            Some(probes) => probes,
            None => return,
        },
    };
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
        probes = match rebind(&family, Duration::ZERO, delay, &cancel).await {
            Some(probes) => probes,
            None => return,
        };
    }
}

/// Binds `family` again once `first_wait` has passed, then every `delay`
/// until it succeeds or `cancel` fires.
async fn rebind(
    family: &UdpFamily,
    first_wait: Duration,
    delay: Duration,
    cancel: &CancellationToken,
) -> Option<ProbeReceiver> {
    let mut wait = first_wait;
    loop {
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = cancel.cancelled() => return None,
        }
        match BoundSocket::bind(family.bind, family.server.clone()) {
            Ok((bound, probes)) => {
                tracing::info!(bind = %family.bind, "carrier: UDP socket bound");
                tracing::debug!(local = %bound.local_addr, "carrier: bound UDP socket address");
                *family.bound.lock() = Some(bound);
                return Some(probes);
            }
            Err(error) => {
                tracing::debug!(bind = %family.bind, %error, retry_in = ?delay, "carrier: UDP rebind failed; retrying");
                wait = delay;
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
