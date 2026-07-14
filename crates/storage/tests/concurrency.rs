//! Regression guard: many threads hammering the store must not corrupt it.
//!
//! A sqlite connection owns an unsynchronised private heap — its lookaside
//! allocator — and the C API mutates it while *decoding*, not just while
//! querying: `sqlite3_value_text()` allocates in order to NUL-terminate a TEXT
//! column. Let two threads into one connection and they race on the lookaside
//! free list; the process then dies in an unrelated allocation, inside
//! `sqlite3DbMallocRawNN`, far from whatever broke it. `SqlitePool` exists to
//! make that unrepresentable — a connection is checked out exclusively for the
//! whole closure, and `rusqlite::Connection` being `!Sync` means the compiler
//! enforces it.
//!
//! Reproducing that corruption needs true parallelism *and* a TEXT decode,
//! which is why this test insists on all four:
//!   * a multi-thread runtime — a current-thread one never triggers it,
//!   * an on-disk WAL database, so the readers and the writer are real,
//!   * columns that are TEXT and long, i.e. the allocating decode path,
//!   * readers and writers overlapping, not merely interleaving.
//!
//! A regression here is a SIGSEGV, not a failed assertion: the test binary dies
//! and the runner reports the signal. That is the point — this class of bug
//! must not be able to fail quietly.

use std::sync::Arc;

use baybo_model::{ChannelType, ChatMessage, ContentBlock, Session, SessionId, SessionState, User};
use baybo_storage::sqlite::{SqlitePool, SqliteSessionStore};
use baybo_store::session::SessionStore;
use chrono::Utc;

const SESSIONS: usize = 12;
const READERS: usize = 16;
const WRITERS: usize = 4;
const ROUNDS: usize = 40;

fn session(id: &str) -> Session {
    let id = SessionId::from(id);
    Session {
        id: id.clone(),
        user: User {
            id: "u1".to_string(),
            name: Some("Concurrency".to_string()),
            channel: ChannelType::tui(),
        },
        channel: ChannelType::tui(),
        created_at: Utc::now(),
        last_active: Utc::now(),
        state: SessionState::default(),
        root_session_id: id,
        trigger: baybo_model::TriggerSource::User,
        lineage: None,
        hidden: false,
        pinned: false,
        archived: false,
        folder_id: None,
        title: None,
    }
}

/// Long enough that decoding it cannot be served from a short-string
/// optimisation, and non-ASCII so an encoding conversion is on the table —
/// both push `sqlite3_value_text` down its allocating path.
fn text(round: usize) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(format!(
        "round {round} — {}",
        "配置一下这个会话的上下文压缩阈值，顺便把工具调用的超时也调长一点。".repeat(4)
    ))]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn concurrent_readers_and_writers_do_not_corrupt_the_connection() {
    let dir = tempfile::tempdir().expect("tempdir");
    // An on-disk WAL database, not `:memory:` — the racing readers and writer
    // must be real.
    let pool = SqlitePool::open(dir.path().join("storage.db"))
        .await
        .expect("open pool");
    let store = Arc::new(SqliteSessionStore::new(pool));

    let ids: Vec<SessionId> = (0..SESSIONS)
        .map(|i| SessionId::from(format!("sess-{i}").as_str()))
        .collect();
    for id in &ids {
        let s = session(id.as_str());
        store.save(&s).await.expect("save session");
        store
            .append_session_message(id, &ChatMessage::user(text(0)))
            .await
            .expect("seed message");
    }

    let mut tasks = Vec::new();

    // Readers: exactly what `list_sessions` fans out — a back-of-index probe
    // per session, then a full decode of its TEXT columns.
    for _ in 0..READERS {
        let store = Arc::clone(&store);
        let ids = ids.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..ROUNDS {
                for id in &ids {
                    let got = store
                        .load_last_user_message(id)
                        .await
                        .expect("load_last_user_message");
                    assert!(got.is_some(), "seeded session must have a user message");
                }
                store
                    .list_by_channel(&ChannelType::tui())
                    .await
                    .expect("list_by_channel");
            }
        }));
    }

    // Writers, overlapping the readers: the agent loop and the trace sink write
    // while the chat list is being rendered.
    for w in 0..WRITERS {
        let store = Arc::clone(&store);
        let ids = ids.clone();
        tasks.push(tokio::spawn(async move {
            for round in 1..=ROUNDS {
                let id = &ids[(round + w) % ids.len()];
                store
                    .append_session_message(id, &ChatMessage::user(text(round)))
                    .await
                    .expect("append_session_message");
            }
        }));
    }

    for t in tasks {
        t.await.expect("task panicked");
    }

    // The data is still coherent — corruption that stops short of a segfault
    // would surface here as a bad decode or a lost row.
    for id in &ids {
        let (_, msg) = store
            .load_last_user_message(id)
            .await
            .expect("final read")
            .expect("final row");
        assert!(
            matches!(msg.content.first(), Some(ContentBlock::Text(t)) if t.starts_with("round ")),
            "decoded TEXT column survived the concurrent hammering",
        );
    }
}
