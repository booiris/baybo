//! sqlite implementation of [`HumanAskStore`] — the `human_asks` ledger.
//!
//! Rows are never deleted. Both state transitions are single-statement claim
//! `UPDATE`s, so a BUSY re-run of the write closure cannot double-apply.

use anyhow::Context as _;
use async_trait::async_trait;
use baybo_model::human_ask::{AskId, AskKind, AskPhase, AskResolution};
use baybo_model::{ChannelType, SessionId};
use chrono::{DateTime, Utc};

use super::{SqlitePool, row::RowDecoder};
use baybo_store::StorageError;
use baybo_store::human_ask::{HumanAskRow, HumanAskStore, Result};

pub struct SqliteHumanAskStore {
    pool: SqlitePool,
}

impl SqliteHumanAskStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

const SELECT_COLS: &str = "ask_id, session_id, channel, tool_call_id, turn_id, kind, phase, \
     request, created_at, resolved_at, resolution, applied_at";

const ASK: RowDecoder<HumanAskRow> = RowDecoder {
    table: "human_asks",
    key: "ask_id",
    decode: |row| {
        let text = |column: &str| row.get::<_, String>(column);
        let timestamp = |us| super::time::from_us(us).context("timestamp out of range");
        Ok(HumanAskRow {
            ask_id: text("ask_id")?.parse().context("invalid ask_id")?,
            session_id: SessionId::from(text("session_id")?),
            channel: ChannelType::from(text("channel")?),
            tool_call_id: row.get("tool_call_id")?,
            turn_id: text("turn_id")?.parse().context("invalid turn_id")?,
            kind: AskKind::from_storage_str(&text("kind")?).context("unknown kind")?,
            phase: AskPhase::from_storage_str(&text("phase")?).context("unknown phase")?,
            request: serde_json::from_str(&text("request")?).context("invalid request")?,
            created_at: timestamp(row.get("created_at")?)?,
            resolved_at: row
                .get::<_, Option<i64>>("resolved_at")?
                .map(timestamp)
                .transpose()?,
            resolution: row
                .get::<_, Option<String>>("resolution")?
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .context("invalid resolution")?,
            applied_at: row
                .get::<_, Option<i64>>("applied_at")?
                .map(timestamp)
                .transpose()?,
        })
    },
};

fn to_json<T: serde::Serialize>(what: &str, value: &T) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|e| StorageError::Storage(format!("serialize human_asks {what}: {e}")))
}

fn list_where(
    conn: &rusqlite::Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> anyhow::Result<Vec<HumanAskRow>> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM human_asks WHERE applied_at IS NULL {filter} \
         ORDER BY created_at, ask_id"
    );
    let mut stmt = conn.prepare(&sql)?;
    ASK.list(stmt.query(params)?)
}

#[async_trait]
impl HumanAskStore for SqliteHumanAskStore {
    async fn create(&self, row: &HumanAskRow) -> Result<()> {
        let ask_id = row.ask_id.to_string();
        let session_id = row.session_id.as_str().to_owned();
        let channel = row.channel.to_string();
        let tool_call_id = row.tool_call_id.clone();
        let turn_id = row.turn_id.to_string();
        let kind = row.kind.as_str();
        let phase = row.phase.as_str();
        let request = to_json("request", &row.request)?;
        let created_at = super::time::to_us(row.created_at);
        let resolved_at = row.resolved_at.map(super::time::to_us);
        let resolution = row
            .resolution
            .as_ref()
            .map(|r| to_json("resolution", r))
            .transpose()?;
        let applied_at = row.applied_at.map(super::time::to_us);
        self.pool
            .interact_write("human_asks.create", move |conn| {
                conn.execute(
                    "INSERT INTO human_asks \
                         (ask_id, session_id, channel, tool_call_id, turn_id, kind, phase, \
                          request, created_at, resolved_at, resolution, applied_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
                     ON CONFLICT(ask_id) DO NOTHING",
                    rusqlite::params![
                        ask_id,
                        session_id,
                        channel,
                        tool_call_id,
                        turn_id,
                        kind,
                        phase,
                        request,
                        created_at,
                        resolved_at,
                        resolution,
                        applied_at,
                    ],
                )?;
                Ok(())
            })
            .await
    }

    async fn resolve(
        &self,
        ask_id: &AskId,
        resolution: &AskResolution,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        let ask_id = ask_id.to_string();
        let resolution = to_json("resolution", resolution)?;
        let now = super::time::to_us(now);
        let affected = self
            .pool
            .interact_write("human_asks.resolve", move |conn| {
                Ok(conn.execute(
                    "UPDATE human_asks SET resolved_at = ?2, resolution = ?3 \
                     WHERE ask_id = ?1 AND resolved_at IS NULL",
                    rusqlite::params![ask_id, now, resolution],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn mark_applied(&self, ask_id: &AskId, now: DateTime<Utc>) -> Result<bool> {
        let ask_id = ask_id.to_string();
        let now = super::time::to_us(now);
        let affected = self
            .pool
            .interact_write("human_asks.mark_applied", move |conn| {
                Ok(conn.execute(
                    "UPDATE human_asks SET applied_at = ?2 \
                     WHERE ask_id = ?1 AND applied_at IS NULL",
                    rusqlite::params![ask_id, now],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn get(&self, ask_id: &AskId) -> Result<Option<HumanAskRow>> {
        let ask_id = ask_id.to_string();
        self.pool
            .interact("human_asks.get", move |conn| {
                let sql = format!("SELECT {SELECT_COLS} FROM human_asks WHERE ask_id = ?1");
                ASK.optional(conn.prepare(&sql)?.query([ask_id])?)
            })
            .await
    }

    async fn list_unapplied(&self) -> Result<Vec<HumanAskRow>> {
        self.pool
            .interact("human_asks.list_unapplied", move |conn| {
                list_where(conn, "", [])
            })
            .await
    }

    async fn list_unapplied_for_session(&self, session_id: &SessionId) -> Result<Vec<HumanAskRow>> {
        let session_id = session_id.as_str().to_owned();
        self.pool
            .interact("human_asks.list_unapplied_for_session", move |conn| {
                list_where(conn, "AND session_id = ?1", rusqlite::params![session_id])
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::approval::{ApprovalDecision, ApprovalRequest, ResourceAccess};
    use baybo_model::human_ask::{AskRequest, Question, QuestionOption, QuestionRequest};
    use baybo_model::{Session, SessionState, TriggerSource, User};
    use baybo_store::SessionStore;

    use crate::sqlite::session::SqliteSessionStore;

    fn at(us: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_micros(us).unwrap()
    }

    fn make_session(id: &str) -> Session {
        let now = Utc::now();
        Session {
            id: SessionId::from(id),
            user: User {
                id: format!("u-{id}"),
                name: None,
                channel: ChannelType::tui(),
            },
            channel: ChannelType::tui(),
            created_at: now,
            last_active: now,
            state: SessionState::default(),
            root_session_id: SessionId::from(id),
            trigger: TriggerSource::User,
            lineage: None,
            hidden: false,
            pinned: false,
            archived: false,
            folder_id: None,
            title: None,
        }
    }

    async fn store() -> (SqliteHumanAskStore, SqlitePool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(dir.path().join("t.db")).await.unwrap();
        let sessions = SqliteSessionStore::new(pool.clone());
        for id in ["s1", "s2"] {
            sessions.save(&make_session(id)).await.unwrap();
        }
        (SqliteHumanAskStore::new(pool.clone()), pool, dir)
    }

    fn approval_row(ask: &str, session: &str, created_us: i64) -> HumanAskRow {
        HumanAskRow {
            ask_id: ask.parse().unwrap(),
            session_id: SessionId::from(session),
            channel: ChannelType::from("web"),
            tool_call_id: format!("tu-{ask}"),
            turn_id: baybo_model::TurnId::default(),
            kind: AskKind::Approval,
            phase: AskPhase::BeforeExecute,
            request: AskRequest::Approval(ApprovalRequest {
                ask_id: ask.parse().unwrap(),
                tool_call_id: format!("tu-{ask}"),
                session_id: SessionId::from(session),
                user_id: "u".into(),
                tool: "Bash".into(),
                accesses: vec![ResourceAccess::ExecCommand {
                    command: "ls".into(),
                }],
                params_preview: "{}".into(),
                description: Some("list".into()),
            }),
            created_at: at(created_us),
            resolved_at: None,
            resolution: None,
            applied_at: None,
        }
    }

    #[tokio::test]
    async fn create_then_get_round_trips_both_kinds() {
        let (store, _pool, _dir) = store().await;
        let approval = approval_row("a1", "s1", 1_000);
        let question = HumanAskRow {
            ask_id: "q1".parse().unwrap(),
            kind: AskKind::Question,
            phase: AskPhase::DuringExecuteNonResumable,
            request: AskRequest::Question(QuestionRequest {
                questions: vec![Question {
                    header: "Lang".into(),
                    question: "Which?".into(),
                    options: vec![QuestionOption {
                        label: "Rust".into(),
                        description: "fast".into(),
                    }],
                    multi_select: true,
                }],
            }),
            resolved_at: Some(at(3_000)),
            resolution: Some(AskResolution::Declined),
            applied_at: Some(at(4_000)),
            ..approval_row("q1", "s1", 2_000)
        };
        store.create(&approval).await.unwrap();
        store.create(&question).await.unwrap();

        assert_eq!(store.get(&approval.ask_id).await.unwrap(), Some(approval));
        assert_eq!(store.get(&question.ask_id).await.unwrap(), Some(question));
        assert_eq!(store.get(&"missing".parse().unwrap()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn create_is_idempotent_and_keeps_the_first_row() {
        let (store, _pool, _dir) = store().await;
        let first = approval_row("a1", "s1", 1_000);
        store.create(&first).await.unwrap();
        let second = HumanAskRow {
            tool_call_id: "other".into(),
            ..first.clone()
        };
        store.create(&second).await.unwrap();
        assert_eq!(store.get(&first.ask_id).await.unwrap(), Some(first));
    }

    #[tokio::test]
    async fn resolve_and_mark_applied_are_single_winner_claims() {
        let (store, _pool, _dir) = store().await;
        let row = approval_row("a1", "s1", 1_000);
        store.create(&row).await.unwrap();

        let approve = AskResolution::Approval(ApprovalDecision::Approve);
        assert!(
            store
                .resolve(&row.ask_id, &approve, at(2_000))
                .await
                .unwrap()
        );
        assert!(
            !store
                .resolve(&row.ask_id, &AskResolution::Abandoned, at(3_000))
                .await
                .unwrap()
        );
        assert!(store.mark_applied(&row.ask_id, at(4_000)).await.unwrap());
        assert!(!store.mark_applied(&row.ask_id, at(5_000)).await.unwrap());

        let got = store.get(&row.ask_id).await.unwrap().unwrap();
        assert_eq!(got.resolution, Some(approve));
        assert_eq!(got.resolved_at, Some(at(2_000)));
        assert_eq!(got.applied_at, Some(at(4_000)));

        let missing: AskId = "missing".parse().unwrap();
        assert!(
            !store
                .resolve(&missing, &AskResolution::Abandoned, at(1))
                .await
                .unwrap()
        );
        assert!(!store.mark_applied(&missing, at(1)).await.unwrap());
    }

    #[tokio::test]
    async fn list_unapplied_covers_pending_and_resolved_oldest_first() {
        let (store, _pool, _dir) = store().await;
        let late = approval_row("a-late", "s1", 3_000);
        let early = approval_row("a-early", "s2", 1_000);
        let resolved = approval_row("a-resolved", "s1", 2_000);
        let applied = approval_row("a-applied", "s1", 500);
        for row in [&late, &early, &resolved, &applied] {
            store.create(row).await.unwrap();
        }
        store
            .resolve(&resolved.ask_id, &AskResolution::Abandoned, at(4_000))
            .await
            .unwrap();
        store
            .mark_applied(&applied.ask_id, at(4_000))
            .await
            .unwrap();

        let ids = |rows: Vec<HumanAskRow>| -> Vec<String> {
            rows.into_iter().map(|r| r.ask_id.to_string()).collect()
        };
        assert_eq!(
            ids(store.list_unapplied().await.unwrap()),
            vec!["a-early", "a-resolved", "a-late"]
        );
        assert_eq!(
            ids(store
                .list_unapplied_for_session(&SessionId::from("s1"))
                .await
                .unwrap()),
            vec!["a-resolved", "a-late"]
        );
    }

    #[tokio::test]
    async fn rows_this_build_cannot_read_are_skipped() {
        let (store, pool, _dir) = store().await;
        store
            .create(&approval_row("good", "s1", 2_000))
            .await
            .unwrap();
        pool.interact_write("test.insert_future_kind", |conn| {
            conn.execute(
                "INSERT INTO human_asks (ask_id, session_id, channel, tool_call_id, turn_id, \
                 kind, phase, request, created_at) \
                 VALUES ('future', 's1', 'web', 'tu', 'turn', 'telepathy', 'before_execute', \
                 '{}', 1000)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        let rows = store.list_unapplied().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ask_id.as_str(), "good");
        assert!(
            matches!(store.get(&"future".parse().unwrap()).await, Err(StorageError::Corrupt { table: "human_asks", key, .. }) if key == "future")
        );
    }
}
