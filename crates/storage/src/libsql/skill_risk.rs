use async_trait::async_trait;

use super::LibsqlPool;
use baybo_store::StorageError;
use baybo_store::skill_risk::{
    AssessmentJob, AssessmentJobStatus, Result, RiskLevel, RiskVerdict, SkillRiskStore,
};

pub struct LibsqlSkillRiskStore {
    pool: LibsqlPool,
}

impl LibsqlSkillRiskStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SkillRiskStore for LibsqlSkillRiskStore {
    async fn get(&self, skill_name: &str, content_hash: &str) -> Result<Option<RiskVerdict>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT level, rationale, model, assessed_at \
                 FROM skill_risk_assessments \
                 WHERE skill_name = ?1 AND content_hash = ?2",
                libsql::params![skill_name.to_string(), content_hash.to_string()],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query risk: {e}")))?;

        let row = rows
            .next()
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row risk: {e}")))?;

        match row {
            Some(row) => {
                let level: String = row.get(0).map_err(|e| {
                    StorageError::Internal(anyhow::anyhow!("libsql get level: {e}"))
                })?;
                let rationale: String = row.get(1).map_err(|e| {
                    StorageError::Internal(anyhow::anyhow!("libsql get rationale: {e}"))
                })?;
                let model: String = row.get(2).map_err(|e| {
                    StorageError::Internal(anyhow::anyhow!("libsql get model: {e}"))
                })?;
                let assessed_at: i64 = row.get(3).map_err(|e| {
                    StorageError::Internal(anyhow::anyhow!("libsql get assessed_at: {e}"))
                })?;

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
        let conn = self.pool.conn();
        conn.execute(
            "INSERT OR REPLACE INTO skill_risk_assessments \
             (skill_name, content_hash, level, rationale, model, assessed_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            libsql::params![
                verdict.skill_name.clone(),
                verdict.content_hash.clone(),
                verdict.level.as_str().to_string(),
                verdict.rationale.clone(),
                verdict.model.clone(),
                verdict.assessed_at,
            ],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql insert risk: {e}")))?;
        Ok(())
    }

    async fn forget(&self, skill_name: &str) -> Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "DELETE FROM skill_risk_assessments WHERE skill_name = ?1",
            libsql::params![skill_name.to_string()],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql delete risk: {e}")))?;
        conn.execute(
            "DELETE FROM skill_risk_assessment_jobs WHERE skill_name = ?1",
            libsql::params![skill_name.to_string()],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql delete risk jobs: {e}")))?;
        Ok(())
    }

    async fn upsert_job(&self, job: &AssessmentJob) -> Result<()> {
        let conn = self.pool.conn();
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
            libsql::params![
                job.skill_name.clone(),
                job.content_hash.clone(),
                job.source_path.clone(),
                job.status.as_str().to_string(),
                job.attempts as i64,
                job.last_error.clone(),
                job.created_at,
                job.updated_at,
            ],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql upsert risk job: {e}")))?;
        Ok(())
    }

    async fn set_job_status(
        &self,
        skill_name: &str,
        content_hash: &str,
        status: AssessmentJobStatus,
        attempts: u32,
        last_error: Option<&str>,
    ) -> Result<()> {
        let conn = self.pool.conn();
        let now = super::time::now_us();
        conn.execute(
            "UPDATE skill_risk_assessment_jobs \
             SET status = ?1, attempts = ?2, last_error = ?3, updated_at = ?4 \
             WHERE skill_name = ?5 AND content_hash = ?6",
            libsql::params![
                status.as_str().to_string(),
                attempts as i64,
                last_error.map(|s| s.to_string()),
                now,
                skill_name.to_string(),
                content_hash.to_string(),
            ],
        )
        .await
        .map_err(|e| {
            StorageError::Internal(anyhow::anyhow!("libsql update risk job status: {e}"))
        })?;
        Ok(())
    }

    async fn delete_job(&self, skill_name: &str, content_hash: &str) -> Result<()> {
        let conn = self.pool.conn();
        conn.execute(
            "DELETE FROM skill_risk_assessment_jobs \
             WHERE skill_name = ?1 AND content_hash = ?2",
            libsql::params![skill_name.to_string(), content_hash.to_string()],
        )
        .await
        .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql delete risk job: {e}")))?;
        Ok(())
    }

    async fn load_pending_jobs(&self) -> Result<Vec<AssessmentJob>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT skill_name, content_hash, source_path, status, attempts, last_error, \
                        created_at, updated_at \
                 FROM skill_risk_assessment_jobs \
                 ORDER BY created_at ASC",
                libsql::params![],
            )
            .await
            .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql query risk jobs: {e}")))?;

        let mut out = Vec::new();
        loop {
            let row = rows
                .next()
                .await
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql row risk job: {e}")))?;
            let Some(row) = row else { break };

            let skill_name: String = row.get(0).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get skill_name: {e}"))
            })?;
            let content_hash: String = row.get(1).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get content_hash: {e}"))
            })?;
            let source_path: String = row.get(2).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get source_path: {e}"))
            })?;
            let status: String = row
                .get(3)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get status: {e}")))?;
            let attempts: i64 = row
                .get(4)
                .map_err(|e| StorageError::Internal(anyhow::anyhow!("libsql get attempts: {e}")))?;
            let last_error: Option<String> = row.get(5).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get last_error: {e}"))
            })?;
            let created_at: i64 = row.get(6).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get created_at: {e}"))
            })?;
            let updated_at: i64 = row.get(7).map_err(|e| {
                StorageError::Internal(anyhow::anyhow!("libsql get updated_at: {e}"))
            })?;

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
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSkillRiskStore::new(pool);

        let v = make_verdict("deploy", "abc123", RiskLevel::Suspicious);
        store.put(&v).await.unwrap();

        let got = store.get("deploy", "abc123").await.unwrap().unwrap();
        assert_eq!(got.level, RiskLevel::Suspicious);
        assert_eq!(got.rationale, "looks fine");
        assert_eq!(got.model, "test-model");
    }

    #[tokio::test]
    async fn get_misses_when_hash_differs() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSkillRiskStore::new(pool);

        store
            .put(&make_verdict("deploy", "abc123", RiskLevel::Safe))
            .await
            .unwrap();
        assert!(store.get("deploy", "different").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn put_replaces_same_key() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSkillRiskStore::new(pool);

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
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSkillRiskStore::new(pool);

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
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSkillRiskStore::new(pool);

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
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSkillRiskStore::new(pool);

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
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSkillRiskStore::new(pool);

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
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlSkillRiskStore::new(pool);

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
