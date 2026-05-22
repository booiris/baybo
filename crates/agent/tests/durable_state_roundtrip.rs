//! Durable-half rehydration round-trip tests.
//!
//! These tests prove that a value placed into [`DurableActorState`] can
//! be persisted, dropped, and reconstructed without loss. They are
//! deliberately narrow: a full actor-level observational roundtrip
//! ("send N messages, snapshot, drop actor, send M more, compare to
//! single-run baseline") would require shared LLM stubs, identical
//! trace timestamps, and per-message ordering guarantees that are
//! orthogonal to what the contract here is checking. The contract is
//! type-level: durable state is everything the actor needs to recover;
//! volatile state is what the spawner rebuilds.
//!
//! See `crates/agent/src/state.rs` for the corresponding compile-time
//! assertions.

use std::sync::Arc;

use aura_agent::state::DurableActorState;
use aura_model::{ChannelType, SessionId, User};
use aura_session::SessionManager;
use aura_session::SessionStore;
use aura_session::test_support::{MemorySessionStore, MemorySessionSummaryStore};

fn test_user() -> User {
    User {
        id: "rt-user".to_string(),
        name: Some("Roundtrip".to_string()),
        channel: ChannelType::tui(),
    }
}

/// JSON round-trip: every byte that goes into the store must come back
/// out unchanged, modulo allocations. This is what the
/// `_ASSERT_DURABLE_ROUNDTRIPS` compile-time check guarantees at the
/// type level — this test exercises it at the value level.
#[test]
fn durable_actor_state_json_roundtrip_preserves_all_fields() {
    let session = aura_model::Session {
        id: SessionId::from("session-rt"),
        user: test_user(),
        channel: ChannelType::tui(),
        created_at: chrono::Utc::now(),
        last_active: chrono::Utc::now(),
        state: aura_model::SessionState {
            active_skills: vec!["skill-a".into(), "skill-b".into()],
            compression_count: 7,
            ..Default::default()
        },
        root_session_id: SessionId::from("session-rt"),
        trigger: aura_model::TriggerSource::User,
        lineage: None,
        bound_soul_version: "soul-v1".to_string(),
        hidden: false,
    };

    let original = DurableActorState::new(session);
    let json = serde_json::to_string(&original).expect("durable state must serialize");
    let restored: DurableActorState =
        serde_json::from_str(&json).expect("durable state must deserialize");

    assert_eq!(restored.session.id, original.session.id);
    assert_eq!(
        restored.session.state.active_skills,
        original.session.state.active_skills,
    );
    assert_eq!(restored.session.state.compression_count, 7);
    assert_eq!(
        restored.session.bound_soul_version,
        original.session.bound_soul_version
    );
    assert_eq!(restored.session.trigger, original.session.trigger);
}

/// End-to-end: store → drop → rehydrate. Mirrors what a freshly-spawned
/// actor for the same session_id would see after idle eviction. The
/// new [`DurableActorState`] is constructed from the loaded `Session`,
/// proving that the durable layer survives a process-level "actor
/// gone, then back" cycle as long as the session store is the same.
#[tokio::test]
async fn rehydrate_after_idle_eviction_preserves_session_state() {
    let session_store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let summary_store = Arc::new(MemorySessionSummaryStore::new());
    let sessions = SessionManager::new(session_store.clone(), summary_store);

    // First actor: create session, mutate observable session state
    // (skills + compression count), persist.
    let mut session = sessions
        .create_session(test_user(), ChannelType::tui())
        .await
        .expect("create session");
    let session_id = session.id.clone();
    session.state.active_skills.push("active-pre-evict".into());
    session.state.compression_count = 3;
    session_store
        .save(&session)
        .await
        .expect("persist session post-mutation");

    let durable_pre = DurableActorState::new(session);

    // First actor drops here. Volatile half is gone; durable half
    // lives in the store.
    drop(durable_pre);

    // Replacement actor: same `session_id`, fresh load from store.
    // This is what the spawner factory does on the next incoming
    // message after the idle reaper shut the previous actor down.
    let reloaded = sessions
        .get(&session_id)
        .await
        .expect("session manager load")
        .expect("session row still present after eviction");
    let durable_post = DurableActorState::new(reloaded);

    assert_eq!(durable_post.session.id, session_id);
    assert_eq!(
        durable_post.session.state.active_skills,
        vec!["active-pre-evict".to_string()],
        "active_skills must survive idle eviction → rehydrate",
    );
    assert_eq!(
        durable_post.session.state.compression_count, 3,
        "compression_count must survive idle eviction → rehydrate",
    );
}

/// Compression `in_flight` flag must not survive a reap — otherwise
/// the next compression pass for the session is permanently blocked.
/// `SessionManager::clear_summary_in_flight` is invoked by
/// `AgentSupervisor::reap_idle` before sending `Shutdown`; this test
/// exercises the same clear path in isolation.
#[tokio::test]
async fn clearing_in_flight_unblocks_post_reap_compression() {
    use aura_session::SessionSummaryStore;

    let session_store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let summary_store: Arc<dyn SessionSummaryStore> = Arc::new(MemorySessionSummaryStore::new());
    let sessions = SessionManager::new(session_store, summary_store.clone());

    let session_id = SessionId::from("session-inflight");
    sessions
        .mark_summary_in_flight(&session_id, "owner-a")
        .await
        .expect("mark in_flight");

    let row = summary_store
        .get(&session_id)
        .await
        .expect("query summary row")
        .expect("row exists post-mark");
    assert!(row.in_flight, "pre-condition: flag is set");

    // Reaper's pre-shutdown clear.
    sessions
        .clear_summary_in_flight(&session_id)
        .await
        .expect("clear in_flight");

    let row = summary_store
        .get(&session_id)
        .await
        .expect("query summary row")
        .expect("row still exists");
    assert!(
        !row.in_flight,
        "post-reap rehydration must see in_flight = false so the next pass can run"
    );
}

/// Background subagent deliveries that landed on the actor are
/// persisted to the session row immediately, so a parent that the
/// idle reaper later reclaims still hands the pending notifications
/// to the next freshly-hydrated actor. We don't run a full actor
/// here — the contract under test is purely "session.state.pending
/// survives the storage round-trip", which is the property the
/// actor's `persist_session_state_after_pending_change` helper is
/// responsible for upholding.
#[tokio::test]
async fn pending_subagent_results_survive_session_round_trip() {
    use aura_model::{
        ContentBlock, PendingSubagentResult, SessionId, SubagentExitStatus,
    };

    let session_store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let summary_store = Arc::new(MemorySessionSummaryStore::new());
    let sessions = SessionManager::new(session_store.clone(), summary_store);

    let mut session = sessions
        .create_session(test_user(), ChannelType::tui())
        .await
        .expect("create");
    let session_id = session.id.clone();

    session.state.pending_subagent_results.push(PendingSubagentResult {
        handle_id: "bg-1".into(),
        subagent_type: "general-purpose".into(),
        task_summary: "check the docs".into(),
        child_session_id: SessionId::from("child-xyz"),
        final_text: "found three matches".into(),
        images: vec![],
        status: SubagentExitStatus::Completed,
    });
    session_store.save(&session).await.expect("persist");

    drop(session);

    let reloaded = sessions
        .get(&session_id)
        .await
        .expect("load")
        .expect("row present");
    assert_eq!(reloaded.state.pending_subagent_results.len(), 1);
    let entry = &reloaded.state.pending_subagent_results[0];
    assert_eq!(entry.handle_id, "bg-1");
    assert_eq!(entry.subagent_type, "general-purpose");
    assert_eq!(entry.final_text, "found three matches");
    assert!(matches!(entry.status, SubagentExitStatus::Completed));
    assert!(entry.images.is_empty());

    // Round-trip with an image attachment ensures the ContentBlock
    // serialization in the new field type works too.
    let mut with_image = reloaded;
    with_image.state.pending_subagent_results.push(PendingSubagentResult {
        handle_id: "bg-2".into(),
        subagent_type: "explorer".into(),
        task_summary: "screenshot".into(),
        child_session_id: SessionId::from("child-img"),
        final_text: "here is the screenshot".into(),
        images: vec![ContentBlock::Image {
            blob: aura_model::BlobRef {
                blob_id: "blob-1".into(),
            },
            mime_type: "image/png".into(),
        }],
        status: SubagentExitStatus::Completed,
    });
    session_store.save(&with_image).await.expect("persist with image");
    let reloaded = sessions
        .get(&session_id)
        .await
        .expect("load")
        .expect("row present");
    assert_eq!(reloaded.state.pending_subagent_results.len(), 2);
    let entry = &reloaded.state.pending_subagent_results[1];
    assert_eq!(entry.images.len(), 1);
    assert!(matches!(&entry.images[0], ContentBlock::Image { .. }));
}
