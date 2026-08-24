//! What the board starts without being told to.
//!
//! Todo is "ready, waiting for capacity", and this is the thing that
//! notices the capacity. Everything here is a rule over rows the caller
//! already read — the manager owns the reads, the writes, and the order
//! they happen in; this module owns *which cards*, and nothing else.
//!
//! Three rules, and they are deliberately different shapes:
//!
//! - [`slate`] is **level-triggered**. It answers "given the board as it
//!   is right now, what should be running?", so running it twice on an
//!   unchanged board promotes nothing the second time and a missed call
//!   costs a delay rather than a card.
//! - The lead asks ([`awaiting_triage`], [`awaiting_review`], [`stalled`],
//!   [`blocked`], [`awaiting_grooming`]) are level-triggered too, but
//!   waking the lead does not change what they look at — an unassigned card
//!   the lead decided to leave alone is still an unassigned card.
//!   [`already_asked`] is what stops that from being a loop that bills for
//!   the same question every time a run ends anywhere on the board: the
//!   lead is asked again only when the card has *changed* since it last
//!   looked — or when the board changed a rule the answer was given under
//!   ([`reopened_at`]).
//! - [`ran_dry`] is the one question about the **board**, and it is asked
//!   only once every rule above it has declined. Each of those reads a
//!   single card, and a card cannot see the thing that most often strands
//!   it: an answer given about it whose premise was somewhere else. Every
//!   per-card question declining is exactly the state in which that has
//!   happened, which is what makes "the board has looked at all of it and
//!   has no move left" a fact worth spending a run on.
//!   [`nothing_has_happened_since_the_lead_looked`] is its guard.
//!
//! Backlog is the one column the board **pulls** nothing from and still
//! **asks** about, and the two halves of that are deliberate: a card only
//! ever leaves Backlog because somebody decided it should, and
//! [`awaiting_grooming`] asks the lead to be that somebody — but only for
//! the cards the board itself filed there.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use baybo_store::project::{
    DrainMarks, IssuePriority, IssueRow, IssueRunRow, IssueStatus, RunStatus, RunTrigger,
};

/// The cards that already have a run recorded against them, by number.
///
/// A card holds one run slot, so one that is already spoken for cannot be
/// given another — and promoting it anyway would move it into In Progress
/// and then fail to start anything, which is the one state that column must
/// never be in. A comment on a Todo card wakes its assignee where it
/// stands, so this is not a rare shape.
pub(crate) fn busy(runs: impl IntoIterator<Item = i64>) -> BTreeSet<i64> {
    runs.into_iter().collect()
}

/// Whether this card is genuinely waiting on the board — everything the
/// driver needs to be true *before* it looks at who is on it.
///
/// Stricter than [`accepts_runs`](crate::runs::accepts_runs), which asks
/// only whether a card is still live: a person dragging a blocked card into
/// In Progress is overriding the block on purpose, and the board doing the
/// same thing on its own would be overriding it on nobody's authority.
///
/// The two things the driver can do about a waiting card — start it, or ask
/// who should — split on staffing and on nothing else, so every other gate
/// belongs here rather than once per branch.
fn is_waiting(issue: &IssueRow, busy: &BTreeSet<i64>) -> bool {
    issue.status == IssueStatus::Todo
        && issue.cancelled_at.is_none()
        && board_may_start(issue)
        && !busy.contains(&issue.number)
}

/// Whether automatic board actions may start this card; operators may override blocks.
pub(crate) fn board_may_start(issue: &IssueRow) -> bool {
    issue.blocked_reason.is_none()
}

/// A waiting card somebody is on: the board can start it.
fn is_promotable(issue: &IssueRow, busy: &BTreeSet<i64>) -> bool {
    is_waiting(issue, busy) && issue.assignee.is_some()
}

/// A waiting card nobody is on: the board can only ask who should be.
///
/// Deliberately not `!is_promotable`, which would be true of every card on
/// the board — cancelled, blocked, in another column, already running — and
/// would send all of them to the lead as "unstaffed work".
fn needs_staffing(issue: &IssueRow, busy: &BTreeSet<i64>) -> bool {
    is_waiting(issue, busy) && issue.assignee.is_none()
}

/// Which of two ready cards goes first: the more urgent, then the one the
/// operator put higher in the column.
///
/// The same order [`IssueList`](crate::tools::IssueList) already reads a
/// column in, and that is the point — "what is next in Todo" has one
/// answer, whether an agent asks for it or the board acts on it.
///
/// `pinned` is deliberately not in here. A pin is how the operator wants a
/// column *read*; `priority` is what the board should work on first, and
/// two fields answering that question is one of them being wrong. Pinning
/// a card to keep an eye on it must not quietly promote it past urgent
/// work.
fn promotion_order(a: &IssueRow, b: &IssueRow) -> Ordering {
    let rank = |p: IssuePriority| IssuePriority::ALL.iter().position(|slot| *slot == p);
    rank(a.priority)
        .cmp(&rank(b.priority))
        .then(a.position.cmp(&b.position))
        // Positions are dense within a column, so this only decides between
        // two rows mid-reorder. It still has to be *something*: a sort that
        // can return Equal for distinct cards makes the slate depend on the
        // order the store happened to return.
        .then(a.number.cmp(&b.number))
}

/// The cards to move into In Progress, best first, at most `slots` of them.
pub(crate) fn slate(issues: &[IssueRow], busy: &BTreeSet<i64>, slots: usize) -> Vec<IssueRow> {
    if slots == 0 {
        return Vec::new();
    }
    let mut ready: Vec<IssueRow> = issues
        .iter()
        .filter(|i| is_promotable(i, busy))
        .cloned()
        .collect();
    ready.sort_by(promotion_order);
    ready.truncate(slots);
    ready
}

/// The cards sitting in Todo with nobody on them, in the order the lead
/// should be asked about them.
pub(crate) fn awaiting_triage(issues: &[IssueRow], busy: &BTreeSet<i64>) -> Vec<IssueRow> {
    let mut unstaffed: Vec<IssueRow> = issues
        .iter()
        .filter(|i| needs_staffing(i, busy))
        .cloned()
        .collect();
    unstaffed.sort_by(promotion_order);
    unstaffed
}

/// Whether the lead may be asked about a live card it does not own.
fn takes_a_lead_question(
    issue: &IssueRow,
    in_flight: &BTreeSet<i64>,
    lead: &baybo_model::AgentProfileId,
) -> bool {
    issue.cancelled_at.is_none()
        && !in_flight.contains(&issue.number)
        && issue.assignee.as_ref() != Some(lead)
}

/// Blocked live cards for lead review, in promotion order.
pub(crate) fn blocked(
    issues: &[IssueRow],
    in_flight: &BTreeSet<i64>,
    lead: &baybo_model::AgentProfileId,
) -> Vec<IssueRow> {
    let mut paused: Vec<IssueRow> = issues
        .iter()
        .filter(|i| {
            !board_may_start(i)
                && crate::runs::accepts_runs(i)
                && takes_a_lead_question(i, in_flight, lead)
        })
        .cloned()
        .collect();
    paused.sort_by(promotion_order);
    paused
}

/// The cards sitting in Review with nothing running on them, in the order
/// the lead should be asked about them.
pub(crate) fn awaiting_review(
    issues: &[IssueRow],
    busy: &BTreeSet<i64>,
    lead: &baybo_model::AgentProfileId,
) -> Vec<IssueRow> {
    let mut waiting: Vec<IssueRow> = issues
        .iter()
        .filter(|i| {
            i.status == IssueStatus::Review
                && board_may_start(i)
                && takes_a_lead_question(i, busy, lead)
        })
        .cloned()
        .collect();
    waiting.sort_by(promotion_order);
    waiting
}

/// The cards sitting in In Progress with no run working them and nothing
/// queued — work that has silently stopped — in the order the lead should
/// be asked about them.
///
/// The runs-dependent half of the stall question lives in
/// [`newest_run_was_cancelled`], asked by the caller once it has the
/// card's runs in hand.
pub(crate) fn stalled(
    issues: &[IssueRow],
    busy: &BTreeSet<i64>,
    lead: &baybo_model::AgentProfileId,
) -> Vec<IssueRow> {
    let mut stuck: Vec<IssueRow> = issues
        .iter()
        .filter(|i| {
            i.status == IssueStatus::InProgress
                && board_may_start(i)
                && takes_a_lead_question(i, busy, lead)
        })
        .cloned()
        .collect();
    stuck.sort_by(promotion_order);
    stuck
}

/// Whether the latest block was agent-authored.
pub(crate) fn block_is_an_agents_question(events: &[baybo_store::project::IssueEventRow]) -> bool {
    newest_block(events)
        .is_some_and(|e| matches!(e.actor, baybo_store::project::IssueActor::Agent(_)))
}

pub(crate) fn blocked_at(
    events: &[baybo_store::project::IssueEventRow],
) -> Option<chrono::DateTime<chrono::Utc>> {
    newest_block(events).map(|e| e.created_at)
}

pub(crate) fn a_block_was_lifted(events: &[baybo_store::project::IssueEventRow]) -> bool {
    events
        .iter()
        .any(|e| matches!(e.body, baybo_store::project::IssueEventBody::Unblocked))
}

/// When the block standing at `at` began, if any.
pub(crate) fn block_standing_at(
    events: &[baybo_store::project::IssueEventRow],
    at: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let latest = events.iter().rev().find(|e| {
        e.created_at <= at
            && matches!(
                e.body,
                baybo_store::project::IssueEventBody::Blocked { .. }
                    | baybo_store::project::IssueEventBody::Unblocked
            )
    })?;
    matches!(
        latest.body,
        baybo_store::project::IssueEventBody::Blocked { .. }
    )
    .then_some(latest.created_at)
}

fn newest_block(
    events: &[baybo_store::project::IssueEventRow],
) -> Option<&baybo_store::project::IssueEventRow> {
    events
        .iter()
        .rev()
        .find(|e| matches!(e.body, baybo_store::project::IssueEventBody::Blocked { .. }))
}

/// The cards an **agent** parked in Backlog, in the order the lead should
/// be asked about them.
///
/// Backlog is the one live column [`is_waiting`] does not open on, so a
/// card left there is work nothing will ever start. When a person put it
/// there that is the column doing its job — parked work the board is to
/// leave alone, the same standing as a block a person set. When the board
/// filled it itself it is a dead end, and `agent_opened` is what tells the
/// two apart. Authorship, not the assignee: who *filed* the card is who
/// decided where it sits.
///
/// Deliberately assignee-agnostic, unlike [`awaiting_triage`]. A staffed
/// Backlog card is not work waiting for a slot — nothing is coming for it —
/// so asking only about unstaffed ones would strand precisely the cards a
/// lead had already thought about.
pub(crate) fn awaiting_grooming(
    issues: &[IssueRow],
    in_flight: &BTreeSet<i64>,
    agent_opened: &BTreeSet<i64>,
    lead: &baybo_model::AgentProfileId,
) -> Vec<IssueRow> {
    let mut parked: Vec<IssueRow> = issues
        .iter()
        .filter(|i| {
            i.status == IssueStatus::Backlog
                && agent_opened.contains(&i.number)
                && board_may_start(i)
                && takes_a_lead_question(i, in_flight, lead)
        })
        .cloned()
        .collect();
    parked.sort_by(promotion_order);
    parked
}

/// Whether the board has run dry: nothing executing, nothing this pass
/// promoted, and live work still sitting on it.
///
/// The one board-scale question in this module, and it has to be one: it is
/// asked last, after every per-card question declined, so what it reports is
/// not a fact about any card but a fact about the board — every card on it
/// has been asked about and answered, and there is no move left. No row
/// carries that, which is exactly why the hole it closes stayed open. A
/// lead's "not yet, wait for #8" is a complete answer while #8 is running
/// and a dead end the moment #8 lands, and #8 landing touches nothing on
/// the deferred card.
///
/// Capacity is the caller's gate, as it is for every other question here:
/// `ask_the_lead` runs only with a slot to spare, and a board an operator
/// stopped by setting its parallelism to zero is stopped, not drained.
pub(crate) fn ran_dry(issues: &[IssueRow], in_flight: &BTreeSet<i64>, promoted: usize) -> bool {
    promoted == 0 && in_flight.is_empty() && issues.iter().any(crate::runs::accepts_runs)
}

/// Whether the board has done anything since the lead last had it in front
/// of it — the guard on the drain question.
///
/// The board-scale twin of [`already_asked`], and a comparison for the same
/// reason. What it compares is deliberately asymmetric: **any** wake counts
/// as the lead having looked, because a coordination brief hands it the
/// whole board, but only **work** counts as something having happened. So
/// the question survives exactly one shape — the board did work, that work
/// is now over, and nobody has read the board since. That is the shape a
/// deferral rots in: "not yet, wait for the other card" is a complete
/// answer when it is given and a dead end the moment the other card lands,
/// and the landing touches nothing the deferred card carries.
///
/// It also closes the obvious spin for free: the drain question is itself a
/// wake, so being asked is being looked at, and answering it is not work.
///
/// A board nothing has ever run on has never been looked at, and that is the
/// truth rather than an edge case: cards were filed and nothing came.
pub(crate) fn nothing_has_happened_since_the_lead_looked(marks: &DrainMarks) -> bool {
    match (marks.looked_at, marks.worked_at) {
        (None, _) => false,
        (Some(looked), worked) => worked.is_none_or(|worked| worked <= looked),
    }
}

/// The card a board-scale question is filed against.
///
/// A run is a row on a card, so a question that has no card of its own still
/// needs one to live on. The pick is the order every column is already read
/// in, over the cards the board could act on — filing it against a blocked
/// card would put a run on work an operator paused, and `parked_by_a_block`
/// would hold that run rather than deliver it.
///
/// Deliberately **not** [`takes_a_lead_question`]: a card whose assignee is
/// the lead is excluded from every question *about a card*, because the
/// lead's own card has no other party. This question is not about the card,
/// and a board whose only live card is the lead's is precisely a board with
/// nothing else to anchor to.
pub(crate) fn drain_anchor(issues: &[IssueRow], in_flight: &BTreeSet<i64>) -> Option<IssueRow> {
    issues
        .iter()
        .filter(|i| {
            crate::runs::accepts_runs(i) && board_may_start(i) && !in_flight.contains(&i.number)
        })
        .min_by(|a, b| promotion_order(a, b))
        .cloned()
}

/// A card whose newest run was **cancelled** is not stalled: a cancel is a
/// decision — a human's stop, or the board calling a row off — and waking
/// the lead to get the work going again would countermand it within one
/// tick. The stop stands until somebody acts on the card, which makes it a
/// new question.
pub(crate) fn newest_run_was_cancelled(runs: &[IssueRunRow]) -> bool {
    runs.iter()
        .max_by_key(|run| run.created_at)
        .is_some_and(|run| run.status == RunStatus::Cancelled)
}

/// When this card last became a fresh question, before any run is weighed:
/// its own row changing, or the board changing a rule it schedules by.
///
/// The second half is the part a card cannot see about itself. "Escalate
/// this to somebody who may merge" is a *complete* answer while the board's
/// agents may not merge, and the board being told they now may is the only
/// thing that ever happens next — it touches no card, so a guard reading the
/// card alone goes on holding an answer whose premise is gone. Same shape
/// for a ceiling raised, a parallelism raised off zero, and a board restored
/// from the archive: the operator changed what the board may do, and every
/// standing answer was given under the old rules.
///
/// Deliberately the whole board at once rather than an edge per rule. Which
/// card a given rule could unstick is not knowable here — the reason lives
/// in the lead's prose — and one re-ask per operator action is the bound
/// that makes guessing unnecessary.
fn reopened_at(
    issue: &IssueRow,
    rules_changed_at: chrono::DateTime<chrono::Utc>,
) -> chrono::DateTime<chrono::Utc> {
    issue.updated_at.max(rules_changed_at)
}

/// When this card last changed in a way the lead has not seen:
/// [`reopened_at`], or the settle of its newest *work* run, whichever is
/// later. Coordination runs are excluded on both sides of the question —
/// the lead looking at a card is not the card changing.
fn last_activity(
    issue: &IssueRow,
    runs: &[IssueRunRow],
    rules_changed_at: chrono::DateTime<chrono::Utc>,
) -> chrono::DateTime<chrono::Utc> {
    let reopened = reopened_at(issue, rules_changed_at);
    runs.iter()
        .filter(|run| !run.trigger.is_coordination())
        .filter_map(|run| run.settled_at)
        .max()
        .map_or(reopened, |settled| settled.max(reopened))
}

/// How many times one question may be re-asked while the card row itself
/// stands unchanged. Work runs settling re-raise a question (a reviewer's
/// verdict is news), but the coordination machinery can *generate* that
/// activity — the lead's wake comments, the assignee answers, the settle
/// re-arms the wake — and each cycle bills two real runs. The cap is the
/// mechanical bound the loop lacks: past it, only somebody editing, moving
/// or restaffing the card makes it a question again.
const MAX_ASKS_PER_CARD_STATE: usize = 2;

/// Whether the lead has already been asked this question about this card
/// **as it stands**.
///
/// The comparison is against the card's last activity rather than a flag,
/// because the honest question is "has anything changed since the lead
/// looked?" — and a lead that read the card and decided to leave it alone
/// changed nothing, which is precisely the case that must not ask again.
/// Editing the card, moving it, or a work run settling on it makes it a
/// new question — up to [`MAX_ASKS_PER_CARD_STATE`] times.
///
/// An ask that failed without ever being claimed never reached the lead —
/// that is the dispatcher dying before the brief was cut — so it does not
/// count as the question having been asked. It still counts against the
/// cap: a card whose checkout refuses to cut should not be retried every
/// tick forever.
pub(crate) fn already_asked(
    issue: &IssueRow,
    runs: &[IssueRunRow],
    question: RunTrigger,
    rules_changed_at: chrono::DateTime<chrono::Utc>,
) -> bool {
    let activity = last_activity(issue, runs, rules_changed_at);
    let asks = runs.iter().filter(|run| run.trigger == question);
    let delivered = asks.clone().any(|run| {
        run.created_at >= activity && !(run.status == RunStatus::Failed && !run.was_claimed())
    });
    // The cap counts against the same mark, and not against `updated_at`
    // alone: a board whose rules changed has not asked this question under
    // them at all, so a card that spent its two asks under the old ones is
    // not a card that has been asked.
    delivered
        || asks
            .filter(|run| run.created_at >= reopened_at(issue, rules_changed_at))
            .count()
            >= MAX_ASKS_PER_CARD_STATE
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{AgentProfileId, IssueId, IssueRunId, ProjectId};
    use baybo_store::project::RunStatus;
    use chrono::{Duration, Utc};

    fn issue(number: i64, status: IssueStatus) -> IssueRow {
        let now = Utc::now();
        IssueRow {
            id: IssueId::generate(),
            project_id: ProjectId::parse("proj-a".to_owned()).expect("id"),
            number,
            title: format!("card {number}"),
            description: String::new(),
            attachments: Vec::new(),
            status,
            priority: IssuePriority::None,
            assignee: Some(AgentProfileId::parse("dev-1".to_owned()).expect("agent")),
            position: number,
            pinned: false,
            blocked_reason: None,
            branch: None,
            parent_issue_id: None,
            stage: 0,
            source_key: None,
            filed_from: None,
            cancelled_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn ready(number: i64) -> IssueRow {
        issue(number, IssueStatus::Todo)
    }

    fn numbers(slate: &[IssueRow]) -> Vec<i64> {
        slate.iter().map(|i| i.number).collect()
    }

    /// A board with nothing recorded against any card.
    fn idle() -> BTreeSet<i64> {
        BTreeSet::new()
    }

    /// A board whose rules the operator has never touched. Older than every
    /// card the tests build, so it re-opens nothing and the guard reads
    /// exactly as it did before boards could change their own rules.
    fn rules_never_changed() -> chrono::DateTime<chrono::Utc> {
        Utc::now() - Duration::days(365)
    }

    /// One reason to leave a card alone: why, and how to put a card in that
    /// state.
    type Gate = (&'static str, Box<dyn Fn(&mut IssueRow)>);

    /// Every reason **on the row itself** for the driver to leave a card
    /// alone, other than staffing. The fifth gate, `busy`, is a separate
    /// input rather than a field, so it has its own test below — asserting
    /// the same two things.
    ///
    /// Driven as a table because these are the clauses [`is_waiting`] holds
    /// on behalf of both branches: a gate that stops a promotion has to stop
    /// a triage too, and one added to only one of them is a rule the other
    /// quietly ignores. A new gate is a new row here, asserted on both sides
    /// for free.
    fn gates() -> Vec<Gate> {
        let mut rows: Vec<Gate> = vec![
            (
                "a block is a person saying stop, and the board does not overrule it",
                Box::new(|i: &mut IssueRow| {
                    i.blocked_reason = Some("waiting on the operator".into())
                }),
            ),
            (
                "a cancelled card is not live work",
                Box::new(|i: &mut IssueRow| i.cancelled_at = Some(Utc::now())),
            ),
        ];
        for status in [
            IssueStatus::Backlog,
            IssueStatus::InProgress,
            IssueStatus::Review,
            IssueStatus::Done,
        ] {
            rows.push((
                "only Todo is a queue the board pulls from",
                Box::new(move |i: &mut IssueRow| i.status = status),
            ));
        }
        rows
    }

    #[test]
    fn staffing_is_the_only_thing_the_two_branches_disagree_about() {
        assert!(
            is_promotable(&ready(1), &idle()),
            "the control: a staffed card in Todo with nothing against it is startable"
        );

        for (why, break_it) in gates() {
            let mut staffed = ready(1);
            break_it(&mut staffed);
            let mut unstaffed = staffed.clone();
            unstaffed.assignee = None;

            assert!(!is_promotable(&staffed, &idle()), "promoted anyway: {why}");
            assert!(
                awaiting_triage(&[unstaffed], &idle()).is_empty(),
                "sent to the lead as unstaffed work anyway: {why}"
            );
        }
    }

    #[test]
    fn the_board_never_starts_work_nobody_is_on() {
        let mut unstaffed = ready(1);
        unstaffed.assignee = None;
        assert!(
            !is_promotable(&unstaffed, &idle()),
            "In Progress needs an assignee, so an unassigned card cannot be promoted into it"
        );
        assert_eq!(
            numbers(&awaiting_triage(&[unstaffed], &idle())),
            vec![1],
            "it is the lead's question instead"
        );
    }

    #[test]
    fn urgent_jumps_the_queue_and_position_settles_the_rest() {
        let mut low = ready(1);
        low.priority = IssuePriority::Low;
        low.position = 0;
        let mut urgent = ready(2);
        urgent.priority = IssuePriority::Urgent;
        urgent.position = 1;
        let mut also_low = ready(3);
        also_low.priority = IssuePriority::Low;
        also_low.position = 2;

        assert_eq!(
            numbers(&slate(&[low, urgent, also_low], &idle(), 3)),
            vec![2, 1, 3],
            "priority first, then the order the operator put the column in"
        );
    }

    #[test]
    fn the_slate_is_capped_by_the_slots_it_was_given() {
        let column: Vec<IssueRow> = (1..=5).map(ready).collect();
        assert_eq!(numbers(&slate(&column, &idle(), 2)), vec![1, 2]);
        assert!(
            slate(&column, &idle(), 0).is_empty(),
            "a full board starts nothing, and a ceiling of zero is a full board"
        );
        assert_eq!(
            numbers(&slate(&column, &idle(), 99)),
            vec![1, 2, 3, 4, 5],
            "more room than work is not an error"
        );
    }

    #[test]
    fn a_card_that_already_has_a_run_is_left_where_it_is() {
        let ready_and_running = ready(1);
        let mut unstaffed_and_running = ready(2);
        unstaffed_and_running.assignee = None;
        let busy = busy([1, 2]);

        assert!(
            slate(&[ready_and_running], &busy, 3).is_empty(),
            "promoting it would move it into In Progress and then start nothing, \
             because a card holds one run slot"
        );
        assert!(
            awaiting_triage(&[unstaffed_and_running], &busy).is_empty(),
            "and a card the lead is already looking at is not a fresh question"
        );
    }

    fn triage_run(issue: &IssueRow, at: chrono::DateTime<Utc>) -> IssueRunRow {
        IssueRunRow {
            id: IssueRunId::generate(),
            issue_id: issue.id.clone(),
            project_id: issue.project_id.clone(),
            number: issue.number,
            agent_id: AgentProfileId::parse("lead".to_owned()).expect("agent"),
            session_id: None,
            trigger: RunTrigger::Triage,
            status: RunStatus::Done,
            attempt: 1,
            resumes: 0,
            error: None,
            created_at: at,
            started_at: None,
            settled_at: None,
        }
    }

    #[test]
    fn the_lead_is_asked_once_per_state_of_a_card() {
        let mut card = ready(1);
        card.assignee = None;
        assert!(
            !already_asked(&card, &[], RunTrigger::Triage, rules_never_changed()),
            "a card nobody has looked at is a fresh question"
        );

        let asked = triage_run(&card, card.updated_at + Duration::seconds(1));
        assert!(
            already_asked(
                &card,
                std::slice::from_ref(&asked),
                RunTrigger::Triage,
                rules_never_changed()
            ),
            "a lead that read the card and left it alone must not be asked again"
        );

        card.updated_at = asked.created_at + Duration::seconds(1);
        assert!(
            !already_asked(
                &card,
                std::slice::from_ref(&asked),
                RunTrigger::Triage,
                rules_never_changed()
            ),
            "but editing the card makes it a question the lead has not answered"
        );

        let other = IssueRunRow {
            trigger: RunTrigger::Comment,
            ..triage_run(&card, card.updated_at + Duration::seconds(1))
        };
        assert!(
            !already_asked(
                &card,
                &[asked, other],
                RunTrigger::Triage,
                rules_never_changed()
            ),
            "and a run that was not a triage is not an answer to one"
        );
    }

    #[test]
    fn a_board_has_run_dry_only_with_work_left_on_it_and_nothing_moving() {
        let live = issue(1, IssueStatus::Backlog);
        assert!(
            ran_dry(std::slice::from_ref(&live), &idle(), 0),
            "a card nothing will ever start, and nothing running to start it"
        );
        assert!(
            !ran_dry(std::slice::from_ref(&live), &busy([2]), 0),
            "a board with a run on it is working, whatever that run is on"
        );
        assert!(
            !ran_dry(std::slice::from_ref(&live), &idle(), 1),
            "and a pass that just promoted something has not run dry — the \
             slate it filled is not in flight yet"
        );

        let finished = issue(1, IssueStatus::Done);
        let mut cancelled = issue(2, IssueStatus::Todo);
        cancelled.cancelled_at = Some(Utc::now());
        assert!(
            !ran_dry(&[finished, cancelled], &idle(), 0),
            "a board the work is over on is quiet, not stuck"
        );
    }

    #[test]
    fn the_board_is_told_it_ran_dry_only_when_work_outlived_the_leads_last_look() {
        let looked = Utc::now();
        assert!(
            !nothing_has_happened_since_the_lead_looked(&DrainMarks::default()),
            "a board nothing ever ran on has never been looked at; cards were \
             filed and nothing came for them"
        );
        assert!(
            nothing_has_happened_since_the_lead_looked(&DrainMarks {
                looked_at: Some(looked),
                worked_at: None,
            }),
            "a lead woken on a board nothing has run on has been shown all of it"
        );
        assert!(
            nothing_has_happened_since_the_lead_looked(&DrainMarks {
                looked_at: Some(looked),
                worked_at: Some(looked - Duration::seconds(1)),
            }),
            "and work that settled before that look is work the look covered"
        );
        assert!(
            !nothing_has_happened_since_the_lead_looked(&DrainMarks {
                looked_at: Some(looked),
                worked_at: Some(looked + Duration::seconds(1)),
            }),
            "but work that outlived the last look is a board nobody has read \
             since it stopped — the one shape a stale deferral hides in"
        );
    }

    #[test]
    fn the_drain_anchor_is_a_card_a_run_can_live_on_and_not_a_question_about_it() {
        let lead = AgentProfileId::parse("lead".to_owned()).expect("agent");
        let mut urgent = issue(2, IssueStatus::Backlog);
        urgent.priority = IssuePriority::Urgent;
        let ordinary = issue(1, IssueStatus::Backlog);
        assert_eq!(
            drain_anchor(&[ordinary.clone(), urgent.clone()], &idle()).map(|i| i.number),
            Some(2),
            "the same order every column is read in"
        );

        let mut the_leads_own = issue(3, IssueStatus::Review);
        the_leads_own.assignee = Some(lead);
        assert_eq!(
            drain_anchor(std::slice::from_ref(&the_leads_own), &idle()).map(|i| i.number),
            Some(3),
            "a board whose only live card is the lead's still has a board to \
             ask about — the question is not about the card"
        );

        let mut paused = ordinary;
        paused.blocked_reason = Some("waiting on the operator".into());
        let mut also_paused = urgent;
        also_paused.blocked_reason = Some("waiting on the operator".into());
        assert!(
            drain_anchor(&[paused, also_paused], &idle()).is_none(),
            "every live card paused is nothing the board may act on, and each \
             block has already been put to the lead once"
        );
    }

    #[test]
    fn a_rule_the_board_changed_re_opens_a_question_the_card_cannot_see() {
        let mut card = ready(1);
        card.assignee = None;
        let asked = triage_run(&card, card.updated_at + Duration::seconds(1));
        let runs = std::slice::from_ref(&asked);

        assert!(
            already_asked(&card, runs, RunTrigger::Triage, rules_never_changed()),
            "the control: answered, and the card has not moved since"
        );
        assert!(
            already_asked(
                &card,
                runs,
                RunTrigger::Triage,
                asked.created_at - Duration::seconds(1)
            ),
            "a rule changed before the lead answered is a rule it answered under"
        );
        assert!(
            !already_asked(
                &card,
                runs,
                RunTrigger::Triage,
                asked.created_at + Duration::seconds(1)
            ),
            "but an answer given under rules the board no longer has is not an              answer to the board as it stands, and nothing on the card says so"
        );
    }

    #[test]
    fn a_rule_change_hands_back_the_asks_a_card_spent_under_the_old_ones() {
        let card = ready(1);
        // Two asks that died before their brief: they never reached the
        // lead, so only the cap is holding the question shut.
        let spent: Vec<IssueRunRow> = (1..=2)
            .map(|n| IssueRunRow {
                status: RunStatus::Failed,
                settled_at: Some(card.updated_at + Duration::seconds(n * 2)),
                ..triage_run(&card, card.updated_at + Duration::seconds(n * 2 - 1))
            })
            .collect();
        assert!(
            already_asked(&card, &spent, RunTrigger::Triage, rules_never_changed()),
            "the control: the cap is what stops a card that will not dispatch"
        );

        let newest_ask = spent.iter().map(|run| run.created_at).max().expect("asks");
        assert!(
            !already_asked(
                &card,
                &spent,
                RunTrigger::Triage,
                newest_ask + Duration::seconds(1)
            ),
            "a cap spent under the old rules is not a cap spent under these:              the board has not asked this question once since they changed"
        );
    }

    #[test]
    fn a_work_run_settling_re_raises_the_question_and_the_leads_own_look_does_not() {
        let card = ready(1);
        let asked = triage_run(&card, card.updated_at + Duration::seconds(1));

        let worked = IssueRunRow {
            trigger: RunTrigger::Comment,
            settled_at: Some(asked.created_at + Duration::seconds(10)),
            ..triage_run(&card, card.updated_at)
        };
        assert!(
            !already_asked(
                &card,
                &[asked.clone(), worked],
                RunTrigger::Triage,
                rules_never_changed()
            ),
            "a work run settling after the lead looked is news the lead has not seen"
        );

        let looked_again = IssueRunRow {
            trigger: RunTrigger::Review,
            settled_at: Some(asked.created_at + Duration::seconds(10)),
            ..triage_run(&card, card.updated_at)
        };
        assert!(
            already_asked(
                &card,
                &[asked, looked_again],
                RunTrigger::Triage,
                rules_never_changed()
            ),
            "but the lead's own coordination runs are not the card changing"
        );
    }

    #[test]
    fn review_and_stall_candidates_have_their_own_gates() {
        let lead = AgentProfileId::parse("lead".to_owned()).expect("agent");
        let dev = AgentProfileId::parse("dev-1".to_owned()).expect("agent");
        let mut in_review = ready(1);
        in_review.status = IssueStatus::Review;
        in_review.assignee = Some(dev);
        let mut leads_own = ready(2);
        leads_own.status = IssueStatus::Review;
        leads_own.assignee = Some(lead.clone());
        let mut blocked = ready(3);
        blocked.status = IssueStatus::Review;
        blocked.blocked_reason = Some("waiting on the API".to_owned());

        let picked = awaiting_review(&[in_review.clone(), leads_own, blocked], &busy([]), &lead);
        assert_eq!(
            picked.iter().map(|i| i.number).collect::<Vec<_>>(),
            vec![1],
            "the lead is asked about other people's review cards — not its own, not blocked ones"
        );
        assert!(
            awaiting_review(&[in_review.clone()], &busy([1]), &lead).is_empty(),
            "a review card somebody is already running on is not waiting on the lead"
        );

        let mut stuck = ready(4);
        stuck.status = IssueStatus::InProgress;
        let mut paused = ready(5);
        paused.status = IssueStatus::InProgress;
        paused.blocked_reason = Some("blocked on purpose".to_owned());
        let mut leads_stall = ready(6);
        leads_stall.status = IssueStatus::InProgress;
        leads_stall.assignee = Some(lead.clone());
        assert_eq!(
            stalled(&[stuck.clone(), paused, leads_stall], &busy([]), &lead)
                .iter()
                .map(|i| i.number)
                .collect::<Vec<_>>(),
            vec![4],
            "In Progress with nothing running is stalled; a block is its own explanation, \
             and the lead's own card is a question with no other party"
        );
        assert!(
            stalled(&[stuck], &busy([4]), &lead).is_empty(),
            "a card with a run recorded against it is not stalled"
        );
    }

    #[test]
    fn only_a_card_an_agent_parked_is_the_leads_to_groom() {
        let lead = AgentProfileId::parse("lead".to_owned()).expect("agent");
        let parked = |number: i64| {
            let mut card = ready(number);
            card.status = IssueStatus::Backlog;
            card
        };

        let agents = parked(1);
        let mut agents_unstaffed = parked(2);
        agents_unstaffed.assignee = None;
        let operators = parked(3);
        let mut agents_blocked = parked(4);
        agents_blocked.blocked_reason = Some("waiting on the operator".to_owned());
        let mut agents_cancelled = parked(5);
        agents_cancelled.cancelled_at = Some(Utc::now());
        let mut leads_own = parked(6);
        leads_own.assignee = Some(lead.clone());
        let agents_running = parked(7);

        let board = [
            agents.clone(),
            agents_unstaffed,
            operators,
            agents_blocked,
            agents_cancelled,
            leads_own,
            agents_running,
        ];
        // Every card here but #3 was opened by an agent; #3 is the
        // operator's, and is the whole point of the set.
        let opened = busy([1, 2, 4, 5, 6, 7]);
        assert_eq!(
            numbers(&awaiting_grooming(&board, &busy([7]), &opened, &lead)),
            vec![1, 2],
            "the board asks about the cards it parked itself — staffed or not — and leaves the \
             operator's card, a block, a cancelled card, the lead's own and a card already \
             running exactly where they are"
        );
        assert!(
            awaiting_grooming(&board, &busy([]), &busy([]), &lead).is_empty(),
            "a board whose Backlog is entirely the operator's has no grooming question at all"
        );
        assert!(
            awaiting_grooming(&[ready(1)], &busy([]), &busy([1]), &lead).is_empty(),
            "and a Todo card is somebody else's question — grooming reads Backlog only"
        );
    }

    #[test]
    fn asking_about_backlog_does_not_make_it_a_queue() {
        let lead = AgentProfileId::parse("lead".to_owned()).expect("agent");
        let mut parked = ready(1);
        parked.status = IssueStatus::Backlog;
        let board = [parked.clone()];
        let opened = busy([1]);

        assert_eq!(
            numbers(&awaiting_grooming(&board, &busy([]), &opened, &lead)),
            vec![1],
            "the control: this card is the lead's to groom"
        );
        assert!(
            !is_promotable(&parked, &idle()),
            "and the board still starts nothing out of Backlog"
        );
        assert!(
            slate(&board, &idle(), 3).is_empty(),
            "…however much room it has"
        );
        let mut unstaffed = parked.clone();
        unstaffed.assignee = None;
        assert!(
            awaiting_triage(&[unstaffed], &idle()).is_empty(),
            "…and an unstaffed one is a grooming question, not a triage one"
        );
    }

    #[test]
    fn the_lead_is_asked_about_a_block_and_about_nothing_else_a_block_touches() {
        let lead = AgentProfileId::parse("lead".to_owned()).expect("agent");
        let paused = |number: i64, status: IssueStatus| {
            let mut card = ready(number);
            card.status = status;
            card.blocked_reason = Some("the goal contradicts the Go spec".to_owned());
            card
        };

        let working = paused(1, IssueStatus::InProgress);
        let in_review = paused(2, IssueStatus::Review);
        let mut leads_own = paused(3, IssueStatus::InProgress);
        leads_own.assignee = Some(lead.clone());
        let running = paused(4, IssueStatus::InProgress);
        let mut cancelled = paused(5, IssueStatus::InProgress);
        cancelled.cancelled_at = Some(Utc::now());
        let live = ready(6);

        let board = [
            working,
            in_review,
            leads_own,
            running,
            cancelled,
            live.clone(),
        ];
        assert_eq!(
            numbers(&blocked(&board, &busy([4]), &lead)),
            vec![1, 2],
            "a block is the lead's question — unless it is the lead's own card, already being \
             run, or on a card nobody is coming back to anyway"
        );

        assert!(
            awaiting_review(&board, &busy([]), &lead).is_empty(),
            "the gate split must not leak a blocked card into the review question"
        );
        assert!(
            stalled(&board, &busy([]), &lead).is_empty(),
            "…nor into the stall question"
        );
        assert!(
            awaiting_triage(&board, &busy([])).is_empty(),
            "…nor into triage, which still asks through `is_waiting`"
        );
        assert!(
            blocked(&[live], &busy([]), &lead).is_empty(),
            "and a card nothing has stopped is not a block to adjudicate"
        );
    }

    #[test]
    fn a_card_the_board_has_finished_with_is_not_a_block_to_adjudicate() {
        let lead = AgentProfileId::parse("lead".to_owned()).expect("agent");
        let stale = |number: i64, finish: &dyn Fn(&mut IssueRow)| {
            let mut card = ready(number);
            card.blocked_reason = Some("waiting on the API".to_owned());
            finish(&mut card);
            card
        };

        let done = stale(1, &|card: &mut IssueRow| card.status = IssueStatus::Done);
        let cancelled = stale(2, &|card: &mut IssueRow| {
            card.cancelled_at = Some(Utc::now())
        });
        assert!(
            blocked(&[done, cancelled], &busy([]), &lead).is_empty(),
            "a card no run can be started on is not a question the lead can answer"
        );
    }

    #[test]
    fn a_card_whose_only_run_the_block_itself_parked_is_still_the_leads_question() {
        let lead = AgentProfileId::parse("lead".to_owned()).expect("agent");
        let mut paused = ready(1);
        paused.status = IssueStatus::InProgress;
        paused.blocked_reason = Some("the goal contradicts the Go spec".to_owned());

        assert_eq!(
            numbers(&blocked(std::slice::from_ref(&paused), &busy([]), &lead)),
            vec![1],
            "a parked row is not work in flight, so it is not in the set at all"
        );
        assert!(
            blocked(&[paused], &busy([1]), &lead).is_empty(),
            "and a card something IS executing stays the executor's, not the lead's"
        );
    }

    #[test]
    fn a_stop_stands_and_dispatch_failures_do_not_mute_a_question_but_do_cap_it() {
        let card = ready(1);
        let stopped = IssueRunRow {
            trigger: RunTrigger::Comment,
            status: RunStatus::Cancelled,
            settled_at: Some(card.updated_at + Duration::seconds(5)),
            ..triage_run(&card, card.updated_at + Duration::seconds(1))
        };
        assert!(
            newest_run_was_cancelled(std::slice::from_ref(&stopped)),
            "a cancelled newest run is somebody's stop, not a silent stall"
        );

        // An ask the dispatcher failed before the brief was cut: settled,
        // never claimed. It never reached the lead, so the question is
        // still open —
        let failed_ask = IssueRunRow {
            status: RunStatus::Failed,
            settled_at: Some(card.updated_at + Duration::seconds(2)),
            ..triage_run(&card, card.updated_at + Duration::seconds(1))
        };
        assert!(
            !already_asked(
                &card,
                std::slice::from_ref(&failed_ask),
                RunTrigger::Triage,
                rules_never_changed()
            ),
            "an ask that died before its brief is not the lead having looked"
        );
        // — but a card that keeps refusing to dispatch is not retried
        // forever: the attempts still count against the per-state cap.
        let second_failed = IssueRunRow {
            status: RunStatus::Failed,
            settled_at: Some(card.updated_at + Duration::seconds(4)),
            ..triage_run(&card, card.updated_at + Duration::seconds(3))
        };
        assert!(
            already_asked(
                &card,
                &[failed_ask, second_failed],
                RunTrigger::Triage,
                rules_never_changed()
            ),
            "two dead asks on an unchanged card stop the retry loop"
        );
    }

    #[test]
    fn the_ask_cap_holds_while_the_card_row_stands_unchanged() {
        let card = ready(1);
        let mut runs = Vec::new();
        let mut at = card.updated_at;
        // Two full ask-answer-activity cycles: the lead was asked, a work
        // run settled afterwards (the machinery's own echo), twice over.
        for _ in 0..2 {
            at += Duration::seconds(1);
            runs.push(IssueRunRow {
                session_id: Some(baybo_model::SessionId::from("lead-look".to_owned())),
                settled_at: Some(at + Duration::seconds(1)),
                ..triage_run(&card, at)
            });
            at += Duration::seconds(2);
            runs.push(IssueRunRow {
                trigger: RunTrigger::Comment,
                session_id: Some(baybo_model::SessionId::from("assignee-answer".to_owned())),
                settled_at: Some(at + Duration::seconds(1)),
                ..triage_run(&card, at)
            });
            at += Duration::seconds(2);
        }
        assert!(
            already_asked(&card, &runs, RunTrigger::Triage, rules_never_changed()),
            "the machinery's own echo cannot re-raise the question a third time"
        );

        let mut edited = card.clone();
        edited.updated_at = at + Duration::seconds(1);
        assert!(
            !already_asked(&edited, &runs, RunTrigger::Triage, rules_never_changed()),
            "but somebody changing the card resets the cap"
        );
    }

    fn entry(
        body: baybo_store::project::IssueEventBody,
        at: chrono::DateTime<chrono::Utc>,
    ) -> baybo_store::project::IssueEventRow {
        baybo_store::project::IssueEventRow {
            id: baybo_model::IssueEventId::generate(),
            issue_id: IssueId::generate(),
            project_id: ProjectId::parse("proj-a".to_owned()).expect("id"),
            number: 1,
            actor: baybo_store::project::IssueActor::User,
            body,
            created_at: at,
        }
    }

    fn block_entry(at: chrono::DateTime<chrono::Utc>) -> baybo_store::project::IssueEventRow {
        entry(
            baybo_store::project::IssueEventBody::Blocked {
                reason: "which of the two goals wins?".to_owned(),
            },
            at,
        )
    }

    fn unblock_entry(at: chrono::DateTime<chrono::Utc>) -> baybo_store::project::IssueEventRow {
        entry(baybo_store::project::IssueEventBody::Unblocked, at)
    }

    #[test]
    fn a_run_briefed_under_a_block_owes_the_window_that_block_opened() {
        let landed = Utc::now();
        let briefed = landed + Duration::minutes(5);
        let events = vec![
            block_entry(landed),
            unblock_entry(briefed + Duration::minutes(1)),
        ];

        assert_eq!(
            block_standing_at(&events, briefed),
            Some(landed),
            "the run was briefed under the block, so its window opens where the block did"
        );
        assert!(a_block_was_lifted(&events[1..]));
    }

    #[test]
    fn a_card_unblocked_before_the_brief_owes_that_run_nothing() {
        let landed = Utc::now();
        let lifted = landed + Duration::minutes(1);
        let events = vec![block_entry(landed), unblock_entry(lifted)];

        assert_eq!(
            block_standing_at(&events, lifted + Duration::minutes(1)),
            None,
            "the unblock handed that window over already; re-delivering it wakes the \
             assignee on the answer it has been given"
        );
        assert_eq!(
            block_standing_at(&events, landed - Duration::seconds(1)),
            None,
            "and nothing was blocked before the block landed"
        );
        assert!(!a_block_was_lifted(&events[..1]));
    }

    #[test]
    fn the_block_a_moment_was_under_is_the_one_that_moment_answers_for() {
        let first = Utc::now();
        let briefed = first + Duration::minutes(5);
        let events = vec![
            block_entry(first),
            unblock_entry(briefed - Duration::minutes(1)),
            block_entry(briefed + Duration::minutes(1)),
        ];
        assert_eq!(
            block_standing_at(&events, briefed),
            None,
            "the card was live when this run was briefed, whatever happened after"
        );
        assert_eq!(
            block_standing_at(&events, briefed + Duration::minutes(2)),
            Some(briefed + Duration::minutes(1))
        );
    }
}
