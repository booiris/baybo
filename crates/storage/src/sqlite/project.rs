//! sqlite implementation of [`ProjectStore`].

use async_trait::async_trait;
use baybo_model::{AgentProfileId, IssueId, IssueRunId, ProjectId, SessionId};
use rusqlite::OptionalExtension;

use super::SqlitePool;
use baybo_store::StorageError;
use baybo_store::project::{
    ACTOR_AGENT_PREFIX, AttentionCounts, BoardActivity, CardSignals, DrainMarks,
    IdempotentIssueEvent, IssueActor, IssueAttachment, IssueEventAppendOutcome, IssueEventBody,
    IssueEventClientMsgId, IssueEventRow, IssuePriority, IssueRow, IssueRunRow, IssueStatus,
    IssueUpdate, NewIssue, NewIssueEvent, NewIssueRun, ProjectRow, ProjectStore, ProjectUpdate,
    Result, RunSpend, RunStatus, RunTrigger, SettledRunFacts, Spend,
};

pub struct SqliteProjectStore {
    pool: SqlitePool,
}

impl SqliteProjectStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

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

    async fn append_event_with_client_msg_id(
        &self,
        new: &NewIssueEvent,
        client_msg_id: Option<IssueEventClientMsgId>,
    ) -> Result<IssueEventAppendOutcome> {
        let row = IssueEventRow {
            id: baybo_model::IssueEventId::generate(),
            issue_id: new.issue_id.clone(),
            project_id: new.project_id.clone(),
            number: new.number,
            actor: new.actor.clone(),
            body: new.body.clone(),
            client_msg_id,
            created_at: chrono::Utc::now(),
        };
        let id = row.id.as_str().to_string();
        let issue_id = row.issue_id.as_str().to_string();
        let project = row.project_id.as_str().to_string();
        let number = row.number;
        let actor = row.actor.to_storage();
        let kind = row.body.kind().to_owned();
        let body = serde_json::to_string(&row.body)
            .map_err(|e| StorageError::Storage(format!("serialize issue event: {e}")))?;
        let created = super::time::to_us(row.created_at);
        let consequences_applied = row.client_msg_id.is_none();
        let client_msg_id = row.client_msg_id.as_ref().map(|id| id.as_str().to_owned());
        let existing = self
            .pool
            .interact_write("issue_events.append_idempotent", move |conn| {
                let tx = conn.transaction()?;
                let inserted = tx.execute(
                    "INSERT INTO issue_events (id, issue_id, project_id, number, actor, kind, \
                     body, created_at, client_msg_id, comment_consequences_applied) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                     ON CONFLICT DO NOTHING",
                    rusqlite::params![
                        id,
                        issue_id,
                        project,
                        number,
                        actor,
                        kind,
                        body,
                        created,
                        client_msg_id,
                        consequences_applied,
                    ],
                )?;
                if inserted == 1 {
                    tx.commit()?;
                    return Ok(None);
                }
                let key = client_msg_id.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "issue event insert conflicted without a client message id to resolve"
                    )
                })?;
                let existing = tx
                    .query_row(
                        &format!(
                            "SELECT {EVENT_COLUMNS} FROM issue_events \
                             WHERE issue_id = ?1 AND client_msg_id = ?2"
                        ),
                        rusqlite::params![issue_id, key],
                        read_raw_event,
                    )
                    .optional()?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "idempotent issue event insert returned no row and no existing key"
                        )
                    })?;
                tx.commit()?;
                Ok(Some(existing))
            })
            .await?;
        match existing {
            Some(raw) => Ok(IssueEventAppendOutcome::Existing(
                idempotent_event_from_raw(raw)?,
            )),
            None => Ok(IssueEventAppendOutcome::Inserted(row)),
        }
    }
}

const EVENT_COLUMNS: &str = "id, issue_id, project_id, number, actor, body, created_at, client_msg_id, \
     comment_consequences_applied";

type RawEvent = (
    String,
    String,
    String,
    i64,
    String,
    String,
    i64,
    Option<String>,
    bool,
);

fn read_raw_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawEvent> {
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
    ))
}

fn event_from_raw(raw: RawEvent) -> Result<IssueEventRow> {
    let (id, issue_id, project_id, number, actor, body, created_at, client_msg_id, _) = raw;
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
        client_msg_id: client_msg_id
            .map(|id| {
                IssueEventClientMsgId::parse(&id).map_err(|e| {
                    StorageError::Storage(format!("issue_events.client_msg_id invalid: {e}"))
                })
            })
            .transpose()?,
        created_at: ts("issue_events.created_at", created_at)?,
    })
}

fn idempotent_event_from_raw(raw: RawEvent) -> Result<IdempotentIssueEvent> {
    let consequences_applied = raw.8;
    Ok(IdempotentIssueEvent {
        event: event_from_raw(raw)?,
        consequences_applied,
    })
}

/// The projection order **is** [`RawProject`]'s tuple order, and nothing
/// links the two at compile time. A new column goes on the end, always —
/// see [`ISSUE_COLUMNS`] for what inserting one mid-list does.
const PROJECT_COLUMNS: &str = "id, name, description, workdir, daily_budget_micros, \
     daily_budget_tokens, max_parallel_issue_runs, archived_at, created_at, updated_at, \
     agents_may_merge, COALESCE(rules_changed_at, created_at)";

/// Which of a project's editable fields are **rules the board schedules
/// by**: the ones whose change makes every question the lead has already
/// answered a question again ([`ProjectRow::rules_changed_at`]). Name and
/// description are deliberately absent — renaming a board changes no answer.
///
/// Read against the pre-update row with the incoming values bound, which is
/// what makes it a comparison rather than a flag. The `COALESCE` pair is why
/// a legacy `NULL` that resolves to the same effective value is not a
/// change; `IS NOT` is SQLite's null-safe inequality.
const A_BOARD_RULE_CHANGED: &str = "daily_budget_micros IS NOT ?4 \
     OR daily_budget_tokens IS NOT ?5 \
     OR COALESCE(max_parallel_issue_runs, ?9) IS NOT ?6 \
     OR COALESCE(agents_may_merge, ?10) IS NOT ?7";

/// Whether a patch would actually change the card, expression for
/// expression against the `SET` list in `update_issue`.
///
/// `issues.updated_at` is the board's answer to "has this card changed
/// since the lead looked" (`driver::already_asked`), and it was stamped
/// unconditionally while every value column beside it is guarded — so a
/// write that set each field to what it already held moved the clock,
/// recorded nothing on the timeline (`timeline::diff_events` compares
/// values), and re-opened every question the lead had answered. It was
/// invisible in exactly the way that matters: a byte-identical rewrite of
/// five cards' `blocked_reason` cost five lead runs and left not one row
/// behind to say why.
///
/// Read against the pre-update row with the incoming values bound, like
/// [`A_BOARD_RULE_CHANGED`]: SQLite evaluates every expression in an
/// `UPDATE` against the row as it was. `IS NOT` is the null-safe
/// inequality, which is the whole reason the comparison can be written at
/// all — half these columns are nullable.
///
/// **Every arm here mirrors one line of that `SET`.** A column added to one
/// and not the other is a card that changes without saying so, which is the
/// defect this exists to close; `a_write_that_changes_nothing_does_not_move_the_clock`
/// pins the pair.
const AN_ISSUE_FIELD_CHANGED: &str = "COALESCE(?3, title) IS NOT title \
     OR COALESCE(?4, description) IS NOT description \
     OR COALESCE(?16, attachments) IS NOT attachments \
     OR COALESCE(?5, priority) IS NOT priority \
     OR (CASE WHEN ?6 THEN ?7 ELSE assignee END) IS NOT assignee \
     OR (CASE WHEN ?8 THEN ?9 ELSE blocked_reason END) IS NOT blocked_reason \
     OR (CASE WHEN ?10 THEN ?11 ELSE cancelled_at END) IS NOT cancelled_at \
     OR (CASE WHEN ?13 THEN ?14 ELSE parent_issue_id END) IS NOT parent_issue_id \
     OR COALESCE(?15, stage) IS NOT stage \
     OR COALESCE(?17, pinned) IS NOT pinned";

/// The projection order **is** [`RawIssue`]'s tuple order, and nothing links
/// the two at compile time. A new column goes on the end, always: inserting
/// one mid-list silently re-decodes every field after it, and only the ones
/// that happen to be parsed would fail loudly.
const ISSUE_COLUMNS: &str = "id, project_id, number, title, description, status, priority, \
     assignee, position, blocked_reason, branch, parent_issue_id, stage, source_key, \
     cancelled_at, created_at, updated_at, attachments, pinned, filed_from_issue_id";

/// Shared unread predicate for agent comments, blocks, and moves to Review.
/// Used by card badges and board attention; binds `:review`.
const UNREAD_EVENT_PREDICATE: &str = "e.created_at > COALESCE(i.read_at, 0) \
     AND e.actor LIKE 'agent:%' \
     AND (e.kind = 'comment' \
          OR e.kind = 'blocked' \
          OR (e.kind = 'moved' AND json_extract(e.body, '$.to') = :review))";

/// The coordination triggers as a SQL list, with one home.
///
/// A macro rather than a `const` because `concat!` takes only literals, and
/// the queries that must tell the lead's wakes from the card's own work are
/// built at compile time. `coordination_triggers_match_the_enum` pins it to
/// [`RunTrigger::is_coordination`], so a variant added to one cannot quietly
/// miss the other.
macro_rules! coordination_triggers {
    () => {
        "'triage', 'review', 'stalled', 'blocked', 'grooming', 'board_idle'"
    };
}

/// One column off the card's newest **work** run, by the one ordering every
/// reader of "the newest run" must share. A macro so that the two
/// predicates below read `status` and `settled_at` off the *same row* by
/// construction: they answer "did it fail" and "has the operator seen that
/// failure", and two orderings that drifted apart would answer them about
/// different runs.
///
/// Coordination runs — the lead woken to triage, review or look at stalled
/// work — are skipped: the board wakes the lead *because* of a card's
/// state, and a wake that then became "the newest run" would clear the very
/// failed badge that state is, without anybody acting on it. The literal
/// list is pinned to [`RunTrigger::is_coordination`] by
/// `coordination_triggers_match_the_enum` below.
///
/// Written against `issues i`.
macro_rules! newest_run {
    ($column:literal) => {
        concat!(
            "(SELECT r.",
            $column,
            " FROM issue_runs r WHERE r.issue_id = i.id AND r.trigger NOT IN (",
            coordination_triggers!(),
            ") ORDER BY r.created_at DESC, r.id DESC LIMIT 1)"
        )
    };
}

/// The board's cards an agent opened, by number. Binds the project, the
/// `Opened` discriminator and the agent actor prefix, so neither the kind
/// column nor the actor spelling is written a second time here.
///
/// `DISTINCT` guards a hand-edited row rather than anything the runtime
/// writes: a card is opened once.
const AGENT_OPENED_ISSUES: &str = "SELECT DISTINCT number FROM issue_events \
     WHERE project_id = ?1 AND kind = ?2 AND actor LIKE ?3";

/// A live card whose newest run failed, written once. Written against
/// `issues i`; binds `:done` and `:failed`.
///
/// This is the card's **state**, and it clears only by acting — retry,
/// finish, cancel, block. Nothing retries by itself, so a badge that a
/// glance could clear would take the board's own record of what is broken
/// with it. The badge on the card reads exactly this.
///
/// It is not, on its own, what lights the rail: see
/// [`UNSEEN_FAILURE_PREDICATE`].
const FAILED_CARD_PREDICATE: &str = concat!(
    "i.status <> :done AND i.cancelled_at IS NULL \
     AND i.blocked_reason IS NULL \
     AND ",
    newest_run!("status"),
    " = :failed"
);

/// Whether a failure is also **news** — the newest run settled after the
/// operator last opened the card.
///
/// The rail's mark is a pointer ("something over there wants you"), not a
/// tally of what is broken, and a pointer that survives being followed is
/// noise: the operator opens the card, reads the failure, and the mark is
/// still lit with nothing left to do about it but act on a card they may
/// deliberately be leaving for tomorrow. So `attention` counts a failure
/// only until it has been seen, on the same `read_at` cursor as
/// [`UNREAD_EVENT_PREDICATE`], while the card keeps wearing
/// [`FAILED_CARD_PREDICATE`]'s badge until it is actually dealt with.
///
/// The two therefore disagree on purpose, and only in the safe direction:
/// the rail goes quiet while the board still shows the failure. The
/// dangerous direction — a lit rail over a board on which every card reads
/// zero — is what [`UNREAD_EVENT_PREDICATE`] warns about, and this cannot
/// produce it.
///
/// A card that fails *again* relights by itself: the new run's `settled_at`
/// clears the same cursor. That is the whole rule, not a second one.
const UNSEEN_FAILURE_PREDICATE: &str =
    concat!(newest_run!("settled_at"), " > COALESCE(i.read_at, 0)");

/// What attributes one `cost_records` row to one run: the run's own
/// claim→settle window on the session it worked in. Written against
/// `issue_runs r` and `cost_records c`; timestamps are microseconds.
///
/// A window and not a stored `run_id`, because the id is not reachable
/// where a cost row is written — the ledger sees an `Attribution` of
/// user/session/turn/span and has never heard of a board. The window is
/// only unambiguous because of two invariants that live elsewhere, and it
/// silently double-counts if either is relaxed:
///
/// - `idx_issue_runs_live_agent` allows at most one unsettled run per
///   (issue, agent), so two windows on one session cannot overlap. That is
///   the narrow constraint this depends on, and it is stated separately
///   from `idx_issue_runs_live` — which allows one per *issue* — precisely
///   so that widening the card's slot does not silently take this with it.
/// - `Router::issue_session` mints one session per card **per agent**, so
///   a session never spans two cards.
///
/// A run that was never claimed has a NULL `started_at`, which makes every
/// comparison NULL: it matches nothing and reads zero rather than
/// inheriting the session's whole history.
/// The session test is a spawn *tree*, not one id: a subagent bills against
/// its own session, and billing the run only for the session it works in
/// directly would let a subagent-heavy run read near zero. A subagent
/// inherits `root_session_id` from the ultimate ancestor and a run's session
/// is itself a root, so `root_session_id = r.session_id` is exactly that
/// run's tree. The direct `c.session_id = r.session_id` disjunct stays in
/// front of it deliberately: it needs no `sessions` row, so a run's own
/// spend is never contingent on a second table resolving.
const RUN_COST_WINDOW: &str = "(c.session_id = r.session_id \
          OR c.session_id IN (SELECT s.id FROM sessions s \
                              WHERE s.root_session_id = r.session_id)) \
     AND c.timestamp >= r.started_at \
     AND (r.settled_at IS NULL OR c.timestamp < r.settled_at)";

/// Every session one board answers for, as a predicate over `cost_records c`
/// with the board id bound at `?1`. Three membership sources, united rather
/// than reduced to the one that subsumes the others in practice:
///
/// - the board's own run sessions, straight off `issue_runs` — the only
///   source that needs no `sessions` row, and so the one that keeps a
///   board's burn from depending on a second table;
/// - every session whose trigger names the board. That is what brings a
///   board-bound **cron fire** inside the ceiling: it files work on the
///   board and bills real tokens, but it is nobody's run, so no
///   `issue_runs` row will ever point at it;
/// - every session rooted at one of those run sessions — the same spawn
///   tree [`RUN_COST_WINDOW`] bills a run for.
///
/// The third is redundant with the second while subagents inherit their
/// parent's trigger, and is kept anyway because it is what makes
/// `board ⊇ every run on it` true *structurally*. The card-level and
/// board-level meters have to widen together; a card whose total exceeded
/// its board's would be the exact failure this pairing exists to prevent.
///
/// Written as a session *set* the row must fall in, and deliberately not as
/// a per-row `COALESCE(...)` that resolves each cost row's board: the set is
/// built once from three indexed reads, while the per-row form costs three
/// probes on every cost record in the window. Measured on 220k sessions /
/// 400k cost records, one day's window: 1.9 ms for this, 235 ms for the
/// per-row form. The window holds far more cost rows than a board has
/// sessions, and that is the ordinary shape.
const BOARD_SESSIONS: &str = "(c.session_id IN (SELECT session_id FROM issue_runs \
                                  WHERE project_id = ?1 AND session_id IS NOT NULL) \
      OR c.session_id IN (SELECT s.id FROM sessions s \
                          WHERE s.project_id = ?1 \
                             OR s.root_session_id IN (SELECT session_id FROM issue_runs \
                                  WHERE project_id = ?1 AND session_id IS NOT NULL)))";

/// Shared money/token aggregates; cached-token columns are input subsets.
const SPEND_SUMS: &str = "COALESCE(SUM(c.input_tokens), 0), COALESCE(SUM(c.output_tokens), 0), \
     COALESCE(SUM(c.cost_usd), 0)";

fn spend_from_row(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<Spend> {
    Ok(Spend {
        input_tokens: row.get(first)?,
        output_tokens: row.get(first + 1)?,
        cost: baybo_model::MicroUsd::from_micros(row.get(first + 2)?),
    })
}

/// Positional run columns; append fields to preserve [`read_raw_run`] indexes.
const RUN_COLUMNS: &str = "id, issue_id, project_id, number, agent_id, session_id, trigger, \
     status, attempt, error, created_at, started_at, settled_at, resumes";

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
    i64,
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
        row.get(13)?,
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
        resumes,
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
        resumes,
        error,
        created_at: ts("issue_runs.created_at", created_at)?,
        started_at: ts_opt("issue_runs.started_at", started_at)?,
        settled_at: ts_opt("issue_runs.settled_at", settled_at)?,
    })
}

type RawProject = (
    String,
    String,
    String,
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    i64,
    i64,
    Option<i64>,
    i64,
);

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
    Option<String>,
    Option<i64>,
    i64,
    i64,
    String,
    i64,
    Option<String>,
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
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
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
        row.get(16)?,
        row.get(17)?,
        row.get(18)?,
        row.get(19)?,
    ))
}

fn encode_attachments(attachments: &[IssueAttachment]) -> Result<String> {
    serde_json::to_string(attachments)
        .map_err(|e| StorageError::Storage(format!("issues.attachments could not encode: {e}")))
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
    let (
        id,
        name,
        description,
        workdir,
        daily_budget_micros,
        daily_budget_tokens,
        max_parallel_issue_runs,
        archived_at,
        created_at,
        updated_at,
        agents_may_merge,
        rules_changed_at,
    ) = raw;
    Ok(ProjectRow {
        // A stored id runs the grammar again on the way out: the row is the
        // source of a directory name, and a hand-edited DB is still a way in.
        id: ProjectId::parse(id).map_err(|e| StorageError::Storage(e.to_string()))?,
        name,
        description,
        workdir,
        daily_budget: daily_budget_micros.map(baybo_model::MicroUsd::from_micros),
        daily_budget_tokens,
        // NULL is a row written before the column existed, not a board that
        // chose "no ceiling": the driver has to have a number, and the one
        // it gets is the same one a board opened today starts with.
        max_parallel_issue_runs: max_parallel_issue_runs
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(baybo_store::project::DEFAULT_MAX_PARALLEL_ISSUE_RUNS),
        // NULL is the same "written before the column existed" as above, and
        // the same answer a board opened today starts with.
        agents_may_merge: agents_may_merge
            .map(|n| n != 0)
            .unwrap_or(baybo_store::project::DEFAULT_AGENTS_MAY_MERGE),
        // The projection coalesces this to the board's own `created_at`, so
        // a row written before the column existed reads as "no rule has
        // changed since this board opened" rather than as a rule change at
        // the epoch.
        rules_changed_at: ts("projects.rules_changed_at", rules_changed_at)?,
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
        source_key,
        cancelled_at,
        created_at,
        updated_at,
        attachments,
        pinned,
        filed_from,
    ) = raw;
    Ok(IssueRow {
        id: IssueId::from(id),
        project_id: ProjectId::parse(project_id)
            .map_err(|e| StorageError::Storage(e.to_string()))?,
        number,
        title,
        description,
        // Fail-closed like every other field here, and safe to be: every
        // field `IssueAttachment` grows after this ships must carry
        // `#[serde(default)]`, exactly as the comment body's does — the
        // discipline is what keeps an old row readable, not leniency here.
        attachments: serde_json::from_str(&attachments)
            .map_err(|e| StorageError::Storage(format!("issues.attachments is not a list: {e}")))?,
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
        pinned: pinned != 0,
        blocked_reason,
        branch,
        parent_issue_id: parent_issue_id.map(IssueId::from),
        stage,
        source_key,
        filed_from: filed_from.map(IssueId::from),
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
        let daily_budget_tokens = row.daily_budget_tokens;
        let max_parallel_issue_runs =
            i64::try_from(row.max_parallel_issue_runs).unwrap_or(i64::MAX);
        let agents_may_merge = i64::from(row.agents_may_merge);
        let rules_changed_at = super::time::to_us(row.rules_changed_at);
        let created_at = super::time::to_us(row.created_at);
        let updated_at = super::time::to_us(row.updated_at);
        self.pool
            .interact_write("projects.create", move |conn| {
                conn.execute(
                    "INSERT INTO projects \
                     (id, name, description, workdir, daily_budget_micros, \
                      daily_budget_tokens, max_parallel_issue_runs, agents_may_merge, \
                      rules_changed_at, archived_at, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?11)",
                    rusqlite::params![
                        id,
                        name,
                        description,
                        workdir,
                        daily_budget,
                        daily_budget_tokens,
                        max_parallel_issue_runs,
                        agents_may_merge,
                        rules_changed_at,
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
        let daily_budget_tokens = update.daily_budget_tokens;
        let max_parallel_issue_runs =
            i64::try_from(update.max_parallel_issue_runs).unwrap_or(i64::MAX);
        let agents_may_merge = i64::from(update.agents_may_merge);
        let default_parallel = i64::try_from(baybo_store::project::DEFAULT_MAX_PARALLEL_ISSUE_RUNS)
            .unwrap_or(i64::MAX);
        let default_may_merge = i64::from(baybo_store::project::DEFAULT_AGENTS_MAY_MERGE);
        let now = super::time::now_us();
        let affected = self
            .pool
            .interact_write("projects.update", move |conn| {
                // One statement, so the stamp cannot be written against a
                // row other than the one it compared. SQLite evaluates every
                // expression in an UPDATE against the pre-update row, which
                // is what lets the CASE read the old values by name.
                Ok(conn.execute(
                    &format!(
                        "UPDATE projects SET name = ?2, description = ?3, \
                         daily_budget_micros = ?4, daily_budget_tokens = ?5, \
                         max_parallel_issue_runs = ?6, agents_may_merge = ?7, \
                         rules_changed_at = CASE WHEN {A_BOARD_RULE_CHANGED} \
                             THEN ?8 ELSE rules_changed_at END, \
                         updated_at = ?8 \
                         WHERE id = ?1"
                    ),
                    rusqlite::params![
                        id,
                        name,
                        description,
                        daily_budget,
                        daily_budget_tokens,
                        max_parallel_issue_runs,
                        agents_may_merge,
                        now,
                        default_parallel,
                        default_may_merge
                    ],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn spend_since(
        &self,
        project: &ProjectId,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<Spend> {
        let project = project.as_str().to_string();
        let since = super::time::to_us(since);
        self.pool
            .interact("projects.spend_since", move |conn| {
                Ok(conn.query_row(
                    &format!(
                        "SELECT {SPEND_SUMS} FROM cost_records c \
                         WHERE c.timestamp >= ?2 AND {BOARD_SESSIONS}"
                    ),
                    rusqlite::params![project, since],
                    |row| spend_from_row(row, 0),
                )?)
            })
            .await
    }

    async fn mark_issue_read(
        &self,
        issue: &IssueId,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool> {
        let issue = issue.as_str().to_string();
        let at = super::time::to_us(at);
        let affected = self
            .pool
            .interact_write("issues.mark_read", move |conn| {
                Ok(conn.execute(
                    "UPDATE issues SET read_at = MAX(COALESCE(read_at, 0), ?2) WHERE id = ?1",
                    rusqlite::params![issue, at],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn mark_project_read(
        &self,
        project: &ProjectId,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<usize> {
        let project = project.as_str().to_string();
        let at = super::time::to_us(at);
        let affected = self
            .pool
            .interact_write("issues.mark_project_read", move |conn| {
                Ok(conn.execute(
                    // The guard is where `mark_issue_read`'s `MAX(...)` is:
                    // both keep the cursor monotonic, and here it also makes
                    // the row count mean "cards this actually moved" rather
                    // than "cards on this board".
                    "UPDATE issues SET read_at = ?2 \
                     WHERE project_id = ?1 AND COALESCE(read_at, 0) < ?2",
                    rusqlite::params![project, at],
                )?)
            })
            .await?;
        Ok(affected)
    }

    async fn card_signals(
        &self,
        project: &ProjectId,
    ) -> Result<std::collections::HashMap<IssueId, CardSignals>> {
        let project = project.as_str().to_string();
        let review_status = IssueStatus::Review.as_str();
        let failed_status = RunStatus::Failed.as_str();
        let done_status = IssueStatus::Done.as_str();
        let rows = self
            .pool
            .interact("issues.card_signals", move |conn| {
                let mut signals: std::collections::HashMap<String, CardSignals> =
                    std::collections::HashMap::new();

                {
                    let mut stmt = conn.prepare(&format!(
                        "SELECT e.issue_id, COUNT(*) FROM issue_events e \
                         JOIN issues i ON i.id = e.issue_id \
                         WHERE i.project_id = :project AND {UNREAD_EVENT_PREDICATE} \
                         GROUP BY e.issue_id"
                    ))?;
                    let rows = stmt.query_map(
                        rusqlite::named_params! {
                            ":project": project,
                            ":review": review_status,
                        },
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                    )?;
                    for row in rows {
                        let (issue, count) = row?;
                        signals.entry(issue).or_default().unread += count.max(0) as usize;
                    }
                }

                {
                    let mut stmt = conn.prepare(&format!(
                        "SELECT i.id FROM issues i \
                         WHERE i.project_id = :project AND {FAILED_CARD_PREDICATE}"
                    ))?;
                    let rows = stmt.query_map(
                        rusqlite::named_params! {
                            ":project": project,
                            ":done": done_status,
                            ":failed": failed_status,
                        },
                        |row| row.get::<_, String>(0),
                    )?;
                    for row in rows {
                        signals.entry(row?).or_default().last_run_failed = true;
                    }
                }
                Ok(signals.into_iter().collect::<Vec<_>>())
            })
            .await?;

        Ok(rows
            .into_iter()
            .map(|(issue, signals)| (IssueId::from(issue), signals))
            .collect())
    }

    async fn agent_opened_issues(&self, project: &ProjectId) -> Result<Vec<i64>> {
        let project = project.as_str().to_string();
        let opened = IssueEventBody::Opened.kind();
        let prefix = format!("{ACTOR_AGENT_PREFIX}%");
        self.pool
            .interact("issues.agent_opened", move |conn| {
                let mut stmt = conn.prepare(AGENT_OPENED_ISSUES)?;
                let rows = stmt
                    .query_map(rusqlite::params![project, opened, prefix], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<i64>>>()?;
                Ok(rows)
            })
            .await
    }

    async fn set_project_archived(&self, id: &ProjectId, archived: bool) -> Result<bool> {
        let id = id.as_str().to_string();
        let now = super::time::now_us();
        let stamp = archived.then_some(now);
        let affected = self
            .pool
            .interact_write("projects.set_archived", move |conn| {
                // Restoring is an operator turning a board back on, which is
                // the same class of change as raising a ceiling; shelving one
                // starts nothing, so it stamps nothing.
                Ok(conn.execute(
                    "UPDATE projects SET archived_at = ?2, updated_at = ?3, \
                     rules_changed_at = CASE WHEN ?4 THEN rules_changed_at ELSE ?3 END \
                     WHERE id = ?1 AND (archived_at IS NOT NULL) <> ?4",
                    rusqlite::params![id, stamp, now, archived],
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
        let source_key = new.source_key.clone();
        let filed_from = new.filed_from.as_ref().map(|id| id.as_str().to_string());
        let attachments = encode_attachments(&new.attachments)?;
        let created_at = super::time::to_us(new.created_at);
        let raw = self
            .pool
            .interact_write("issues.create", move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
                     source_key, filed_from_issue_id, cancelled_at, created_at, updated_at, \
                     attachments) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?11, ?13, ?15, \
                             NULL, ?12, ?12, ?14)",
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
                        created_at,
                        source_key,
                        attachments,
                        filed_from
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
            .interact_write("issues.update", move |conn| {
                let title = update.title.clone();
                let description = update.description.clone();
                // Encoded here rather than beside `description` because a
                // full replace and a COALESCE are different questions: the
                // absent case must leave the column alone, and `'[]'` is a
                // perfectly good *present* value meaning "no files".
                let attachments = update
                    .attachments
                    .as_deref()
                    .map(encode_attachments)
                    .transpose()
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                let priority = update.priority.map(|p| p.as_str());
                // COALESCE and not a CASE-WHEN pair: the three below are
                // doubly optional and need "set it to NULL" told apart from
                // "leave it alone". A pin has no such third state.
                let pinned = update.pinned.map(i64::from);
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
                    &format!(
                        "UPDATE issues SET \
                       title          = COALESCE(?3, title), \
                       description    = COALESCE(?4, description), \
                       attachments    = COALESCE(?16, attachments), \
                       priority       = COALESCE(?5, priority), \
                       assignee       = CASE WHEN ?6 THEN ?7 ELSE assignee END, \
                       blocked_reason = CASE WHEN ?8 THEN ?9 ELSE blocked_reason END, \
                       cancelled_at   = CASE WHEN ?10 THEN ?11 ELSE cancelled_at END, \
                       parent_issue_id = CASE WHEN ?13 THEN ?14 ELSE parent_issue_id END, \
                       stage          = COALESCE(?15, stage), \
                       pinned         = COALESCE(?17, pinned), \
                       updated_at     = CASE WHEN {AN_ISSUE_FIELD_CHANGED} \
                           THEN ?12 ELSE updated_at END \
                     WHERE project_id = ?1 AND number = ?2"
                    ),
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
                        stage,
                        attachments,
                        pinned
                    ],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn attention(&self) -> Result<Vec<(ProjectId, AttentionCounts)>> {
        let failed_status = RunStatus::Failed.as_str();
        let done_status = IssueStatus::Done.as_str();
        let review_status = IssueStatus::Review.as_str();
        let rows = self
            .pool
            .interact("projects.attention", move |conn| {
                // Held runs are deliberately absent — see [`AttentionCounts`].
                let mut counts: std::collections::HashMap<String, (usize, usize)> =
                    std::collections::HashMap::new();

                {
                    // Both halves: the card is broken (`FAILED_CARD_PREDICATE`)
                    // AND the operator has not looked since it broke. The board
                    // shows the first on the card; only the second is worth a
                    // mark in the rail.
                    let mut stmt = conn.prepare(&format!(
                        "SELECT i.project_id, COUNT(*) FROM issues i \
                         JOIN projects p ON p.id = i.project_id \
                         WHERE p.archived_at IS NULL AND {FAILED_CARD_PREDICATE} \
                           AND {UNSEEN_FAILURE_PREDICATE} \
                         GROUP BY i.project_id"
                    ))?;
                    let rows = stmt.query_map(
                        rusqlite::named_params! {
                            ":done": done_status,
                            ":failed": failed_status,
                        },
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                    )?;
                    for row in rows {
                        let (project, count) = row?;
                        counts.entry(project).or_default().0 += count.max(0) as usize;
                    }
                }

                {
                    // Joined through `issues` rather than read off
                    // `issue_events.project_id`, because the cursor this
                    // compares against lives on the card.
                    let mut stmt = conn.prepare(&format!(
                        "SELECT i.project_id, COUNT(*) FROM issue_events e \
                         JOIN issues i ON i.id = e.issue_id \
                         JOIN projects p ON p.id = i.project_id \
                         WHERE p.archived_at IS NULL AND {UNREAD_EVENT_PREDICATE} \
                         GROUP BY i.project_id"
                    ))?;
                    let rows = stmt.query_map(
                        rusqlite::named_params! { ":review": review_status },
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                    )?;
                    for row in rows {
                        let (project, count) = row?;
                        counts.entry(project).or_default().1 += count.max(0) as usize;
                    }
                }
                Ok(counts.into_iter().collect::<Vec<_>>())
            })
            .await?;

        rows.into_iter()
            .map(|(project, (failed, unread))| {
                Ok((
                    ProjectId::parse(project).map_err(|e| StorageError::Storage(e.to_string()))?,
                    AttentionCounts {
                        approvals: 0,
                        failed,
                        unread,
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

    async fn live_issue_by_source_key(
        &self,
        project: &ProjectId,
        source_key: &str,
    ) -> Result<Option<IssueRow>> {
        let project = project.as_str().to_string();
        let source_key = source_key.to_string();
        let done = IssueStatus::Done.as_str();
        let raw = self
            .pool
            .interact("issues.by_source_key", move |conn| {
                Ok(conn
                    .query_row(
                        &format!(
                            "SELECT {ISSUE_COLUMNS} FROM issues \
                             WHERE project_id = ?1 AND source_key = ?2 \
                               AND cancelled_at IS NULL AND status <> ?3"
                        ),
                        rusqlite::params![project, source_key, done],
                        read_raw_issue,
                    )
                    .optional()?)
            })
            .await?;
        raw.map(issue_from_raw).transpose()
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
            .interact_write("issues.move", move |conn| {
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
                // Same-column drags land here too — the reorder below is
                // the point of them — and the stamp is guarded for the same
                // reason `update_issue`'s is: tidying a column is not the
                // card changing, and the board reads that clock as "this is
                // a question again". `position` never stamps at all, which
                // is the rule this brings the column move into line with.
                tx.execute(
                    "UPDATE issues SET status = ?3, \
                       updated_at = CASE WHEN status IS NOT ?3 THEN ?4 ELSE updated_at END \
                     WHERE project_id = ?1 AND number = ?2",
                    rusqlite::params![project, number, status, now],
                )?;
                for (index, target) in ordered.iter().enumerate() {
                    tx.execute(
                        "UPDATE issues SET position = ?3 \
                         WHERE project_id = ?1 AND number = ?2 AND status = ?4",
                        rusqlite::params![project, target, index as i64, status],
                    )?;
                }
                if previous != status {
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
            .interact_write("issue_runs.enqueue", move |conn| {
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
                //
                // **Only** a constraint refusal comes out that way. Every
                // other failure here — a busy database, a full disk, a
                // datatype mismatch — is propagated as the error it is,
                // because the caller turns this side of the fork into
                // "issue already has a run in flight" and, since
                // `enqueue_as` began recording a `RunRefused` entry on it,
                // into a permanent line on the card saying a dedupe
                // refusal happened. A transient sqlite failure must not be
                // written into a card's history as one.
                if let Err(e) = tx.execute(
                    "INSERT INTO issue_runs (id, issue_id, project_id, number, agent_id, \
                     session_id, trigger, status, attempt, error, created_at, started_at, \
                     settled_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, 'queued', ?7, NULL, ?8, NULL, NULL)",
                    rusqlite::params![id, issue_id, project, number, agent, trigger, attempt, now],
                ) {
                    drop(tx);
                    if !super::already_there(&e) {
                        return Err(e.into());
                    }
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
        match self.append_event_with_client_msg_id(new, None).await? {
            IssueEventAppendOutcome::Inserted(row) => Ok(row),
            IssueEventAppendOutcome::Existing(_) => Err(StorageError::Storage(
                "ordinary issue event append unexpectedly resolved an existing row".to_owned(),
            )),
        }
    }

    async fn append_event_idempotent(
        &self,
        new: &NewIssueEvent,
        client_msg_id: &IssueEventClientMsgId,
    ) -> Result<IssueEventAppendOutcome> {
        self.append_event_with_client_msg_id(new, Some(client_msg_id.clone()))
            .await
    }

    async fn event_by_client_msg_id(
        &self,
        issue: &IssueId,
        client_msg_id: &IssueEventClientMsgId,
    ) -> Result<Option<IdempotentIssueEvent>> {
        let issue = issue.as_str().to_owned();
        let client_msg_id = client_msg_id.as_str().to_owned();
        let raw = self
            .pool
            .interact("issue_events.by_client_msg_id", move |conn| {
                Ok(conn
                    .query_row(
                        &format!(
                            "SELECT {EVENT_COLUMNS} FROM issue_events \
                             WHERE issue_id = ?1 AND client_msg_id = ?2"
                        ),
                        rusqlite::params![issue, client_msg_id],
                        read_raw_event,
                    )
                    .optional()?)
            })
            .await?;
        raw.map(idempotent_event_from_raw).transpose()
    }

    async fn mark_comment_consequences_applied(
        &self,
        event: &baybo_model::IssueEventId,
    ) -> Result<bool> {
        let event = event.as_str().to_owned();
        let affected = self
            .pool
            .interact_write(
                "issue_events.mark_comment_consequences_applied",
                move |conn| {
                    Ok(conn.execute(
                        "UPDATE issue_events SET comment_consequences_applied = 1 \
                     WHERE id = ?1 AND client_msg_id IS NOT NULL",
                        rusqlite::params![event],
                    )?)
                },
            )
            .await?;
        Ok(affected > 0)
    }

    async fn list_events(&self, issue: &IssueId) -> Result<Vec<IssueEventRow>> {
        self.events_query(issue.as_str().to_string(), None).await
    }

    async fn first_unread_event(
        &self,
        issue: &IssueId,
    ) -> Result<Option<baybo_model::IssueEventId>> {
        let issue = issue.as_str().to_string();
        let review_status = IssueStatus::Review.as_str();
        let id = self
            .pool
            .interact("issue_events.first_unread", move |conn| {
                Ok(conn
                    .query_row(
                        // `UNREAD_EVENT_PREDICATE` verbatim, and the ordering
                        // `events_query` lists with — "the first unread one"
                        // has to mean the first one the page will draw, not
                        // the first one some other sort would have put there.
                        &format!(
                            "SELECT e.id FROM issue_events e \
                             JOIN issues i ON i.id = e.issue_id \
                             WHERE e.issue_id = :issue AND {UNREAD_EVENT_PREDICATE} \
                             ORDER BY e.created_at, e.id LIMIT 1"
                        ),
                        rusqlite::named_params! {
                            ":issue": issue,
                            ":review": review_status,
                        },
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?)
            })
            .await?;
        Ok(id.map(baybo_model::IssueEventId::from))
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
            .interact_write("issues.set_branch", move |conn| {
                Ok(conn.execute(
                    "UPDATE issues SET branch = ?2, \
                       updated_at = CASE WHEN branch IS NOT ?2 THEN ?3 ELSE updated_at END \
                     WHERE id = ?1",
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

    async fn run_spend(&self, issue: &IssueId) -> Result<Vec<RunSpend>> {
        let issue = issue.as_str().to_string();
        let rows = self
            .pool
            .interact("issue_runs.spend", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT r.id, \
                            COALESCE(SUM(c.input_tokens), 0), \
                            COALESCE(SUM(c.output_tokens), 0), \
                            COALESCE(SUM(c.cost_usd), 0) \
                     FROM issue_runs r \
                     LEFT JOIN cost_records c ON {RUN_COST_WINDOW} \
                     WHERE r.issue_id = ?1 \
                     GROUP BY r.id"
                ))?;
                Ok(stmt
                    .query_map(rusqlite::params![issue], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;
        Ok(rows
            .into_iter()
            .map(|(id, input_tokens, output_tokens, cost)| RunSpend {
                run_id: IssueRunId::from(id),
                spend: Spend {
                    input_tokens,
                    output_tokens,
                    cost: baybo_model::MicroUsd::from_micros(cost),
                },
            })
            .collect())
    }

    async fn settled_run_facts(&self, runs: &[IssueRunId]) -> Result<Vec<SettledRunFacts>> {
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = runs.iter().map(|id| id.as_str().to_string()).collect();
        let rows = self
            .pool
            .interact("issue_runs.settled_facts", move |conn| {
                let holes = std::iter::repeat_n("?", ids.len())
                    .collect::<Vec<_>>()
                    .join(",");
                let mut stmt = conn.prepare(&format!(
                    "SELECT r.id, \
                            (r.settled_at - r.started_at) / 1000, \
                            COALESCE(SUM(c.input_tokens), 0), \
                            COALESCE(SUM(c.output_tokens), 0), \
                            COALESCE(SUM(c.cost_usd), 0) \
                     FROM issue_runs r \
                     LEFT JOIN cost_records c ON {RUN_COST_WINDOW} \
                     WHERE r.id IN ({holes}) \
                     GROUP BY r.id"
                ))?;
                Ok(stmt
                    .query_map(rusqlite::params_from_iter(ids.iter()), |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<i64>>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, duration_ms, input_tokens, output_tokens, cost)| SettledRunFacts {
                    run_id: IssueRunId::from(id),
                    duration_ms,
                    spend: Spend {
                        input_tokens,
                        output_tokens,
                        cost: baybo_model::MicroUsd::from_micros(cost),
                    },
                },
            )
            .collect())
    }

    async fn board_activity(
        &self,
        since: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<(ProjectId, BoardActivity)>> {
        let running = RunStatus::Running.as_str();
        let since = super::time::to_us(since);
        let rows = self
            .pool
            .interact("projects.activity", move |conn| {
                let mut activity: std::collections::HashMap<String, (usize, Spend)> =
                    std::collections::HashMap::new();

                {
                    let mut stmt = conn.prepare(
                        "SELECT project_id, COUNT(*) FROM issue_runs \
                         WHERE status = ?1 AND settled_at IS NULL \
                         GROUP BY project_id",
                    )?;
                    for row in stmt.query_map(rusqlite::params![running], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    })? {
                        let (project, count) = row?;
                        activity.entry(project).or_default().0 += count.max(0) as usize;
                    }
                }

                {
                    // The all-boards spelling of `spend_since`, deliberately
                    // identical in semantics: this number sits next to the
                    // budget in the dropdown, and a burn that measured
                    // something else than the gate does would accuse the
                    // board of overspending a ceiling it never crossed.
                    // Hence the same three membership sources
                    // `BOARD_SESSIONS` unites, spelled here as the
                    // (board, session) pairs this has to GROUP BY. `UNION`
                    // and not `UNION ALL`: a session reachable two ways —
                    // the ordinary case, since a run session also carries
                    // the board on its trigger — must contribute once, and
                    // so must a session shared by several runs.
                    let mut stmt = conn.prepare(&format!(
                        "SELECT r.project_id, {SPEND_SUMS} \
                         FROM cost_records c \
                         JOIN (SELECT project_id, session_id FROM issue_runs \
                                 WHERE session_id IS NOT NULL \
                               UNION \
                               SELECT s.project_id, s.id FROM sessions s \
                                 WHERE s.project_id IS NOT NULL \
                               UNION \
                               SELECT r2.project_id, s.id \
                                 FROM sessions s \
                                 JOIN issue_runs r2 ON r2.session_id = s.root_session_id \
                                 WHERE r2.session_id IS NOT NULL) r \
                           ON r.session_id = c.session_id \
                         WHERE c.timestamp >= ?1 \
                         GROUP BY r.project_id"
                    ))?;
                    for row in stmt.query_map(rusqlite::params![since], |row| {
                        Ok((row.get::<_, String>(0)?, spend_from_row(row, 1)?))
                    })? {
                        let (project, spend) = row?;
                        let entry = activity.entry(project).or_default();
                        entry.1 = entry.1 + spend;
                    }
                }

                Ok(activity.into_iter().collect::<Vec<_>>())
            })
            .await?;
        rows.into_iter()
            .map(|(id, (working, burn))| {
                Ok((
                    ProjectId::parse(id).map_err(|e| {
                        StorageError::Storage(format!("projects.id unreadable: {e}"))
                    })?,
                    BoardActivity { working, burn },
                ))
            })
            .collect()
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

    async fn drain_marks(&self, project: &ProjectId) -> Result<DrainMarks> {
        let project = project.as_str().to_string();
        let failed = RunStatus::Failed.as_str();
        let cancelled = RunStatus::Cancelled.as_str();
        let (looked_at, worked_at) = self
            .pool
            .interact("issue_runs.drain_marks", move |conn| {
                // The one list, expanded twice with opposite senses: what
                // wakes the lead, and what counts as the board working.
                //
                // A run somebody called off lands on the *look* side and
                // never on the work side: it is not the board working, and
                // whoever called it off had the board in front of them to do
                // it. On the work side, as it was, a person pressing stop
                // re-armed the very question that countermands them.
                //
                // `MAX` over a `UNION ALL` rather than the scalar `max()`,
                // which is NULL if either arm is: each arm is a board that
                // has not happened yet, and the aggregate skips those.
                Ok(conn.query_row(
                    concat!(
                        "SELECT \
                         (SELECT MAX(mark) FROM ( \
                            SELECT MAX(created_at) AS mark FROM issue_runs \
                              WHERE project_id = ?1 AND trigger IN (",
                        coordination_triggers!(),
                        ") AND NOT (status = ?2 AND session_id IS NULL) \
                            UNION ALL \
                            SELECT MAX(settled_at) FROM issue_runs \
                              WHERE project_id = ?1 AND status = ?3 \
                                AND settled_at IS NOT NULL)), \
                         (SELECT MAX(settled_at) FROM issue_runs \
                            WHERE project_id = ?1 AND settled_at IS NOT NULL \
                              AND status <> ?3 \
                              AND trigger NOT IN (",
                        coordination_triggers!(),
                        "))"
                    ),
                    rusqlite::params![project, failed, cancelled],
                    |row| Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, Option<i64>>(1)?)),
                )?)
            })
            .await?;
        Ok(DrainMarks {
            looked_at: ts_opt("issue_runs.created_at", looked_at)?,
            worked_at: ts_opt("issue_runs.settled_at", worked_at)?,
        })
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

    async fn claim_run(&self, id: &IssueRunId, session: &SessionId) -> Result<bool> {
        let id = id.as_str().to_string();
        let session = session.as_str().to_string();
        let now = super::time::now_us();
        let affected = self
            .pool
            .interact_write("issue_runs.claim", move |conn| {
                // Scoped to `queued`, so two dispatches of the same row
                // resolve into one execution rather than two — the
                // execution, not the work each dispatcher did to get here.
                // `COALESCE`, so a run re-claimed after a restart keeps the
                // instant it first started: that edge is what its spend is
                // attributed by, and moving it forward would drop every call
                // the run made before the process went down.
                Ok(conn.execute(
                    "UPDATE issue_runs \
                     SET status = 'running', session_id = ?2, \
                         started_at = COALESCE(started_at, ?3) \
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
            .interact_write("issue_runs.settle", move |conn| {
                Ok(conn.execute(
                    "UPDATE issue_runs SET status = ?2, error = ?3, settled_at = ?4 \
                     WHERE id = ?1 AND settled_at IS NULL",
                    rusqlite::params![id, status, error, now],
                )?)
            })
            .await?;
        Ok(affected > 0)
    }

    async fn hold_run(&self, id: &IssueRunId) -> Result<Option<IssueRunRow>> {
        let id = id.as_str().to_string();
        let raw = self
            .pool
            .interact_write("issue_runs.hold", move |conn| {
                // Write and read back in one statement, as `requeue_unsettled`
                // does: the caller is handed the row this wrote rather than
                // its own pre-write copy plus a guess at what changed.
                let mut stmt = conn.prepare(&format!(
                    "UPDATE issue_runs SET status = 'held' \
                     WHERE id = ?1 AND status = 'queued' AND settled_at IS NULL \
                     RETURNING {RUN_COLUMNS}"
                ))?;
                Ok(stmt
                    .query_map(rusqlite::params![id], read_raw_run)?
                    .next()
                    .transpose()?)
            })
            .await?;
        raw.map(run_from_raw).transpose()
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

    async fn release_run(&self, id: &IssueRunId) -> Result<Option<IssueRunRow>> {
        let id = id.as_str().to_string();
        let raw = self
            .pool
            .interact_write("issue_runs.release", move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "UPDATE issue_runs SET status = 'queued' \
                     WHERE id = ?1 AND status = 'held' \
                     RETURNING {RUN_COLUMNS}"
                ))?;
                Ok(stmt
                    .query_map(rusqlite::params![id], read_raw_run)?
                    .next()
                    .transpose()?)
            })
            .await?;
        raw.map(run_from_raw).transpose()
    }

    async fn requeue_unsettled(&self) -> Result<Vec<IssueRunRow>> {
        let raw = self
            .pool
            .interact_write("issue_runs.requeue", move |conn| {
                // Preserve first claim so pre-restart spend stays in the run window.
                // Update and return atomically so narrated resume counts match.
                let mut stmt = conn.prepare(&format!(
                    "UPDATE issue_runs SET status = 'queued', resumes = resumes + 1 \
                     WHERE settled_at IS NULL AND status = 'running' \
                     RETURNING {RUN_COLUMNS}"
                ))?;
                let rows = stmt
                    .query_map([], read_raw_run)?
                    .collect::<rusqlite::Result<Vec<RawRun>>>()?;
                Ok(rows)
            })
            .await?;
        raw.into_iter().map(run_from_raw).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_store::project::DEFAULT_MAX_PARALLEL_ISSUE_RUNS;

    /// Pins `newest_run!`'s literal trigger list to the enum's own idea of
    /// coordination, so a variant added to one cannot silently miss the
    /// other.
    #[test]
    fn coordination_triggers_match_the_enum() {
        let listed = [
            "triage",
            "review",
            "stalled",
            "blocked",
            "grooming",
            "board_idle",
        ];
        let probe = newest_run!("status");
        for name in listed {
            assert!(
                probe.contains(&format!("'{name}'")),
                "the macro's NOT IN list must carry {name}"
            );
            let trigger = RunTrigger::parse(name).expect("a listed trigger parses");
            assert!(trigger.is_coordination(), "{name} claims coordination");
        }
        let coordination_count = [
            RunTrigger::Started,
            RunTrigger::Assigned,
            RunTrigger::Retry,
            RunTrigger::Comment,
            RunTrigger::Promoted,
            RunTrigger::Triage,
            RunTrigger::StageBarrier,
            RunTrigger::Review,
            RunTrigger::Stalled,
            RunTrigger::Blocked,
            RunTrigger::Grooming,
            RunTrigger::BoardIdle,
        ]
        .into_iter()
        .filter(|t| t.is_coordination())
        .count();
        assert_eq!(
            coordination_count,
            listed.len(),
            "a coordination trigger exists that the SQL list does not name"
        );
    }

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
            daily_budget_tokens: None,
            max_parallel_issue_runs: DEFAULT_MAX_PARALLEL_ISSUE_RUNS,
            rules_changed_at: now,
            archived_at: None,
            created_at: now,
            updated_at: now,
            agents_may_merge: false,
        }
    }

    #[tokio::test]
    async fn a_row_written_before_the_token_ceiling_existed_has_no_ceiling() {
        let (_dir, store) = store().await;
        let row = project("01JLEGACY", "grandfathered");
        store.create_project(&row).await.unwrap();

        store
            .pool
            .interact_write("test.blank_the_column", |conn| {
                conn.execute("UPDATE projects SET daily_budget_tokens = NULL", [])?;
                Ok(())
            })
            .await
            .unwrap();

        let read = store.get_project(&row.id).await.unwrap().expect("row");
        assert_eq!(
            read.daily_budget_tokens, None,
            "an absent ceiling is no ceiling; Some(0) would pause the board"
        );
        assert_eq!(
            read.max_parallel_issue_runs, DEFAULT_MAX_PARALLEL_ISSUE_RUNS,
            "and the column beside it still resolves its own NULL, for its own reason"
        );
    }

    /// A card on a fresh board, with every column carrying a value so that
    /// "write it back over itself" has something to write.
    async fn card(store: &SqliteProjectStore, project: &ProjectId) -> IssueRow {
        let now = chrono::Utc::now();
        store
            .create_issue(&NewIssue {
                id: IssueId::generate(),
                project_id: project.clone(),
                title: "a step".to_owned(),
                description: "what it is for".to_owned(),
                attachments: Vec::new(),
                status: IssueStatus::Todo,
                priority: IssuePriority::High,
                assignee: Some(AgentProfileId::parse("dev-1".to_owned()).unwrap()),
                parent_issue_id: None,
                stage: 0,
                source_key: None,
                filed_from: None,
                created_at: now,
            })
            .await
            .unwrap()
    }

    /// The patch that sets every field to what the card already holds —
    /// the shape a model answering a strict tool schema sends, and the one
    /// that used to reopen every question the lead had answered.
    fn same_card(row: &IssueRow) -> IssueUpdate {
        IssueUpdate {
            title: Some(row.title.clone()),
            description: Some(row.description.clone()),
            attachments: Some(row.attachments.clone()),
            priority: Some(row.priority),
            parent: Some(row.parent_issue_id.clone()),
            stage: Some(row.stage),
            assignee: Some(row.assignee.clone()),
            blocked_reason: Some(row.blocked_reason.clone()),
            cancelled: Some(row.cancelled_at.is_some()),
            pinned: Some(row.pinned),
        }
    }

    async fn reread(store: &SqliteProjectStore, project: &ProjectId, number: i64) -> IssueRow {
        store
            .list_issues(project)
            .await
            .unwrap()
            .into_iter()
            .find(|issue| issue.number == number)
            .expect("the card")
    }

    /// Pins [`AN_ISSUE_FIELD_CHANGED`] to `update_issue`'s own `SET` list:
    /// every guarded column, written back over itself, must leave the clock
    /// where it was — and each of them, actually changed, must move it. A
    /// column added to the `SET` and missed by the predicate fails the
    /// second half.
    #[tokio::test]
    async fn a_write_that_changes_nothing_does_not_move_the_clock() {
        let (_dir, store) = store().await;
        let board = project("01JCLOCK", "board");
        store.create_project(&board).await.unwrap();
        let opened = card(&store, &board.id).await;

        assert!(
            store
                .update_issue(&board.id, opened.number, &same_card(&opened))
                .await
                .unwrap(),
            "the row is still there, which is all the return value claims"
        );
        assert_eq!(
            reread(&store, &board.id, opened.number).await.updated_at,
            opened.updated_at,
            "a patch that changes nothing is not the card changing, and the \
             board reads this clock as \"ask the lead again\""
        );

        let mut moved = opened.clone();
        for (field, patch) in [
            (
                "title",
                IssueUpdate {
                    title: Some("renamed".to_owned()),
                    ..IssueUpdate::default()
                },
            ),
            (
                "description",
                IssueUpdate {
                    description: Some("rewritten".to_owned()),
                    ..IssueUpdate::default()
                },
            ),
            (
                "priority",
                IssueUpdate {
                    priority: Some(IssuePriority::Low),
                    ..IssueUpdate::default()
                },
            ),
            (
                "assignee",
                IssueUpdate {
                    assignee: Some(None),
                    ..IssueUpdate::default()
                },
            ),
            (
                "blocked_reason",
                IssueUpdate {
                    blocked_reason: Some(Some("waiting".to_owned())),
                    ..IssueUpdate::default()
                },
            ),
            (
                "stage",
                IssueUpdate {
                    stage: Some(2),
                    ..IssueUpdate::default()
                },
            ),
            (
                "pinned",
                IssueUpdate {
                    pinned: Some(true),
                    ..IssueUpdate::default()
                },
            ),
            (
                "attachments",
                IssueUpdate {
                    attachments: Some(vec![IssueAttachment {
                        blob_id: "b".to_owned(),
                        mime_type: "text/plain".to_owned(),
                        size: 1,
                        filename: None,
                    }]),
                    ..IssueUpdate::default()
                },
            ),
            (
                "cancelled_at",
                IssueUpdate {
                    cancelled: Some(true),
                    ..IssueUpdate::default()
                },
            ),
        ] {
            store
                .update_issue(&board.id, opened.number, &patch)
                .await
                .unwrap();
            let after = reread(&store, &board.id, opened.number).await;
            assert!(
                after.updated_at > moved.updated_at,
                "{field} changed and the clock did not move: the predicate \
                 has drifted from the SET list beside it"
            );
            moved = after;
        }
    }

    #[tokio::test]
    async fn tidying_a_column_is_not_the_card_changing() {
        let (_dir, store) = store().await;
        let board = project("01JTIDY", "board");
        store.create_project(&board).await.unwrap();
        let opened = card(&store, &board.id).await;

        store
            .move_issue(
                &board.id,
                opened.number,
                IssueStatus::Todo,
                &[opened.number],
            )
            .await
            .unwrap();
        assert_eq!(
            reread(&store, &board.id, opened.number).await.updated_at,
            opened.updated_at,
            "a drag inside the column the card is already in reorders it and \
             changes nothing about it"
        );

        store
            .move_issue(
                &board.id,
                opened.number,
                IssueStatus::InProgress,
                &[opened.number],
            )
            .await
            .unwrap();
        assert!(
            reread(&store, &board.id, opened.number).await.updated_at > opened.updated_at,
            "but a card that actually left its column did change"
        );
    }

    /// The update that changes nothing about the board it is applied to.
    fn same_rules(row: &ProjectRow) -> ProjectUpdate {
        ProjectUpdate {
            name: row.name.clone(),
            description: row.description.clone(),
            daily_budget: row.daily_budget,
            daily_budget_tokens: row.daily_budget_tokens,
            max_parallel_issue_runs: row.max_parallel_issue_runs,
            agents_may_merge: row.agents_may_merge,
        }
    }

    #[tokio::test]
    async fn only_a_rule_the_board_schedules_by_stamps_a_rule_change() {
        let (_dir, store) = store().await;
        let row = project("01JRULES", "board");
        store.create_project(&row).await.unwrap();
        let opened = store.get_project(&row.id).await.unwrap().expect("row");

        store
            .update_project(
                &row.id,
                &ProjectUpdate {
                    name: "renamed".to_owned(),
                    description: "now with prose".to_owned(),
                    ..same_rules(&opened)
                },
            )
            .await
            .unwrap();
        let renamed = store.get_project(&row.id).await.unwrap().expect("row");
        assert_eq!(
            renamed.rules_changed_at, opened.rules_changed_at,
            "renaming a board changes no answer anybody gave on it"
        );

        store
            .update_project(
                &row.id,
                &ProjectUpdate {
                    agents_may_merge: !opened.agents_may_merge,
                    ..same_rules(&renamed)
                },
            )
            .await
            .unwrap();
        let merging = store.get_project(&row.id).await.unwrap().expect("row");
        assert!(
            merging.rules_changed_at > opened.rules_changed_at,
            "but whether the board may land its own work is a rule, and every              standing answer was given while it could not"
        );

        store
            .update_project(&row.id, &same_rules(&merging))
            .await
            .unwrap();
        assert_eq!(
            store
                .get_project(&row.id)
                .await
                .unwrap()
                .expect("row")
                .rules_changed_at,
            merging.rules_changed_at,
            "and a save that re-writes the same values changed no rule"
        );
    }

    #[tokio::test]
    async fn a_board_coming_back_from_the_archive_is_a_rule_change_and_going_is_not() {
        let (_dir, store) = store().await;
        let row = project("01JSHELF", "board");
        store.create_project(&row).await.unwrap();
        let opened = store.get_project(&row.id).await.unwrap().expect("row");

        store.set_project_archived(&row.id, true).await.unwrap();
        let shelved = store.get_project(&row.id).await.unwrap().expect("row");
        assert_eq!(
            shelved.rules_changed_at, opened.rules_changed_at,
            "shelving a board starts nothing, so it re-opens nothing"
        );

        store.set_project_archived(&row.id, false).await.unwrap();
        assert!(
            store
                .get_project(&row.id)
                .await
                .unwrap()
                .expect("row")
                .rules_changed_at
                > opened.rules_changed_at,
            "coming back is the operator turning the board on again"
        );
    }

    #[tokio::test]
    async fn a_row_written_before_rules_changed_at_existed_has_never_changed_a_rule() {
        let (_dir, store) = store().await;
        let row = project("01JOLDRULES", "grandfathered");
        store.create_project(&row).await.unwrap();

        store
            .pool
            .interact_write("test.blank_the_column", |conn| {
                conn.execute("UPDATE projects SET rules_changed_at = NULL", [])?;
                Ok(())
            })
            .await
            .unwrap();

        let read = store.get_project(&row.id).await.unwrap().expect("row");
        assert_eq!(
            read.rules_changed_at, read.created_at,
            "every card on a board is younger than the board, so resolving to              its own opening re-opens nothing"
        );
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
            attachments: Vec::new(),
            status,
            priority: IssuePriority::None,
            assignee: None,
            parent_issue_id: None,
            stage: 0,
            source_key: None,
            filed_from: None,
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
                attachments: Vec::new(),
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
    async fn a_client_message_id_claims_one_timeline_row() {
        let (_dir, store) = store().await;
        let project = project("proj-comment-id", "Comment ids");
        store.create_project(&project).await.unwrap();
        let issue = store
            .create_issue(&new_issue(&project.id, "Do it once", IssueStatus::Backlog))
            .await
            .unwrap();
        let client_msg_id =
            IssueEventClientMsgId::parse("01944c32-cc5e-7f5c-9f1c-efaa2a5488a2").unwrap();
        let new = event(
            &issue,
            IssueActor::User,
            IssueEventBody::Comment {
                text: "one request, twice".to_owned(),
                attachments: Vec::new(),
            },
        );

        let inserted = store
            .append_event_idempotent(&new, &client_msg_id)
            .await
            .unwrap();
        let existing = store
            .append_event_idempotent(&new, &client_msg_id)
            .await
            .unwrap();

        let first = match inserted {
            IssueEventAppendOutcome::Inserted(row) => row,
            IssueEventAppendOutcome::Existing(_) => panic!("the first claim was not inserted"),
        };
        let second = match existing {
            IssueEventAppendOutcome::Existing(row) => row,
            IssueEventAppendOutcome::Inserted(_) => panic!("the retry inserted a second row"),
        };
        assert_eq!(first.id, second.event.id);
        assert_eq!(second.event.client_msg_id.as_ref(), Some(&client_msg_id));
        assert!(!second.consequences_applied);
        assert!(
            store
                .mark_comment_consequences_applied(&first.id)
                .await
                .unwrap()
        );
        assert_eq!(store.list_events(&issue.id).await.unwrap().len(), 1);
        let completed = store
            .event_by_client_msg_id(&issue.id, &client_msg_id)
            .await
            .unwrap()
            .expect("comment");
        assert_eq!(completed.event.id, first.id);
        assert!(completed.consequences_applied);
    }

    #[tokio::test]
    async fn who_opened_a_card_is_read_off_the_timeline_and_stays_on_its_own_board() {
        let (_dir, store) = store().await;
        let mine = project("proj-o", "O");
        let theirs = project("proj-x", "X");
        for row in [&mine, &theirs] {
            store.create_project(row).await.unwrap();
        }
        let agent = IssueActor::Agent(AgentProfileId::parse("dev-1".to_owned()).unwrap());
        async fn opened(
            store: &SqliteProjectStore,
            project: &ProjectRow,
            title: &str,
            by: IssueActor,
        ) -> IssueRow {
            let issue = store
                .create_issue(&new_issue(&project.id, title, IssueStatus::Backlog))
                .await
                .unwrap();
            store
                .append_event(&event(&issue, by, IssueEventBody::Opened))
                .await
                .unwrap();
            issue
        }

        let ours = opened(&store, &mine, "spun out", agent.clone()).await;
        opened(&store, &mine, "someday", IssueActor::User).await;
        // A card with no `Opened` row at all: older than the entry, and
        // deliberately not counted — an unknown author is not an agent.
        store
            .create_issue(&new_issue(
                &mine.id,
                "predates the entry",
                IssueStatus::Backlog,
            ))
            .await
            .unwrap();
        let elsewhere = opened(&store, &theirs, "another board's", agent).await;

        assert_eq!(
            store.agent_opened_issues(&mine.id).await.unwrap(),
            vec![ours.number],
            "only the cards an agent opened, and only on the board asked about"
        );
        assert_eq!(
            store.agent_opened_issues(&theirs.id).await.unwrap(),
            vec![elsewhere.number]
        );
    }

    #[tokio::test]
    async fn a_typed_body_survives_the_round_trip_whole() {
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
                    attachments: Vec::new(),
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
                    attachments: Vec::new(),
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
                        daily_budget_tokens: None,
                        max_parallel_issue_runs: DEFAULT_MAX_PARALLEL_ISSUE_RUNS,
                        agents_may_merge: false,
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
        let shelved_at = store
            .get_project(&a.id)
            .await
            .unwrap()
            .unwrap()
            .archived_at
            .unwrap();
        assert!(
            !store.set_project_archived(&a.id, true).await.unwrap(),
            "archiving a board that is already away moves nothing"
        );
        assert_eq!(
            store
                .get_project(&a.id)
                .await
                .unwrap()
                .unwrap()
                .archived_at
                .unwrap(),
            shelved_at,
            "…and does not restamp it either"
        );
        assert!(
            store.set_project_archived(&a.id, false).await.unwrap(),
            "the way back is an edge too"
        );
        assert!(
            !store.set_project_archived(&a.id, false).await.unwrap(),
            "and restoring a board that never left moves nothing"
        );
        assert!(store.set_project_archived(&a.id, true).await.unwrap());
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

    /// A write that could not run at all is not a card that already had a
    /// run.
    ///
    /// Every `tx.execute` failure used to come out of the closure as a bare
    /// string and get labelled `Conflict("issue already has a run in
    /// flight")` — so a busy database, a full disk or a broken statement
    /// all told the operator something that had not happened. It matters
    /// more than a wrong log line: `enqueue_as` records a `RunRefused`
    /// entry on that arm, so a transient failure would be written into the
    /// card's history, permanently, as a dedupe refusal.
    #[tokio::test]
    async fn only_a_row_that_was_already_there_is_reported_as_a_conflict() {
        let (_dir, store) = store().await;
        let p = project("proj-a", "A");
        store.create_project(&p).await.unwrap();
        let busy = store
            .create_issue(&new_issue(&p.id, "one", IssueStatus::Backlog))
            .await
            .unwrap();
        let idle = store
            .create_issue(&new_issue(&p.id, "two", IssueStatus::Backlog))
            .await
            .unwrap();

        store.enqueue_run(&new_run(&busy)).await.unwrap();
        let refused = store
            .enqueue_run(&new_run(&busy))
            .await
            .expect_err("the card's slot is taken");
        assert!(
            matches!(refused, StorageError::Conflict(_)),
            "a uniqueness refusal is a conflict: {refused:?}"
        );

        // Break the INSERT without breaking the statements around it: the
        // attempt lookup still reads, and only the write cannot run. A
        // missing column is `SQLITE_ERROR`, not a constraint of any kind.
        store
            .pool
            .interact_write("test.break_insert", |conn| {
                Ok(conn.execute_batch("ALTER TABLE issue_runs DROP COLUMN error;")?)
            })
            .await
            .unwrap();

        let broken = store
            .enqueue_run(&new_run(&idle))
            .await
            .expect_err("the insert cannot run");
        assert!(
            !matches!(broken, StorageError::Conflict(_)),
            "a statement that could not run is not a slot that was taken: {broken:?}"
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

        let refused = store
            .enqueue_run(&new_run(&issue))
            .await
            .expect_err("an issue holds one run at a time");
        assert!(matches!(refused, StorageError::Conflict(_)), "{refused:?}");

        let session = SessionId::from("issue-1");
        assert!(store.claim_run(&run.id, &session).await.unwrap());
        assert!(
            !store.claim_run(&run.id, &session).await.unwrap(),
            "a claimed run cannot be claimed again — that is how two dispatches of one row \
             collapse into one execution"
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

        let second = store.enqueue_run(&new_run(&issue)).await.unwrap();
        assert_eq!(second.attempt, 2);
    }

    #[tokio::test]
    async fn a_run_somebody_called_off_is_a_look_at_the_board_and_not_work_on_it() {
        let (_dir, store) = store().await;
        let p = project("proj-marks", "Marks");
        store.create_project(&p).await.unwrap();
        let issue = store
            .create_issue(&new_issue(&p.id, "work", IssueStatus::InProgress))
            .await
            .unwrap();

        assert_eq!(
            store.drain_marks(&p.id).await.unwrap(),
            DrainMarks::default(),
            "a board nothing has ever run on carries neither mark"
        );

        let run = store.enqueue_run(&new_run(&issue)).await.unwrap();
        store
            .claim_run(&run.id, &SessionId::from("issue-1"))
            .await
            .unwrap();
        store
            .settle_run(&run.id, RunStatus::Cancelled, None)
            .await
            .unwrap();

        let marks = store.drain_marks(&p.id).await.unwrap();
        assert!(
            marks.worked_at.is_none(),
            "a run somebody stopped is not the board working"
        );
        assert!(
            marks.looked_at.is_some(),
            "it is somebody having read the board and decided, which is the \
             one thing that keeps the drain question from countermanding them"
        );

        let second = store.enqueue_run(&new_run(&issue)).await.unwrap();
        store
            .settle_run(&second.id, RunStatus::Done, None)
            .await
            .unwrap();
        let marks = store.drain_marks(&p.id).await.unwrap();
        assert!(
            marks
                .worked_at
                .is_some_and(|worked| marks.looked_at.is_some_and(|looked| worked > looked)),
            "and work that finished after the stop is the board working again"
        );
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

        store.requeue_unsettled().await.unwrap();

        let swept = store.active_runs(&p.id).await.unwrap();
        let ids: Vec<&str> = swept.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![queued.id.as_str(), running.id.as_str()],
            "both unfinished runs are still owed, the settled one is not"
        );
        assert!(swept.iter().all(|r| r.status == RunStatus::Queued));
        let session = SessionId::from("issue-2");
        let orphan = swept.iter().find(|r| r.id == running.id).unwrap();
        assert_eq!(
            orphan.session_id,
            Some(session.clone()),
            "the interrupted run keeps the session it was working in, so the \
             resumed run continues that transcript instead of opening a second"
        );
        let first_claim = orphan
            .started_at
            .expect("the interrupted run keeps the instant it first started");
        assert!(
            swept
                .iter()
                .find(|r| r.id == queued.id)
                .unwrap()
                .session_id
                .is_none(),
            "a run that was never claimed still has no session"
        );

        store.requeue_unsettled().await.unwrap();
        assert_eq!(store.active_runs(&p.id).await.unwrap().len(), 2);

        assert!(store.claim_run(&running.id, &session).await.unwrap());
        // The re-claim must not move that edge: everything the run spent
        // before the restart is attributed by it, and a fresh stamp would
        // orphan all of it while the board's daily burn kept counting it.
        let resumed = store
            .active_runs(&p.id)
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.id == running.id)
            .unwrap();
        assert_eq!(
            resumed.started_at,
            Some(first_claim),
            "a resumed run keeps its original claim instant"
        );
    }

    #[tokio::test]
    async fn a_requeue_bumps_the_resume_count_and_answers_with_the_rows_it_rolled_back() {
        let (_dir, store) = store().await;
        let p = project("proj-a", "A");
        store.create_project(&p).await.unwrap();
        let worked = store
            .create_issue(&new_issue(&p.id, "worked", IssueStatus::Backlog))
            .await
            .unwrap();
        let waiting = store
            .create_issue(&new_issue(&p.id, "waiting", IssueStatus::Backlog))
            .await
            .unwrap();

        let running = store.enqueue_run(&new_run(&worked)).await.unwrap();
        assert_eq!(running.resumes, 0, "a fresh row has never been interrupted");
        let session = SessionId::from("issue-1");
        store.claim_run(&running.id, &session).await.unwrap();
        let never_claimed = store.enqueue_run(&new_run(&waiting)).await.unwrap();

        let swept = store.requeue_unsettled().await.unwrap();
        assert_eq!(
            swept.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            vec![running.id.as_str()],
            "only the row that was in flight was rolled back"
        );
        let rolled = &swept[0];
        assert_eq!(rolled.status, RunStatus::Queued);
        assert_eq!(rolled.resumes, 1, "the meter counts this interruption");
        assert_eq!(rolled.session_id, Some(session.clone()));
        assert!(rolled.started_at.is_some());
        assert_eq!(
            store
                .get_run(&never_claimed.id)
                .await
                .unwrap()
                .unwrap()
                .resumes,
            0,
            "a row that was already queued was not interrupted, so its counter does not move"
        );

        store.claim_run(&running.id, &session).await.unwrap();
        let again = store.requeue_unsettled().await.unwrap();
        assert_eq!(again[0].resumes, 2, "each process start counts once");
    }

    #[tokio::test]
    async fn a_run_recorded_before_the_resume_meter_existed_migrates_in_never_interrupted() {
        let (_dir, store) = store().await;
        let p = project("proj-a", "A");
        store.create_project(&p).await.unwrap();
        let issue = store
            .create_issue(&new_issue(&p.id, "legacy", IssueStatus::Backlog))
            .await
            .unwrap();
        let run = store.enqueue_run(&new_run(&issue)).await.unwrap();

        store
            .pool
            .interact_write("test.rewind_the_resume_migration", move |conn| {
                conn.execute("ALTER TABLE issue_runs DROP COLUMN resumes", [])?;
                let migration = super::super::ADD_COLUMNS
                    .iter()
                    .find(|m| m.table == "issue_runs" && m.column == "resumes")
                    .expect("the resume meter is a listed migration");
                migration.apply(conn)?;
                migration.apply(conn)?;
                Ok(())
            })
            .await
            .unwrap();

        assert_eq!(
            store.get_run(&run.id).await.unwrap().unwrap().resumes,
            0,
            "a row that predates the column has never been handed back out"
        );
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

    #[tokio::test]
    async fn a_pin_goes_on_and_off_and_survives_every_other_edit() {
        let (_dir, store) = store().await;
        let p = project("proj-a", "A");
        store.create_project(&p).await.unwrap();
        let opened = store
            .create_issue(&new_issue(&p.id, "watch this one", IssueStatus::Todo))
            .await
            .unwrap();
        assert!(!opened.pinned, "a card is opened unpinned");

        store
            .update_issue(
                &p.id,
                1,
                &IssueUpdate {
                    pinned: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(store.get_issue(&p.id, 1).await.unwrap().unwrap().pinned);
        assert!(
            store.list_issues(&p.id).await.unwrap()[0].pinned,
            "the board read carries it too, not only the point lookup"
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
        assert!(
            store.get_issue(&p.id, 1).await.unwrap().unwrap().pinned,
            "an unrelated patch did not unpin it"
        );

        store
            .update_issue(
                &p.id,
                1,
                &IssueUpdate {
                    pinned: Some(false),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(!store.get_issue(&p.id, 1).await.unwrap().unwrap().pinned);
    }

    /// A pin is a reading order, and `move_issue` is the one write that
    /// renumbers a whole column. If the pin ever leaked into that scan a
    /// card would keep the top of its column after being unpinned.
    #[tokio::test]
    async fn a_move_neither_reads_the_pin_nor_clears_it() {
        let (_dir, store) = store().await;
        let p = project("proj-a", "A");
        store.create_project(&p).await.unwrap();
        for title in ["first", "second"] {
            store
                .create_issue(&new_issue(&p.id, title, IssueStatus::Todo))
                .await
                .unwrap();
        }
        store
            .update_issue(
                &p.id,
                2,
                &IssueUpdate {
                    pinned: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        store
            .move_issue(&p.id, 2, IssueStatus::Review, &[2])
            .await
            .unwrap();

        let moved = store.get_issue(&p.id, 2).await.unwrap().unwrap();
        assert!(moved.pinned, "the card kept its pin across the column");
        assert_eq!(
            moved.position, 0,
            "and its rank came from the order it was moved in, not from the pin"
        );
        assert_eq!(
            store.get_issue(&p.id, 1).await.unwrap().unwrap().position,
            0,
            "the column it left closed its gap as usual"
        );
    }

    #[tokio::test]
    async fn a_card_opened_before_the_pin_existed_migrates_in_unpinned() {
        let (_dir, store) = store().await;
        let p = project("proj-a", "A");
        store.create_project(&p).await.unwrap();
        store
            .create_issue(&new_issue(&p.id, "legacy", IssueStatus::Backlog))
            .await
            .unwrap();

        store
            .pool
            .interact_write("test.rewind_the_pin_migration", move |conn| {
                conn.execute("ALTER TABLE issues DROP COLUMN pinned", [])?;
                let migration = super::super::ADD_COLUMNS
                    .iter()
                    .find(|m| m.table == "issues" && m.column == "pinned")
                    .expect("the pin is a listed migration");
                migration.apply(conn)?;
                migration.apply(conn)?;
                Ok(())
            })
            .await
            .unwrap();

        assert!(
            !store.get_issue(&p.id, 1).await.unwrap().unwrap().pinned,
            "a row that predates the column was never pinned"
        );
        store
            .update_issue(
                &p.id,
                1,
                &IssueUpdate {
                    pinned: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(store.get_issue(&p.id, 1).await.unwrap().unwrap().pinned);
    }

    #[tokio::test]
    async fn a_card_opened_before_the_origin_existed_migrates_in_rootless() {
        let (_dir, store) = store().await;
        let p = project("proj-a", "A");
        store.create_project(&p).await.unwrap();
        let origin = store
            .create_issue(&new_issue(&p.id, "origin", IssueStatus::Backlog))
            .await
            .unwrap();
        store
            .create_issue(&NewIssue {
                filed_from: Some(origin.id.clone()),
                ..new_issue(&p.id, "filed out of it", IssueStatus::Backlog)
            })
            .await
            .unwrap();

        assert_eq!(
            store.get_issue(&p.id, 2).await.unwrap().unwrap().filed_from,
            Some(origin.id.clone()),
            "the edge survives the round trip"
        );

        store
            .pool
            .interact_write("test.rewind_the_origin_migration", move |conn| {
                conn.execute("ALTER TABLE issues DROP COLUMN filed_from_issue_id", [])?;
                let migration = super::super::ADD_COLUMNS
                    .iter()
                    .find(|m| m.table == "issues" && m.column == "filed_from_issue_id")
                    .expect("the origin is a listed migration");
                migration.apply(conn)?;
                migration.apply(conn)?;
                Ok(())
            })
            .await
            .unwrap();

        for number in [1, 2] {
            assert_eq!(
                store
                    .get_issue(&p.id, number)
                    .await
                    .unwrap()
                    .unwrap()
                    .filed_from,
                None,
                "a row that predates the column came out of nothing"
            );
        }
    }

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
            attachments: Vec::new(),
            status: IssueStatus::Backlog,
            priority: IssuePriority::None,
            assignee: Some(AgentProfileId::parse("dev-1").unwrap()),
            parent_issue_id: None,
            stage: 0,
            source_key: None,
            filed_from: None,
            created_at: chrono::Utc::now(),
        };
        let ours = store.create_issue(&issue(&mine, "ours")).await.unwrap();
        let other = store.create_issue(&issue(&theirs, "theirs")).await.unwrap();

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
        let spend = |session: &SessionId,
                     micros: i64,
                     input: usize,
                     output: usize,
                     cached: usize,
                     at: chrono::DateTime<chrono::Utc>| {
            baybo_model::CostRecord {
                user_id: "u".into(),
                session_id: session.clone(),
                turn_id: baybo_model::TurnId::new(),
                span_id: baybo_model::SpanId::new(),
                reason: baybo_model::CallReason::default(),
                model: "m".into(),
                reasoning_effort: None,
                input_tokens: input,
                output_tokens: output,
                cached_input_tokens: cached,
                cache_creation_input_tokens: 0,
                cost_usd: baybo_model::MicroUsd::from_micros(micros),
                timestamp: at,
            }
        };
        let yesterday = now - chrono::Duration::days(1);
        for record in [
            spend(&shared, 300, 40, 10, 0, now),
            // Cached tokens are already included in `input_tokens`.
            spend(&shared, 200, 100, 20, 90, now),
            spend(&shared, 9_000, 5_000, 5_000, 0, yesterday),
            spend(&their_session, 7_000, 70, 30, 0, now),
        ] {
            baybo_store::cost::CostStore::record(&costs, &record)
                .await
                .unwrap();
        }

        let since = now - chrono::Duration::hours(1);
        let mine_today = store.spend_since(&mine.id, since).await.unwrap();
        assert_eq!(
            mine_today,
            Spend {
                input_tokens: 140,
                output_tokens: 30,
                cost: baybo_model::MicroUsd::from_micros(500),
            },
            "two calls on one shared session, counted once each"
        );
        assert_eq!(
            mine_today.tokens(),
            170,
            "the cached prefix is inside `input_tokens` already; counting it again \
             would hold a board against a ceiling it never reached"
        );
        assert_eq!(
            store.spend_since(&theirs.id, since).await.unwrap(),
            Spend {
                input_tokens: 70,
                output_tokens: 30,
                cost: baybo_model::MicroUsd::from_micros(7_000),
            }
        );
    }

    #[tokio::test]
    async fn run_spend_bills_each_run_of_a_shared_session_only_its_own_window() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(dir.path().join("test.db")).await.unwrap();
        let store = SqliteProjectStore::new(pool.clone());
        let costs = crate::sqlite::cost::SqliteCostStore::new(pool);

        let p = project("01JSHARED", "shared");
        store.create_project(&p).await.unwrap();
        let issue = store
            .create_issue(&new_issue(
                &p.id,
                "two runs, one session",
                IssueStatus::Todo,
            ))
            .await
            .unwrap();

        // One session, because an issue keeps one session per agent that
        // works it — so the run windows are the only thing telling the two
        // runs' calls apart.
        let session = SessionId::from("sess-one".to_owned());

        let first = store.enqueue_run(&new_run(&issue)).await.unwrap();
        store.claim_run(&first.id, &session).await.unwrap();
        let first_claimed = store.get_run(&first.id).await.unwrap().unwrap();
        let spend = |micros: i64, tokens: usize, at: chrono::DateTime<chrono::Utc>| {
            baybo_model::CostRecord {
                user_id: "u".into(),
                session_id: session.clone(),
                turn_id: baybo_model::TurnId::new(),
                span_id: baybo_model::SpanId::new(),
                reason: baybo_model::CallReason::default(),
                model: "m".into(),
                reasoning_effort: None,
                input_tokens: tokens,
                output_tokens: tokens,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                cost_usd: baybo_model::MicroUsd::from_micros(micros),
                timestamp: at,
            }
        };
        let started = first_claimed.started_at.expect("claimed run has a start");
        baybo_store::cost::CostStore::record(&costs, &spend(100, 5, started))
            .await
            .unwrap();
        store
            .settle_run(&first.id, RunStatus::Done, None)
            .await
            .unwrap();

        let second = store.enqueue_run(&new_run(&issue)).await.unwrap();
        store.claim_run(&second.id, &session).await.unwrap();
        let second_claimed = store.get_run(&second.id).await.unwrap().unwrap();
        let later = second_claimed.started_at.expect("claimed run has a start");
        baybo_store::cost::CostStore::record(&costs, &spend(700, 9, later))
            .await
            .unwrap();

        let by_run: std::collections::HashMap<_, _> = store
            .run_spend(&issue.id)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.run_id, row.spend))
            .collect();
        assert_eq!(
            by_run[&first.id].cost,
            baybo_model::MicroUsd::from_micros(100),
            "the settled run keeps only what it spent before it settled"
        );
        assert_eq!(by_run[&first.id].input_tokens, 5);
        assert_eq!(
            by_run[&second.id].cost,
            baybo_model::MicroUsd::from_micros(700),
            "the live run does not inherit its predecessor's calls"
        );
        assert_eq!(by_run[&second.id].output_tokens, 9);

        // The feed asks the same question by run rather than by card, and
        // must get the same answer — two derivations of one number is how
        // the feed and the execution log start disagreeing about what a
        // card cost.
        let facts: std::collections::HashMap<_, _> = store
            .settled_run_facts(&[first.id.clone(), second.id.clone()])
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.run_id.clone(), row))
            .collect();
        assert_eq!(facts[&first.id].spend, by_run[&first.id]);
        assert_eq!(facts[&second.id].spend, by_run[&second.id]);

        // Milliseconds. The column is microseconds, so the divide is the
        // whole test: without it every duration reads a thousand times long.
        let settled = store.get_run(&first.id).await.unwrap().unwrap();
        let expected = (settled.settled_at.expect("settled").timestamp_micros()
            - settled.started_at.expect("started").timestamp_micros())
            / 1000;
        assert_eq!(facts[&first.id].duration_ms, Some(expected));
        assert!(
            facts[&second.id].duration_ms.is_none(),
            "a run still in flight has not taken any length of time yet"
        );
    }

    #[tokio::test]
    async fn settled_run_facts_reads_nothing_for_a_run_nobody_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(dir.path().join("test.db")).await.unwrap();
        let store = SqliteProjectStore::new(pool.clone());

        let p = project("01JUNCLAIMED", "unclaimed");
        store.create_project(&p).await.unwrap();
        let issue = store
            .create_issue(&new_issue(&p.id, "never ran", IssueStatus::Todo))
            .await
            .unwrap();
        let run = store.enqueue_run(&new_run(&issue)).await.unwrap();
        store
            .settle_run(&run.id, RunStatus::Cancelled, Some("called off"))
            .await
            .unwrap();

        let facts = store
            .settled_run_facts(std::slice::from_ref(&run.id))
            .await
            .unwrap();
        // No window, so no duration — `None`, not `0`, which would render as
        // a run that finished instantly.
        assert_eq!(facts[0].duration_ms, None);
        assert_eq!(facts[0].spend.cost, baybo_model::MicroUsd::from_micros(0));

        assert!(
            store.settled_run_facts(&[]).await.unwrap().is_empty(),
            "an empty page asks nothing"
        );
    }

    #[tokio::test]
    async fn board_activity_burn_agrees_with_the_budget_gate() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(dir.path().join("test.db")).await.unwrap();
        let store = SqliteProjectStore::new(pool.clone());
        let costs = crate::sqlite::cost::SqliteCostStore::new(pool);

        let p = project("01JBURN", "burny");
        store.create_project(&p).await.unwrap();
        let issue = store
            .create_issue(&new_issue(&p.id, "spends", IssueStatus::Todo))
            .await
            .unwrap();

        // Two runs on one session — the shape that makes a naive join
        // double-count, and the reason `spend_since` uses a set membership
        // test rather than a join.
        let session = SessionId::from("sess-burn".to_owned());
        for _ in 0..2 {
            let run = store.enqueue_run(&new_run(&issue)).await.unwrap();
            store.claim_run(&run.id, &session).await.unwrap();
            store
                .settle_run(&run.id, RunStatus::Done, None)
                .await
                .unwrap();
        }

        let now = chrono::Utc::now();
        baybo_store::cost::CostStore::record(
            &costs,
            &baybo_model::CostRecord {
                user_id: "u".into(),
                session_id: session.clone(),
                turn_id: baybo_model::TurnId::new(),
                span_id: baybo_model::SpanId::new(),
                reason: baybo_model::CallReason::default(),
                model: "m".into(),
                reasoning_effort: None,
                // Distinct counts catch swapped aggregate columns.
                input_tokens: 70,
                output_tokens: 30,
                cached_input_tokens: 40,
                cache_creation_input_tokens: 0,
                cost_usd: baybo_model::MicroUsd::from_micros(1_500),
                timestamp: now,
            },
        )
        .await
        .unwrap();

        let since = now - chrono::Duration::hours(1);
        let gate = store.spend_since(&p.id, since).await.unwrap();
        let activity: std::collections::HashMap<_, _> = store
            .board_activity(since)
            .await
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(
            activity[&p.id].burn, gate,
            "the dropdown's burn and the budget gate's spend are the same question"
        );
        assert_eq!(
            gate.cost,
            baybo_model::MicroUsd::from_micros(1_500),
            "one call on a session two runs shared is billed once, not twice"
        );
        assert_eq!(
            gate,
            Spend {
                input_tokens: 70,
                output_tokens: 30,
                cost: baybo_model::MicroUsd::from_micros(1_500),
            },
            "and the cached 40 are inside the 70, not beside them"
        );
        assert_eq!(activity[&p.id].working, 0, "both runs settled");
    }

    #[tokio::test]
    async fn board_activity_counts_runs_not_the_in_progress_column() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(dir.path().join("test.db")).await.unwrap();
        let store = SqliteProjectStore::new(pool);

        let p = project("01JWORKING", "working");
        store.create_project(&p).await.unwrap();
        // Deliberately not in In Progress: dragging a card out never kills
        // its run, so the column and the count are allowed to disagree and
        // the run is what the dropdown reports.
        let issue = store
            .create_issue(&new_issue(&p.id, "dragged out", IssueStatus::Todo))
            .await
            .unwrap();
        let run = store.enqueue_run(&new_run(&issue)).await.unwrap();
        store
            .claim_run(&run.id, &SessionId::from("sess-live".to_owned()))
            .await
            .unwrap();

        let activity: std::collections::HashMap<_, _> = store
            .board_activity(chrono::Utc::now() - chrono::Duration::hours(1))
            .await
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(activity[&p.id].working, 1);
    }

    #[tokio::test]
    async fn run_spend_reads_zero_for_a_run_nobody_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(dir.path().join("test.db")).await.unwrap();
        let store = SqliteProjectStore::new(pool.clone());
        let costs = crate::sqlite::cost::SqliteCostStore::new(pool);

        let p = project("01JUNCLAIMED", "unclaimed");
        store.create_project(&p).await.unwrap();
        let issue = store
            .create_issue(&new_issue(&p.id, "never started", IssueStatus::Todo))
            .await
            .unwrap();
        let queued = store.enqueue_run(&new_run(&issue)).await.unwrap();

        // A record on a session the run never got: an unclaimed run has no
        // window, so it must not inherit the board's history.
        baybo_store::cost::CostStore::record(
            &costs,
            &baybo_model::CostRecord {
                user_id: "u".into(),
                session_id: SessionId::from("sess-elsewhere".to_owned()),
                turn_id: baybo_model::TurnId::new(),
                span_id: baybo_model::SpanId::new(),
                reason: baybo_model::CallReason::default(),
                model: "m".into(),
                reasoning_effort: None,
                input_tokens: 3,
                output_tokens: 3,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                cost_usd: baybo_model::MicroUsd::from_micros(4_000),
                timestamp: chrono::Utc::now(),
            },
        )
        .await
        .unwrap();

        let rows = store.run_spend(&issue.id).await.unwrap();
        assert_eq!(rows.len(), 1, "the queued run is still listed");
        assert_eq!(rows[0].run_id, queued.id);
        assert_eq!(rows[0].spend, baybo_store::project::Spend::default());
    }

    /// The bare `sessions` row the spend readers join against. Raw SQL and
    /// not a `Session`: the only columns under test are the id, the root
    /// that ties a subagent to the run that spawned it, and the trigger the
    /// `project_id` column is generated from.
    async fn session_row(pool: &SqlitePool, id: &str, root: &str, trigger: &str) {
        let (id, root, trigger) = (id.to_owned(), root.to_owned(), trigger.to_owned());
        pool.interact_write("test.session_row", move |conn| {
            conn.execute(
                "INSERT INTO sessions \
                     (id, root_session_id, trigger_kind, created_at, last_active, data) \
                 VALUES (?1, ?2, 'issue', 0, 0, ?3)",
                rusqlite::params![id, root, trigger],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    }

    async fn record_cost(
        costs: &crate::sqlite::cost::SqliteCostStore,
        session: &str,
        at: chrono::DateTime<chrono::Utc>,
        micros: i64,
    ) {
        baybo_store::cost::CostStore::record(
            costs,
            &baybo_model::CostRecord {
                user_id: "u".into(),
                session_id: SessionId::from(session.to_owned()),
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
            },
        )
        .await
        .unwrap();
    }

    /// A subagent bills against its own session id, so the run that spawned
    /// it only sees that spend through `root_session_id`. Without the
    /// rollup a subagent-heavy run reads near zero — and, worse, escapes
    /// the ceiling that is supposed to stop it.
    #[tokio::test]
    async fn a_subagents_spend_bills_the_run_that_spawned_it() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(dir.path().join("test.db")).await.unwrap();
        let store = SqliteProjectStore::new(pool.clone());
        let costs = crate::sqlite::cost::SqliteCostStore::new(pool.clone());

        let p = project("01JSUBAGENT", "delegating");
        store.create_project(&p).await.unwrap();
        let issue = store
            .create_issue(&new_issue(&p.id, "spawns helpers", IssueStatus::Todo))
            .await
            .unwrap();

        let root = "sess-root";
        let trigger = format!(
            r#"{{"trigger":{{"kind":"issue","project_id":"{}","issue_id":"{}","number":{}}}}}"#,
            p.id.as_str(),
            issue.id.as_str(),
            issue.number
        );
        session_row(&pool, root, root, &trigger).await;
        session_row(&pool, "subagent-1", root, &trigger).await;

        let run = store.enqueue_run(&new_run(&issue)).await.unwrap();
        store
            .claim_run(&run.id, &SessionId::from(root.to_owned()))
            .await
            .unwrap();

        // The negative control, and the whole reason a second board is here:
        // the rollup has to be scoped to *this* run's root. Without a tree
        // that must stay out, `root_session_id = r.session_id` and
        // `root_session_id IS NOT NULL` are indistinguishable — every
        // assertion below passes while each run bills for every subagent in
        // the database.
        let other = project("01JOTHERBOARD", "someone else");
        store.create_project(&other).await.unwrap();
        let other_issue = store
            .create_issue(&new_issue(&other.id, "elsewhere", IssueStatus::Todo))
            .await
            .unwrap();
        let other_trigger = format!(
            r#"{{"trigger":{{"kind":"issue","project_id":"{}","issue_id":"{}","number":{}}}}}"#,
            other.id.as_str(),
            other_issue.id.as_str(),
            other_issue.number
        );
        session_row(&pool, "sess-other", "sess-other", &other_trigger).await;
        session_row(&pool, "subagent-other", "sess-other", &other_trigger).await;
        let other_run = store.enqueue_run(&new_run(&other_issue)).await.unwrap();
        store
            .claim_run(&other_run.id, &SessionId::from("sess-other".to_owned()))
            .await
            .unwrap();

        let now = chrono::Utc::now();
        record_cost(&costs, root, now, 1_000).await;
        record_cost(&costs, "subagent-1", now, 500).await;
        record_cost(&costs, "sess-other", now, 4_000).await;
        record_cost(&costs, "subagent-other", now, 9_000).await;

        let rows = store.run_spend(&issue.id).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].spend.cost,
            baybo_model::MicroUsd::from_micros(1_500),
            "the run pays for what its subagent spent"
        );
        assert_eq!(rows[0].spend.tokens(), 4, "…tokens along with the money");

        let facts = store
            .settled_run_facts(std::slice::from_ref(&run.id))
            .await
            .unwrap();
        assert_eq!(
            facts[0].spend, rows[0].spend,
            "the feed and the execution log are the same predicate"
        );

        let since = now - chrono::Duration::hours(1);
        assert_eq!(
            store.spend_since(&p.id, since).await.unwrap().cost,
            baybo_model::MicroUsd::from_micros(1_500),
            "and the board's meter sees at least what its cards do"
        );
        let board = store.board_activity(since).await.unwrap();
        let burn = |id: &ProjectId| {
            board
                .iter()
                .find(|(row, _)| row == id)
                .map(|(_, a)| a.burn)
                .unwrap()
        };
        assert_eq!(
            burn(&p.id).cost,
            baybo_model::MicroUsd::from_micros(1_500),
            "the dropdown's burn measures the same thing the gate does"
        );
        assert_eq!(burn(&p.id).tokens(), 4);

        // The control read back: the other board's tree is wholly its own,
        // at every altitude.
        assert_eq!(
            store.run_spend(&other_issue.id).await.unwrap()[0]
                .spend
                .cost,
            baybo_model::MicroUsd::from_micros(13_000)
        );
        assert_eq!(
            store.spend_since(&other.id, since).await.unwrap().cost,
            baybo_model::MicroUsd::from_micros(13_000)
        );
        assert_eq!(
            burn(&other.id).cost,
            baybo_model::MicroUsd::from_micros(13_000)
        );
    }

    /// A cron fire that files onto a board burns real tokens and is nobody's
    /// run, so no `issue_runs` row will ever point at its session. The board
    /// it names on its trigger is the only thing that can bill it.
    #[tokio::test]
    async fn a_board_bound_cron_fire_bills_the_board_it_files_on() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(dir.path().join("test.db")).await.unwrap();
        let store = SqliteProjectStore::new(pool.clone());
        let costs = crate::sqlite::cost::SqliteCostStore::new(pool.clone());

        let p = project("01JCRONBOARD", "swept");
        store.create_project(&p).await.unwrap();

        let fire = "cron-fire-1";
        session_row(
            &pool,
            fire,
            fire,
            &format!(
                r#"{{"trigger":{{"kind":"cron","cron_job_id":"j1","project_id":"{}"}}}}"#,
                p.id.as_str()
            ),
        )
        .await;
        // Two controls, because the two ways to get this wrong are
        // different. A fire on no board at all catches a predicate that
        // forgot to require one…
        session_row(
            &pool,
            "cron-fire-loose",
            "cron-fire-loose",
            r#"{"trigger":{"kind":"cron","cron_job_id":"j2"}}"#,
        )
        .await;
        // …and a fire bound to a *different* board catches one that
        // required a board but not *this* board — which the loose fire
        // cannot, since it is excluded either way.
        let other = project("01JOTHERCRON", "not swept here");
        store.create_project(&other).await.unwrap();
        session_row(
            &pool,
            "cron-fire-elsewhere",
            "cron-fire-elsewhere",
            &format!(
                r#"{{"trigger":{{"kind":"cron","cron_job_id":"j3","project_id":"{}"}}}}"#,
                other.id.as_str()
            ),
        )
        .await;

        let now = chrono::Utc::now();
        record_cost(&costs, fire, now, 700).await;
        record_cost(&costs, "cron-fire-loose", now, 900).await;
        record_cost(&costs, "cron-fire-elsewhere", now, 1_300).await;

        let since = now - chrono::Duration::hours(1);
        assert_eq!(
            store.spend_since(&p.id, since).await.unwrap().cost,
            baybo_model::MicroUsd::from_micros(700),
            "the bound fire is inside the ceiling; neither control is"
        );
        assert_eq!(
            store.spend_since(&other.id, since).await.unwrap().cost,
            baybo_model::MicroUsd::from_micros(1_300),
            "and the other board bills its own fire, not this one's"
        );

        let board = store.board_activity(since).await.unwrap();
        let burn = |id: &ProjectId| {
            board
                .iter()
                .find(|(row, _)| row == id)
                .map(|(_, a)| a.burn)
                .unwrap()
        };
        assert_eq!(burn(&p.id).cost, baybo_model::MicroUsd::from_micros(700));
        assert_eq!(burn(&p.id).tokens(), 2);
        assert_eq!(
            burn(&other.id).cost,
            baybo_model::MicroUsd::from_micros(1_300)
        );
        assert!(
            board.len() == 2,
            "the loose fire creates no board row at all"
        );
    }

    #[tokio::test]
    async fn an_interrupted_runs_spend_still_counts_against_its_board() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePool::open(dir.path().join("test.db")).await.unwrap();
        let store = SqliteProjectStore::new(pool.clone());
        let costs = crate::sqlite::cost::SqliteCostStore::new(pool);

        let p = project("01JCRASH", "crashy");
        store.create_project(&p).await.unwrap();
        let issue = store
            .create_issue(&new_issue(&p.id, "burns tokens", IssueStatus::Todo))
            .await
            .unwrap();

        let session = SessionId::from("sess-crashed".to_owned());
        let run = store.enqueue_run(&new_run(&issue)).await.unwrap();
        store.claim_run(&run.id, &session).await.unwrap();

        let now = chrono::Utc::now();
        baybo_store::cost::CostStore::record(
            &costs,
            &baybo_model::CostRecord {
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
                cost_usd: baybo_model::MicroUsd::from_micros(500),
                timestamp: now,
            },
        )
        .await
        .unwrap();

        let since = now - chrono::Duration::hours(1);
        let spent = Spend {
            input_tokens: 1,
            output_tokens: 1,
            cost: baybo_model::MicroUsd::from_micros(500),
        };
        assert_eq!(store.spend_since(&p.id, since).await.unwrap(), spent);

        store.requeue_unsettled().await.unwrap();
        assert_eq!(
            store.spend_since(&p.id, since).await.unwrap(),
            spent,
            "a restart does not refund the board"
        );
    }

    /// Where a reader opening a card should land: the OLDEST entry it has
    /// not seen, chosen by the same predicate the unread badge counts with.
    ///
    /// The two must not be able to disagree — a badge saying 2 over a
    /// divider drawn above the operator's own comment is a card that
    /// contradicts itself — which is why the id is resolved here rather
    /// than by a client handed the cursor and the rows.
    #[tokio::test]
    async fn a_card_opens_at_the_oldest_thing_the_operator_has_not_seen() {
        let (_dir, store) = store().await;
        let dev = IssueActor::Agent(AgentProfileId::parse("dev-1").unwrap());
        let p = project("01JFIRSTUNREAD", "Landing");
        store.create_project(&p).await.unwrap();
        let issue = store
            .create_issue(&new_issue(&p.id, "long thread", IssueStatus::Todo))
            .await
            .unwrap();

        let comment = |text: &str| IssueEventBody::Comment {
            text: text.to_owned(),
            attachments: Vec::new(),
        };

        store
            .append_event(&event(&issue, dev.clone(), comment("read this one")))
            .await
            .unwrap();
        assert_eq!(
            store.first_unread_event(&issue.id).await.unwrap(),
            Some(
                store
                    .list_events(&issue.id)
                    .await
                    .unwrap()
                    .first()
                    .unwrap()
                    .id
                    .clone()
            ),
            "a card nobody has opened has read nothing"
        );

        store
            .mark_issue_read(&issue.id, chrono::Utc::now())
            .await
            .unwrap();
        assert_eq!(
            store.first_unread_event(&issue.id).await.unwrap(),
            None,
            "and opening it leaves nothing to land on"
        );

        // The operator's own comment is not news to the operator, and a
        // system entry is machinery — neither is what the badge counts, so
        // neither may be what the divider sits above.
        store
            .append_event(&event(&issue, IssueActor::User, comment("mine")))
            .await
            .unwrap();
        store
            .append_event(&event(
                &issue,
                IssueActor::System,
                IssueEventBody::Moved {
                    from: IssueStatus::Todo,
                    to: IssueStatus::InProgress,
                },
            ))
            .await
            .unwrap();
        assert_eq!(store.first_unread_event(&issue.id).await.unwrap(), None);

        store
            .append_event(&event(&issue, dev.clone(), comment("the first new one")))
            .await
            .unwrap();
        store
            .append_event(&event(&issue, dev.clone(), comment("and a later one")))
            .await
            .unwrap();
        let landed = store.first_unread_event(&issue.id).await.unwrap();
        let rows = store.list_events(&issue.id).await.unwrap();
        let expected = rows
            .iter()
            .find(|row| {
                matches!(&row.body, IssueEventBody::Comment { text, .. } if text == "the first new one")
            })
            .unwrap();
        assert_eq!(
            landed.as_ref(),
            Some(&expected.id),
            "the oldest unread one, so the new run reads downward"
        );
        assert_eq!(
            store
                .card_signals(&p.id)
                .await
                .unwrap()
                .get(&issue.id)
                .map(|signals| signals.unread),
            Some(2),
            "and the badge counts the same set the divider was placed by"
        );
    }

    /// A board-wide stamp is still one cursor per card, and it only ever
    /// moves forward. Two presses in flight at once are the ordinary case —
    /// the older one landing second must not rewind the cards the newer one
    /// cleared and light every badge back up.
    #[tokio::test]
    async fn a_late_board_read_never_rewinds_the_cards_it_finds() {
        let (_dir, store) = store().await;
        let dev = IssueActor::Agent(AgentProfileId::parse("dev-1").unwrap());
        let p = project("01JREADALL", "Reading");
        store.create_project(&p).await.unwrap();
        let elsewhere = project("01JELSEWHERE", "Next door");
        store.create_project(&elsewhere).await.unwrap();
        // A finished card is stamped like any other: the cursor says "seen",
        // and a card being over is not a reason to go on counting what was
        // said on it.
        for (project, title, status) in [
            (&p.id, "one", IssueStatus::Backlog),
            (&p.id, "two", IssueStatus::Done),
            (&elsewhere.id, "theirs", IssueStatus::Backlog),
        ] {
            let issue = store
                .create_issue(&new_issue(project, title, status))
                .await
                .unwrap();
            store
                .append_event(&event(
                    &issue,
                    dev.clone(),
                    IssueEventBody::Comment {
                        text: "which way?".into(),
                        attachments: Vec::new(),
                    },
                ))
                .await
                .unwrap();
        }
        let unread = |project: &ProjectId| {
            let store = &store;
            let project = project.clone();
            async move {
                store
                    .card_signals(&project)
                    .await
                    .unwrap()
                    .values()
                    .map(|signals| signals.unread)
                    .sum::<usize>()
            }
        };
        assert_eq!(unread(&p.id).await, 2);

        let now = chrono::Utc::now();
        assert_eq!(store.mark_project_read(&p.id, now).await.unwrap(), 2);
        assert_eq!(unread(&p.id).await, 0);

        assert_eq!(
            store
                .mark_project_read(&p.id, now - chrono::Duration::hours(1))
                .await
                .unwrap(),
            0,
            "a stamp older than the cursor moves nothing"
        );
        assert_eq!(unread(&p.id).await, 0, "and rewinds nothing either");
        assert_eq!(
            unread(&elsewhere.id).await,
            1,
            "the board next door was never stamped"
        );
    }
}
