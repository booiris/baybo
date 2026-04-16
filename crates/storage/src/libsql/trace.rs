use std::collections::HashMap;

use async_trait::async_trait;
use chrono::Utc;

use super::LibsqlPool;
use crate::trace::{Result, TraceStore};
use aura_trace::{
    ExecutionProvenance, ForkRecord, SessionTrace, SpanInput, SpanResult, TraceError, TraceFilter,
    TraceNode, TraceNodeId, TraceSpan,
};

pub struct LibsqlTraceStore {
    pool: LibsqlPool,
}

impl LibsqlTraceStore {
    pub fn new(pool: LibsqlPool) -> Self {
        Self { pool }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn ser<T: serde::Serialize>(val: &T) -> Result<String> {
    serde_json::to_string(val).map_err(|e| TraceError::Storage(format!("serialize: {e}")))
}

fn de<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(|e| TraceError::Storage(format!("deserialize: {e}")))
}

fn ie(msg: impl std::fmt::Display) -> TraceError {
    TraceError::Internal(anyhow::anyhow!("{msg}"))
}

// ---------------------------------------------------------------------------
// TraceStore implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl TraceStore for LibsqlTraceStore {
    async fn save_trace(&self, trace: &SessionTrace) -> Result<()> {
        let conn = self.pool.conn();
        let sid = &trace.session_id;
        let now = Utc::now().to_rfc3339();

        let tx = conn
            .transaction()
            .await
            .map_err(|e| ie(format!("begin tx: {e}")))?;

        // Upsert trace header (reset deleted_at on re-insert to revive soft-deleted rows).
        tx.execute(
            "INSERT INTO session_traces (session_id, root_node, active_leaf, created_at, updated_at, deleted_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL)
             ON CONFLICT(session_id) DO UPDATE SET
                 root_node   = excluded.root_node,
                 active_leaf = excluded.active_leaf,
                 updated_at  = excluded.updated_at,
                 deleted_at  = NULL",
            libsql::params![
                sid.clone(),
                trace.root.clone(),
                trace.active_leaf.clone(),
                now.clone(),
                now.clone()
            ],
        )
        .await
        .map_err(|e| ie(format!("upsert session_traces: {e}")))?;

        // Soft-delete all existing nodes; current nodes are revived below.
        let ts = Utc::now().timestamp();
        tx.execute(
            "UPDATE trace_nodes SET deleted_at = ?1 WHERE session_id = ?2 AND deleted_at IS NULL",
            libsql::params![ts, sid.clone()],
        )
        .await
        .map_err(|e| ie(format!("soft-delete trace_nodes: {e}")))?;

        // Upsert every node with proper columns.
        for node in trace.nodes.values() {
            let kind_json = ser(&node.span.kind)?;
            let prov_json = ser(&node.span.provenance)?;
            let input_json = ser(&node.span.input)?;
            let result_json: Option<String> = node.span.result.as_ref().map(ser).transpose()?;
            let started = node.span.started_at.to_rfc3339();
            let ended: Option<String> = node.span.ended_at.map(|t| t.to_rfc3339());
            let snap_json: Option<String> =
                node.context_snapshot.as_ref().map(ser).transpose()?;

            tx.execute(
                "INSERT OR REPLACE INTO trace_nodes
                    (id, session_id, parent_id, kind, job_id, provenance,
                     input, result, started_at, ended_at, snapshot, deleted_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL)",
                libsql::params![
                    node.id.clone(),
                    sid.clone(),
                    node.parent.clone().unwrap_or_default(),
                    kind_json,
                    node.span.job_id.clone().unwrap_or_default(),
                    prov_json,
                    input_json,
                    result_json.unwrap_or_default(),
                    started,
                    ended.unwrap_or_default(),
                    snap_json.unwrap_or_default()
                ],
            )
            .await
            .map_err(|e| ie(format!("upsert trace_node {}: {e}", node.id)))?;
        }

        // Soft-delete old forks; current forks are revived below.
        tx.execute(
            "UPDATE trace_forks SET deleted_at = ?1 WHERE session_id = ?2 AND deleted_at IS NULL",
            libsql::params![ts, sid.clone()],
        )
        .await
        .map_err(|e| ie(format!("soft-delete trace_forks: {e}")))?;

        for fork in &trace.forks {
            tx.execute(
                "INSERT OR REPLACE INTO trace_forks
                    (id, session_id, from_node, fork_root, reason, created_at, deleted_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                libsql::params![
                    fork.id.clone(),
                    sid.clone(),
                    fork.from_node.clone(),
                    fork.fork_root.clone(),
                    fork.reason.clone(),
                    fork.created_at.to_rfc3339()
                ],
            )
            .await
            .map_err(|e| ie(format!("upsert trace_fork {}: {e}", fork.id)))?;
        }

        tx.commit()
            .await
            .map_err(|e| ie(format!("commit: {e}")))?;
        Ok(())
    }

    async fn load_trace(&self, session_id: &str) -> Result<Option<SessionTrace>> {
        let conn = self.pool.conn();

        // Load header.
        let mut rows = conn
            .query(
                "SELECT root_node, active_leaf FROM session_traces WHERE session_id = ?1 AND deleted_at IS NULL",
                libsql::params![session_id.to_string()],
            )
            .await
            .map_err(|e| ie(format!("query session_traces: {e}")))?;

        let Some(hdr) = rows.next().await.map_err(|e| ie(format!("row: {e}")))? else {
            return Ok(None);
        };

        let root: String = hdr.get(0).map_err(|e| ie(format!("get root: {e}")))?;
        let active_leaf: String = hdr.get(1).map_err(|e| ie(format!("get leaf: {e}")))?;

        // Load all live nodes.
        let nodes = self.load_nodes(conn, session_id).await?;

        // Load forks.
        let forks = self.load_forks(conn, session_id).await?;

        Ok(Some(SessionTrace {
            session_id: session_id.to_string(),
            root,
            nodes,
            forks,
            active_leaf,
        }))
    }

    async fn query_traces(&self, filter: TraceFilter) -> Result<Vec<SessionTrace>> {
        let conn = self.pool.conn();

        // Query headers.
        let mut rows = if let Some(ref sid) = filter.session_id {
            conn.query(
                "SELECT session_id, root_node, active_leaf FROM session_traces WHERE session_id = ?1 AND deleted_at IS NULL",
                libsql::params![sid.clone()],
            )
            .await
        } else {
            conn.query(
                "SELECT session_id, root_node, active_leaf FROM session_traces WHERE deleted_at IS NULL",
                (),
            )
            .await
        }
        .map_err(|e| ie(format!("query session_traces: {e}")))?;

        let mut headers = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| ie(format!("row: {e}")))? {
            let sid: String = row.get(0).map_err(|e| ie(format!("get sid: {e}")))?;
            let root: String = row.get(1).map_err(|e| ie(format!("get root: {e}")))?;
            let leaf: String = row.get(2).map_err(|e| ie(format!("get leaf: {e}")))?;
            headers.push((sid, root, leaf));
        }

        let mut traces = Vec::new();
        for (sid, root, leaf) in headers {
            let nodes = self.load_nodes(conn, &sid).await?;
            let forks = self.load_forks(conn, &sid).await?;

            let trace = SessionTrace {
                session_id: sid,
                root,
                nodes,
                forks,
                active_leaf: leaf,
            };

            // Apply time filters.
            if let Some(ref from) = filter.from
                && trace.nodes.values().all(|n| n.span.started_at < *from)
            {
                continue;
            }
            if let Some(ref to) = filter.to
                && trace.nodes.values().all(|n| n.span.started_at >= *to)
            {
                continue;
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
                "SELECT id, parent_id, kind, job_id, provenance, input,
                        result, started_at, ended_at, snapshot
                 FROM trace_nodes
                 WHERE session_id = ?1 AND id = ?2 AND deleted_at IS NULL",
                libsql::params![session_id.to_string(), node_id.clone()],
            )
            .await
            .map_err(|e| ie(format!("query trace_node: {e}")))?;

        let Some(row) = rows.next().await.map_err(|e| ie(format!("row: {e}")))? else {
            return Ok(None);
        };

        let node = Self::row_to_node(&row)?;

        // Populate children by querying nodes whose parent_id matches.
        let mut child_rows = conn
            .query(
                "SELECT id FROM trace_nodes
                 WHERE session_id = ?1 AND parent_id = ?2 AND deleted_at IS NULL",
                libsql::params![session_id.to_string(), node_id.clone()],
            )
            .await
            .map_err(|e| ie(format!("query children: {e}")))?;

        let mut children = Vec::new();
        while let Some(cr) = child_rows
            .next()
            .await
            .map_err(|e| ie(format!("child row: {e}")))?
        {
            let cid: String = cr.get(0).map_err(|e| ie(format!("get child id: {e}")))?;
            children.push(cid);
        }

        Ok(Some(TraceNode { children, ..node }))
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

impl LibsqlTraceStore {
    /// Load all live nodes for a session and reconstruct the children lists
    /// from `parent_id` references.
    async fn load_nodes(
        &self,
        conn: &libsql::Connection,
        session_id: &str,
    ) -> Result<HashMap<TraceNodeId, TraceNode>> {
        let mut rows = conn
            .query(
                "SELECT id, parent_id, kind, job_id, provenance, input,
                        result, started_at, ended_at, snapshot
                 FROM trace_nodes
                 WHERE session_id = ?1 AND deleted_at IS NULL
                 ORDER BY started_at",
                libsql::params![session_id.to_string()],
            )
            .await
            .map_err(|e| ie(format!("query trace_nodes: {e}")))?;

        let mut nodes = HashMap::new();
        while let Some(row) = rows.next().await.map_err(|e| ie(format!("row: {e}")))? {
            let node = Self::row_to_node(&row)?;
            nodes.insert(node.id.clone(), node);
        }

        // Derive children from parent_id.
        let parent_map: Vec<(TraceNodeId, Option<TraceNodeId>)> = nodes
            .values()
            .map(|n| (n.id.clone(), n.parent.clone()))
            .collect();
        for (child_id, parent_id) in parent_map {
            if let Some(pid) = parent_id
                && let Some(parent) = nodes.get_mut(&pid)
            {
                parent.children.push(child_id);
            }
        }

        Ok(nodes)
    }

    /// Load fork records for a session.
    async fn load_forks(
        &self,
        conn: &libsql::Connection,
        session_id: &str,
    ) -> Result<Vec<ForkRecord>> {
        let mut rows = conn
            .query(
                "SELECT id, from_node, fork_root, reason, created_at
                 FROM trace_forks WHERE session_id = ?1 AND deleted_at IS NULL ORDER BY created_at",
                libsql::params![session_id.to_string()],
            )
            .await
            .map_err(|e| ie(format!("query trace_forks: {e}")))?;

        let mut forks = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| ie(format!("row: {e}")))? {
            let id: String = row.get(0).map_err(|e| ie(format!("get: {e}")))?;
            let from_node: String = row.get(1).map_err(|e| ie(format!("get: {e}")))?;
            let fork_root: String = row.get(2).map_err(|e| ie(format!("get: {e}")))?;
            let reason: String = row.get(3).map_err(|e| ie(format!("get: {e}")))?;
            let created_str: String = row.get(4).map_err(|e| ie(format!("get: {e}")))?;
            let created_at = chrono::DateTime::parse_from_rfc3339(&created_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            forks.push(ForkRecord {
                id,
                from_node,
                fork_root,
                reason,
                created_at,
            });
        }
        Ok(forks)
    }

    /// Convert a single database row into a `TraceNode` (with empty children;
    /// the caller fills them in from `parent_id` relationships).
    fn row_to_node(row: &libsql::Row) -> Result<TraceNode> {
        let id: String = row.get(0).map_err(|e| ie(format!("get id: {e}")))?;
        let parent_raw: String = row.get(1).map_err(|e| ie(format!("get parent: {e}")))?;
        let kind_json: String = row.get(2).map_err(|e| ie(format!("get kind: {e}")))?;
        let job_raw: String = row.get(3).map_err(|e| ie(format!("get job: {e}")))?;
        let prov_json: String = row.get(4).map_err(|e| ie(format!("get prov: {e}")))?;
        let input_json: String = row.get(5).map_err(|e| ie(format!("get input: {e}")))?;
        let result_raw: String = row.get(6).map_err(|e| ie(format!("get result: {e}")))?;
        let started_str: String = row.get(7).map_err(|e| ie(format!("get started: {e}")))?;
        let ended_raw: String = row.get(8).map_err(|e| ie(format!("get ended: {e}")))?;
        let snap_raw: String = row.get(9).map_err(|e| ie(format!("get snap: {e}")))?;

        let parent = if parent_raw.is_empty() {
            None
        } else {
            Some(parent_raw)
        };

        let kind = de(&kind_json)?;

        let provenance: ExecutionProvenance = if prov_json.is_empty() {
            ExecutionProvenance::default()
        } else {
            de(&prov_json)?
        };

        let input: SpanInput = if input_json.is_empty() {
            SpanInput::None
        } else {
            de(&input_json)?
        };

        let result: Option<SpanResult> = if result_raw.is_empty() {
            None
        } else {
            Some(de(&result_raw)?)
        };

        let started_at = chrono::DateTime::parse_from_rfc3339(&started_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        let ended_at = if ended_raw.is_empty() {
            None
        } else {
            chrono::DateTime::parse_from_rfc3339(&ended_raw)
                .map(|dt| Some(dt.with_timezone(&Utc)))
                .unwrap_or(None)
        };

        let job_id = if job_raw.is_empty() {
            None
        } else {
            Some(job_raw)
        };

        let context_snapshot = if snap_raw.is_empty() {
            None
        } else {
            Some(de(&snap_raw)?)
        };

        Ok(TraceNode {
            id,
            parent,
            children: Vec::new(), // populated by caller
            span: TraceSpan {
                kind,
                job_id,
                provenance,
                input,
                started_at,
                ended_at,
                result,
            },
            context_snapshot,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_context::ContextSnapshot;
    use aura_job::OperationKind;
    use aura_model::ChatMessage;
    use aura_trace::SpanResult;

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

    fn make_trace_with_child(session_id: &str) -> SessionTrace {
        let root_id = "root".to_string();
        let child_id = "child".to_string();
        let root = TraceNode {
            id: root_id.clone(),
            parent: None,
            children: vec![child_id.clone()],
            span: TraceSpan {
                kind: OperationKind::UserMessageHandling {
                    session_id: session_id.to_string(),
                },
                job_id: None,
                provenance: ExecutionProvenance::default(),
                input: SpanInput::None,
                started_at: Utc::now(),
                ended_at: Some(Utc::now()),
                result: None,
            },
            context_snapshot: None,
        };
        let child = TraceNode {
            id: child_id.clone(),
            parent: Some(root_id.clone()),
            children: vec![],
            span: TraceSpan {
                kind: OperationKind::LlmCall {
                    model: "claude".to_string(),
                },
                job_id: Some("job-1".to_string()),
                provenance: ExecutionProvenance {
                    model_id: Some("claude".to_string()),
                    ..Default::default()
                },
                input: SpanInput::None,
                started_at: Utc::now(),
                ended_at: Some(Utc::now()),
                result: Some(SpanResult::LlmResponse {
                    output_content: "hello world".to_string(),
                    input_tokens: 10,
                    output_tokens: 5,
                    thinking: Some("let me think".to_string()),
                    tool_calls: vec![],
                    latency: std::time::Duration::from_millis(100),
                }),
            },
            context_snapshot: None,
        };
        let mut nodes = HashMap::new();
        nodes.insert(root_id.clone(), root);
        nodes.insert(child_id.clone(), child);
        SessionTrace {
            session_id: session_id.to_string(),
            root: root_id,
            nodes,
            forks: vec![],
            active_leaf: child_id,
        }
    }

    #[tokio::test]
    async fn save_and_load() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlTraceStore::new(pool);

        let trace = make_trace("s1");
        store.save_trace(&trace).await.unwrap();

        let loaded = store.load_trace("s1").await.unwrap().unwrap();
        assert_eq!(loaded.session_id, "s1");
        assert_eq!(loaded.root, "n1");
        assert_eq!(loaded.active_leaf, "n1");
        assert_eq!(loaded.nodes.len(), 1);
    }

    #[tokio::test]
    async fn children_reconstructed_from_parent_id() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlTraceStore::new(pool);

        store
            .save_trace(&make_trace_with_child("s1"))
            .await
            .unwrap();

        let loaded = store.load_trace("s1").await.unwrap().unwrap();
        assert_eq!(loaded.nodes.len(), 2);

        let root = &loaded.nodes["root"];
        assert_eq!(root.children, vec!["child"]);
        assert!(root.parent.is_none());

        let child = &loaded.nodes["child"];
        assert!(child.children.is_empty());
        assert_eq!(child.parent.as_deref(), Some("root"));
        assert_eq!(child.span.job_id.as_deref(), Some("job-1"));

        // Verify SpanResult round-trips.
        let result = child.span.result.as_ref().unwrap();
        match result {
            SpanResult::LlmResponse {
                output_content,
                thinking,
                ..
            } => {
                assert_eq!(output_content, "hello world");
                assert_eq!(thinking.as_deref(), Some("let me think"));
            }
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn load_single_node_with_children() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlTraceStore::new(pool);

        store
            .save_trace(&make_trace_with_child("s1"))
            .await
            .unwrap();

        let root = store.load_node("s1", &"root".to_string()).await.unwrap();
        assert!(root.is_some());
        let root = root.unwrap();
        assert_eq!(root.children, vec!["child"]);

        let missing = store
            .load_node("s1", &"nope".to_string())
            .await
            .unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn query_by_session_id() {
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

    #[tokio::test]
    async fn forks_round_trip() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlTraceStore::new(pool);

        let mut trace = make_trace("s1");
        trace.forks.push(ForkRecord {
            id: "fork-1".to_string(),
            from_node: "n1".to_string(),
            fork_root: "n1-fork".to_string(),
            reason: "rollback".to_string(),
            created_at: Utc::now(),
        });
        store.save_trace(&trace).await.unwrap();

        let loaded = store.load_trace("s1").await.unwrap().unwrap();
        assert_eq!(loaded.forks.len(), 1);
        assert_eq!(loaded.forks[0].id, "fork-1");
        assert_eq!(loaded.forks[0].reason, "rollback");
    }

    #[tokio::test]
    async fn snapshot_preserved() {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let store = LibsqlTraceStore::new(pool);

        let mut trace = make_trace("s1");
        trace
            .nodes
            .get_mut("n1")
            .unwrap()
            .context_snapshot = Some(ContextSnapshot {
            messages: vec![ChatMessage {
                role: aura_model::Role::User,
                content: vec![aura_model::ContentBlock::Text("hi".to_string())],
            }],
            token_count: 42,
        });
        store.save_trace(&trace).await.unwrap();

        let loaded = store.load_trace("s1").await.unwrap().unwrap();
        let snap = loaded.nodes["n1"].context_snapshot.as_ref().unwrap();
        assert_eq!(snap.token_count, 42);
        assert_eq!(snap.messages.len(), 1);
    }
}
