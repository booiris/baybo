//! End-to-end exercise of the async-summary-refresh storage +
//! orphan-reaper paths.
//!
//! These tests stand up a libsql-backed `Store` and a real
//! `SessionManager` with the `SessionSummaryStore` wired in, then
//! drive:
//!
//!  1. `record_summary_success` → `summary_metadata` round trip
//!     across the `SessionManager` API.
//!  2. Maintenance-session row management — verified indirectly by
//!     checking that `active_maintenance_for_parent` (the gate the
//!     in-flight serialization rule reads) and the cascade-on-delete
//!     cover the `LineageKind::SystemMaintenance` rows.
//!  3. `reap_maintenance_orphans` — DB rows deleted, parent's
//!     `error_count` bumped, on-disk orphan summary directory
//!     removed.
//!
//! The full agent-loop wiring (LLM call, JobLifecycle, SpanRecorder)
//! is exercised at the unit-test layer in `aura-agent::compression`
//! and `aura-context::compressor`; this file focuses on the
//! storage + filesystem boundary.

use std::sync::Arc;
use std::time::Duration;

use aura_agent::SessionManager;
use aura_agent::compression::reap_maintenance_orphans;
use aura_model::{
    BackgroundCompressionPayload, ChannelType, JobId, Lineage, LineageKind, Session, SessionId,
    SessionState, SystemReason, SystemTrigger, TriggerSource, User,
};
use aura_storage::Store;
use aura_workspace::WorkspacePaths;
use chrono::{Duration as ChronoDuration, Utc};
use tempfile::TempDir;

fn user(id: &str) -> User {
    User {
        id: id.to_string(),
        name: None,
        channel: ChannelType::tui(),
    }
}

fn root_session(id: &str) -> Session {
    let now = Utc::now();
    Session {
        id: SessionId::from(id),
        user: user(&format!("u-{id}")),
        channel: ChannelType::tui(),
        created_at: now,
        last_active: now,
        state: SessionState::default(),
        root_session_id: SessionId::from(id),
        trigger: TriggerSource::User,
        lineage: None,
        bound_soul_version: "soul".into(),
        hidden: false,
    }
}

async fn fresh_store_and_paths() -> (Store, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("storage.db");
    let store = Store::open(&db_path).await.expect("open store");
    (store, dir)
}

/// Round-trip a successful summary pass through the `SessionManager`
/// wrapper layer — the same surface the `BackgroundCompressionRunner` uses.
#[tokio::test]
async fn record_then_read_summary_metadata_via_session_manager() {
    let (store, _dir) = fresh_store_and_paths().await;
    let session = root_session("user-A");
    store.session.save(&session).await.unwrap();

    let mgr = SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        ChronoDuration::minutes(30),
    );

    // Initially no metadata.
    let none = mgr.summary_metadata(&session.id).await.unwrap();
    assert!(none.is_none());

    let now = Utc::now();
    mgr.record_summary_success(&session.id, 42, 12_345, "claude-opus-4-7", "span-1", now)
        .await
        .unwrap();

    let row = mgr.summary_metadata(&session.id).await.unwrap().unwrap();
    assert_eq!(row.cursor, 42);
    assert_eq!(row.pass_count, 1);
    assert_eq!(row.cost_micros, 12_345);
    assert_eq!(row.model_id, "claude-opus-4-7");
    assert_eq!(row.span_id, "span-1");
    assert_eq!(row.error_count, 0);

    // Failure bumps error_count without touching cursor / pass_count.
    mgr.record_summary_failure(&session.id, "model", "span-err", Utc::now())
        .await
        .unwrap();
    let row = mgr.summary_metadata(&session.id).await.unwrap().unwrap();
    assert_eq!(row.error_count, 1);
    assert_eq!(row.cursor, 42);
    assert_eq!(row.pass_count, 1);

    // Successful pass resets error_count.
    mgr.record_summary_success(&session.id, 60, 1, "model", "span-2", Utc::now())
        .await
        .unwrap();
    let row = mgr.summary_metadata(&session.id).await.unwrap().unwrap();
    assert_eq!(row.error_count, 0);
    assert_eq!(row.cursor, 60);
    assert_eq!(row.pass_count, 2);
    assert_eq!(row.cost_micros, 12_346);
}

/// Maintenance sessions are filtered from default listings; the
/// dedicated `active_maintenance_for_parent` lookup surfaces them.
#[tokio::test]
async fn maintenance_sessions_are_invisible_to_default_listings() {
    let (store, _dir) = fresh_store_and_paths().await;
    let parent = root_session("parent-1");
    store.session.save(&parent).await.unwrap();

    let mgr = SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        ChronoDuration::minutes(30),
    );

    // Spawn one maintenance session for the parent.
    let maint = mgr
        .create_maintenance_session(&parent, JobId::new(), SystemReason::BackgroundCompression)
        .await
        .unwrap();
    assert!(maint.id.as_str().starts_with("maint-"));

    // `list()` (operator-facing) shows only the parent.
    let listed = mgr.list().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, parent.id);

    // Lookup by parent surfaces the maintenance row.
    let active = mgr.active_maintenance_for_parent(&parent.id).await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0], maint.id);

    // The maintenance set lookup also surfaces it.
    let all = mgr.all_maintenance_sessions().await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0], maint.id);
}

/// Orphan reaper end-to-end:
///   - DB-side: maintenance session row is deleted, parent's
///     `error_count` is bumped.
///   - FS-side: a `summary.md` directory whose session has no
///     metadata row is removed.
#[tokio::test]
async fn orphan_reaper_cleans_db_and_fs() {
    let (store, dir) = fresh_store_and_paths().await;
    let parent = root_session("parent-orphan");
    store.session.save(&parent).await.unwrap();

    let mgr = Arc::new(SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        ChronoDuration::minutes(30),
    ));

    // Create a maintenance session — represents a pass that was
    // running when the previous process crashed.
    let maint = mgr
        .create_maintenance_session(&parent, JobId::new(), SystemReason::BackgroundCompression)
        .await
        .unwrap();
    assert_eq!(
        mgr.all_maintenance_sessions().await.unwrap().len(),
        1,
        "maintenance row must exist before reap"
    );

    // Set up a workspace dir with one orphan summary file under
    // `<root>/state/sessions/orphan-abc/summary.md`. No metadata
    // row for `orphan-abc` exists, so the FS sweep should reap it.
    let ws_root = dir.path().join("workspace");
    let paths = WorkspacePaths::new(ws_root.clone());
    let orphan_dir = paths.session_state_dir("orphan-abc");
    tokio::fs::create_dir_all(&orphan_dir).await.unwrap();
    tokio::fs::write(
        paths.session_summary_file("orphan-abc"),
        "stale summary body",
    )
    .await
    .unwrap();
    assert!(paths.session_summary_file("orphan-abc").exists());

    // Set up a *non*-orphan summary too — this one HAS a metadata
    // row, so the sweep must leave it alone.
    let kept_id = "still-known";
    let kept_session = root_session(kept_id);
    store.session.save(&kept_session).await.unwrap();
    mgr.record_summary_success(&kept_session.id, 5, 0, "m", "span", Utc::now())
        .await
        .unwrap();
    let kept_dir = paths.session_state_dir(kept_id);
    tokio::fs::create_dir_all(&kept_dir).await.unwrap();
    tokio::fs::write(paths.session_summary_file(kept_id), "kept body")
        .await
        .unwrap();

    // Run the reaper.
    reap_maintenance_orphans(&mgr, &paths).await;

    // DB: maintenance session deleted; parent's error_count = 1.
    assert!(
        mgr.all_maintenance_sessions().await.unwrap().is_empty(),
        "maintenance row must be reaped"
    );
    assert!(
        store.session.get(&maint.id).await.unwrap().is_none(),
        "maintenance row must be hard-deleted"
    );
    let parent_meta = mgr.summary_metadata(&parent.id).await.unwrap().unwrap();
    assert_eq!(parent_meta.error_count, 1);

    // FS: orphan dir removed.
    assert!(
        !paths.session_summary_file("orphan-abc").exists(),
        "FS orphan must be deleted"
    );

    // FS: known dir preserved.
    assert!(
        paths.session_summary_file(kept_id).exists(),
        "non-orphan summary must survive the sweep"
    );
}

/// Reaper preserves maintenance sessions whose job reached a
/// terminal state — they are kept as audit history for the
/// cost-records join. Only sessions whose job is still pending /
/// in_progress / stuck (or has no job at all) get reaped.
#[tokio::test]
async fn orphan_reaper_preserves_completed_maintenance_sessions() {
    let (store, dir) = fresh_store_and_paths().await;
    let parent = root_session("parent-mixed");
    store.session.save(&parent).await.unwrap();

    let mgr = Arc::new(SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        ChronoDuration::minutes(30),
    ));

    // (a) A maintenance session whose pass *succeeded* — its job is
    //     `Completed`. Reaper must keep it.
    let completed_maint = mgr
        .create_maintenance_session(&parent, JobId::new(), SystemReason::BackgroundCompression)
        .await
        .unwrap();
    let mut completed_job = aura_job::Job::new(
        completed_maint.id.clone(),
        aura_job::JobInput::System {
            trigger: SystemTrigger::BackgroundCompression(BackgroundCompressionPayload {
                parent_session_id: parent.id.clone(),
                up_to_ordinal: 0,
                in_flight_owner: "test-owner-completed".into(),
            }),
        },
        "soul-v1",
        None,
    );
    store.job.create(&completed_job).await.unwrap();
    let _ = completed_job.start().unwrap();
    let _ = completed_job
        .complete(aura_job::JobOutput::Structured {
            value: serde_json::json!({"cursor": 1}),
        })
        .unwrap();
    store.job.save(&completed_job).await.unwrap();

    // (b) A maintenance session whose job stayed `InProgress` — a
    //     crash mid-pass. Reaper must delete it.
    let in_flight_maint = mgr
        .create_maintenance_session(&parent, JobId::new(), SystemReason::BackgroundCompression)
        .await
        .unwrap();
    let mut in_flight_job = aura_job::Job::new(
        in_flight_maint.id.clone(),
        aura_job::JobInput::System {
            trigger: SystemTrigger::BackgroundCompression(BackgroundCompressionPayload {
                parent_session_id: parent.id.clone(),
                up_to_ordinal: 0,
                in_flight_owner: "test-owner-in-flight".into(),
            }),
        },
        "soul-v1",
        None,
    );
    store.job.create(&in_flight_job).await.unwrap();
    let _ = in_flight_job.start().unwrap();
    store.job.save(&in_flight_job).await.unwrap();

    // (c) A maintenance session with **no** job row — also reaped
    //     (process died between session create and `with_job`).
    let no_job_maint = mgr
        .create_maintenance_session(&parent, JobId::new(), SystemReason::BackgroundCompression)
        .await
        .unwrap();

    let ws_root = dir.path().join("workspace");
    let paths = WorkspacePaths::new(ws_root.clone());
    reap_maintenance_orphans(&mgr, &paths).await;

    // The completed maintenance session survives.
    assert!(
        store
            .session
            .get(&completed_maint.id)
            .await
            .unwrap()
            .is_some(),
        "completed maintenance session must be preserved as audit history"
    );

    // The in-flight and no-job ones get deleted.
    assert!(
        store
            .session
            .get(&in_flight_maint.id)
            .await
            .unwrap()
            .is_none(),
        "in-flight maintenance session must be reaped"
    );
    assert!(
        store.session.get(&no_job_maint.id).await.unwrap().is_none(),
        "maintenance session without a job row must be reaped"
    );

    // Parent's error_count reflects exactly the two reaped passes.
    let parent_meta = mgr.summary_metadata(&parent.id).await.unwrap().unwrap();
    assert_eq!(
        parent_meta.error_count, 2,
        "error_count must bump only for reaped (unfinished) passes, not for completed ones"
    );
}

/// Cascade-on-delete: removing a parent session takes its
/// `session_summaries` row with it (`ON DELETE CASCADE` FK).
#[tokio::test]
async fn parent_delete_cascades_summary_metadata() {
    let (store, _dir) = fresh_store_and_paths().await;
    let parent = root_session("parent-cascade");
    store.session.save(&parent).await.unwrap();

    let mgr = SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        ChronoDuration::minutes(30),
    );

    mgr.record_summary_success(&parent.id, 1, 1, "m", "span", Utc::now())
        .await
        .unwrap();
    assert!(mgr.summary_metadata(&parent.id).await.unwrap().is_some());

    mgr.delete(&parent.id).await.unwrap();
    assert!(
        mgr.summary_metadata(&parent.id).await.unwrap().is_none(),
        "summary metadata must cascade-delete with parent"
    );
}

/// `LineageKind::SystemMaintenance` round-trips through the libsql
/// `lineage_kind_str` mapping — `list_lineage_children` returns the
/// new variant. Belt-and-suspenders verification that the
/// `system_maintenance` SQL string matches both write and read sides.
#[tokio::test]
async fn lineage_kind_round_trips_for_system_maintenance() {
    let (store, _dir) = fresh_store_and_paths().await;
    let parent = root_session("p-lineage");
    store.session.save(&parent).await.unwrap();

    let maint = Session {
        id: SessionId::from("m-1"),
        user: parent.user.clone(),
        channel: parent.channel.clone(),
        created_at: Utc::now(),
        last_active: Utc::now(),
        state: SessionState::default(),
        root_session_id: parent.id.clone(),
        trigger: TriggerSource::System {
            reason: SystemReason::BackgroundCompression,
        },
        lineage: Some(Lineage {
            parent_session_id: parent.id.clone(),
            parent_job_id: JobId::new(),
            kind: LineageKind::SystemMaintenance,
        }),
        bound_soul_version: "soul".into(),
        hidden: false,
    };
    store.session.save(&maint).await.unwrap();

    let kids = store
        .session
        .list_lineage_children(&parent.id)
        .await
        .unwrap();
    assert_eq!(kids.len(), 1);
    assert!(matches!(kids[0].1, LineageKind::SystemMaintenance));
}

/// Trigger-gate `in_flight` lifecycle: marking the parent in-flight
/// stamps the owner token; a recorded success clears flag and owner
/// in one UPSERT; a stale CAS clear with the original owner is a
/// no-op now that the owner is NULL.
#[tokio::test]
async fn in_flight_marks_then_clears_on_recorded_pass() {
    let (store, _dir) = fresh_store_and_paths().await;
    let parent = root_session("parent-in-flight");
    store.session.save(&parent).await.unwrap();

    let mgr = SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        ChronoDuration::minutes(30),
    );

    // Initially no metadata row at all.
    assert!(mgr.summary_metadata(&parent.id).await.unwrap().is_none());

    // Gate marks in-flight before emitting the spawn — placeholder row
    // is created carrying `in_flight = true`, the owner token, and
    // zero-valued counters.
    let owner_a = "owner-A";
    mgr.mark_summary_in_flight(&parent.id, owner_a)
        .await
        .unwrap();
    let row = mgr.summary_metadata(&parent.id).await.unwrap().unwrap();
    assert!(row.in_flight);
    assert_eq!(row.in_flight_owner.as_deref(), Some(owner_a));
    assert_eq!(row.cursor, 0);
    assert_eq!(row.pass_count, 0);

    // Runner records success → flag cleared in the same UPSERT that
    // bumps `pass_count`, advances `cursor`, and nulls the owner.
    mgr.record_summary_success(&parent.id, 7, 1_000, "m", "span", Utc::now())
        .await
        .unwrap();
    let row = mgr.summary_metadata(&parent.id).await.unwrap().unwrap();
    assert!(!row.in_flight);
    assert!(row.in_flight_owner.is_none());
    assert_eq!(row.cursor, 7);
    assert_eq!(row.pass_count, 1);

    // Stale defensive cleanup with the same owner is now a no-op
    // (the row has owner = NULL after the success). Counters stay put.
    let cleared = mgr
        .clear_summary_in_flight_if_owned(&parent.id, owner_a)
        .await
        .unwrap();
    assert!(!cleared, "stale cleanup must not match a NULL owner");
    let row = mgr.summary_metadata(&parent.id).await.unwrap().unwrap();
    assert!(!row.in_flight);
    assert_eq!(row.cursor, 7);
    assert_eq!(row.pass_count, 1);
}

/// Issue 2 regression: a stale Pass A finishing after Pass B
/// remarked the parent must NOT wipe Pass B's mark via the
/// runner's defensive cleanup.
#[tokio::test]
async fn defensive_clear_does_not_clobber_newer_pass_mark() {
    let (store, _dir) = fresh_store_and_paths().await;
    let parent = root_session("parent-cas-race");
    store.session.save(&parent).await.unwrap();

    let mgr = SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        ChronoDuration::minutes(30),
    );

    // Pass A marks itself in-flight, then lands a successful summary
    // (which clears flag + owner in one terminal UPSERT).
    mgr.mark_summary_in_flight(&parent.id, "owner-A")
        .await
        .unwrap();
    mgr.record_summary_success(&parent.id, 5, 100, "m", "span", Utc::now())
        .await
        .unwrap();

    // Pass B picks up the parent (gate sees in_flight=false, marks
    // again with a fresh token).
    mgr.mark_summary_in_flight(&parent.id, "owner-B")
        .await
        .unwrap();

    // Pass A's stale outer cleanup fires. Pre-fix this would clear
    // Pass B's mark — with the owner-token CAS it must be a no-op.
    let cleared = mgr
        .clear_summary_in_flight_if_owned(&parent.id, "owner-A")
        .await
        .unwrap();
    assert!(!cleared, "stale Pass A cleanup must not match Pass B owner");

    let row = mgr.summary_metadata(&parent.id).await.unwrap().unwrap();
    assert!(row.in_flight, "Pass B's mark must survive stale cleanup");
    assert_eq!(row.in_flight_owner.as_deref(), Some("owner-B"));

    // Pass B's matching cleanup clears as expected.
    let cleared = mgr
        .clear_summary_in_flight_if_owned(&parent.id, "owner-B")
        .await
        .unwrap();
    assert!(cleared);
    let row = mgr.summary_metadata(&parent.id).await.unwrap().unwrap();
    assert!(!row.in_flight);
    assert!(row.in_flight_owner.is_none());
}

/// Orphan reaper closes the "mark-without-row" gap. Trigger gate
/// marks `in_flight = true` *before* emitting the SystemSpawnRequest;
/// if the process dies after the mark but before the router creates
/// the maintenance session row, no maintenance row exists for the
/// next boot's reaper to walk. Without an explicit sweep the parent
/// would stay blocked. The reaper's `clear_all_summary_in_flight`
/// step handles exactly this case.
#[tokio::test]
async fn orphan_reaper_clears_orphan_in_flight_without_maintenance_row() {
    let (store, dir) = fresh_store_and_paths().await;
    let parent = root_session("parent-mark-only");
    store.session.save(&parent).await.unwrap();

    let mgr = Arc::new(SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        ChronoDuration::minutes(30),
    ));

    // Simulate the gap: gate persisted in_flight = 1, process crashed
    // before the router could call create_maintenance_session.
    mgr.mark_summary_in_flight(&parent.id, "owner-orphan-mark")
        .await
        .unwrap();
    assert!(
        mgr.summary_metadata(&parent.id)
            .await
            .unwrap()
            .unwrap()
            .in_flight
    );
    assert!(
        mgr.all_maintenance_sessions().await.unwrap().is_empty(),
        "this scenario specifically has no maintenance session row to walk"
    );

    let paths = WorkspacePaths::new(dir.path().join("workspace"));
    reap_maintenance_orphans(&mgr, &paths).await;

    let row = mgr.summary_metadata(&parent.id).await.unwrap().unwrap();
    assert!(
        !row.in_flight,
        "reaper's clear_all_summary_in_flight must recover orphaned in_flight \
         even when no maintenance session row exists for it to walk"
    );
    // No spurious error_count bump: the orphan-without-row path doesn't
    // know which model to attribute the failure to, so the sweep just
    // clears the flag silently.
    assert_eq!(row.error_count, 0);
}

/// Orphan reaper recovery: a parent left with `in_flight = true`
/// after a process crash gets its flag cleared on next boot via
/// `record_summary_failure` (which the reaper invokes per reaped
/// maintenance session).
#[tokio::test]
async fn orphan_reaper_clears_in_flight_on_recovered_parent() {
    let (store, dir) = fresh_store_and_paths().await;
    let parent = root_session("parent-stale-flight");
    store.session.save(&parent).await.unwrap();

    let mgr = Arc::new(SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        ChronoDuration::minutes(30),
    ));

    // Simulate the crash mid-pass: maintenance row exists, parent's
    // `in_flight` is still set.
    let _maint = mgr
        .create_maintenance_session(&parent, JobId::new(), SystemReason::BackgroundCompression)
        .await
        .unwrap();
    mgr.mark_summary_in_flight(&parent.id, "owner-stale-flight")
        .await
        .unwrap();
    assert!(
        mgr.summary_metadata(&parent.id)
            .await
            .unwrap()
            .unwrap()
            .in_flight
    );

    let paths = WorkspacePaths::new(dir.path().join("workspace"));
    reap_maintenance_orphans(&mgr, &paths).await;

    let row = mgr.summary_metadata(&parent.id).await.unwrap().unwrap();
    assert!(
        !row.in_flight,
        "orphan reaper must clear in_flight on the parent so the gate can emit again"
    );
    assert_eq!(row.error_count, 1);
}

#[tokio::test]
async fn rapid_record_calls_accumulate_cost_and_pass_count() {
    let (store, _dir) = fresh_store_and_paths().await;
    let parent = root_session("rapid");
    store.session.save(&parent).await.unwrap();

    let mgr = SessionManager::new(
        store.session.clone(),
        store.session_summary.clone(),
        ChronoDuration::minutes(30),
    );

    // 5 quick passes — same span_id pattern as production "many
    // refreshes per session" exercise.
    for i in 0..5 {
        mgr.record_summary_success(
            &parent.id,
            i as i64,
            10,
            "m",
            &format!("span-{i}"),
            Utc::now(),
        )
        .await
        .unwrap();
    }
    let row = mgr.summary_metadata(&parent.id).await.unwrap().unwrap();
    assert_eq!(row.pass_count, 5);
    assert_eq!(row.cost_micros, 50);
    assert_eq!(row.cursor, 4);
    assert_eq!(row.span_id, "span-4");

    // Sanity: drain timeout is unrelated; tests run synchronously.
    tokio::time::timeout(Duration::from_millis(10), async {})
        .await
        .unwrap();
}
