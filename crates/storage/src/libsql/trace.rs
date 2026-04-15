use async_trait::async_trait;
use chrono::Utc;

use super::LibsqlPool;
use crate::trace::{Result, TraceStore};
use aura_trace::{SessionTrace, TraceError, TraceFilter, TraceNode, TraceNodeId};

pub struct LibsqlTraceStore {
    pool: LibsqlPool,
}

impl LibsqlTraceStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TraceStore for LibsqlTraceStore {
    async fn save_trace(&self, trace: &SessionTrace) -> Result<()> {
        let conn = self.pool.conn();
        let session_id = &trace.session_id;
        let trace_data = serde_json::to_string(trace)
            .map_err(|e| TraceError::Storage(format!("serialize session trace: {e}")))?;

        let node_rows: Vec<(String, String)> = trace
            .nodes
            .iter()
            .map(|(node_id, node)| {
                let node_data = serde_json::to_string(node)
                    .map_err(|e| TraceError::Storage(format!("serialize trace node: {e}")))?;
                Ok((node_id.clone(), node_data))
            })
            .collect::<Result<Vec<_>>>()?;

        let tx = conn
            .transaction()
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql begin transaction: {e}")))?;

        tx.execute(
            "INSERT OR REPLACE INTO session_traces (session_id, data) VALUES (?1, ?2)",
            libsql::params![session_id.clone(), trace_data],
        )
        .await
        .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql insert session_trace: {e}")))?;

        // Soft-delete every prior node for this session; any node_id that the
        // new trace re-introduces will be revived by the INSERT OR REPLACE
        // below (which defaults deleted_at back to NULL).
        let now = Utc::now().timestamp();
        tx.execute(
            "UPDATE trace_nodes SET deleted_at = ?1 \
             WHERE session_id = ?2 AND deleted_at IS NULL",
            libsql::params![now, session_id.clone()],
        )
        .await
        .map_err(|e| {
            TraceError::Internal(anyhow::anyhow!("libsql soft-delete old trace_nodes: {e}"))
        })?;

        for (node_id, node_data) in &node_rows {
            tx.execute(
                "INSERT OR REPLACE INTO trace_nodes (id, session_id, data) VALUES (?1, ?2, ?3)",
                libsql::params![node_id.clone(), session_id.clone(), node_data.clone()],
            )
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql insert trace_node: {e}")))?;
        }

        tx.commit()
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql commit: {e}")))?;
        Ok(())
    }

    async fn load_trace(&self, session_id: &str) -> Result<Option<SessionTrace>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM session_traces WHERE session_id = ?1",
                libsql::params![session_id.to_string()],
            )
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let row = rows
            .next()
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql row: {e}")))?;

        match row {
            Some(row) => {
                let data: String = row
                    .get(0)
                    .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                let trace: SessionTrace = serde_json::from_str(&data)
                    .map_err(|e| TraceError::Storage(format!("deserialize session trace: {e}")))?;
                Ok(Some(trace))
            }
            None => Ok(None),
        }
    }

    async fn query_traces(&self, filter: TraceFilter) -> Result<Vec<SessionTrace>> {
        let conn = self.pool.conn();

        let mut rows = if let Some(ref sid) = filter.session_id {
            conn.query(
                "SELECT data FROM session_traces WHERE session_id = ?1",
                libsql::params![sid.clone()],
            )
            .await
        } else {
            conn.query("SELECT data FROM session_traces", ()).await
        }
        .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let mut traces = Vec::new();
        while let Some(row) = rows
            .next()
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql row: {e}")))?
        {
            let data: String = row
                .get(0)
                .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
            let trace: SessionTrace = serde_json::from_str(&data)
                .map_err(|e| TraceError::Storage(format!("deserialize session trace: {e}")))?;

            if let Some(ref from) = filter.from {
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
    }

    async fn load_node(
        &self,
        session_id: &str,
        node_id: &TraceNodeId,
    ) -> Result<Option<TraceNode>> {
        let conn = self.pool.conn();
        let mut rows = conn
            .query(
                "SELECT data FROM trace_nodes \
                 WHERE session_id = ?1 AND id = ?2 AND deleted_at IS NULL",
                libsql::params![session_id.to_string(), node_id.clone()],
            )
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql query: {e}")))?;

        let row = rows
            .next()
            .await
            .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql row: {e}")))?;

        match row {
            Some(row) => {
                let data: String = row
                    .get(0)
                    .map_err(|e| TraceError::Internal(anyhow::anyhow!("libsql get: {e}")))?;
                let node: TraceNode = serde_json::from_str(&data)
                    .map_err(|e| TraceError::Storage(format!("deserialize trace node: {e}")))?;
                Ok(Some(node))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_job::OperationKind;
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
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlTraceStore::new(pool);

        let trace = make_trace("s1");
        store.save_trace(&trace).await.unwrap();

        let loaded = store.load_trace("s1").await.unwrap();
        assert!(loaded.is_some());
        assert_eq!(loaded.as_ref().map(|t| t.session_id.as_str()), Some("s1"));
    }

    #[tokio::test]
    async fn test_trace_load_node() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlTraceStore::new(pool);

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
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlTraceStore::new(pool);

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
