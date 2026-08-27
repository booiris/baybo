//! Relay-mode gateway API client over API tunnel legs.
//!
//! A call takes a warm leg from [`super::leg_pool`] if there is one, and dials its
//! own otherwise — it never waits for one. See the pool's module docs for why
//! wait-free is the only shape that is never worse than dialing every time.

use std::time::Duration;

use device_proto::noise::StaticKeypair;
use serde::de::DeserializeOwned;

use super::leg_pool::{BindingKey, PooledLeg, pool};
use super::pairing::load_paired_record;
use super::tunnel::{
    LegError, LegIo, TunnelFrames, TunnelReuse, body_frames, content_length_header,
    declared_body_len, dial_tunnel_leg,
};
use crate::core::{TunnelHeader, TunnelRequest};
use crate::gateway_api::{GatewayJsonClient, MEDIA_TYPE_JSON};

const HEADER_CONTENT_TYPE: &str = "content-type";

/// Ceiling on one request/response exchange, once the leg is up. There was no
/// per-request budget at all before: a leg that went quiet mid-response simply
/// hung, forever, and took its caller with it.
const TUNNEL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a POOLED leg gets to answer before we give up on it.
///
/// A parked leg can be a zombie — a NAT dropped its flow, or the process was
/// suspended and the socket went with it — and there is no signal for that but
/// silence, so this bounds what a zombie costs instead of letting it run to the
/// full request budget.
///
/// **It cannot tell a dead leg from a slow gateway.** The gateway sends the
/// response head only AFTER the router has fully served the request, so silence
/// here measures the gateway's SERVICE TIME as much as the leg's liveness. That
/// is why this is not tighter: at 4s, a `/sync?limit=200` that takes five seconds
/// on a loaded self-hosted gateway would be abandoned and replayed on a fresh leg
/// — making the gateway run that same expensive scan twice, precisely when it is
/// least able to. (The abandoned execution keeps running: the tunnel has no
/// cancellation, so its task is blocked in `oneshot` and only discovers the dead
/// leg when it tries to write.)
///
/// 15s sits above the p99 of every pooled route (they are all bounded sqlite
/// reads and writes) while still capping a zombie well under the 30s request
/// budget. The dual-clock staleness check and the `.background` barrier are what
/// actually keep zombies rare; this is only the backstop.
const POOLED_LEG_FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) struct GatewayApi;

#[allow(clippy::manual_async_fn)]
impl GatewayJsonClient for GatewayApi {
    fn get_json<'a, T>(
        &'a self,
        path: &'a str,
    ) -> impl std::future::Future<Output = Result<T, String>> + Send + 'a
    where
        T: DeserializeOwned + Send + 'static,
    {
        async move {
            let body = request("GET", path, Vec::new(), None).await?;
            serde_json::from_slice(&body).map_err(|e| format!("decode response: {e}"))
        }
    }

    fn post_json<'a, T>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<T, String>> + Send + 'a
    where
        T: DeserializeOwned + Send + 'static,
    {
        async move {
            let body = request(
                "POST",
                path,
                vec![TunnelHeader::new(HEADER_CONTENT_TYPE, MEDIA_TYPE_JSON)],
                Some(body),
            )
            .await?;
            serde_json::from_slice(&body).map_err(|e| format!("decode response: {e}"))
        }
    }

    fn post_json_once<'a, T>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<T, String>> + Send + 'a
    where
        T: DeserializeOwned + Send + 'static,
    {
        async move {
            let body = request_with_policy(
                "POST",
                path,
                vec![TunnelHeader::new(HEADER_CONTENT_TYPE, MEDIA_TYPE_JSON)],
                Some(body),
                ReplayPolicy::Never,
            )
            .await?;
            serde_json::from_slice(&body).map_err(|e| format!("decode response: {e}"))
        }
    }

    fn patch_json<'a, T>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<T, String>> + Send + 'a
    where
        T: DeserializeOwned + Send + 'static,
    {
        async move {
            let body = request(
                "PATCH",
                path,
                vec![TunnelHeader::new(HEADER_CONTENT_TYPE, MEDIA_TYPE_JSON)],
                Some(body),
            )
            .await?;
            serde_json::from_slice(&body).map_err(|e| format!("decode response: {e}"))
        }
    }

    fn post_empty<'a>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send + 'a {
        async move {
            request(
                "POST",
                path,
                vec![TunnelHeader::new(HEADER_CONTENT_TYPE, MEDIA_TYPE_JSON)],
                Some(body),
            )
            .await?;
            Ok(())
        }
    }

    fn put_empty<'a>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send + 'a {
        async move {
            request(
                "PUT",
                path,
                vec![TunnelHeader::new(HEADER_CONTENT_TYPE, MEDIA_TYPE_JSON)],
                Some(body),
            )
            .await?;
            Ok(())
        }
    }

    fn delete_empty<'a>(
        &'a self,
        path: &'a str,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send + 'a {
        async move {
            request("DELETE", path, Vec::new(), None).await?;
            Ok(())
        }
    }

    fn post_raw<'a>(
        &'a self,
        path: &'a str,
        body: Vec<u8>,
        retryable: bool,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, String>> + Send + 'a {
        async move {
            // A deck op runs agent-written card code — not the client-keyed,
            // replay-convergent surface `should_retry` reasons about — so a
            // silent pooled leg is replayed only when the op's mandatory
            // `x-baybo-retryable` declaration says replay is harmless;
            // otherwise the caller sees the failure and decides.
            let policy = if retryable {
                ReplayPolicy::Converges
            } else {
                ReplayPolicy::Never
            };
            request_with_policy(
                "POST",
                path,
                vec![TunnelHeader::new(HEADER_CONTENT_TYPE, MEDIA_TYPE_JSON)],
                Some(body),
                policy,
            )
            .await
        }
    }
}

/// What one request/response did to the leg it ran on.
pub(crate) struct Exchange {
    pub(crate) result: Result<Vec<u8>, LegError>,
    /// The gateway's offer to hold the leg open. `Some` only when the leg is still
    /// whole AND the gateway offered — so it doubles as "may this leg be parked?".
    pub(crate) reuse: Option<TunnelReuse>,
    /// Whether any byte of the response arrived.
    ///
    /// This is the ONLY thing a retry may be reasoned about, and it is weaker than
    /// it looks: the gateway runs the router FIRST and answers second, so a failure
    /// before the head does NOT mean the request had no effect. See `should_retry`.
    pub(crate) response_seen: bool,
}

/// Drive one request/response over a leg, leaving it DRAINED.
///
/// A non-2xx carries a body too, and abandoning it on the wire would hand those
/// bytes to whoever writes the next head. `chat_lookup_message`'s 404 is the
/// ordinary result of an outbox rebase, not an edge case — this is the common
/// path, not the sad one.
pub(crate) async fn exchange<F: TunnelFrames>(
    leg: &mut LegIo<F>,
    method: &str,
    path: &str,
    headers: Vec<TunnelHeader>,
    body: Option<&[u8]>,
    first_byte_timeout: Duration,
) -> Exchange {
    let outcome = tokio::time::timeout(
        TUNNEL_REQUEST_TIMEOUT,
        run_exchange(leg, method, path, headers, body, first_byte_timeout),
    )
    .await;
    // Asked of the leg, not tracked alongside it: an `Error` frame and a desync are
    // both failures that produce no head, yet both mean the gateway ANSWERED. Only
    // silence — a timeout, or a socket that died before a word — licenses a replay.
    let response_seen = leg.saw_response();

    match outcome {
        Ok(Ok((body, reuse))) => Exchange {
            result: Ok(body),
            reuse,
            response_seen,
        },
        // The gateway answered "no". The leg is drained and intact, so it may still
        // be parked.
        Ok(Err((LegError::Http { status }, reuse))) => Exchange {
            result: Err(LegError::Http { status }),
            reuse,
            response_seen,
        },
        Ok(Err((e, _))) => Exchange {
            result: Err(e),
            reuse: None,
            response_seen,
        },
        // A leg that went quiet is not one we can reason about: whatever it still
        // owes us would arrive tagged with an id nobody is expecting.
        Err(_) => Exchange {
            result: Err(LegError::dead("tunnel request timed out")),
            reuse: None,
            response_seen,
        },
    }
}

type ExchangeResult = Result<(Vec<u8>, Option<TunnelReuse>), (LegError, Option<TunnelReuse>)>;

async fn run_exchange<F: TunnelFrames>(
    leg: &mut LegIo<F>,
    method: &str,
    path: &str,
    mut headers: Vec<TunnelHeader>,
    body: Option<&[u8]>,
    first_byte_timeout: Duration,
) -> ExchangeResult {
    let request_id = leg.claim_request_id();
    let body_len = declared_body_len(body);
    if let Some(len) = body_len {
        headers.push(content_length_header(len));
    }

    let head = TunnelRequest::Head {
        request_id,
        method: method.into(),
        path: path.into(),
        headers,
        body_len,
    };
    leg.send(&head).await.map_err(|e| (e, None))?;
    if let Some(body) = body {
        for frame in body_frames(request_id, body) {
            leg.send(&frame).await.map_err(|e| (e, None))?;
        }
    }

    let head = tokio::time::timeout(first_byte_timeout, leg.expect_response_head(request_id))
        .await
        .map_err(|_| (LegError::dead("tunnel leg went silent"), None))?
        .map_err(|e| (e, None))?;

    // Drain first, judge second.
    let response_body = leg
        .collect_response_body(request_id, head.body_len)
        .await
        .map_err(|e| (e, None))?;
    if !(200..300).contains(&head.status) {
        return Err((
            LegError::Http {
                status: head.status,
            },
            head.reuse,
        ));
    }
    Ok((response_body, head.reuse))
}

/// Whether a request that died in pooled-leg silence may be replayed.
/// `Converges` is this surface's default bargain (see [`should_retry`]);
/// `Never` is for the routes WITHOUT the client-keyed convergence property —
/// issue creation and non-retryable deck ops.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplayPolicy {
    Converges,
    Never,
}

/// One gateway call: on a warm leg if the pool has one, on a fresh leg otherwise.
async fn request(
    method: &str,
    path: &str,
    headers: Vec<TunnelHeader>,
    body: Option<Vec<u8>>,
) -> Result<Vec<u8>, String> {
    request_with_policy(method, path, headers, body, ReplayPolicy::Converges).await
}

async fn request_with_policy(
    method: &str,
    path: &str,
    headers: Vec<TunnelHeader>,
    body: Option<Vec<u8>>,
    replay: ReplayPolicy,
) -> Result<Vec<u8>, String> {
    let record = load_paired_record()?.ok_or("not paired; pair a gateway first")?;
    let key = BindingKey::from(&record);

    if let Some(mut leg) = pool().take(&key) {
        let outcome = exchange(
            &mut leg.io,
            method,
            path,
            headers.clone(),
            body.as_deref(),
            POOLED_LEG_FIRST_BYTE_TIMEOUT,
        )
        .await;

        // A pooled leg can die between requests through nobody's fault — the
        // gateway reclaimed it, the relay flapped, a NAT dropped the flow. That is
        // the ONE failure worth retrying, and only when the route can survive being
        // run twice.
        let retry = replay == ReplayPolicy::Converges && should_retry(&outcome, leg.was_pooled);
        settle(leg, parkable(outcome.reuse, &outcome.result)).await;
        if !retry {
            return outcome.result.map_err(String::from);
        }
    }

    let local = StaticKeypair::from_parts(record.noise_public, record.noise_secret);
    // Snapshot the epoch BEFORE the dial, not after. A dial that straddles the
    // app's `.background` edge would otherwise be stamped with the epoch the
    // barrier just bumped TO, and parked — which is precisely the zombie the
    // barrier exists to prevent.
    let epoch = pool().epoch();
    let io = dial_tunnel_leg(&record, &local, remote_host_protocol::relay::LegClass::Api).await?;
    let mut leg = PooledLeg::fresh(io, key, epoch);

    // No first-byte shortcut on a fresh socket: it cannot be a zombie, and a slow
    // gateway is not a dead one. And no retry either — a fresh leg that fails means
    // the network is down, and retrying there turns one outage into a dial storm.
    let outcome = exchange(
        &mut leg.io,
        method,
        path,
        headers,
        body.as_deref(),
        TUNNEL_REQUEST_TIMEOUT,
    )
    .await;
    settle(leg, parkable(outcome.reuse, &outcome.result)).await;
    outcome.result.map_err(String::from)
}

/// Park the leg if the gateway offered to hold it, close it otherwise.
async fn settle(leg: PooledLeg, reuse: Option<TunnelReuse>) {
    if let Some(orphan) = pool().park(leg, reuse) {
        orphan.io.close().await;
    }
}

/// The offer to park on, once the response has been judged.
///
/// An auth rejection is a perfectly well-formed response — body drained, framing
/// intact — so the reuse rules would happily park it. But 401/403 means the
/// gateway's snapshot of our credentials is stale: it resolves `AuthenticatedDevice`
/// ONCE, at the leg's handshake, and a re-pair rotates the token underneath it. And
/// because parking RE-STAMPS the TTL, such a leg would keep renewing itself and
/// rejecting every caller that took it. Bound the damage to the one request.
fn parkable(reuse: Option<TunnelReuse>, result: &Result<Vec<u8>, LegError>) -> Option<TunnelReuse> {
    match result {
        Err(LegError::Http { status: 401 | 403 }) => None,
        _ => reuse,
    }
}

/// The rule that decides whether a failed call gets a second try.
///
/// A pure function because the reasoning is the fragile part, not the plumbing:
/// this is the ONE place the at-least-once bargain is struck.
///
/// **The replay is at-least-once, not exactly-once.** The gateway runs the router
/// FIRST and answers second, so a failure before the response head does NOT prove
/// the request had no effect — it may well have created the session and then died
/// on the way to saying so.
///
/// It is safe anyway, and only because of a property of THIS surface: every route
/// the pool can carry keys its effect on data the CLIENT supplied — the session id
/// we minted (`get_or_create`), the APNs token we hold (an upsert per device), an
/// absolute archive/pin flag, a max-wins read cursor, a soft hide. A replay
/// converges on the same state.
///
/// The one route that does NOT have that property is `POST /v1/blobs`: every put
/// mints a fresh `blob_id`, which is exactly why attachment dedup keys on the
/// sha256 digest instead. It cannot reach this code — uploads ride
/// `GatewayBlobClient` on a one-shot blob leg, never `GatewayJsonClient` — and
/// that structural separation is what keeps it safe. **A new route without the
/// client-keyed property must not be added to `GatewayJsonClient` without
/// revisiting this.** The one revisit so far: `post_raw` (the deck's per-card
/// op calls, which run agent-written code) opts out wholesale via
/// [`ReplayPolicy::Never`] — this rule never even runs for it.
fn should_retry(outcome: &Exchange, was_pooled: bool) -> bool {
    outcome.result.is_err()
        // A leg WE just dialed that fails means the network is down. Retrying there
        // turns one outage into a dial storm.
        && was_pooled
        // The gateway answered — an Error, a desync, anything. Past its first word
        // we cannot claim it did not run the request. Only silence licenses a replay.
        && !outcome.response_seen
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::TunnelResponse;
    use crate::relay::tunnel::testing::{ScriptedFrames, body, head, reusable_head};

    fn exchange_result(result: Result<Vec<u8>, LegError>, response_seen: bool) -> Exchange {
        Exchange {
            result,
            reuse: None,
            response_seen,
        }
    }

    /// The leg died before the gateway said anything, on a leg we did not dial. That
    /// is the reclaimed-while-parked case the pool exists to absorb, and the only
    /// one a retry can be reasoned about.
    #[test]
    fn a_pooled_leg_that_dies_before_any_response_is_retried() {
        let dead = exchange_result(Err(LegError::dead("connection reset")), false);
        assert!(should_retry(&dead, true));
    }

    #[test]
    fn a_freshly_dialed_leg_is_never_retried() {
        let dead = exchange_result(Err(LegError::dead("connection reset")), false);
        assert!(
            !should_retry(&dead, false),
            "one outage must not become a dial storm"
        );
    }

    /// Past the head, the gateway has demonstrably run the request. "No response
    /// head" is already a weak signal — the router runs BEFORE the answer is sent —
    /// and past the head it is no signal at all.
    #[test]
    fn a_failure_after_the_response_head_is_never_retried() {
        let dead = exchange_result(Err(LegError::dead("connection reset")), true);
        assert!(!should_retry(&dead, true));
    }

    /// A gateway that answered "no" is not a transport failure. The leg is drained
    /// and whole; re-running the request would just get the same 404.
    #[test]
    fn a_gateway_that_answered_is_never_retried() {
        let not_found = exchange_result(Err(LegError::Http { status: 404 }), true);
        assert!(!should_retry(&not_found, true));
    }

    #[test]
    fn success_is_never_retried() {
        let ok = exchange_result(Ok(b"[]".to_vec()), true);
        assert!(!should_retry(&ok, true));
    }

    /// The gateway's offer rides back out of the exchange, because it is what
    /// decides whether the leg is parked at all.
    #[tokio::test]
    async fn a_reusable_gateways_offer_survives_the_exchange() {
        let reuse = TunnelReuse { idle_ms: 60_000 };
        let mut leg = LegIo::new(ScriptedFrames::new(vec![
            vec![reusable_head(1, 200, 2, Some(reuse))],
            vec![body(1, b"[]")],
        ]));

        let outcome = exchange(
            &mut leg,
            "GET",
            "/v1/chat/sessions",
            vec![],
            None,
            TUNNEL_REQUEST_TIMEOUT,
        )
        .await;

        assert_eq!(outcome.result.expect("body"), b"[]");
        assert_eq!(outcome.reuse, Some(reuse));
        assert!(outcome.response_seen);
    }

    /// An old gateway offers nothing, so its leg is never parked — the client keeps
    /// dialing exactly as it always did. This is the new-app/old-gateway skew
    /// direction, and it has to cost nothing.
    #[tokio::test]
    async fn an_old_gateway_leaves_no_offer_to_park_on() {
        let mut leg = LegIo::new(ScriptedFrames::new(vec![
            vec![head(1, 200, 2)],
            vec![body(1, b"[]")],
        ]));

        let outcome = exchange(
            &mut leg,
            "GET",
            "/v1/chat/sessions",
            vec![],
            None,
            TUNNEL_REQUEST_TIMEOUT,
        )
        .await;

        assert!(outcome.result.is_ok());
        assert_eq!(outcome.reuse, None, "nothing to park on");
    }

    /// A non-2xx leaves the leg parkable — the body was drained, so the stream is
    /// clean. `chat_lookup_message`'s 404 is a hot path; it must not cost a leg.
    #[tokio::test]
    async fn a_404_still_hands_the_leg_back() {
        let reuse = TunnelReuse { idle_ms: 60_000 };
        let mut leg = LegIo::new(ScriptedFrames::new(vec![
            vec![reusable_head(1, 404, 9, Some(reuse))],
            vec![body(1, b"not found")],
        ]));

        let outcome = exchange(
            &mut leg,
            "GET",
            "/v1/chat/sessions/s/messages",
            vec![],
            None,
            TUNNEL_REQUEST_TIMEOUT,
        )
        .await;

        assert!(matches!(
            outcome.result,
            Err(LegError::Http { status: 404 })
        ));
        assert_eq!(
            outcome.reuse,
            Some(reuse),
            "the leg is drained and still good"
        );
    }

    /// An `Error` frame is a failure with no head — but it is still the gateway
    /// TALKING. Treating it as "no response" would license a replay of a request the
    /// gateway may well have run, against the design's own no-retry rule for errors.
    #[tokio::test]
    async fn an_error_frame_counts_as_a_response() {
        let mut leg = LegIo::new(ScriptedFrames::new(vec![vec![TunnelResponse::Error {
            request_id: 1,
            status: 400,
            reason: "forbidden tunnel header".into(),
        }]]));

        let outcome = exchange(
            &mut leg,
            "GET",
            "/v1/chat/sessions",
            vec![],
            None,
            TUNNEL_REQUEST_TIMEOUT,
        )
        .await;

        assert!(matches!(outcome.result, Err(LegError::Dead(_))));
        assert!(outcome.response_seen, "the gateway answered");
        assert!(
            !should_retry(&outcome, true),
            "and a deterministic rejection must not be replayed"
        );
    }

    /// Same for a desync. A frame for the wrong request is the gateway's voice too —
    /// and one we can no longer interpret, which is the worst possible moment to
    /// re-run the request.
    #[tokio::test]
    async fn a_desynced_frame_counts_as_a_response() {
        let mut leg = LegIo::new(ScriptedFrames::new(vec![vec![head(7, 200, 0)]]));

        let outcome = exchange(
            &mut leg,
            "GET",
            "/v1/chat/sessions",
            vec![],
            None,
            TUNNEL_REQUEST_TIMEOUT,
        )
        .await;

        assert!(matches!(outcome.result, Err(LegError::Dead(_))));
        assert!(outcome.response_seen);
        assert!(!should_retry(&outcome, true));
    }

    /// The one thing that DOES license a replay: silence. A leg the gateway never
    /// answered on is the reclaimed-while-parked case the pool exists to absorb.
    #[tokio::test(start_paused = true)]
    async fn only_silence_licenses_a_replay() {
        let mut leg = LegIo::new(ScriptedFrames::silent());

        let outcome = exchange(
            &mut leg,
            "GET",
            "/v1/chat/sessions",
            vec![],
            None,
            POOLED_LEG_FIRST_BYTE_TIMEOUT,
        )
        .await;

        assert!(!outcome.response_seen, "not a word came back");
        assert!(should_retry(&outcome, true));
    }

    /// A parked leg can be a zombie: the write lands, and then nothing comes back.
    /// The pooled first-byte budget is what stops that costing the full request
    /// timeout.
    #[tokio::test(start_paused = true)]
    async fn a_silent_pooled_leg_costs_only_the_first_byte_budget() {
        // A script with no messages: `recv` errors rather than hangs, so drive the
        // timeout with a leg that never answers.
        let mut leg = LegIo::new(ScriptedFrames::silent());

        let started = tokio::time::Instant::now();
        let outcome = exchange(
            &mut leg,
            "GET",
            "/v1/chat/sessions",
            vec![],
            None,
            POOLED_LEG_FIRST_BYTE_TIMEOUT,
        )
        .await;

        assert!(matches!(outcome.result, Err(LegError::Dead(_))));
        assert!(!outcome.response_seen, "so it is eligible for one retry");
        assert_eq!(outcome.reuse, None, "and the zombie is never parked");
        assert_eq!(started.elapsed(), POOLED_LEG_FIRST_BYTE_TIMEOUT);
    }
}
