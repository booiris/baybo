//! Direct-first, relay-fallback connection policy.
//!
//! After pairing the app holds A's direct-reachability candidates plus the
//! C-assigned `relay_node_id` ([`crate::PairedGateway`]). On open it tries each
//! **direct** endpoint first — the common "phone + server on the same network"
//! case stays off the operator relay entirely — and falls back to the **relay**
//! (through C, blind) only when every direct attempt fails.
//!
//! This module is the transport-agnostic *policy* + ordering; the actual byte
//! transport (a WebSocket, direct or relayed) is supplied by the Tauri shell as
//! the `attempt` closure, so the fallback logic stays host-testable here.

use std::future::Future;

use crate::pairing::PairedGateway;

/// One endpoint the app can try to reach A at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A direct-reachability candidate (LAN / Tailscale / reverse proxy).
    Direct(String),
    /// Via C's blind relay, keyed by the C-assigned `relay_node_id`.
    Relay { node_id: String },
}

/// The ordered list of endpoints to try: every direct candidate first, then the
/// relay (only if A advertised a `relay_node_id`).
pub fn connection_plan(paired: &PairedGateway) -> Vec<Endpoint> {
    let mut plan: Vec<Endpoint> = paired
        .direct_candidates
        .iter()
        .cloned()
        .map(Endpoint::Direct)
        .collect();
    if !paired.relay_node_id.is_empty() {
        plan.push(Endpoint::Relay {
            node_id: paired.relay_node_id.clone(),
        });
    }
    plan
}

/// Why a connection attempt over the whole plan failed.
#[derive(Debug, PartialEq, Eq)]
pub enum ConnectError<E> {
    /// The plan was empty (no direct candidates and no relay).
    NoEndpoints,
    /// Every endpoint was tried and failed; carries the last error.
    AllFailed(E),
}

/// Try `attempt` against each endpoint in `plan` in order, returning the first
/// success — direct candidates before the relay. Each attempt receives an
/// **owned** [`Endpoint`] (cheap to clone) so its future can hold it across the
/// connect await. The shell passes a real WS connector; the policy + ordering
/// live here.
pub async fn connect_first<T, E, F, Fut>(
    plan: &[Endpoint],
    mut attempt: F,
) -> Result<T, ConnectError<E>>
where
    F: FnMut(Endpoint) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut last_err = None;
    for endpoint in plan {
        match attempt(endpoint.clone()).await {
            Ok(transport) => return Ok(transport),
            Err(e) => last_err = Some(e),
        }
    }
    match last_err {
        Some(e) => Err(ConnectError::AllFailed(e)),
        None => Err(ConnectError::NoEndpoints),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use device_proto::aead::KEY_LEN;

    fn paired(direct: &[&str], relay: &str) -> PairedGateway {
        PairedGateway {
            user_id: "u".into(),
            auth_token: "t".into(),
            gateway_static_pubkey: [0u8; KEY_LEN],
            push_key: [0u8; KEY_LEN],
            relay_node_id: relay.into(),
            direct_candidates: direct.iter().map(|s| s.to_string()).collect(),
            pairing_code: "c".into(),
        }
    }

    #[test]
    fn plan_tries_each_direct_then_relay() {
        let plan = connection_plan(&paired(&["wss://baybo.lan", "wss://baybo.ts"], "node-1"));
        assert_eq!(
            plan,
            vec![
                Endpoint::Direct("wss://baybo.lan".into()),
                Endpoint::Direct("wss://baybo.ts".into()),
                Endpoint::Relay { node_id: "node-1".into() },
            ],
        );
    }

    #[test]
    fn plan_omits_relay_when_unassigned() {
        let plan = connection_plan(&paired(&["wss://baybo.lan"], ""));
        assert_eq!(plan, vec![Endpoint::Direct("wss://baybo.lan".into())]);
    }

    #[tokio::test]
    async fn prefers_direct_when_it_succeeds() {
        let plan = connection_plan(&paired(&["wss://direct"], "node-1"));
        let got: Result<&str, ConnectError<()>> = connect_first(&plan, |ep| async move {
            match ep {
                Endpoint::Direct(_) => Ok("via-direct"),
                Endpoint::Relay { .. } => Ok("via-relay"),
            }
        })
        .await;
        assert_eq!(got.unwrap(), "via-direct");
    }

    #[tokio::test]
    async fn falls_back_to_relay_when_direct_fails() {
        let plan = connection_plan(&paired(&["wss://direct"], "node-1"));
        let got: Result<&str, ConnectError<&str>> = connect_first(&plan, |ep| async move {
            match ep {
                Endpoint::Direct(_) => Err("unreachable"),
                Endpoint::Relay { .. } => Ok("via-relay"),
            }
        })
        .await;
        assert_eq!(got.unwrap(), "via-relay");
    }

    #[tokio::test]
    async fn all_failed_carries_last_error() {
        let plan = connection_plan(&paired(&["a", "b"], "n"));
        let got: Result<&str, ConnectError<i32>> = connect_first(&plan, |ep| {
            let code = match &ep {
                Endpoint::Direct(d) if d.as_str() == "a" => 1,
                Endpoint::Direct(_) => 2,
                Endpoint::Relay { .. } => 3,
            };
            async move { Err(code) }
        })
        .await;
        assert_eq!(got, Err(ConnectError::AllFailed(3)));
    }

    #[tokio::test]
    async fn empty_plan_is_no_endpoints() {
        let got: Result<&str, ConnectError<()>> = connect_first(&[], |_| async { Ok("x") }).await;
        assert_eq!(got, Err(ConnectError::NoEndpoints));
    }
}
