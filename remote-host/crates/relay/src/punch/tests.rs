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
    gateway: RendezvousTicket,
    device: RendezvousTicket,
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
        gateway: register.expect("gateway rendezvous").ticket,
        device: response.rendezvous.expect("device rendezvous").ticket,
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
    tokio::time::advance(Duration::from_secs(5)).await;
    let _second = answered(&registry, NODE).await;
    assert_eq!(
        refused(&registry, request(NODE, None, true)),
        PunchRefusal::Limited {
            limit: OfferLimit::InFlight,
            retry_after: PUNCH_TTL - Duration::from_secs(5),
        },
        "the wait is until the oldest in-flight punch expires"
    );

    let gateway = mapping(1, 4000);
    let device = mapping(2, 5000);
    registry.register_datagram(first.punch_id, PunchRole::Gateway, &first.gateway, gateway);
    registry.register_datagram(first.punch_id, PunchRole::Device, &first.device, device);
    assert!(
        registry.admit(request(NODE, None, true)).is_err(),
        "the gateway has not had its Peer yet"
    );
    assert_eq!(
        registry.register_datagram(first.punch_id, PunchRole::Gateway, &first.gateway, gateway),
        Some(ProbeDatagram::Peer {
            punch_id: first.punch_id,
            srflx: device,
        })
    );
    let _third = registry
        .admit(request(NODE, None, true))
        .expect("a punch paired for both roles frees its slot");
    assert_eq!(
        registry.register_datagram(first.punch_id, PunchRole::Device, &first.device, device),
        Some(ProbeDatagram::Peer {
            punch_id: first.punch_id,
            srflx: gateway,
        }),
        "a re-sent Register is still answered until the TTL"
    );
}

#[tokio::test(start_paused = true)]
async fn a_punch_expires_at_its_ttl_and_the_sweep_drops_it() {
    let registry = registry();
    let paired = answered(&registry, NODE).await;
    let gateway = mapping(1, 4000);
    tokio::time::advance(PUNCH_TTL - Duration::from_millis(1)).await;
    assert_eq!(
        registry.register_datagram(
            paired.punch_id,
            PunchRole::Gateway,
            &paired.gateway,
            gateway
        ),
        Some(ProbeDatagram::Registered {
            punch_id: paired.punch_id
        })
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(
        registry.register_datagram(
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
async fn a_role_ticket_registers_only_its_own_role() {
    let registry = registry();
    let paired = answered(&registry, NODE).await;
    let source = mapping(1, 4000);
    assert_eq!(
        registry.register_datagram(paired.punch_id, PunchRole::Gateway, &paired.device, source),
        None,
        "the device's ticket cannot register the gateway's role"
    );
    assert_eq!(
        registry.register_datagram(paired.punch_id, PunchRole::Device, &paired.gateway, source),
        None,
        "the gateway's ticket cannot register the device's role"
    );
    assert_eq!(
        registry.register_datagram(paired.punch_id, PunchRole::Device, &paired.device, source),
        Some(ProbeDatagram::Registered {
            punch_id: paired.punch_id
        })
    );
}

#[tokio::test(start_paused = true)]
async fn only_a_live_rendezvous_punch_answers() {
    let registry = registry();
    let ticket = RendezvousTicket::generate();
    assert_eq!(
        registry.register_datagram(
            PunchId::generate(),
            PunchRole::Gateway,
            &ticket,
            mapping(1, 4000)
        ),
        None
    );
    let host_only = registry.admit(request(NODE, None, false)).unwrap();
    assert_eq!(host_only.register, None);
    assert_eq!(
        registry.register_datagram(
            host_only.pending.punch_id(),
            PunchRole::Gateway,
            &ticket,
            mapping(1, 4000)
        ),
        None,
        "a punch without a rendezvous has no tickets to match"
    );
}

#[tokio::test(start_paused = true)]
async fn each_role_latches_its_first_source() {
    let registry = registry();
    let paired = answered(&registry, NODE).await;
    let gateway = mapping(1, 4000);
    let device = mapping(2, 5000);
    let registered = Some(ProbeDatagram::Registered {
        punch_id: paired.punch_id,
    });
    assert_eq!(
        registry.register_datagram(
            paired.punch_id,
            PunchRole::Gateway,
            &paired.gateway,
            gateway
        ),
        registered
    );
    assert_eq!(
        registry.register_datagram(
            paired.punch_id,
            PunchRole::Gateway,
            &paired.gateway,
            mapping(1, 4001)
        ),
        None,
        "a Register for an observed role from a new source is dropped"
    );
    assert_eq!(
        registry.register_datagram(
            paired.punch_id,
            PunchRole::Gateway,
            &paired.gateway,
            gateway
        ),
        registered
    );
    assert_eq!(
        registry.register_datagram(paired.punch_id, PunchRole::Device, &paired.device, device),
        Some(ProbeDatagram::Peer {
            punch_id: paired.punch_id,
            srflx: gateway,
        })
    );
    assert_eq!(
        registry.register_datagram(
            paired.punch_id,
            PunchRole::Gateway,
            &paired.gateway,
            gateway
        ),
        Some(ProbeDatagram::Peer {
            punch_id: paired.punch_id,
            srflx: device,
        })
    );
}

#[tokio::test(start_paused = true)]
async fn a_punch_considers_at_most_its_datagram_budget() {
    let registry = registry();
    let paired = answered(&registry, NODE).await;
    let source = mapping(1, 4000);
    let wrong = RendezvousTicket::generate();
    for _ in 0..MAX_DATAGRAMS_PER_PUNCH / 2 {
        registry.register_datagram(paired.punch_id, PunchRole::Gateway, &wrong, source);
    }
    let answered_count = (0..MAX_DATAGRAMS_PER_PUNCH)
        .filter_map(|_| {
            registry.register_datagram(paired.punch_id, PunchRole::Gateway, &paired.gateway, source)
        })
        .count();
    assert_eq!(
        answered_count,
        usize::try_from(MAX_DATAGRAMS_PER_PUNCH / 2).unwrap(),
        "every Register naming the punch counts, valid or not"
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
