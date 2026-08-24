use async_trait::async_trait;
use rusqlite::OptionalExtension;

use super::SqlitePool;
use baybo_store::StorageError;
use baybo_store::skill_risk::{
    AssessmentJob, AssessmentJobStatus, Result, RiskLevel, RiskVerdict, SkillRiskStore,
};

pub struct SqliteSkillRiskStore {
    pool: SqlitePool,
}

impl SqliteSkillRiskStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SkillRiskStore for SqliteSkillRiskStore {
    async fn get(&self, skill_name: &str, content_hash: &str) -> Result<Option<RiskVerdict>> {
        let name = skill_name.to_string();
        let hash = content_hash.to_string();
        let row: Option<(String, String, String, i64)> = self
            .pool
            .interact("skill_risk.get", move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT level, rationale, model, assessed_at \
                         FROM skill_risk_assessments \
                         WHERE skill_name = ?1 AND content_hash = ?2",
                        rusqlite::params![name, hash],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?)
            })
            .await?;

        match row {
            Some((level, rationale, model, assessed_at)) => {
                let Some(level) = RiskLevel::parse(&level) else {
                    return Err(StorageError::Storage(format!(
                        "unrecognized risk level '{level}' in cache"
                    )));
                };

                Ok(Some(RiskVerdict {
                    skill_name: skill_name.to_string(),
                    content_hash: content_hash.to_string(),
                    level,
                    rationale,
                    model,
                    assessed_at,
                }))
            }
            None => Ok(None),
        }
    }

    async fn put(&self, verdict: &RiskVerdict) -> Result<()> {
        let skill_name = verdict.skill_name.clone();
        let content_hash = verdict.content_hash.clone();
        let level = verdict.level.as_str().to_string();
        let rationale = verdict.rationale.clone();
        let model = verdict.model.clone();
        let assessed_at = verdict.assessed_at;
        self.pool
            .interact_write("skill_risk.put", move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO skill_risk_assessments \
                     (skill_name, content_hash, level, rationale, model, assessed_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        skill_name,
                        content_hash,
                        level,
                        rationale,
                        model,
                        assessed_at,
                    ],
                )?;
                Ok(())
            })
            .await
    }

    async fn forget(&self, skill_name: &str) -> Result<()> {
        let skill_name = skill_name.to_string();
        self.pool
            .interact_write("skill_risk.forget", move |conn| {
                // One transaction because forgetting a skill is one decision:
                // an assessment left without its job (or the reverse) is a
                // state no reader expects. It is also what makes the write
                // retry-safe — a bare second `DELETE` that failed BUSY would
                // otherwise re-run the first on the next attempt.
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                tx.execute(
                    "DELETE FROM skill_risk_assessments WHERE skill_name = ?1",
                    rusqlite::params![skill_name],
                )?;
                tx.execute(
                    "DELETE FROM skill_risk_assessment_jobs WHERE skill_name = ?1",
                    rusqlite::params![skill_name],
                )?;
                tx.commit()?;
                Ok(())
            })
            .await
    }

    async fn upsert_job(&self, job: &AssessmentJob) -> Result<()> {
        let skill_name = job.skill_name.clone();
        let content_hash = job.content_hash.clone();
        let source_path = job.source_path.clone();
        let status = job.status.as_str().to_string();
        let attempts = job.attempts as i64;
        let last_error = job.last_error.clone();
        let created_at = job.created_at;
        let updated_at = job.updated_at;
        self.pool
            .interact_write("skill_risk.upsert_job", move |conn| {
                conn.execute(
                    "INSERT INTO skill_risk_assessment_jobs \
                     (skill_name, content_hash, source_path, status, attempts, last_error, \
                      created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
                     ON CONFLICT(skill_name, content_hash) DO UPDATE SET \
                         source_path = excluded.source_path, \
                         status      = excluded.status, \
                         attempts    = excluded.attempts, \
                         last_error  = excluded.last_error, \
                         updated_at  = excluded.updated_at",
                    rusqlite::params![
                        skill_name,
                        content_hash,
                        source_path,
                        status,
                        attempts,
                        last_error,
                        created_at,
                        updated_at,
                    ],
                )?;
                Ok(())
            })
            .await
    }

    async fn set_job_status(
        &self,
        skill_name: &str,
        content_hash: &str,
        status: AssessmentJobStatus,
        attempts: u32,
        last_error: Option<&str>,
    ) -> Result<()> {
        let skill_name = skill_name.to_string();
        let content_hash = content_hash.to_string();
        let status = status.as_str().to_string();
        let attempts = attempts as i64;
        let last_error = last_error.map(|s| s.to_string());
        let now = super::time::now_us();
        self.pool
            .interact_write("skill_risk.set_job_status", move |conn| {
                conn.execute(
                    "UPDATE skill_risk_assessment_jobs \
                     SET status = ?1, attempts = ?2, last_error = ?3, updated_at = ?4 \
                     WHERE skill_name = ?5 AND content_hash = ?6",
                    rusqlite::params![status, attempts, last_error, now, skill_name, content_hash,],
                )?;
                Ok(())
            })
            .await
    }

    async fn delete_job(&self, skill_name: &str, content_hash: &str) -> Result<()> {
        let skill_name = skill_name.to_string();
        let content_hash = content_hash.to_string();
        self.pool
            .interact_write("skill_risk.delete_job", move |conn| {
                conn.execute(
                    "DELETE FROM skill_risk_assessment_jobs \
                     WHERE skill_name = ?1 AND content_hash = ?2",
                    rusqlite::params![skill_name, content_hash],
                )?;
                Ok(())
            })
            .await
    }

    async fn load_pending_jobs(&self) -> Result<Vec<AssessmentJob>> {
        type TurnRow = (
            String,
            String,
            String,
            String,
            i64,
            Option<String>,
            i64,
            i64,
        );
        let rows: Vec<TurnRow> = self
            .pool
            .interact("skill_risk.load_pending_jobs", move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT skill_name, content_hash, source_path, status, attempts, last_error, \
                            created_at, updated_at \
                     FROM skill_risk_assessment_jobs \
                     ORDER BY created_at ASC",
                )?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;

        let mut out = Vec::new();
        for (
            skill_name,
            content_hash,
            source_path,
            status,
            attempts,
            last_error,
            created_at,
            updated_at,
        ) in rows
        {
            let Some(status) = AssessmentJobStatus::parse(&status) else {
                return Err(StorageError::Storage(format!(
                    "unrecognised risk-job status '{status}' in cache"
                )));
            };

            out.push(AssessmentJob {
                skill_name,
                content_hash,
                source_path,
                status,
                attempts: attempts.max(0) as u32,
                last_error,
                created_at,
                updated_at,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_verdict(name: &str, hash: &str, level: RiskLevel) -> RiskVerdict {
        RiskVerdict {
            skill_name: name.to_string(),
            content_hash: hash.to_string(),
            level,
            rationale: "looks fine".to_string(),
            model: "test-model".to_string(),
            assessed_at: 1_700_000_000_000_000,
        }
    }

    fn make_job(name: &str, hash: &str, status: AssessmentJobStatus) -> AssessmentJob {
        AssessmentJob {
            skill_name: name.to_string(),
            content_hash: hash.to_string(),
            source_path: format!("/skills/{name}"),
            status,
            attempts: 0,
            last_error: None,
            created_at: 1_700_000_000_000_000,
            updated_at: 1_700_000_000_000_000,
        }
    }

    #[tokio::test]
    async fn put_then_get_roundtrips() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteSkillRiskStore::new(pool);

        let v = make_verdict("deploy", "abc123", RiskLevel::Suspicious);
        store.put(&v).await.unwrap();

        let got = store.get("deploy", "abc123").await.unwrap().unwrap();
        assert_eq!(got.level, RiskLevel::Suspicious);
        assert_eq!(got.rationale, "looks fine");
        assert_eq!(got.model, "test-model");
    }

    #[tokio::test]
    async fn get_misses_when_hash_differs() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteSkillRiskStore::new(pool);

        store
            .put(&make_verdict("deploy", "abc123", RiskLevel::Safe))
            .await
            .unwrap();
        assert!(store.get("deploy", "different").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn put_replaces_same_key() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteSkillRiskStore::new(pool);

        store
            .put(&make_verdict("deploy", "h", RiskLevel::Safe))
            .await
            .unwrap();
        store
            .put(&make_verdict("deploy", "h", RiskLevel::Dangerous))
            .await
            .unwrap();

        let got = store.get("deploy", "h").await.unwrap().unwrap();
        assert_eq!(got.level, RiskLevel::Dangerous);
    }

    #[tokio::test]
    async fn forget_removes_verdicts_and_jobs() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteSkillRiskStore::new(pool);

        store
            .put(&make_verdict("deploy", "v1", RiskLevel::Safe))
            .await
            .unwrap();
        store
            .upsert_job(&make_job("deploy", "full-v1", AssessmentJobStatus::Pending))
            .await
            .unwrap();
        store
            .put(&make_verdict("greet", "v1", RiskLevel::Safe))
            .await
            .unwrap();

        store.forget("deploy").await.unwrap();

        assert!(store.get("deploy", "v1").await.unwrap().is_none());
        assert!(store.get("greet", "v1").await.unwrap().is_some());
        let remaining = store.load_pending_jobs().await.unwrap();
        assert!(remaining.iter().all(|j| j.skill_name != "deploy"));
    }

    #[tokio::test]
    async fn upsert_job_is_idempotent() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteSkillRiskStore::new(pool);

        let mut job = make_job("deploy", "hash-1", AssessmentJobStatus::Pending);
        store.upsert_job(&job).await.unwrap();

        job.attempts = 2;
        job.status = AssessmentJobStatus::Failed;
        job.last_error = Some("transient timeout".to_string());
        store.upsert_job(&job).await.unwrap();

        let loaded = store.load_pending_jobs().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].attempts, 2);
        assert_eq!(loaded[0].status, AssessmentJobStatus::Failed);
        assert_eq!(loaded[0].last_error.as_deref(), Some("transient timeout"));
    }

    #[tokio::test]
    async fn set_job_status_updates_row() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteSkillRiskStore::new(pool);

        store
            .upsert_job(&make_job("deploy", "h", AssessmentJobStatus::Pending))
            .await
            .unwrap();
        store
            .set_job_status("deploy", "h", AssessmentJobStatus::InProgress, 1, None)
            .await
            .unwrap();
        let loaded = store.load_pending_jobs().await.unwrap();
        assert_eq!(loaded[0].status, AssessmentJobStatus::InProgress);
        assert_eq!(loaded[0].attempts, 1);
    }

    #[tokio::test]
    async fn delete_job_removes_only_that_row() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteSkillRiskStore::new(pool);

        store
            .upsert_job(&make_job("deploy", "h1", AssessmentJobStatus::Pending))
            .await
            .unwrap();
        store
            .upsert_job(&make_job("deploy", "h2", AssessmentJobStatus::Pending))
            .await
            .unwrap();

        store.delete_job("deploy", "h1").await.unwrap();

        let loaded = store.load_pending_jobs().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].content_hash, "h2");
    }

    #[tokio::test]
    async fn load_pending_jobs_orders_by_created_at() {
        let tmpdir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(tmpdir.path().join("test.db"))
            .await
            .unwrap();
        let store = SqliteSkillRiskStore::new(pool);

        let mut a = make_job("a", "h", AssessmentJobStatus::Pending);
        a.created_at = 100;
        a.updated_at = 100;
        let mut b = make_job("b", "h", AssessmentJobStatus::Pending);
        b.created_at = 50;
        b.updated_at = 50;

        store.upsert_job(&a).await.unwrap();
        store.upsert_job(&b).await.unwrap();

        let loaded = store.load_pending_jobs().await.unwrap();
        assert_eq!(loaded[0].skill_name, "b");
        assert_eq!(loaded[1].skill_name, "a");
    }
}
