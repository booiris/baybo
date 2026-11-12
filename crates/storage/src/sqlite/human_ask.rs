//! sqlite implementation of [`HumanAskStore`] — the `human_asks` ledger.
//!
//! Rows are never deleted. Both state transitions are single-statement claim
//! `UPDATE`s, so a BUSY re-run of the write closure cannot double-apply.

use async_trait::async_trait;
use baybo_model::human_ask::{AskId, AskKind, AskPhase, AskRequest, AskResolution};
use baybo_model::{ChannelType, SessionId};
use chrono::{DateTime, Utc};
use rusqlite::OptionalExtension;

use super::SqlitePool;
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

const SELECT_COLS: &str = "ask_id, session_id, channel, tool_call_id, kind, phase, request, \
     created_at, resolved_at, resolution, applied_at";

/// One row's columns as sqlite returned them, before any domain decoding, so
/// a row this build cannot interpret is skipped instead of failing the query.
struct RawRow {
    ask_id: String,
    session_id: String,
    channel: String,
    tool_call_id: String,
    kind: String,
    phase: String,
    request: String,
    created_at: i64,
    resolved_at: Option<i64>,
    resolution: Option<String>,
    applied_at: Option<i64>,
}

fn read_raw(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        ask_id: row.get(0)?,
        session_id: row.get(1)?,
        channel: row.get(2)?,
        tool_call_id: row.get(3)?,
        kind: row.get(4)?,
        phase: row.get(5)?,
        request: row.get(6)?,
        created_at: row.get(7)?,
        resolved_at: row.get(8)?,
        resolution: row.get(9)?,
        applied_at: row.get(10)?,
    })
}

fn optional_time(ask_id: &str, column: &str, us: Option<i64>) -> Option<Option<DateTime<Utc>>> {
    match us {
        None => Some(None),
        Some(us) => match super::time::from_us(us) {
            Some(t) => Some(Some(t)),
            None => {
                tracing::warn!(ask_id, column, us, "skipping human_asks row: bad timestamp");
                None
            }
        },
    }
}

fn decode(raw: RawRow) -> Option<HumanAskRow> {
    let id = raw.ask_id.as_str();
    let ask_id = match raw.ask_id.parse::<AskId>() {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "skipping human_asks row: bad ask_id");
            return None;
        }
    };
    let Some(kind) = AskKind::from_storage_str(&raw.kind) else {
        tracing::warn!(ask_id = id, kind = %raw.kind, "skipping human_asks row: unknown kind");
        return None;
    };
    let Some(phase) = AskPhase::from_storage_str(&raw.phase) else {
        tracing::warn!(ask_id = id, phase = %raw.phase, "skipping human_asks row: unknown phase");
        return None;
    };
    let request: AskRequest = match serde_json::from_str(&raw.request) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(ask_id = id, error = %e, "skipping human_asks row: bad request json");
            return None;
        }
    };
    let resolution: Option<AskResolution> = match raw.resolution.as_deref() {
        None => None,
        Some(json) => match serde_json::from_str(json) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(ask_id = id, error = %e, "skipping human_asks row: bad resolution json");
                return None;
            }
        },
    };
    let Some(created_at) = super::time::from_us(raw.created_at) else {
        tracing::warn!(ask_id = id, "skipping human_asks row: bad created_at");
        return None;
    };
    let resolved_at = optional_time(id, "resolved_at", raw.resolved_at)?;
    let applied_at = optional_time(id, "applied_at", raw.applied_at)?;
    Some(HumanAskRow {
        ask_id,
        session_id: SessionId::from(raw.session_id),
        channel: ChannelType::from(raw.channel),
        tool_call_id: raw.tool_call_id,
        kind,
        phase,
        request,
        created_at,
        resolved_at,
        resolution,
        applied_at,
    })
}

fn to_json<T: serde::Serialize>(what: &str, value: &T) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|e| StorageError::Storage(format!("serialize human_asks {what}: {e}")))
}

fn list_where(
    conn: &rusqlite::Connection,
    filter: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<Vec<HumanAskRow>> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM human_asks WHERE applied_at IS NULL {filter} \
         ORDER BY created_at, ask_id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params, read_raw)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(raws.into_iter().filter_map(decode).collect())
}

#[async_trait]
impl HumanAskStore for SqliteHumanAskStore {
    async fn create(&self, row: &HumanAskRow) -> Result<()> {
        let ask_id = row.ask_id.to_string();
        let session_id = row.session_id.as_str().to_owned();
        let channel = row.channel.to_string();
        let tool_call_id = row.tool_call_id.clone();
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
                         (ask_id, session_id, channel, tool_call_id, kind, phase, request, \
                          created_at, resolved_at, resolution, applied_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) \
                     ON CONFLICT(ask_id) DO NOTHING",
                    rusqlite::params![
                        ask_id,
                        session_id,
                        channel,
                        tool_call_id,
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
                let raw = conn
                    .query_row(&sql, rusqlite::params![ask_id], read_raw)
                    .optional()?;
                Ok(raw.and_then(decode))
            })
            .await
    }

    async fn list_unapplied(&self) -> Result<Vec<HumanAskRow>> {
        self.pool
            .interact("human_asks.list_unapplied", move |conn| {
                Ok(list_where(conn, "", [])?)
            })
            .await
    }

    async fn list_unapplied_for_session(&self, session_id: &SessionId) -> Result<Vec<HumanAskRow>> {
        let session_id = session_id.as_str().to_owned();
        self.pool
            .interact("human_asks.list_unapplied_for_session", move |conn| {
                Ok(list_where(
                    conn,
                    "AND session_id = ?1",
                    rusqlite::params![session_id],
                )?)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::approval::{ApprovalDecision, ApprovalRequest, ResourceAccess};
    use baybo_model::human_ask::{Question, QuestionOption, QuestionRequest};
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
            kind: AskKind::Approval,
            phase: AskPhase::BeforeExecute,
            request: AskRequest::Approval(ApprovalRequest {
                call_id: ask.into(),
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
            phase: AskPhase::DuringExecute,
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
                "INSERT INTO human_asks (ask_id, session_id, channel, tool_call_id, kind, \
                 phase, request, created_at) \
                 VALUES ('future', 's1', 'web', 'tu', 'telepathy', 'before_execute', '{}', 1000)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();

        let rows = store.list_unapplied().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ask_id.as_str(), "good");
        assert_eq!(store.get(&"future".parse().unwrap()).await.unwrap(), None);
    }
}
