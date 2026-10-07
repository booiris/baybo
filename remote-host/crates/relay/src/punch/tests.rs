use std::net::{IpAddr, Ipv4Addr};

use remote_host_protocol::relay::{AddressPolicy, MAX_SEALED_CANDIDATES_BYTES};

use super::*;

const RENDEZVOUS: &str = "rendezvous.example.com:7777";
const TOKEN: u64 = 7;
const NODE: &str = "node-1";

fn registry() -> Arc<PunchRegistry> {
    Arc::new(PunchRegistry::default())
}

fn source(last_octet: u8) -> Option<SourceKey> {
    Some(AddressPolicy::source_key(IpAddr::V4(Ipv4Addr::new(
        203, 0, 113, last_octet,
    ))))
}

fn request(node: &str, source: Option<SourceKey>, rendezvous: bool) -> PunchRequest<'_> {
    PunchRequest {
        relay_node_id: node,
        remote_api_key: "inst-A",
        control_token: TOKEN,
        source,
        rendezvous: rendezvous.then_some(RENDEZVOUS),
        answer_timeout: DIRECT_ANSWER_TIMEOUT,
    }
}

fn sealed() -> SealedCandidates {
    SealedCandidates {
        n: "nonce".into(),
        enc: "ciphertext".into(),
    }
}

fn mapping(last_octet: u8, port: u16) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::new(198, 51, 100, last_octet), port)
}

fn refused(registry: &Arc<PunchRegistry>, request: PunchRequest<'_>) -> PunchRefusal {
    match registry.admit(request) {
        Ok(_) => panic!("expected the offer to be refused"),
        Err(refusal) => refusal,
    }
}

/// Admits an offer and ends its POST at once with no answer, which frees its
/// in-flight slot but keeps its budget spent.
fn spend(registry: &Arc<PunchRegistry>, request: PunchRequest<'_>) {
    let admitted = registry.admit(request).expect("offer admitted");
    drop(admitted.pending);
}

struct Paired {
    punch_id: PunchId,
    gateway: RendezvousKey,
    device: RendezvousKey,
}

/// A role's `Register` for `punch_id`, tagged under `key`, as C receives it
/// from `source`.
fn register(
    registry: &PunchRegistry,
    punch_id: PunchId,
    role: PunchRole,
    key: &RendezvousKey,
    source: SocketAddrV4,
) -> Option<ProbeDatagram> {
    registry.register_datagram(&key.register(punch_id, role).unwrap(), source)
}

/// A punch whose POST ended `200` with a rendezvous, so it stays for the
/// roles' `Register`s.
async fn answered(registry: &Arc<PunchRegistry>, node: &str) -> Paired {
    let AdmittedPunch { pending, register } = registry
        .admit(request(node, None, true))
        .expect("offer admitted");
    let punch_id = pending.punch_id();
    assert_eq!(
        registry.resolve(
            node,
            TOKEN,
            ControlReport::DirectAnswer {
                punch_id,
                answer: sealed(),
            }
        ),
        ReportOutcome::Delivered
    );
    let OfferOutcome::Answered(response) = pending.outcome().await else {
        panic!("expected an answer");
    };
    assert_eq!(response.punch_id, punch_id);
    assert_eq!(response.answer, sealed());
    Paired {
        punch_id,
        gateway: register.expect("gateway rendezvous").key,
        device: response.rendezvous.expect("device rendezvous").key,
    }
}

#[tokio::test(start_paused = true)]
async fn the_per_source_budget_refuses_a_seventh_offer_until_its_window_slides() {
    let registry = registry();
    for _ in 0..DIRECT_OFFERS_PER_SOURCE_PER_MINUTE {
        spend(&registry, request(NODE, source(1), false));
    }
    assert_eq!(
        refused(&registry, request(NODE, source(1), false)),
        PunchRefusal::Limited {
            limit: OfferLimit::SourceRate,
            retry_after: OFFER_BUDGET_WINDOW,
        }
    );
    spend(&registry, request(NODE, source(2), false));
    spend(&registry, request("node-2", source(1), false));

    tokio::time::advance(OFFER_BUDGET_WINDOW / 2).await;
    assert_eq!(
        refused(&registry, request(NODE, source(1), false)),
        PunchRefusal::Limited {
            limit: OfferLimit::SourceRate,
            retry_after: OFFER_BUDGET_WINDOW / 2,
        }
    );
    tokio::time::advance(OFFER_BUDGET_WINDOW / 2).await;
    spend(&registry, request(NODE, source(1), false));
}

#[tokio::test(start_paused = true)]
async fn the_per_node_ceiling_holds_across_sources() {
    let registry = registry();
    let sources_to_fill = DIRECT_OFFERS_PER_NODE_PER_MINUTE / DIRECT_OFFERS_PER_SOURCE_PER_MINUTE;
    for octet in 1..=sources_to_fill {
        let octet = u8::try_from(octet).unwrap();
        for _ in 0..DIRECT_OFFERS_PER_SOURCE_PER_MINUTE {
            spend(&registry, request(NODE, source(octet), false));
        }
    }
    assert_eq!(
        refused(&registry, request(NODE, source(200), false)),
        PunchRefusal::Limited {
            limit: OfferLimit::NodeRate,
            retry_after: OFFER_BUDGET_WINDOW,
        }
    );
    spend(&registry, request("node-2", source(200), false));
}

#[tokio::test(start_paused = true)]
async fn a_refused_offer_consumes_no_budget() {
    let registry = registry();
    let held: Vec<_> = (0..MAX_INFLIGHT_PUNCHES_PER_NODE)
        .map(|_| registry.admit(request(NODE, source(1), false)).unwrap())
        .collect();
    for _ in 0..DIRECT_OFFERS_PER_SOURCE_PER_MINUTE {
        assert!(matches!(
            refused(&registry, request(NODE, source(1), false)),
            PunchRefusal::Limited {
                limit: OfferLimit::InFlight,
                ..
            }
        ));
    }
    drop(held);
    for _ in MAX_INFLIGHT_PUNCHES_PER_NODE..DIRECT_OFFERS_PER_SOURCE_PER_MINUTE {
        spend(&registry, request(NODE, source(1), false));
    }
    assert!(matches!(
        refused(&registry, request(NODE, source(1), false)),
        PunchRefusal::Limited {
            limit: OfferLimit::SourceRate,
            ..
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn when_several_limits_refuse_the_wait_is_the_longest() {
    let registry = registry();
    for _ in MAX_INFLIGHT_PUNCHES_PER_NODE..DIRECT_OFFERS_PER_SOURCE_PER_MINUTE {
        spend(&registry, request(NODE, source(1), false));
    }
    let source_room_after = PUNCH_TTL / 2;
    tokio::time::advance(OFFER_BUDGET_WINDOW - source_room_after).await;
    let _held: Vec<_> = (0..MAX_INFLIGHT_PUNCHES_PER_NODE)
        .map(|_| registry.admit(request(NODE, source(1), false)).unwrap())
        .collect();
    assert_eq!(
        refused(&registry, request(NODE, source(1), false)),
        PunchRefusal::Limited {
            limit: OfferLimit::SourceRate,
            retry_after: PUNCH_TTL,
        },
        "the source window has room sooner than the in-flight cap"
    );
    tokio::time::advance(source_room_after).await;
    assert!(matches!(
        refused(&registry, request(NODE, source(1), false)),
        PunchRefusal::Limited {
            limit: OfferLimit::InFlight,
            ..
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn a_declined_or_unanswered_offer_frees_its_slot_at_once() {
    let registry = registry();
    let first = registry.admit(request(NODE, None, true)).unwrap().pending;
    let second = registry.admit(request(NODE, None, true)).unwrap().pending;
    assert_eq!(
        refused(&registry, request(NODE, None, true)),
        PunchRefusal::Limited {
            limit: OfferLimit::InFlight,
            retry_after: PUNCH_TTL,
        }
    );

    let declined = first.punch_id();
    assert_eq!(
        registry.resolve(
            NODE,
            TOKEN,
            ControlReport::DirectDeclined { punch_id: declined }
        ),
        ReportOutcome::Delivered
    );
    assert!(matches!(first.outcome().await, OfferOutcome::Declined));
    assert!(!registry.contains(declined));

    let unanswered = second.punch_id();
    assert!(matches!(second.outcome().await, OfferOutcome::NoAnswer));
    assert!(!registry.contains(unanswered));
    assert_eq!(registry.len(), 0);
}

#[tokio::test(start_paused = true)]
async fn an_answer_without_a_rendezvous_frees_its_slot_at_once() {
    let registry = registry();
    let pending = registry.admit(request(NODE, None, false)).unwrap().pending;
    let punch_id = pending.punch_id();
    registry.resolve(
        NODE,
        TOKEN,
        ControlReport::DirectAnswer {
            punch_id,
            answer: sealed(),
        },
    );
    let OfferOutcome::Answered(response) = pending.outcome().await else {
        panic!("expected an answer");
    };
    assert_eq!(response.rendezvous, None);
    assert_eq!(registry.len(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_rendezvous_punch_stays_in_flight_until_both_roles_have_their_peer() {
    let registry = registry();
    let first = answered(&registry, NODE).await;
    let waited = Duration::from_secs(1);
    tokio::time::advance(waited).await;
    let _second = answered(&registry, NODE).await;
    assert_eq!(
        refused(&registry, request(NODE, None, true)),
        PunchRefusal::Limited {
            limit: OfferLimit::InFlight,
            retry_after: REGISTRATION_GRACE - waited,
        },
        "the wait is until the oldest in-flight punch expires"
    );

    let gateway = mapping(1, 4000);
    let device = mapping(2, 5000);
    register(
        &registry,
        first.punch_id,
        PunchRole::Gateway,
        &first.gateway,
        gateway,
    );
    register(
        &registry,
        first.punch_id,
        PunchRole::Device,
        &first.device,
        device,
    );
    assert!(
        registry.admit(request(NODE, None, true)).is_err(),
        "the gateway has not had its Peer yet"
    );
    assert_eq!(
        register(
            &registry,
            first.punch_id,
            PunchRole::Gateway,
            &first.gateway,
            gateway
        ),
        first.gateway.peer(first.punch_id, device)
    );
    let _third = registry
        .admit(request(NODE, None, true))
        .expect("a punch paired for both roles frees its slot");
    assert_eq!(
        register(
            &registry,
            first.punch_id,
            PunchRole::Device,
            &first.device,
            device
        ),
        first.device.peer(first.punch_id, gateway),
        "a re-sent Register is still answered after pairing"
    );
}

#[tokio::test(start_paused = true)]
async fn an_answered_punch_that_p_never_registers_for_expires_after_the_grace() {
    let registry = registry();
    let paired = answered(&registry, NODE).await;
    let gateway = mapping(1, 4000);
    tokio::time::advance(REGISTRATION_GRACE - Duration::from_millis(1)).await;
    assert_eq!(
        register(
            &registry,
            paired.punch_id,
            PunchRole::Gateway,
            &paired.gateway,
            gateway
        ),
        paired.gateway.registered(paired.punch_id),
        "the gateway's own Register does not extend the grace"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(
        register(
            &registry,
            paired.punch_id,
            PunchRole::Gateway,
            &paired.gateway,
            gateway
        ),
        None,
        "an expired punch answers nothing before any sweep"
    );
    assert_eq!(registry.len(), 1);
    registry.sweep();
    assert_eq!(registry.len(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_paired_punch_lasts_peer_wait_after_pairing_and_never_past_its_ttl() {
    let registry = registry();
    let paired = answered(&registry, NODE).await;
    let gateway = mapping(1, 4000);
    let device = mapping(2, 5000);
    register(
        &registry,
        paired.punch_id,
        PunchRole::Device,
        &paired.device,
        device,
    );
    tokio::time::advance(REGISTRATION_GRACE * 2).await;
    assert_eq!(
        register(
            &registry,
            paired.punch_id,
            PunchRole::Gateway,
            &paired.gateway,
            gateway
        ),
        paired.gateway.peer(paired.punch_id, device),
        "P registered within the grace, so the punch waits for the gateway"
    );
    tokio::time::advance(PEER_WAIT - Duration::from_millis(1)).await;
    assert!(
        register(
            &registry,
            paired.punch_id,
            PunchRole::Device,
            &paired.device,
            device
        )
        .is_some()
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(
        register(
            &registry,
            paired.punch_id,
            PunchRole::Device,
            &paired.device,
            device
        ),
        None
    );

    let late = answered(&registry, NODE).await;
    register(
        &registry,
        late.punch_id,
        PunchRole::Device,
        &late.device,
        device,
    );
    tokio::time::advance(PUNCH_TTL).await;
    assert_eq!(
        register(
            &registry,
            late.punch_id,
            PunchRole::Gateway,
            &late.gateway,
            gateway
        ),
        None,
        "an unpaired punch P registered for still ends at its TTL"
    );
}

#[tokio::test(start_paused = true)]
async fn the_pending_cap_answers_with_the_oldest_expiry() {
    let registry = registry();
    let nodes = MAX_PENDING_PUNCHES / MAX_INFLIGHT_PUNCHES_PER_NODE;
    let mut held = Vec::with_capacity(MAX_PENDING_PUNCHES);
    for node in 0..nodes {
        let node = format!("node-{node}");
        for _ in 0..MAX_INFLIGHT_PUNCHES_PER_NODE {
            held.push(registry.admit(request(&node, None, false)).unwrap());
        }
        if node == "node-0" {
            tokio::time::advance(Duration::from_secs(3)).await;
        }
    }
    assert_eq!(registry.len(), MAX_PENDING_PUNCHES);
    assert_eq!(
        refused(&registry, request("one-more", None, false)),
        PunchRefusal::AtCapacity {
            retry_after: PUNCH_TTL - Duration::from_secs(3),
        }
    );
}

#[tokio::test(start_paused = true)]
async fn one_client_holds_at_most_its_share_of_the_pending_punches() {
    let registry = registry();
    let nodes = MAX_PENDING_PUNCHES_PER_CLIENT / MAX_INFLIGHT_PUNCHES_PER_NODE;
    let mut held = Vec::with_capacity(MAX_PENDING_PUNCHES_PER_CLIENT);
    for node in 0..nodes {
        let node = format!("node-{node}");
        for _ in 0..MAX_INFLIGHT_PUNCHES_PER_NODE {
            held.push(registry.admit(request(&node, source(1), false)).unwrap());
        }
    }
    assert_eq!(
        refused(&registry, request("one-more", source(1), false)),
        PunchRefusal::Limited {
            limit: OfferLimit::ClientPending,
            retry_after: PUNCH_TTL,
        }
    );
    held.push(
        registry
            .admit(request("one-more", source(2), false))
            .expect("another client keeps its own share"),
    );
}

#[tokio::test(start_paused = true)]
async fn a_role_key_registers_only_its_own_role() {
    let registry = registry();
    let paired = answered(&registry, NODE).await;
    let source = mapping(1, 4000);
    assert_eq!(
        register(
            &registry,
            paired.punch_id,
            PunchRole::Gateway,
            &paired.device,
            source
        ),
        None,
        "the device's key cannot register the gateway's role"
    );
    assert_eq!(
        register(
            &registry,
            paired.punch_id,
            PunchRole::Device,
            &paired.gateway,
            source
        ),
        None,
        "the gateway's key cannot register the device's role"
    );
    let reply = register(
        &registry,
        paired.punch_id,
        PunchRole::Device,
        &paired.device,
        source,
    )
    .expect("the device's own key registers it");
    assert!(
        paired.device.verifies(&reply),
        "C tags its reply under the role's key"
    );
    assert!(!paired.gateway.verifies(&reply));
}

#[tokio::test(start_paused = true)]
async fn only_a_live_rendezvous_punch_answers() {
    let registry = registry();
    let key = RendezvousKey::generate();
    assert_eq!(
        register(
            &registry,
            PunchId::generate(),
            PunchRole::Gateway,
            &key,
            mapping(1, 4000)
        ),
        None
    );
    let host_only = registry.admit(request(NODE, None, false)).unwrap();
    assert_eq!(host_only.register, None);
    assert_eq!(
        register(
            &registry,
            host_only.pending.punch_id(),
            PunchRole::Gateway,
            &key,
            mapping(1, 4000)
        ),
        None,
        "a punch without a rendezvous has no keys to verify under"
    );
}

#[tokio::test(start_paused = true)]
async fn each_role_latches_its_first_source() {
    let registry = registry();
    let paired = answered(&registry, NODE).await;
    let gateway = mapping(1, 4000);
    let device = mapping(2, 5000);
    let registered = paired.gateway.registered(paired.punch_id);
    assert_eq!(
        register(
            &registry,
            paired.punch_id,
            PunchRole::Gateway,
            &paired.gateway,
            gateway
        ),
        registered
    );
    assert_eq!(
        register(
            &registry,
            paired.punch_id,
            PunchRole::Gateway,
            &paired.gateway,
            mapping(1, 4001)
        ),
        None,
        "a Register for an observed role from a new source is dropped"
    );
    assert_eq!(
        register(
            &registry,
            paired.punch_id,
            PunchRole::Gateway,
            &paired.gateway,
            gateway
        ),
        registered
    );
    assert_eq!(
        register(
            &registry,
            paired.punch_id,
            PunchRole::Device,
            &paired.device,
            device
        ),
        paired.device.peer(paired.punch_id, gateway)
    );
    assert_eq!(
        register(
            &registry,
            paired.punch_id,
            PunchRole::Gateway,
            &paired.gateway,
            gateway
        ),
        paired.gateway.peer(paired.punch_id, device)
    );
}

#[tokio::test(start_paused = true)]
async fn only_verified_registers_from_the_latched_source_spend_the_datagram_budget() {
    let registry = registry();
    let paired = answered(&registry, NODE).await;
    let source = mapping(1, 4000);
    let forged = RendezvousKey::generate();
    for _ in 0..MAX_DATAGRAMS_PER_PUNCH {
        assert_eq!(
            register(
                &registry,
                paired.punch_id,
                PunchRole::Gateway,
                &forged,
                source
            ),
            None
        );
    }
    let first = register(
        &registry,
        paired.punch_id,
        PunchRole::Gateway,
        &paired.gateway,
        source,
    );
    assert!(first.is_some(), "a forged tag spent nothing");
    for _ in 0..MAX_DATAGRAMS_PER_PUNCH {
        assert_eq!(
            register(
                &registry,
                paired.punch_id,
                PunchRole::Gateway,
                &paired.gateway,
                mapping(9, 9000)
            ),
            None,
            "a copy replayed from another source is dropped"
        );
    }
    let answered_count = 1
        + (1..MAX_DATAGRAMS_PER_PUNCH)
            .filter_map(|_| {
                register(
                    &registry,
                    paired.punch_id,
                    PunchRole::Gateway,
                    &paired.gateway,
                    source,
                )
            })
            .count();
    assert_eq!(
        answered_count,
        usize::try_from(MAX_DATAGRAMS_PER_PUNCH).unwrap(),
        "the latched source keeps its whole budget"
    );
    assert_eq!(
        register(
            &registry,
            paired.punch_id,
            PunchRole::Gateway,
            &paired.gateway,
            source
        ),
        None,
        "and no more"
    );
}

#[tokio::test(start_paused = true)]
async fn an_oversized_answer_is_not_forwarded() {
    let registry = registry();
    let pending = registry.admit(request(NODE, None, false)).unwrap().pending;
    let punch_id = pending.punch_id();
    assert_eq!(
        registry.resolve(
            NODE,
            TOKEN,
            ControlReport::DirectAnswer {
                punch_id,
                answer: SealedCandidates {
                    n: String::new(),
                    enc: "x".repeat(MAX_SEALED_CANDIDATES_BYTES + 1),
                },
            }
        ),
        ReportOutcome::OversizedAnswer
    );
    assert!(matches!(pending.outcome().await, OfferOutcome::NoAnswer));
}
