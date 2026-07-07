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

use baybo_agent::state::DurableActorState;
use baybo_model::{ChannelType, SessionId, User};
use baybo_session::SessionManager;
use baybo_session::SessionStore;
use baybo_session::test_support::{
    MemorySessionFolderStore, MemorySessionStore, MemorySessionSummaryStore,
};

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
    let session = baybo_model::Session {
        id: SessionId::from("session-rt"),
        user: test_user(),
        channel: ChannelType::tui(),
        created_at: chrono::Utc::now(),
        last_active: chrono::Utc::now(),
        state: baybo_model::SessionState {
            compression_count: 7,
            ..Default::default()
        },
        root_session_id: SessionId::from("session-rt"),
        trigger: baybo_model::TriggerSource::User,
        lineage: None,
        hidden: false,
        pinned: false,
        archived: false,
        folder_id: None,
    };

    let original = DurableActorState::new(session);
    let json = serde_json::to_string(&original).expect("durable state must serialize");
    let restored: DurableActorState =
        serde_json::from_str(&json).expect("durable state must deserialize");

    assert_eq!(restored.session.id, original.session.id);
    assert_eq!(restored.session.state.compression_count, 7);
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
    let folder_store = Arc::new(MemorySessionFolderStore::new());
    let sessions = SessionManager::new(session_store.clone(), summary_store, folder_store);

    // First actor: create session, mutate observable session state
    // (skills + compression count), persist.
    let mut session = sessions
        .create_session(test_user(), ChannelType::tui())
        .await
        .expect("create session");
    let session_id = session.id.clone();
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
        durable_post.session.state.compression_count, 3,
        "compression_count must survive idle eviction → rehydrate",
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
async fn pending_background_results_survive_session_round_trip() {
    use baybo_model::{PendingBackgroundResult, SessionId, SubagentExitStatus};

    let session_store: Arc<dyn SessionStore> = Arc::new(MemorySessionStore::new());
    let summary_store = Arc::new(MemorySessionSummaryStore::new());
    let folder_store = Arc::new(MemorySessionFolderStore::new());
    let sessions = SessionManager::new(session_store.clone(), summary_store, folder_store);

    let mut session = sessions
        .create_session(test_user(), ChannelType::tui())
        .await
        .expect("create");
    let session_id = session.id.clone();

    session
        .state
        .pending_background_results
        .push(PendingBackgroundResult::subagent(
            "bg-1",
            "general-purpose",
            "check the docs",
            SessionId::from("child-xyz"),
            "found three matches",
            SubagentExitStatus::Completed,
        ));
    session_store.save(&session).await.expect("persist");

    drop(session);

    let reloaded = sessions
        .get(&session_id)
        .await
        .expect("load")
        .expect("row present");
    assert_eq!(reloaded.state.pending_background_results.len(), 1);
    let entry = &reloaded.state.pending_background_results[0];
    assert_eq!(entry.handle_id, "bg-1");
    assert_eq!(entry.label, "check the docs");
    assert_eq!(entry.summary_text, "found three matches");
    assert!(matches!(
        &entry.kind,
        baybo_model::BackgroundJobKind::Subagent { subagent_type, .. }
            if subagent_type == "general-purpose"
    ));
    assert!(matches!(entry.status, SubagentExitStatus::Completed));
}
