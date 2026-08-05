//! sqlite implementation of [`ProjectStore`].

use async_trait::async_trait;
use baybo_model::{AgentProfileId, IssueId, IssueRunId, ProjectId, SessionId};
use rusqlite::OptionalExtension;

use super::SqlitePool;
use baybo_store::StorageError;
use baybo_store::project::{
    AttentionCounts, IssueActor, IssueEventRow, IssuePriority, IssueRow, IssueRunRow, IssueStatus,
    IssueUpdate, NewIssue, NewIssueEvent, NewIssueRun, ProjectRow, ProjectStore, ProjectUpdate,
    Result, RunStatus, RunTrigger,
};

pub struct SqliteProjectStore {
    pool: SqlitePool,
}

impl SqliteProjectStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// One issue's timeline, oldest first, optionally only what landed
    /// strictly after `since_us`. Both readers want the same ordering and
    /// the same row mapping; the only difference is the bound.
    async fn events_query(
        &self,
        issue_id: String,
        since_us: Option<i64>,
    ) -> Result<Vec<IssueEventRow>> {
        let raws = self
            .pool
            .interact("issue_events.list", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {EVENT_COLUMNS} FROM issue_events WHERE issue_id = ?1 \
                     AND (?2 IS NULL OR created_at > ?2) ORDER BY created_at, id"
                ))?;
                Ok(stmt
                    .query_map(rusqlite::params![issue_id, since_us], read_raw_event)?
                    .collect::<rusqlite::Result<Vec<RawEvent>>>()?)
            })
            .await?;
        raws.into_iter().map(event_from_raw).collect()
    }
}

const EVENT_COLUMNS: &str = "id, issue_id, project_id, number, actor, body, created_at";

/// Raw event tuple, in `EVENT_COLUMNS` order. `kind` is deliberately not
/// read back: it is derived from `body`, so reading it would invite the two
/// to disagree.
type RawEvent = (String, String, String, i64, String, String, i64);

fn read_raw_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawEvent> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
    ))
}

fn event_from_raw(raw: RawEvent) -> Result<IssueEventRow> {
    let (id, issue_id, project_id, number, actor, body, created_at) = raw;
    Ok(IssueEventRow {
        id: baybo_model::IssueEventId::from(id),
        issue_id: IssueId::from(issue_id),
        project_id: ProjectId::parse(project_id)
            .map_err(|e| StorageError::Storage(e.to_string()))?,
        number,
        actor: IssueActor::parse(&actor)
            .ok_or_else(|| StorageError::Storage(format!("issue_events.actor unknown: {actor}")))?,
        body: serde_json::from_str(&body)
            .map_err(|e| StorageError::Storage(format!("issue_events.body unreadable: {e}")))?,
        created_at: ts("issue_events.created_at", created_at)?,
    })
}

const PROJECT_COLUMNS: &str = "id, name, description, workdir, daily_budget_micros, \
     archived_at, created_at, updated_at";

const ISSUE_COLUMNS: &str = "id, project_id, number, title, description, status, priority, \
     assignee, position, blocked_reason, branch, parent_issue_id, stage, cancelled_at, \
     created_at, updated_at";

const RUN_COLUMNS: &str = "id, issue_id, project_id, number, agent_id, session_id, trigger, \
     status, attempt, error, created_at, started_at, settled_at";

/// Raw run tuple, in `RUN_COLUMNS` order.
type RawRun = (
    String,
    String,
    String,
    i64,
    String,
    Option<String>,
    String,
    String,
    i64,
    Option<String>,
    i64,
    Option<i64>,
    Option<i64>,
);

fn read_raw_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRun> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
    ))
}

fn run_from_raw(raw: RawRun) -> Result<IssueRunRow> {
    let (
        id,
        issue_id,
        project_id,
        number,
        agent_id,
        session_id,
        trigger,
        status,
        attempt,
        error,
        created_at,
        started_at,
        settled_at,
    ) = raw;
    Ok(IssueRunRow {
        id: IssueRunId::from(id),
        issue_id: IssueId::from(issue_id),
        project_id: ProjectId::parse(project_id)
            .map_err(|e| StorageError::Storage(e.to_string()))?,
        number,
        agent_id: AgentProfileId::parse(agent_id)
            .map_err(|e| StorageError::Storage(e.to_string()))?,
        session_id: session_id.map(SessionId::from),
        trigger: RunTrigger::parse(&trigger).ok_or_else(|| {
            StorageError::Storage(format!("issue_runs.trigger unknown: {trigger}"))
        })?,
        status: RunStatus::parse(&status)
            .ok_or_else(|| StorageError::Storage(format!("issue_runs.status unknown: {status}")))?,
        attempt,
        error,
        created_at: ts("issue_runs.created_at", created_at)?,
        started_at: ts_opt("issue_runs.started_at", started_at)?,
        settled_at: ts_opt("issue_runs.settled_at", settled_at)?,
    })
}

/// Raw project tuple, in `PROJECT_COLUMNS` order. Timestamps are µs.
type RawProject = (
    String,
    String,
    String,
    String,
    Option<i64>,
    Option<i64>,
    i64,
    i64,
);

/// Raw issue tuple, in `ISSUE_COLUMNS` order.
type RawIssue = (
    String,
    String,
    i64,
    String,
    String,
    String,
    String,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    Option<i64>,
    i64,
    i64,
);

fn read_raw_project(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawProject> {
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
}

fn read_raw_issue(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawIssue> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
        row.get(15)?,
    ))
}

fn ts(column: &str, us: i64) -> Result<chrono::DateTime<chrono::Utc>> {
    super::time::from_us(us)
        .ok_or_else(|| StorageError::Storage(format!("{column} out of range: {us}")))
}

fn ts_opt(column: &str, us: Option<i64>) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    match us {
        Some(us) => Ok(Some(ts(column, us)?)),
        None => Ok(None),
    }
}

fn project_from_raw(raw: RawProject) -> Result<ProjectRow> {
    let (id, name, description, workdir, daily_budget_micros, archived_at, created_at, updated_at) =
        raw;
    Ok(ProjectRow {
        // A stored id runs the grammar again on the way out: the row is the
        // source of a directory name, and a hand-edited DB is still a way in.
        id: ProjectId::parse(id).map_err(|e| StorageError::Storage(e.to_string()))?,
        name,
        description,
        workdir,
        daily_budget: daily_budget_micros.map(baybo_model::MicroUsd::from_micros),
        archived_at: ts_opt("projects.archived_at", archived_at)?,
        created_at: ts("projects.created_at", created_at)?,
        updated_at: ts("projects.updated_at", updated_at)?,
    })
}

fn issue_from_raw(raw: RawIssue) -> Result<IssueRow> {
    let (
        id,
        project_id,
        number,
        title,
        description,
        status,
        priority,
        assignee,
        position,
        blocked_reason,
        branch,
        parent_issue_id,
        stage,
        cancelled_at,
        created_at,
        updated_at,
    ) = raw;
    Ok(IssueRow {
        id: IssueId::from(id),
        project_id: ProjectId::parse(project_id)
            .map_err(|e| StorageError::Storage(e.to_string()))?,
        number,
        title,
        description,
        status: IssueStatus::parse(&status)
            .ok_or_else(|| StorageError::Storage(format!("issues.status unknown: {status}")))?,
        priority: IssuePriority::parse(&priority)
            .ok_or_else(|| StorageError::Storage(format!("issues.priority unknown: {priority}")))?,
        // The stored assignee re-runs its own grammar on the way out, the
        // same reason the project id does: an agent id names a directory.
        assignee: assignee
            .map(AgentProfileId::parse)
            .transpose()
            .map_err(|e| StorageError::Storage(e.to_string()))?,
        position,
        blocked_reason,
        branch,
        parent_issue_id: parent_issue_id.map(IssueId::from),
        stage,
        cancelled_at: ts_opt("issues.cancelled_at", cancelled_at)?,
        created_at: ts("issues.created_at", created_at)?,
        updated_at: ts("issues.updated_at", updated_at)?,
    })
}

#[async_trait]
impl ProjectStore for SqliteProjectStore {
    async fn list_projects(&self, include_archived: bool) -> Result<Vec<ProjectRow>> {
        let raws = self
            .pool
            .interact("projects.list", move |conn| {
                let sql = format!(
                    "SELECT {PROJECT_COLUMNS} FROM projects {} ORDER BY updated_at DESC, id",
                    if include_archived {
                        ""
                    } else {
                        "WHERE archived_at IS NULL"
                    }
                );
                let mut stmt = conn.prepare(&sql)?;
                let raws = stmt
                    .query_map([], read_raw_project)?
                    .collect::<rusqlite::Result<Vec<RawProject>>>()?;
                Ok(raws)
            })
            .await?;
        raws.into_iter().map(project_from_raw).collect()
    }

    async fn get_project(&self, id: &ProjectId) -> Result<Option<ProjectRow>> {
        let id = id.as_str().to_string();
        let raw = self
            .pool
            .interact("projects.get", move |conn| {
                Ok(conn
                    .query_row(
                        &format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE id = ?1"),
                        rusqlite::params![id],
                        read_raw_project,
                    )
                    .optional()?)
            })
            .await?;
        raw.map(project_from_raw).transpose()
    }

    async fn create_project(&self, row: &ProjectRow) -> Result<()> {
        let id = row.id.as_str().to_string();
        let name = row.name.clone();
        let description = row.description.clone();
        let workdir = row.workdir.clone();
        let daily_budget = row.daily_budget.map(baybo_model::MicroUsd::into_micros);
        let created_at = super::time::to_us(row.created_at);
        let updated_at = super::time::to_us(row.updated_at);
        self.pool
            .interact("projects.create", move |conn| {
                conn.execute(
                    "INSERT INTO projects \
                     (id, name, description, workdir, daily_budget_micros, archived_at, \
                      created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7)",
                    rusqlite::params![
                        id,
                        name,
                        description,
                        workdir,
                        daily_budget,
                        created_at,
                        updated_at
                    ],
                )?;
                Ok(())
            })
            .await
    }

    async fn update_project(&self, id: &ProjectId, update: &ProjectUpdate) -> Result<bool> {
        let id = id.as_str().to_string();
        let name = update.name.clone();
        let description = update.description.clone();
        let daily_budget = update.daily_budget.map(baybo_model::MicroUsd::into_micros);
        let now = super::time::now_us();
        let affected = self
            .pool
            .interact("projects.update", move |conn| {
                Ok(conn.execute(
                    "UPDATE projects SET name = ?2, description = ?3, \
                     daily_budget_micros = ?4, updated_at = ?5 \
                     WHERE id = ?1",
                    rusqlite::params![id, name, description, daily_budget, now],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn spend_since(
        &self,
        project: &ProjectId,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<baybo_model::MicroUsd> {
        let project = project.as_str().to_string();
        let since = super::time::to_us(since);
        let micros = self
            .pool
            .interact("projects.spend_since", move |conn| {
                // `IN (SELECT …)` over this board's run sessions rather than
                // a join: an issue's session is reused by every run of it, so
                // a join would count one call once per run that shared the
                // session. The subquery deduplicates by construction.
                let total: i64 = conn.query_row(
                    "SELECT COALESCE(SUM(cost_usd), 0) FROM cost_records \
                     WHERE timestamp >= ?2 AND session_id IN ( \
                         SELECT session_id FROM issue_runs \
                         WHERE project_id = ?1 AND session_id IS NOT NULL)",
                    rusqlite::params![project, since],
                    |row| row.get(0),
                )?;
                Ok(total)
            })
            .await?;
        Ok(baybo_model::MicroUsd::from_micros(micros))
    }

    async fn set_project_archived(&self, id: &ProjectId, archived: bool) -> Result<bool> {
        let id = id.as_str().to_string();
        let now = super::time::now_us();
        let stamp = archived.then_some(now);
        let affected = self
            .pool
            .interact("projects.set_archived", move |conn| {
                Ok(conn.execute(
                    "UPDATE projects SET archived_at = ?2, updated_at = ?3 WHERE id = ?1",
                    rusqlite::params![id, stamp, now],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn list_issues(&self, project: &ProjectId) -> Result<Vec<IssueRow>> {
        let project = project.as_str().to_string();
        let raws = self
            .pool
            .interact("issues.list", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {ISSUE_COLUMNS} FROM issues WHERE project_id = ?1 \
                     ORDER BY status, position, number"
                ))?;
                let raws = stmt
                    .query_map(rusqlite::params![project], read_raw_issue)?
                    .collect::<rusqlite::Result<Vec<RawIssue>>>()?;
                Ok(raws)
            })
            .await?;
        raws.into_iter().map(issue_from_raw).collect()
    }

    async fn get_issue(&self, project: &ProjectId, number: i64) -> Result<Option<IssueRow>> {
        let project = project.as_str().to_string();
        let raw = self
            .pool
            .interact("issues.get", move |conn| {
                Ok(conn
                    .query_row(
                        &format!(
                            "SELECT {ISSUE_COLUMNS} FROM issues \
                             WHERE project_id = ?1 AND number = ?2"
                        ),
                        rusqlite::params![project, number],
                        read_raw_issue,
                    )
                    .optional()?)
            })
            .await?;
        raw.map(issue_from_raw).transpose()
    }

    async fn create_issue(&self, new: &NewIssue) -> Result<IssueRow> {
        let id = new.id.as_str().to_string();
        let project = new.project_id.as_str().to_string();
        let title = new.title.clone();
        let description = new.description.clone();
        let status = new.status.as_str();
        let priority = new.priority.as_str();
        let assignee = new.assignee.as_ref().map(|a| a.as_str().to_string());
        let parent_issue_id = new
            .parent_issue_id
            .as_ref()
            .map(|id| id.as_str().to_string());
        let stage = new.stage;
        let created_at = super::time::to_us(new.created_at);
        let raw = self
            .pool
            .interact("issues.create", move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                // Both derived values are read inside the transaction that
                // writes them: computed outside, two concurrent creates would
                // pick the same number and the second would trip the unique
                // index instead of getting the next one.
                let number: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(number), 0) + 1 FROM issues WHERE project_id = ?1",
                    rusqlite::params![project],
                    |row| row.get(0),
                )?;
                let position: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(position), -1) + 1 FROM issues \
                     WHERE project_id = ?1 AND status = ?2",
                    rusqlite::params![project, status],
                    |row| row.get(0),
                )?;
                tx.execute(
                    "INSERT INTO issues (id, project_id, number, title, description, status, \
                     priority, assignee, position, blocked_reason, parent_issue_id, stage, \
                     cancelled_at, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?11, NULL, ?12, ?12)",
                    rusqlite::params![
                        id,
                        project,
                        number,
                        title,
                        description,
                        status,
                        priority,
                        assignee,
                        position,
                        parent_issue_id,
                        stage,
                        created_at
                    ],
                )?;
                let raw = tx.query_row(
                    &format!(
                        "SELECT {ISSUE_COLUMNS} FROM issues WHERE project_id = ?1 AND number = ?2"
                    ),
                    rusqlite::params![project, number],
                    read_raw_issue,
                )?;
                tx.commit()?;
                Ok(raw)
            })
            .await?;
        issue_from_raw(raw)
    }

    async fn update_issue(
        &self,
        project: &ProjectId,
        number: i64,
        update: &IssueUpdate,
    ) -> Result<bool> {
        let project = project.as_str().to_string();
        let update = update.clone();
        let now = super::time::now_us();
        let affected = self
            .pool
            .interact("issues.update", move |conn| {
                // COALESCE(?, column) leaves an unset field alone, so a
                // sparse patch never clobbers a field the caller didn't name.
                // `blocked_reason` and `cancelled_at` are clearable, so they
                // take a flag beside the value rather than relying on NULL to
                // mean "unset" — NULL is a value they can legitimately take.
                let title = update.title.clone();
                let description = update.description.clone();
                let priority = update.priority.map(|p| p.as_str());
                let (set_blocked, blocked) = match &update.blocked_reason {
                    Some(value) => (true, value.clone()),
                    None => (false, None),
                };
                let (set_assignee, assignee) = match &update.assignee {
                    Some(value) => (true, value.as_ref().map(|a| a.as_str().to_string())),
                    None => (false, None),
                };
                let (set_cancelled, cancelled) = match update.cancelled {
                    Some(true) => (true, Some(now)),
                    Some(false) => (true, None),
                    None => (false, None),
                };
                let (set_parent, parent) = match &update.parent {
                    Some(value) => (true, value.as_ref().map(|id| id.as_str().to_string())),
                    None => (false, None),
                };
                // Detaching from a parent resets the stage: a stage number
                // on a top-level issue is a barrier under a parent that is
                // no longer there, and the next reader would honour it.
                let stage = match (&update.parent, update.stage) {
                    (Some(None), _) => Some(0),
                    (_, stage) => stage,
                };
                Ok(conn.execute(
                    "UPDATE issues SET \
                       title          = COALESCE(?3, title), \
                       description    = COALESCE(?4, description), \
                       priority       = COALESCE(?5, priority), \
                       assignee       = CASE WHEN ?6 THEN ?7 ELSE assignee END, \
                       blocked_reason = CASE WHEN ?8 THEN ?9 ELSE blocked_reason END, \
                       cancelled_at   = CASE WHEN ?10 THEN ?11 ELSE cancelled_at END, \
                       parent_issue_id = CASE WHEN ?13 THEN ?14 ELSE parent_issue_id END, \
                       stage          = COALESCE(?15, stage), \
                       updated_at     = ?12 \
                     WHERE project_id = ?1 AND number = ?2",
                    rusqlite::params![
                        project,
                        number,
                        title,
                        description,
                        priority,
                        set_assignee,
                        assignee,
                        set_blocked,
                        blocked,
                        set_cancelled,
                        cancelled,
                        now,
                        set_parent,
                        parent,
                        stage
                    ],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn attention(&self) -> Result<Vec<(ProjectId, AttentionCounts)>> {
        let held_status = RunStatus::Held.as_str();
        let failed_status = RunStatus::Failed.as_str();
        let done_status = IssueStatus::Done.as_str();
        let rows = self
            .pool
            .interact("projects.attention", move |conn| {
                let mut counts: std::collections::HashMap<String, (usize, usize)> =
                    std::collections::HashMap::new();

                // Held runs. Served by `idx_issue_runs_unsettled`, which is
                // small by construction, and already per-issue because
                // `idx_issue_runs_live` makes at most one unfinished run per
                // issue structurally possible.
                {
                    let mut stmt = conn.prepare(
                        "SELECT r.project_id, COUNT(*) FROM issue_runs r \
                         JOIN projects p ON p.id = r.project_id \
                         WHERE r.status = ?1 AND r.settled_at IS NULL \
                           AND p.archived_at IS NULL \
                         GROUP BY r.project_id",
                    )?;
                    for row in stmt.query_map(rusqlite::params![held_status], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    })? {
                        let (project, count) = row?;
                        counts.entry(project).or_default().0 += count.max(0) as usize;
                    }
                }

                // Live cards whose NEWEST run failed. A correlated subquery
                // rather than a window function over every run: this way the
                // cost is one index seek per live card
                // (`idx_issue_runs_log`), bounded by the working set, where
                // a partition would be bounded by run history.
                //
                // `id DESC` tiebreaks alongside `created_at DESC` for the
                // same reason the feed does: two runs written in one
                // microsecond must not flip the answer.
                {
                    let mut stmt = conn.prepare(
                        "SELECT i.project_id, COUNT(*) FROM issues i \
                         JOIN projects p ON p.id = i.project_id \
                         WHERE i.status <> ?1 AND i.cancelled_at IS NULL \
                           AND i.blocked_reason IS NULL AND p.archived_at IS NULL \
                           AND (SELECT r.status FROM issue_runs r \
                                WHERE r.issue_id = i.id \
                                ORDER BY r.created_at DESC, r.id DESC LIMIT 1) = ?2 \
                         GROUP BY i.project_id",
                    )?;
                    for row in stmt
                        .query_map(rusqlite::params![done_status, failed_status], |row| {
                            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                        })?
                    {
                        let (project, count) = row?;
                        counts.entry(project).or_default().1 += count.max(0) as usize;
                    }
                }
                Ok(counts.into_iter().collect::<Vec<_>>())
            })
            .await?;

        rows.into_iter()
            .map(|(project, (held, failed))| {
                Ok((
                    ProjectId::parse(project).map_err(|e| StorageError::Storage(e.to_string()))?,
                    AttentionCounts {
                        approvals: 0,
                        held,
                        failed,
                    },
                ))
            })
            .collect()
    }

    async fn projects_for_sessions(
        &self,
        sessions: &[SessionId],
    ) -> Result<Vec<(SessionId, ProjectId)>> {
        if sessions.is_empty() {
            return Ok(Vec::new());
        }
        let sessions: Vec<String> = sessions.iter().map(|s| s.as_str().to_string()).collect();
        let rows = self
            .pool
            .interact("issue_runs.projects_for_sessions", move |conn| {
                // Built from the bind count, never from the values: the
                // placeholders are the only thing interpolated.
                let placeholders = vec!["?"; sessions.len()].join(",");
                let mut stmt = conn.prepare(&format!(
                    "SELECT DISTINCT session_id, project_id FROM issue_runs \
                     WHERE session_id IN ({placeholders})"
                ))?;
                let params = rusqlite::params_from_iter(sessions.iter());
                Ok(stmt
                    .query_map(params, |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;
        rows.into_iter()
            .map(|(session, project)| {
                Ok((
                    SessionId::from(session),
                    ProjectId::parse(project).map_err(|e| StorageError::Storage(e.to_string()))?,
                ))
            })
            .collect()
    }

    async fn project_feed(
        &self,
        project: &ProjectId,
        before: Option<chrono::DateTime<chrono::Utc>>,
        limit: usize,
    ) -> Result<Vec<IssueEventRow>> {
        let project = project.as_str().to_string();
        let before = before.map(super::time::to_us);
        let limit = limit as i64;
        let raws = self
            .pool
            .interact("issue_events.feed", move |conn| {
                // `id DESC` breaks the tie: two entries written in the same
                // microsecond would otherwise page unstably, showing one row
                // twice and skipping another.
                let mut stmt = conn.prepare(&format!(
                    "SELECT {EVENT_COLUMNS} FROM issue_events WHERE project_id = ?1 \
                     AND (?2 IS NULL OR created_at < ?2) \
                     ORDER BY created_at DESC, id DESC LIMIT ?3"
                ))?;
                Ok(stmt
                    .query_map(rusqlite::params![project, before, limit], read_raw_event)?
                    .collect::<rusqlite::Result<Vec<RawEvent>>>()?)
            })
            .await?;
        raws.into_iter().map(event_from_raw).collect()
    }

    async fn list_children(&self, parent: &IssueId) -> Result<Vec<IssueRow>> {
        let parent = parent.as_str().to_string();
        let raws = self
            .pool
            .interact("issues.children", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {ISSUE_COLUMNS} FROM issues WHERE parent_issue_id = ?1 \
                     ORDER BY stage, position, number"
                ))?;
                Ok(stmt
                    .query_map(rusqlite::params![parent], read_raw_issue)?
                    .collect::<rusqlite::Result<Vec<RawIssue>>>()?)
            })
            .await?;
        raws.into_iter().map(issue_from_raw).collect()
    }

    async fn move_issue(
        &self,
        project: &ProjectId,
        number: i64,
        status: IssueStatus,
        ordered_numbers: &[i64],
    ) -> Result<bool> {
        let project = project.as_str().to_string();
        let status = status.as_str();
        let ordered: Vec<i64> = ordered_numbers.to_vec();
        let now = super::time::now_us();
        self.pool
            .interact("issues.move", move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                // Read the column it is leaving before the update overwrites
                // it: the card it vacates has to close ranks behind it, and
                // afterwards there is no way to know where it came from.
                let previous: Option<String> = tx
                    .query_row(
                        "SELECT status FROM issues WHERE project_id = ?1 AND number = ?2",
                        rusqlite::params![project, number],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(previous) = previous else {
                    drop(tx);
                    return Ok(false);
                };
                tx.execute(
                    "UPDATE issues SET status = ?3, updated_at = ?4 \
                     WHERE project_id = ?1 AND number = ?2",
                    rusqlite::params![project, number, status, now],
                )?;
                // Renumber the destination column densely. Every UPDATE is
                // scoped to (project, status), so a number from another
                // project — or from a column the caller mis-read — updates
                // nothing instead of being adopted into this column.
                for (index, target) in ordered.iter().enumerate() {
                    tx.execute(
                        "UPDATE issues SET position = ?3 \
                         WHERE project_id = ?1 AND number = ?2 AND status = ?4",
                        rusqlite::params![project, target, index as i64, status],
                    )?;
                }
                if previous != status {
                    // The source column keeps its order and closes the gap.
                    // Left alone it would hold a hole at the departed card's
                    // rank — harmless for sorting, but it makes `position`
                    // mean two different things depending on history, and a
                    // later reader that trusts density would be wrong.
                    let remaining: Vec<i64> = {
                        let mut stmt = tx.prepare(
                            "SELECT number FROM issues \
                             WHERE project_id = ?1 AND status = ?2 \
                             ORDER BY position, number",
                        )?;
                        stmt.query_map(rusqlite::params![project, previous], |row| row.get(0))?
                            .collect::<rusqlite::Result<Vec<i64>>>()?
                    };
                    for (index, target) in remaining.iter().enumerate() {
                        tx.execute(
                            "UPDATE issues SET position = ?3 \
                             WHERE project_id = ?1 AND number = ?2",
                            rusqlite::params![project, target, index as i64],
                        )?;
                    }
                }
                tx.commit()?;
                Ok(true)
            })
            .await
    }

    async fn enqueue_run(&self, new: &NewIssueRun) -> Result<IssueRunRow> {
        let id = new.id.as_str().to_string();
        let issue_id = new.issue_id.as_str().to_string();
        let project = new.project_id.as_str().to_string();
        let number = new.number;
        let agent = new.agent_id.as_str().to_string();
        let trigger = new.trigger.as_str();
        let now = super::time::now_us();
        let outcome = self
            .pool
            .interact("issue_runs.enqueue", move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let attempt: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(attempt), 0) + 1 FROM issue_runs WHERE issue_id = ?1",
                    rusqlite::params![issue_id],
                    |row| row.get(0),
                )?;
                // The live index rejects a second unfinished run. Returned as
                // data rather than an error because a non-Internal
                // StorageError cannot be built inside this closure.
                if let Err(e) = tx.execute(
                    "INSERT INTO issue_runs (id, issue_id, project_id, number, agent_id, \
                     session_id, trigger, status, attempt, error, created_at, started_at, \
                     settled_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, 'queued', ?7, NULL, ?8, NULL, NULL)",
                    rusqlite::params![id, issue_id, project, number, agent, trigger, attempt, now],
                ) {
                    drop(tx);
                    return Ok(Err(e.to_string()));
                }
                let raw = tx.query_row(
                    &format!("SELECT {RUN_COLUMNS} FROM issue_runs WHERE id = ?1"),
                    rusqlite::params![id],
                    read_raw_run,
                )?;
                tx.commit()?;
                Ok(Ok(raw))
            })
            .await?;
        match outcome {
            Ok(raw) => run_from_raw(raw),
            Err(reason) => Err(StorageError::Conflict(format!(
                "issue already has a run in flight: {reason}"
            ))),
        }
    }

    async fn append_event(&self, new: &NewIssueEvent) -> Result<IssueEventRow> {
        let row = IssueEventRow {
            id: baybo_model::IssueEventId::generate(),
            issue_id: new.issue_id.clone(),
            project_id: new.project_id.clone(),
            number: new.number,
            actor: new.actor.clone(),
            body: new.body.clone(),
            created_at: chrono::Utc::now(),
        };
        let id = row.id.as_str().to_string();
        let issue_id = row.issue_id.as_str().to_string();
        let project = row.project_id.as_str().to_string();
        let number = row.number;
        let actor = row.actor.to_storage();
        let kind = row.body.kind();
        let body = serde_json::to_string(&row.body)
            .map_err(|e| StorageError::Storage(format!("serialize issue event: {e}")))?;
        let created = super::time::to_us(row.created_at);
        self.pool
            .interact("issue_events.append", move |conn| {
                conn.execute(
                    "INSERT INTO issue_events (id, issue_id, project_id, number, actor, kind, \
                     body, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    rusqlite::params![id, issue_id, project, number, actor, kind, body, created],
                )?;
                Ok(())
            })
            .await?;
        Ok(row)
    }

    async fn list_events(&self, issue: &IssueId) -> Result<Vec<IssueEventRow>> {
        self.events_query(issue.as_str().to_string(), None).await
    }

    async fn events_since(
        &self,
        issue: &IssueId,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<IssueEventRow>> {
        self.events_query(issue.as_str().to_string(), Some(super::time::to_us(since)))
            .await
    }

    async fn set_issue_branch(&self, id: &IssueId, branch: &str) -> Result<bool> {
        let id = id.as_str().to_string();
        let branch = branch.to_owned();
        let now = super::time::now_us();
        let changed = self
            .pool
            .interact("issues.set_branch", move |conn| {
                Ok(conn.execute(
                    "UPDATE issues SET branch = ?2, updated_at = ?3 WHERE id = ?1",
                    rusqlite::params![id, branch, now],
                )?)
            })
            .await?;
        Ok(changed > 0)
    }

    async fn list_runs(&self, issue: &IssueId) -> Result<Vec<IssueRunRow>> {
        let issue = issue.as_str().to_string();
        let raws = self
            .pool
            .interact("issue_runs.list", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {RUN_COLUMNS} FROM issue_runs WHERE issue_id = ?1 \
                     ORDER BY attempt DESC"
                ))?;
                Ok(stmt
                    .query_map(rusqlite::params![issue], read_raw_run)?
                    .collect::<rusqlite::Result<Vec<RawRun>>>()?)
            })
            .await?;
        raws.into_iter().map(run_from_raw).collect()
    }

    async fn active_runs(&self, project: &ProjectId) -> Result<Vec<IssueRunRow>> {
        let project = project.as_str().to_string();
        let raws = self
            .pool
            .interact("issue_runs.active", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {RUN_COLUMNS} FROM issue_runs \
                     WHERE project_id = ?1 AND settled_at IS NULL ORDER BY number"
                ))?;
                Ok(stmt
                    .query_map(rusqlite::params![project], read_raw_run)?
                    .collect::<rusqlite::Result<Vec<RawRun>>>()?)
            })
            .await?;
        raws.into_iter().map(run_from_raw).collect()
    }

    async fn get_run(&self, id: &IssueRunId) -> Result<Option<IssueRunRow>> {
        let id = id.as_str().to_string();
        let raw = self
            .pool
            .interact("issue_runs.get", move |conn| {
                Ok(conn
                    .query_row(
                        &format!("SELECT {RUN_COLUMNS} FROM issue_runs WHERE id = ?1"),
                        rusqlite::params![id],
                        read_raw_run,
                    )
                    .optional()?)
            })
            .await?;
        raw.map(run_from_raw).transpose()
    }

    async fn unsettled_runs(&self) -> Result<Vec<IssueRunRow>> {
        let raws = self
            .pool
            .interact("issue_runs.unsettled", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {RUN_COLUMNS} FROM issue_runs WHERE settled_at IS NULL \
                     ORDER BY created_at"
                ))?;
                Ok(stmt
                    .query_map([], read_raw_run)?
                    .collect::<rusqlite::Result<Vec<RawRun>>>()?)
            })
            .await?;
        raws.into_iter().map(run_from_raw).collect()
    }

    async fn claim_run(&self, id: &IssueRunId, session: &SessionId) -> Result<bool> {
        let id = id.as_str().to_string();
        let session = session.as_str().to_string();
        let now = super::time::now_us();
        let affected = self
            .pool
            .interact("issue_runs.claim", move |conn| {
                // Scoped to `queued`, so two dispatches of the same row
                // resolve into one execution rather than two.
                Ok(conn.execute(
                    "UPDATE issue_runs SET status = 'running', session_id = ?2, started_at = ?3 \
                     WHERE id = ?1 AND status = 'queued'",
                    rusqlite::params![id, session, now],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn settle_run(
        &self,
        id: &IssueRunId,
        status: RunStatus,
        error: Option<&str>,
    ) -> Result<bool> {
        let id = id.as_str().to_string();
        let status = status.as_str();
        let error = error.map(str::to_owned);
        let now = super::time::now_us();
        let affected = self
            .pool
            .interact("issue_runs.settle", move |conn| {
                // `settled_at IS NULL` is what makes the boot re-drive safe
                // to replay: a second settle of the same run touches nothing.
                Ok(conn.execute(
                    "UPDATE issue_runs SET status = ?2, error = ?3, settled_at = ?4 \
                     WHERE id = ?1 AND settled_at IS NULL",
                    rusqlite::params![id, status, error, now],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn hold_run(&self, id: &IssueRunId) -> Result<bool> {
        let id = id.as_str().to_string();
        let affected = self
            .pool
            .interact("issue_runs.hold", move |conn| {
                Ok(conn.execute(
                    "UPDATE issue_runs SET status = 'held' \
                     WHERE id = ?1 AND status = 'queued' AND settled_at IS NULL",
                    rusqlite::params![id],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn held_runs(&self, project: &ProjectId) -> Result<Vec<IssueRunRow>> {
        let project = project.as_str().to_string();
        let raws = self
            .pool
            .interact("issue_runs.held", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {RUN_COLUMNS} FROM issue_runs \
                     WHERE project_id = ?1 AND status = 'held' AND settled_at IS NULL \
                     ORDER BY created_at"
                ))?;
                Ok(stmt
                    .query_map(rusqlite::params![project], read_raw_run)?
                    .collect::<rusqlite::Result<Vec<RawRun>>>()?)
            })
            .await?;
        raws.into_iter().map(run_from_raw).collect()
    }

    async fn release_run(&self, id: &IssueRunId) -> Result<bool> {
        let id = id.as_str().to_string();
        let affected = self
            .pool
            .interact("issue_runs.release", move |conn| {
                Ok(conn.execute(
                    "UPDATE issue_runs SET status = 'queued' WHERE id = ?1 AND status = 'held'",
                    rusqlite::params![id],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn requeue_unsettled(&self) -> Result<Vec<IssueRunRow>> {
        let raws = self
            .pool
            .interact("issue_runs.requeue", move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                // A `running` row whose actor died with the process is work
                // that never finished. Returning it to `queued` — and
                // dropping the dead session — is what lets the sweep hand it
                // back out without the claim being refused.
                tx.execute(
                    "UPDATE issue_runs SET status = 'queued', session_id = NULL, \
                     started_at = NULL WHERE settled_at IS NULL AND status = 'running'",
                    [],
                )?;
                let raws = {
                    // `held` is excluded on purpose: those runs were never
                    // started, so they are not orphans to roll forward. The
                    // manager re-evaluates them against today's budget right
                    // after this sweep, which is the only thing that should
                    // decide whether they start.
                    let mut stmt = tx.prepare(&format!(
                        "SELECT {RUN_COLUMNS} FROM issue_runs WHERE settled_at IS NULL \
                         AND status != 'held' ORDER BY created_at"
                    ))?;
                    stmt.query_map([], read_raw_run)?
                        .collect::<rusqlite::Result<Vec<RawRun>>>()?
                };
                tx.commit()?;
                Ok(raws)
            })
            .await?;
        raws.into_iter().map(run_from_raw).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_store::project::IssueEventBody;

    async fn store() -> (tempfile::TempDir, SqliteProjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(dir.path().join("test.db")).await.unwrap();
        (dir, SqliteProjectStore::new(pool))
    }

    fn project(id: &str, name: &str) -> ProjectRow {
        let now = chrono::Utc::now();
        ProjectRow {
            id: ProjectId::parse(id).unwrap(),
            name: name.to_owned(),
            description: String::new(),
            workdir: format!("/tmp/{id}"),
            daily_budget: None,
            archived_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn new_run(issue: &IssueRow) -> NewIssueRun {
        NewIssueRun {
            id: IssueRunId::generate(),
            issue_id: issue.id.clone(),
            project_id: issue.project_id.clone(),
            number: issue.number,
            agent_id: AgentProfileId::parse("dev-1").unwrap(),
            trigger: RunTrigger::Started,
        }
    }

    fn new_issue(project: &ProjectId, title: &str, status: IssueStatus) -> NewIssue {
        NewIssue {
            id: IssueId::generate(),
            project_id: project.clone(),
            title: title.to_owned(),
            description: String::new(),
            status,
            priority: IssuePriority::None,
            assignee: None,
            parent_issue_id: None,
            stage: 0,
            created_at: chrono::Utc::now(),
        }
    }

    fn event(issue: &IssueRow, actor: IssueActor, body: IssueEventBody) -> NewIssueEvent {
        NewIssueEvent {
            issue_id: issue.id.clone(),
            project_id: issue.project_id.clone(),
            number: issue.number,
            actor,
            body,
        }
    }

    #[tokio::test]
    async fn a_timeline_reads_back_in_the_order_it_was_written() {
        let (_dir, store) = store().await;
        let p = project("proj-t", "T");
        store.create_project(&p).await.unwrap();
        let issue = store
            .create_issue(&new_issue(&p.id, "Wire it", IssueStatus::Backlog))
            .await
            .unwrap();

        let bodies = [
            IssueEventBody::Opened,
            IssueEventBody::Comment {
                text: "start with the store".into(),
            },
            IssueEventBody::Moved {
                from: IssueStatus::Backlog,
                to: IssueStatus::InProgress,
            },
        ];
        for body in &bodies {
            store
                .append_event(&event(&issue, IssueActor::User, body.clone()))
                .await
                .unwrap();
        }

        let timeline = store.list_events(&issue.id).await.unwrap();
        assert_eq!(
            timeline.iter().map(|e| e.body.clone()).collect::<Vec<_>>(),
            bodies,
            "oldest first — reading order, not newest-first like the run log"
        );
        assert!(timeline.iter().all(|e| e.actor == IssueActor::User));
    }

    #[tokio::test]
    async fn a_typed_body_survives_the_round_trip_whole() {
        // The payload is JSON in one column, so this is the test that a
        // reader gets back exactly the variant a writer stored — every
        // field, including the ones that are only sometimes there.
        let (_dir, store) = store().await;
        let p = project("proj-b", "B");
        store.create_project(&p).await.unwrap();
        let issue = store
            .create_issue(&new_issue(&p.id, "Round trip", IssueStatus::Todo))
            .await
            .unwrap();
        let agent = AgentProfileId::parse("dev-1".to_owned()).unwrap();
        let run = IssueRunId::generate();
        let bodies = [
            IssueEventBody::Assigned {
                from: None,
                to: Some(agent.clone()),
            },
            IssueEventBody::RunStarted {
                run_id: run.clone(),
                attempt: 2,
                trigger: RunTrigger::Retry,
            },
            IssueEventBody::RunSettled {
                run_id: run,
                attempt: 2,
                status: RunStatus::Failed,
                error: Some("the model gave up".into()),
            },
            IssueEventBody::Blocked {
                reason: "waiting on tmux".into(),
            },
        ];
        for body in &bodies {
            store
                .append_event(&event(
                    &issue,
                    IssueActor::Agent(agent.clone()),
                    body.clone(),
                ))
                .await
                .unwrap();
        }

        let timeline = store.list_events(&issue.id).await.unwrap();
        assert_eq!(
            timeline.iter().map(|e| e.body.clone()).collect::<Vec<_>>(),
            bodies
        );
        assert_eq!(
            timeline[0].actor,
            IssueActor::Agent(agent),
            "an agent actor round-trips as that agent, not as the user"
        );
    }

    #[tokio::test]
    async fn the_delta_since_a_moment_excludes_what_came_before_it() {
        // What a follow-up run's brief is built from: everything said since
        // the last run started, and nothing it already read.
        let (_dir, store) = store().await;
        let p = project("proj-d", "D");
        store.create_project(&p).await.unwrap();
        let issue = store
            .create_issue(&new_issue(&p.id, "Delta", IssueStatus::Todo))
            .await
            .unwrap();

        let first = store
            .append_event(&event(
                &issue,
                IssueActor::User,
                IssueEventBody::Comment {
                    text: "before".into(),
                },
            ))
            .await
            .unwrap();
        let later = store
            .append_event(&event(
                &issue,
                IssueActor::User,
                IssueEventBody::Comment {
                    text: "after".into(),
                },
            ))
            .await
            .unwrap();

        let delta = store
            .events_since(&issue.id, first.created_at)
            .await
            .unwrap();
        assert_eq!(
            delta.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            vec![later.id],
            "strictly after: the marker event itself was already read"
        );
        assert_eq!(store.list_events(&issue.id).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn projects_round_trip_and_archive_hides() {
        let (_dir, store) = store().await;
        let a = project("proj-a", "A");
        store.create_project(&a).await.unwrap();
        store.create_project(&project("proj-b", "B")).await.unwrap();

        assert_eq!(store.list_projects(false).await.unwrap().len(), 2);
        let fetched = store.get_project(&a.id).await.unwrap().unwrap();
        assert_eq!(fetched.name, "A");
        assert_eq!(fetched.workdir, "/tmp/proj-a");

        assert!(
            store
                .update_project(
                    &a.id,
                    &ProjectUpdate {
                        name: "Alpha".into(),
                        description: "the first".into(),
                        daily_budget: None,
                    }
                )
                .await
                .unwrap()
        );
        let fetched = store.get_project(&a.id).await.unwrap().unwrap();
        assert_eq!(fetched.name, "Alpha");
        assert_eq!(fetched.description, "the first");

        assert!(store.set_project_archived(&a.id, true).await.unwrap());
        assert_eq!(
            store.list_projects(false).await.unwrap().len(),
            1,
            "an archived project leaves the default listing"
        );
        assert_eq!(
            store.list_projects(true).await.unwrap().len(),
            2,
            "…and comes back when the recycle bin is asked for"
        );
        assert!(
            store
                .get_project(&a.id)
                .await
                .unwrap()
                .unwrap()
                .archived_at
                .is_some(),
            "the row itself is never removed"
        );

        // An unknown id reports no row rather than erroring.
        let ghost = ProjectId::parse("ghost").unwrap();
        assert!(!store.set_project_archived(&ghost, true).await.unwrap());
    }

    #[tokio::test]
    async fn issue_numbers_are_per_project_and_sequential() {
        let (_dir, store) = store().await;
        let a = project("proj-a", "A");
        let b = project("proj-b", "B");
        store.create_project(&a).await.unwrap();
        store.create_project(&b).await.unwrap();

        let a1 = store
            .create_issue(&new_issue(&a.id, "first", IssueStatus::Backlog))
            .await
            .unwrap();
        let a2 = store
            .create_issue(&new_issue(&a.id, "second", IssueStatus::Backlog))
            .await
            .unwrap();
        let b1 = store
            .create_issue(&new_issue(&b.id, "elsewhere", IssueStatus::Backlog))
            .await
            .unwrap();

        assert_eq!((a1.number, a2.number), (1, 2));
        assert_eq!(b1.number, 1, "numbering restarts inside each project");
        assert_eq!(
            (a1.position, a2.position),
            (0, 1),
            "a new card lands at the tail of its column"
        );

        // Addressing is (project, number): b's #1 is not a's #1.
        assert_eq!(
            store.get_issue(&a.id, 1).await.unwrap().unwrap().title,
            "first"
        );
        assert_eq!(
            store.get_issue(&b.id, 1).await.unwrap().unwrap().title,
            "elsewhere"
        );
        assert!(store.get_issue(&a.id, 99).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn concurrent_creates_never_collide_on_a_number() {
        let (_dir, store) = store().await;
        let p = project("proj-a", "A");
        store.create_project(&p).await.unwrap();
        let store = std::sync::Arc::new(store);

        let mut handles = Vec::new();
        for i in 0..16 {
            let store = std::sync::Arc::clone(&store);
            let pid = p.id.clone();
            handles.push(tokio::spawn(async move {
                store
                    .create_issue(&new_issue(
                        &pid,
                        &format!("issue {i}"),
                        IssueStatus::Backlog,
                    ))
                    .await
            }));
        }
        let mut numbers = Vec::new();
        for handle in handles {
            numbers.push(handle.await.unwrap().unwrap().number);
        }
        numbers.sort_unstable();
        assert_eq!(
            numbers,
            (1..=16).collect::<Vec<_>>(),
            "sixteen racing creates take sixteen distinct numbers"
        );
    }

    #[tokio::test]
    async fn move_changes_column_and_renumbers_it() {
        let (_dir, store) = store().await;
        let p = project("proj-a", "A");
        store.create_project(&p).await.unwrap();
        for title in ["a", "b", "c"] {
            store
                .create_issue(&new_issue(&p.id, title, IssueStatus::Backlog))
                .await
                .unwrap();
        }
        // Card #3 jumps to Todo; Backlog closes ranks behind it.
        assert!(
            store
                .move_issue(&p.id, 3, IssueStatus::Todo, &[3])
                .await
                .unwrap()
        );
        assert!(
            store
                .move_issue(&p.id, 1, IssueStatus::Backlog, &[2, 1])
                .await
                .unwrap()
        );

        let issues = store.list_issues(&p.id).await.unwrap();
        let by_number = |n: i64| issues.iter().find(|i| i.number == n).unwrap();
        assert_eq!(by_number(3).status, IssueStatus::Todo);
        assert_eq!(by_number(3).position, 0);
        assert_eq!(by_number(2).position, 0);
        assert_eq!(by_number(1).position, 1);

        // A number from another column cannot be renumbered into this one.
        assert!(
            store
                .move_issue(&p.id, 2, IssueStatus::Backlog, &[2, 3])
                .await
                .unwrap()
        );
        assert_eq!(
            store.get_issue(&p.id, 3).await.unwrap().unwrap().status,
            IssueStatus::Todo,
            "#3 stayed in Todo despite being named in a Backlog reorder"
        );

        assert!(
            !store
                .move_issue(&p.id, 99, IssueStatus::Done, &[])
                .await
                .unwrap(),
            "an unknown issue reports no row moved"
        );
    }

    #[tokio::test]
    async fn the_column_a_card_leaves_closes_its_gap() {
        let (_dir, store) = store().await;
        let p = project("proj-a", "A");
        store.create_project(&p).await.unwrap();
        for title in ["a", "b", "c", "d"] {
            store
                .create_issue(&new_issue(&p.id, title, IssueStatus::Backlog))
                .await
                .unwrap();
        }
        // #2 leaves from the middle. The caller only ever knows the
        // destination's contents, so closing the source's rank is the
        // store's job — otherwise position 1 stays vacant forever.
        assert!(
            store
                .move_issue(&p.id, 2, IssueStatus::Review, &[2])
                .await
                .unwrap()
        );

        let issues = store.list_issues(&p.id).await.unwrap();
        let backlog: Vec<(i64, i64)> = issues
            .iter()
            .filter(|i| i.status == IssueStatus::Backlog)
            .map(|i| (i.number, i.position))
            .collect();
        assert_eq!(
            backlog,
            vec![(1, 0), (3, 1), (4, 2)],
            "the survivors keep their order and take consecutive ranks"
        );
    }

    #[tokio::test]
    async fn a_run_is_recorded_before_anything_is_dispatched() {
        let (_dir, store) = store().await;
        let p = project("proj-a", "A");
        store.create_project(&p).await.unwrap();
        let issue = store
            .create_issue(&new_issue(&p.id, "work", IssueStatus::Backlog))
            .await
            .unwrap();

        let run = store.enqueue_run(&new_run(&issue)).await.unwrap();
        assert_eq!(run.status, RunStatus::Queued);
        assert_eq!(run.attempt, 1);
        assert!(run.session_id.is_none(), "a queued run has no session yet");

        // The dedupe guard is the index, not a check-then-write: a second
        // unfinished run for the same issue cannot exist.
        let refused = store
            .enqueue_run(&new_run(&issue))
            .await
            .expect_err("an issue holds one run at a time");
        assert!(matches!(refused, StorageError::Conflict(_)), "{refused:?}");

        // Claim, then settle. Both are scoped, so a replay is a no-op.
        let session = SessionId::from("issue-1");
        assert!(store.claim_run(&run.id, &session).await.unwrap());
        assert!(
            !store.claim_run(&run.id, &session).await.unwrap(),
            "a claimed run cannot be claimed again — that is how a double dispatch collapses"
        );
        let claimed = store.get_run(&run.id).await.unwrap().unwrap();
        assert_eq!(claimed.status, RunStatus::Running);
        assert_eq!(claimed.session_id, Some(session));
        assert!(claimed.started_at.is_some());

        assert!(
            store
                .settle_run(&run.id, RunStatus::Done, None)
                .await
                .unwrap()
        );
        assert!(
            !store
                .settle_run(&run.id, RunStatus::Failed, Some("late"))
                .await
                .unwrap(),
            "a settled run stays settled, so the boot re-drive can replay freely"
        );
        let settled = store.get_run(&run.id).await.unwrap().unwrap();
        assert_eq!(settled.status, RunStatus::Done);
        assert!(settled.error.is_none());

        // …and the slot is free again.
        let second = store.enqueue_run(&new_run(&issue)).await.unwrap();
        assert_eq!(second.attempt, 2);
    }

    #[tokio::test]
    async fn the_boot_sweep_returns_orphaned_runs_to_the_queue() {
        let (_dir, store) = store().await;
        let p = project("proj-a", "A");
        store.create_project(&p).await.unwrap();
        let one = store
            .create_issue(&new_issue(&p.id, "one", IssueStatus::Backlog))
            .await
            .unwrap();
        let two = store
            .create_issue(&new_issue(&p.id, "two", IssueStatus::Backlog))
            .await
            .unwrap();
        let three = store
            .create_issue(&new_issue(&p.id, "three", IssueStatus::Backlog))
            .await
            .unwrap();

        // One never started, one died mid-flight, one finished before the crash.
        let queued = store.enqueue_run(&new_run(&one)).await.unwrap();
        let running = store.enqueue_run(&new_run(&two)).await.unwrap();
        store
            .claim_run(&running.id, &SessionId::from("issue-2"))
            .await
            .unwrap();
        let done = store.enqueue_run(&new_run(&three)).await.unwrap();
        store
            .settle_run(&done.id, RunStatus::Done, None)
            .await
            .unwrap();

        let resumed = store.requeue_unsettled().await.unwrap();
        let ids: Vec<&str> = resumed.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![queued.id.as_str(), running.id.as_str()],
            "both unfinished runs come back, the settled one does not"
        );
        assert!(resumed.iter().all(|r| r.status == RunStatus::Queued));
        assert!(
            resumed.iter().all(|r| r.session_id.is_none()),
            "the dead session is dropped, or the re-claim would be refused"
        );

        // The sweep is idempotent — booting twice is not a double dispatch.
        assert_eq!(store.requeue_unsettled().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn sparse_update_leaves_unnamed_fields_alone() {
        let (_dir, store) = store().await;
        let p = project("proj-a", "A");
        store.create_project(&p).await.unwrap();
        store
            .create_issue(&new_issue(&p.id, "original", IssueStatus::Backlog))
            .await
            .unwrap();

        assert!(
            store
                .update_issue(
                    &p.id,
                    1,
                    &IssueUpdate {
                        description: Some("filled in".into()),
                        ..Default::default()
                    }
                )
                .await
                .unwrap()
        );
        let issue = store.get_issue(&p.id, 1).await.unwrap().unwrap();
        assert_eq!(
            issue.title, "original",
            "a patch that named no title kept it"
        );
        assert_eq!(issue.description, "filled in");

        // Blocking, then clearing the block: `Some(None)` is a real value,
        // distinct from `None` meaning "leave it".
        store
            .update_issue(
                &p.id,
                1,
                &IssueUpdate {
                    blocked_reason: Some(Some("waiting on tmux".into())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .get_issue(&p.id, 1)
                .await
                .unwrap()
                .unwrap()
                .blocked_reason
                .as_deref(),
            Some("waiting on tmux")
        );
        store
            .update_issue(
                &p.id,
                1,
                &IssueUpdate {
                    title: Some("renamed".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let issue = store.get_issue(&p.id, 1).await.unwrap().unwrap();
        assert_eq!(issue.title, "renamed");
        assert_eq!(
            issue.blocked_reason.as_deref(),
            Some("waiting on tmux"),
            "an unrelated patch did not clear the block"
        );
        store
            .update_issue(
                &p.id,
                1,
                &IssueUpdate {
                    blocked_reason: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            store
                .get_issue(&p.id, 1)
                .await
                .unwrap()
                .unwrap()
                .blocked_reason
                .is_none()
        );

        // Cancel is the terminal negative: the row stays, stamped.
        store
            .update_issue(
                &p.id,
                1,
                &IssueUpdate {
                    cancelled: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(
            store
                .get_issue(&p.id, 1)
                .await
                .unwrap()
                .unwrap()
                .cancelled_at
                .is_some()
        );
        assert!(
            !store
                .update_issue(&p.id, 99, &IssueUpdate::default())
                .await
                .unwrap()
        );
    }

    /// The budget gate's read. Two things it has to get right: only this
    /// board's sessions count, and a session reused by several runs counts
    /// once — an issue keeps one session across every run of it, so a join
    /// would multiply the same call by the number of runs that shared it.
    #[tokio::test]
    async fn spend_since_sums_one_board_and_never_double_counts_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(dir.path().join("test.db")).await.unwrap();
        let store = SqliteProjectStore::new(pool.clone());
        let costs = crate::sqlite::cost::SqliteCostStore::new(pool);

        let mine = project("01JMINE", "mine");
        let theirs = project("01JTHEIRS", "theirs");
        for row in [&mine, &theirs] {
            store.create_project(row).await.unwrap();
        }
        let issue = |p: &ProjectRow, title: &str| NewIssue {
            id: IssueId::generate(),
            project_id: p.id.clone(),
            title: title.to_owned(),
            description: String::new(),
            status: IssueStatus::Backlog,
            priority: IssuePriority::None,
            assignee: Some(AgentProfileId::parse("dev-1").unwrap()),
            parent_issue_id: None,
            stage: 0,
            created_at: chrono::Utc::now(),
        };
        let ours = store.create_issue(&issue(&mine, "ours")).await.unwrap();
        let other = store.create_issue(&issue(&theirs, "theirs")).await.unwrap();

        // One session on our issue, shared by two runs of it — the real
        // shape, since an issue's session is reused across runs.
        let shared = SessionId::from("sess-ours".to_owned());
        let their_session = SessionId::from("sess-theirs".to_owned());
        for (row, session, settle) in [
            (&ours, &shared, true),
            (&ours, &shared, false),
            (&other, &their_session, false),
        ] {
            let run = store
                .enqueue_run(&NewIssueRun {
                    id: IssueRunId::generate(),
                    issue_id: row.id.clone(),
                    project_id: row.project_id.clone(),
                    number: row.number,
                    agent_id: AgentProfileId::parse("dev-1").unwrap(),
                    trigger: RunTrigger::Started,
                })
                .await
                .unwrap();
            store.claim_run(&run.id, session).await.unwrap();
            if settle {
                store
                    .settle_run(&run.id, RunStatus::Done, None)
                    .await
                    .unwrap();
            }
        }

        let now = chrono::Utc::now();
        let spend = |session: &SessionId, micros: i64, at: chrono::DateTime<chrono::Utc>| {
            baybo_model::CostRecord {
                user_id: "u".into(),
                session_id: session.clone(),
                turn_id: baybo_model::TurnId::new(),
                span_id: baybo_model::SpanId::new(),
                reason: baybo_model::CallReason::default(),
                model: "m".into(),
                reasoning_effort: None,
                input_tokens: 1,
                output_tokens: 1,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                cost_usd: baybo_model::MicroUsd::from_micros(micros),
                timestamp: at,
            }
        };
        let yesterday = now - chrono::Duration::days(1);
        for record in [
            spend(&shared, 300, now),
            spend(&shared, 200, now),
            // Outside the window, and another board's — neither counts.
            spend(&shared, 9_000, yesterday),
            spend(&their_session, 7_000, now),
        ] {
            baybo_store::cost::CostStore::record(&costs, &record)
                .await
                .unwrap();
        }

        let since = now - chrono::Duration::hours(1);
        assert_eq!(
            store.spend_since(&mine.id, since).await.unwrap(),
            baybo_model::MicroUsd::from_micros(500),
            "two calls on one shared session, counted once each"
        );
        assert_eq!(
            store.spend_since(&theirs.id, since).await.unwrap(),
            baybo_model::MicroUsd::from_micros(7_000)
        );
    }
}
