use async_trait::async_trait;

use aura_core::{AuraError, Result};
use aura_trace::{SessionTrace, TraceFilter, TraceNode, TraceNodeId, TraceStore};

use crate::sqlite::SqlitePool;

/// SQLite-backed implementation of [`TraceStore`].
///
/// `save_trace` uses a transaction to atomically persist both the session-level
/// trace and its individual nodes.
pub struct SqliteTraceStore {
    pool: SqlitePool,
}

impl SqliteTraceStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TraceStore for SqliteTraceStore {
    async fn save_trace(&self, trace: &SessionTrace) -> Result<()> {
        let pool = self.pool.clone();
        let session_id = trace.session_id.clone();
        let trace_data = serde_json::to_string(trace)
            .map_err(|e| AuraError::Serialization(format!("serialize session trace: {e}")))?;

        // Pre-serialize all nodes before entering the blocking closure.
        let node_rows: Vec<(String, String, String)> = trace
            .nodes
            .iter()
            .map(|(node_id, node)| {
                let node_data = serde_json::to_string(node)
                    .map_err(|e| AuraError::Serialization(format!("serialize trace node: {e}")))?;
                Ok((node_id.clone(), session_id.clone(), node_data))
            })
            .collect::<Result<Vec<_>>>()?;

        tokio::task::spawn_blocking(move || {
            let mut conn = pool.lock()?;
            let tx = conn.transaction().map_err(|e| {
                AuraError::Internal(anyhow::anyhow!("sqlite begin transaction: {e}"))
            })?;

            // Upsert the session trace.
            tx.execute(
                "INSERT OR REPLACE INTO session_traces (session_id, data) VALUES (?1, ?2)",
                rusqlite::params![session_id, trace_data],
            )
            .map_err(|e| {
                AuraError::Internal(anyhow::anyhow!("sqlite insert session_trace: {e}"))
            })?;

            // Remove old nodes for this session, then insert the current set.
            tx.execute(
                "DELETE FROM trace_nodes WHERE session_id = ?1",
                rusqlite::params![session_id],
            )
            .map_err(|e| {
                AuraError::Internal(anyhow::anyhow!("sqlite delete old trace_nodes: {e}"))
            })?;

            for (node_id, sid, node_data) in &node_rows {
                tx.execute(
                    "INSERT INTO trace_nodes (id, session_id, data) VALUES (?1, ?2, ?3)",
                    rusqlite::params![node_id, sid, node_data],
                )
                .map_err(|e| {
                    AuraError::Internal(anyhow::anyhow!("sqlite insert trace_node: {e}"))
                })?;
            }

            tx.commit().map_err(|e| {
                AuraError::Internal(anyhow::anyhow!("sqlite commit transaction: {e}"))
            })?;
            Ok(())
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join: {e}")))?
    }

    async fn load_trace(&self, session_id: &str) -> Result<Option<SessionTrace>> {
        let pool = self.pool.clone();
        let session_id = session_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare("SELECT data FROM session_traces WHERE session_id = ?1")
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite prepare: {e}")))?;
            let result = stmt
                .query_row(rusqlite::params![session_id], |row| {
                    let data: String = row.get(0)?;
                    Ok(data)
                })
                .optional()
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query: {e}")))?;
            match result {
                Some(data) => {
                    let trace: SessionTrace = serde_json::from_str(&data).map_err(|e| {
                        AuraError::Serialization(format!("deserialize session trace: {e}"))
                    })?;
                    Ok(Some(trace))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join: {e}")))?
    }

    async fn query_traces(&self, filter: TraceFilter) -> Result<Vec<SessionTrace>> {
        let pool = self.pool.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;

            // Build a dynamic query based on the filter.
            let mut sql = String::from("SELECT data FROM session_traces WHERE 1=1");
            let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

            if let Some(ref sid) = filter.session_id {
                sql.push_str(" AND session_id = ?");
                params.push(Box::new(sid.clone()));
            }

            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite prepare: {e}")))?;

            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();

            let rows = stmt
                .query_map(param_refs.as_slice(), |row| {
                    let data: String = row.get(0)?;
                    Ok(data)
                })
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query: {e}")))?;

            let mut traces = Vec::new();
            for row in rows {
                let data =
                    row.map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite row: {e}")))?;
                let trace: SessionTrace = serde_json::from_str(&data).map_err(|e| {
                    AuraError::Serialization(format!("deserialize session trace: {e}"))
                })?;

                // Apply time-range filtering in-memory since the data is JSON.
                if let Some(ref from) = filter.from {
                    // Check if any node started after `from`.
                    let dominated = trace.nodes.values().all(|n| n.span.started_at < *from);
                    if dominated {
                        continue;
                    }
                }
                if let Some(ref to) = filter.to {
                    let dominated = trace.nodes.values().all(|n| n.span.started_at >= *to);
                    if dominated {
                        continue;
                    }
                }

                traces.push(trace);
            }
            Ok(traces)
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join: {e}")))?
    }

    async fn load_node(
        &self,
        session_id: &str,
        node_id: &TraceNodeId,
    ) -> Result<Option<TraceNode>> {
        let pool = self.pool.clone();
        let session_id = session_id.to_string();
        let node_id = node_id.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.lock()?;
            let mut stmt = conn
                .prepare("SELECT data FROM trace_nodes WHERE session_id = ?1 AND id = ?2")
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite prepare: {e}")))?;
            let result = stmt
                .query_row(rusqlite::params![session_id, node_id], |row| {
                    let data: String = row.get(0)?;
                    Ok(data)
                })
                .optional()
                .map_err(|e| AuraError::Internal(anyhow::anyhow!("sqlite query: {e}")))?;
            match result {
                Some(data) => {
                    let node: TraceNode = serde_json::from_str(&data).map_err(|e| {
                        AuraError::Serialization(format!("deserialize trace node: {e}"))
                    })?;
                    Ok(Some(node))
                }
                None => Ok(None),
            }
        })
        .await
        .map_err(|e| AuraError::Internal(anyhow::anyhow!("spawn_blocking join: {e}")))?
    }
}

/// Helper to convert `rusqlite::Error::QueryReturnedNoRows` into `None`.
trait OptionalExt<T> {
    fn optional(self) -> std::result::Result<Option<T>, rusqlite::Error>;
}

impl<T> OptionalExt<T> for std::result::Result<T, rusqlite::Error> {
    fn optional(self) -> std::result::Result<Option<T>, rusqlite::Error> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_core::OperationKind;
    use aura_trace::{ExecutionProvenance, SpanInput, TraceSpan};
    use chrono::Utc;
    use std::collections::HashMap;

    fn make_trace(session_id: &str) -> SessionTrace {
        let node_id = "n1".to_string();
        let node = TraceNode {
            id: node_id.clone(),
            parent: None,
            children: vec![],
            span: TraceSpan {
                kind: OperationKind::LlmCall {
                    model: "gpt-4".to_string(),
                },
                job_id: None,
                provenance: ExecutionProvenance::default(),
                input: SpanInput::None,
                started_at: Utc::now(),
                ended_at: None,
                result: None,
            },
            context_snapshot: None,
        };
        let mut nodes = HashMap::new();
        nodes.insert(node_id.clone(), node);
        SessionTrace {
            session_id: session_id.to_string(),
            root: node_id.clone(),
            nodes,
            forks: vec![],
            active_leaf: node_id,
        }
    }

    #[tokio::test]
    async fn test_trace_save_and_load() {
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteTraceStore::new(pool);

        let trace = make_trace("s1");
        store.save_trace(&trace).await.unwrap();

        let loaded = store.load_trace("s1").await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.as_ref().map(|t| t.session_id.as_str()), Some("s1"));
    }

    #[tokio::test]
    async fn test_trace_load_node() {
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteTraceStore::new(pool);

        let trace = make_trace("s1");
        store.save_trace(&trace).await.unwrap();

        let node = store.load_node("s1", &"n1".to_string()).await.unwrap();
        assert!(node.is_some());
        assert_eq!(node.as_ref().map(|n| n.id.as_str()), Some("n1"));

        let missing = store.load_node("s1", &"n999".to_string()).await.unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn test_trace_query_by_session_id() {
        let pool = SqlitePool::open_in_memory().unwrap();
        let store = SqliteTraceStore::new(pool);

        store.save_trace(&make_trace("s1")).await.unwrap();
        store.save_trace(&make_trace("s2")).await.unwrap();

        let filter = TraceFilter {
            session_id: Some("s1".to_string()),
            from: None,
            to: None,
        };
        let results = store.query_traces(filter).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].session_id, "s1");
    }
}
