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
//!   looked. Only the card: a rule the board schedules by changing is also
//!   news, but it is news about the **board**, and answering it once per
//!   card is one billed run per card to decide one thing.
//! - [`ran_dry`] is the one question about the **board**, and it is asked
//!   only once every rule above it has declined. Each of those reads a
//!   single card, and a card cannot see the thing that most often strands
//!   it: an answer given about it whose premise was somewhere else. Every
//!   per-card question declining is exactly the state in which that has
//!   happened, which is what makes "the board has looked at all of it and
//!   has no move left" a fact worth spending a run on. It is also where a
//!   changed rule lands, for the same reason: "the operator turned merging
//!   on" is one fact about the whole board, and the lead reads the whole
//!   board to answer it. [`nothing_has_happened_since_the_lead_looked`] is
//!   its guard.
//!
//! Backlog is the one column the board **pulls** nothing from and still
//! **asks** about, and the two halves of that are deliberate: a card only
//! ever leaves Backlog because somebody decided it should, and
//! [`awaiting_grooming`] asks the lead to be that somebody — but only for
//! the cards the board itself filed there.
//!
//! Who filed it is therefore a term in every rule that could move a Backlog
//! card, not only in the question named after it: [`board_may_take_up`] is
//! the one home for it, and [`ran_dry`] and [`drain_anchor`] read it too.
//! [`ran_dry`] asking without it was the hole — a person's parked card is
//! live enough to keep the board from ever going quiet, and the drain
//! question then hands the lead a whole board and asks it to find something
//! to start.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

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
/// belongs here rather than once per branch. [`held_by_a_stage`] is the
/// newest of them, and it is here rather than in [`board_may_start`] on
/// purpose: six rules read that one, and the two that read its *negation*
/// ([`blocked`], and `ran_dry` through [`drain_anchor`]) would start
/// reporting every step of a later stage as a card a block has stopped —
/// handing the lead a question about a card nothing is wrong with.
fn is_waiting(issue: &IssueRow, spoken_for: &BTreeSet<i64>, held: &BTreeSet<i64>) -> bool {
    issue.status == IssueStatus::Todo
        && crate::runs::accepts_runs(issue)
        && board_may_start(issue)
        && !held.contains(&issue.number)
        && !spoken_for.contains(&issue.number)
}

/// The steps whose stage the board has not reached, by number.
///
/// A card's place in a plan is `parent_issue_id` + `stage`, and until this
/// existed nothing on the starting path read either: `is_waiting` asked only
/// whether a row carried a block, so a plan was a thing the board displayed
/// rather than a thing it kept. `IssueCreate`'s own schema promised the
/// model otherwise, which is worse than silence — the model has no way to
/// check, and a lead that filed eight steps across three stages watched the
/// board start all of them straight away.
///
/// One grouping pass over the board the tick already holds, not a store
/// read per card. **Fails open**: a step whose siblings are not in `issues`
/// is not held, because a gate that guesses would strand work on a partial
/// read.
pub(crate) fn held_by_a_stage(issues: &[IssueRow]) -> BTreeSet<i64> {
    let mut plans: BTreeMap<&baybo_model::IssueId, Vec<&IssueRow>> = BTreeMap::new();
    for issue in issues {
        if let Some(parent) = issue.parent_issue_id.as_ref() {
            plans.entry(parent).or_default().push(issue);
        }
    }
    issues
        .iter()
        .filter(|issue| {
            issue.parent_issue_id.as_ref().is_some_and(|parent| {
                plans.get(parent).is_some_and(|siblings| {
                    !crate::stages::stage_is_open(siblings.iter().copied(), issue.stage)
                })
            })
        })
        .map(|issue| issue.number)
        .collect()
}

/// Whether automatic board actions may start this card; operators may override blocks.
pub(crate) fn board_may_start(issue: &IssueRow) -> bool {
    issue.blocked_reason.is_none()
}

/// The columns work is under way in. Backlog is parked and Done is over, so
/// neither is a column a card is woken in.
pub(crate) fn is_live_work(status: IssueStatus) -> bool {
    matches!(
        status,
        IssueStatus::Todo | IssueStatus::InProgress | IssueStatus::Review
    )
}

/// Whether this card is work the **board** may take up on its own authority.
///
/// [`crate::runs::accepts_runs`] plus the one thing a row cannot say about
/// itself: who put it in Backlog. A person parking a card there has exactly
/// the standing of a person setting a block — the answer is "not now", given
/// by somebody entitled to give it — so every rule that would not adjudicate
/// the block must not reopen the column either. `agent_opened` is the
/// board-wide authorship read [`awaiting_grooming`] is built on, and this is
/// the one place the rule is spelled.
///
/// Deliberately *not* [`board_may_start`], which answers the other half —
/// whether a card the board may take up is paused right now. The two are
/// asked together everywhere except [`ran_dry`], whose blocked cards are
/// filtered by [`drain_anchor`] instead.
fn board_may_take_up(issue: &IssueRow, agent_opened: &BTreeSet<i64>) -> bool {
    crate::runs::accepts_runs(issue)
        && (is_live_work(issue.status) || agent_opened.contains(&issue.number))
}

/// A waiting card somebody is on: the board can start it.
fn is_promotable(issue: &IssueRow, spoken_for: &BTreeSet<i64>, held: &BTreeSet<i64>) -> bool {
    is_waiting(issue, spoken_for, held) && issue.assignee.is_some()
}

/// A waiting card nobody is on: the board can only ask who should be.
///
/// Deliberately not `!is_promotable`, which would be true of every card on
/// the board — cancelled, blocked, in another column, already running — and
/// would send all of them to the lead as "unstaffed work".
fn needs_staffing(issue: &IssueRow, spoken_for: &BTreeSet<i64>, held: &BTreeSet<i64>) -> bool {
    is_waiting(issue, spoken_for, held) && issue.assignee.is_none()
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

/// The cards a rule keeps, in the order every column is read in.
///
/// The rules below differ in exactly one thing — which cards they keep —
/// and each used to spell the other three lines itself. Only the boilerplate
/// is shared: every question keeps its own predicate, because they are six
/// different questions with six different answers, and the point of hoisting
/// this is that each one is now the one line that says what it asks.
fn in_promotion_order(issues: &[IssueRow], keep: impl Fn(&IssueRow) -> bool) -> Vec<IssueRow> {
    let mut hits: Vec<IssueRow> = issues.iter().filter(|i| keep(i)).cloned().collect();
    hits.sort_by(promotion_order);
    hits
}

/// The cards to move into In Progress, best first, at most `slots` of them.
pub(crate) fn slate(issues: &[IssueRow], busy: &BTreeSet<i64>, slots: usize) -> Vec<IssueRow> {
    // Ahead of the filter, not after it: a full board answers zero every
    // pass, and a `truncate(0)` would pay for the clone and the sort first.
    if slots == 0 {
        return Vec::new();
    }
    let held = held_by_a_stage(issues);
    let mut ready = in_promotion_order(issues, |i| is_promotable(i, busy, &held));
    ready.truncate(slots);
    ready
}

/// The cards sitting in Todo with nobody on them, in the order the lead
/// should be asked about them.
pub(crate) fn awaiting_triage(issues: &[IssueRow], in_flight: &BTreeSet<i64>) -> Vec<IssueRow> {
    let held = held_by_a_stage(issues);
    in_promotion_order(issues, |i| needs_staffing(i, in_flight, &held))
}

/// Whether the lead may be asked about a live card it does not own.
fn takes_a_lead_question(
    issue: &IssueRow,
    in_flight: &BTreeSet<i64>,
    lead: &baybo_model::AgentProfileId,
) -> bool {
    crate::runs::accepts_runs(issue)
        && !in_flight.contains(&issue.number)
        && issue.assignee.as_ref() != Some(lead)
}

/// Blocked live cards for lead review, in promotion order.
pub(crate) fn blocked(
    issues: &[IssueRow],
    in_flight: &BTreeSet<i64>,
    lead: &baybo_model::AgentProfileId,
) -> Vec<IssueRow> {
    in_promotion_order(issues, |i| {
        !board_may_start(i) && takes_a_lead_question(i, in_flight, lead)
    })
}

/// The cards sitting in Review with nothing running on them, in the order
/// the lead should be asked about them.
pub(crate) fn awaiting_review(
    issues: &[IssueRow],
    in_flight: &BTreeSet<i64>,
    lead: &baybo_model::AgentProfileId,
) -> Vec<IssueRow> {
    in_promotion_order(issues, |i| {
        i.status == IssueStatus::Review
            && board_may_start(i)
            && takes_a_lead_question(i, in_flight, lead)
    })
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
    in_flight: &BTreeSet<i64>,
    lead: &baybo_model::AgentProfileId,
) -> Vec<IssueRow> {
    in_promotion_order(issues, |i| {
        i.status == IssueStatus::InProgress
            && board_may_start(i)
            && takes_a_lead_question(i, in_flight, lead)
    })
}

/// Whether the latest block was agent-authored.
pub(crate) fn block_is_an_agents_question(events: &[baybo_store::project::IssueEventRow]) -> bool {
    newest_block(events)
        .is_some_and(|e| matches!(e.actor, baybo_store::project::IssueActor::Agent(_)))
}

/// Whether the stop standing on this card is a **person's**.
///
/// The same shape as [`block_is_an_agents_question`], and for the same
/// reason: the row says only that a card is cancelled, and who decided it
/// lives on the timeline. A stop the board did not set is not the board's to
/// take back.
///
/// Reads the newest entry of *either* direction, because a card called off
/// and reopened carries both and only the last of them is still standing.
pub(crate) fn cancel_is_a_persons_stop(events: &[baybo_store::project::IssueEventRow]) -> bool {
    events
        .iter()
        .rev()
        .find(|e| {
            matches!(
                e.body,
                baybo_store::project::IssueEventBody::Cancelled
                    | baybo_store::project::IssueEventBody::Uncancelled
            )
        })
        .is_some_and(|e| {
            matches!(e.body, baybo_store::project::IssueEventBody::Cancelled)
                && !matches!(e.actor, baybo_store::project::IssueActor::Agent(_))
        })
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
    in_promotion_order(issues, |i| {
        i.status == IssueStatus::Backlog
            && board_may_take_up(i, agent_opened)
            && board_may_start(i)
            && takes_a_lead_question(i, in_flight, lead)
    })
}

/// Whether the board has run dry: nothing executing, nothing this pass
/// promoted, and work the board may take up still sitting on it
/// ([`board_may_take_up`] — an operator's parked Backlog is not it).
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
pub(crate) fn ran_dry(
    issues: &[IssueRow],
    working: &BTreeSet<i64>,
    agent_opened: &BTreeSet<i64>,
    promoted: usize,
) -> bool {
    promoted == 0 && working.is_empty() && issues.iter().any(|i| board_may_take_up(i, agent_opened))
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
/// A **cancelled** run is on the look side of that asymmetry and not the
/// work side, which is what [`newest_run_was_cancelled`] does per card and
/// this does for the board: somebody stopping a run read the board to do it,
/// and asking the lead what to do next would countermand them one tick after
/// they decided.
///
/// It also closes the obvious spin for free: the drain question is itself a
/// wake, so being asked is being looked at, and answering it is not work.
///
/// A board nothing has ever run on has never been looked at, and that is the
/// truth rather than an edge case: cards were filed and nothing came.
/// `rules_changed_at` is the third mark, and the one no card can carry.
/// "Escalate this to somebody who may merge" is a *complete* answer while
/// the board's agents may not merge, and the board being told they now may
/// is the only thing that ever happens next — it touches no card. Asked
/// here it costs **one** run: answering is a look, which moves `looked_at`
/// past the stamp, so a burst of saves is one question and not one per save
/// per card. Asked per card it cost nine runs across five cards, each of
/// them reading one card to decide something about all of them.
pub(crate) fn nothing_has_happened_since_the_lead_looked(
    marks: &DrainMarks,
    rules_changed_at: chrono::DateTime<chrono::Utc>,
) -> bool {
    match (marks.looked_at, marks.worked_at) {
        (None, _) => false,
        (Some(looked), worked) => {
            rules_changed_at <= looked && worked.is_none_or(|worked| worked <= looked)
        }
    }
}

/// The card a board-scale question is filed against.
///
/// A run is a row on a card, so a question that has no card of its own still
/// needs one to live on. The pick is the order every column is already read
/// in, over the cards the board could act on — filing it against a blocked
/// card would put a run on work an operator paused, and `parked_by_a_block`
/// would hold that run rather than deliver it. A card the operator parked in
/// Backlog is the same paused work under a different name, which is what
/// [`board_may_take_up`] keeps out: this question reads the whole board, so
/// anchoring it on such a card hands the lead the one card it may not move.
///
/// Deliberately **not** [`takes_a_lead_question`]: a card whose assignee is
/// the lead is excluded from every question *about a card*, because the
/// lead's own card has no other party. This question is not about the card,
/// and a board whose only live card is the lead's is precisely a board with
/// nothing else to anchor to.
pub(crate) fn drain_anchor(
    issues: &[IssueRow],
    in_flight: &BTreeSet<i64>,
    agent_opened: &BTreeSet<i64>,
) -> Option<IssueRow> {
    issues
        .iter()
        .filter(|i| {
            board_may_take_up(i, agent_opened)
                && board_may_start(i)
                && !in_flight.contains(&i.number)
        })
        .min_by(|a, b| promotion_order(a, b))
        .cloned()
}

/// Whether the newest run on this card was **cancelled**: a decision — a
/// human's stop, or the board calling a row off — and waking the lead to get
/// the work going again would countermand it within one tick. The stop
/// stands until somebody acts on the card, which makes it a new question.
///
/// Read by every lead question but `Blocked`, whose own preparation settles
/// a run `Cancelled` before asking; and by the drain question through
/// `DrainMarks`, where a cancelled run does not count as the board working.
pub(crate) fn newest_run_was_cancelled(runs: &[IssueRunRow]) -> bool {
    runs.iter()
        .max_by_key(|run| run.created_at)
        .is_some_and(|run| run.status == RunStatus::Cancelled)
}

/// When this card last changed in a way the lead has not seen: its own
/// `updated_at`, or the settle of its newest *work* run, whichever is
/// later. Coordination runs are excluded on both sides of the question —
/// the lead looking at a card is not the card changing.
///
/// Deliberately reads **only the card**. A board-wide rule change is also
/// something the lead has not seen, and it used to be folded in here — but
/// a fact about the board, answered once per card, is asked once per card,
/// and the lead then reads one card at a time to decide something about all
/// of them. It belongs to the board question, where
/// [`nothing_has_happened_since_the_lead_looked`] spends one run on it.
fn last_activity(issue: &IssueRow, runs: &[IssueRunRow]) -> chrono::DateTime<chrono::Utc> {
    let reopened = issue.updated_at;
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
pub(crate) fn already_asked(issue: &IssueRow, runs: &[IssueRunRow], question: RunTrigger) -> bool {
    let activity = last_activity(issue, runs);
    let asks = runs.iter().filter(|run| run.trigger == question);
    let delivered = asks.clone().any(|run| {
        run.created_at >= activity && !(run.status == RunStatus::Failed && !run.was_claimed())
    });
    // Both halves count against the card and nothing else. When the rule
    // stamp reached in here as well, the pair was unbounded: `delivered` is
    // false after every save — the stamp has just moved past every ask —
    // leaving the cap as the only thing counting, and counting it from a
    // mark the save moved counts to zero every time. A short burst of saves
    // of one board's settings bought nine lead runs across five cards that
    // way, none of which changed anything.
    delivered
        || asks
            .filter(|run| run.created_at >= issue.updated_at)
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

    /// A board with no plan on it, so no step is waiting for a stage.
    fn no_stage_holds() -> BTreeSet<i64> {
        BTreeSet::new()
    }

    /// The cards the board filed itself, by number — what
    /// `ProjectStore::agent_opened_issues` answers.
    fn the_boards_own(numbers: impl IntoIterator<Item = i64>) -> BTreeSet<i64> {
        numbers.into_iter().collect()
    }

    /// A board where every parked card is the operator's.
    fn all_the_operators() -> BTreeSet<i64> {
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
            is_promotable(&ready(1), &idle(), &no_stage_holds()),
            "the control: a staffed card in Todo with nothing against it is startable"
        );

        for (why, break_it) in gates() {
            let mut staffed = ready(1);
            break_it(&mut staffed);
            let mut unstaffed = staffed.clone();
            unstaffed.assignee = None;

            assert!(
                !is_promotable(&staffed, &idle(), &no_stage_holds()),
                "promoted anyway: {why}"
            );
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
            !is_promotable(&unstaffed, &idle(), &no_stage_holds()),
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
            !already_asked(&card, &[], RunTrigger::Triage),
            "a card nobody has looked at is a fresh question"
        );

        let asked = triage_run(&card, card.updated_at + Duration::seconds(1));
        assert!(
            already_asked(&card, std::slice::from_ref(&asked), RunTrigger::Triage),
            "a lead that read the card and left it alone must not be asked again"
        );

        card.updated_at = asked.created_at + Duration::seconds(1);
        assert!(
            !already_asked(&card, std::slice::from_ref(&asked), RunTrigger::Triage),
            "but editing the card makes it a question the lead has not answered"
        );

        let other = IssueRunRow {
            trigger: RunTrigger::Comment,
            ..triage_run(&card, card.updated_at + Duration::seconds(1))
        };
        assert!(
            !already_asked(&card, &[asked, other], RunTrigger::Triage),
            "and a run that was not a triage is not an answer to one"
        );
    }

    #[test]
    fn a_board_has_run_dry_only_with_work_left_on_it_and_nothing_moving() {
        let live = issue(1, IssueStatus::Backlog);
        let boards_own = the_boards_own([1]);
        assert!(
            ran_dry(std::slice::from_ref(&live), &idle(), &boards_own, 0),
            "a card nothing will ever start, and nothing running to start it"
        );
        assert!(
            !ran_dry(std::slice::from_ref(&live), &busy([2]), &boards_own, 0),
            "a board with a run on it is working, whatever that run is on"
        );
        assert!(
            !ran_dry(std::slice::from_ref(&live), &idle(), &boards_own, 1),
            "and a pass that just promoted something has not run dry — the \
             slate it filled is not in flight yet"
        );
        assert!(
            !ran_dry(
                std::slice::from_ref(&live),
                &idle(),
                &all_the_operators(),
                0
            ),
            "but the same card parked by a person is not work the board is \
             stuck on — it is work somebody said not yet to, and a board with \
             nothing else on it is quiet"
        );

        let finished = issue(1, IssueStatus::Done);
        let mut cancelled = issue(2, IssueStatus::Todo);
        cancelled.cancelled_at = Some(Utc::now());
        assert!(
            !ran_dry(&[finished, cancelled], &idle(), &the_boards_own([1, 2]), 0),
            "a board the work is over on is quiet, not stuck"
        );
    }

    /// A rule the board schedules by is the one thing the lead has not seen
    /// that no card carries: "escalate this to somebody who may merge" is a
    /// complete answer while the board's agents may not, and the operator
    /// turning it on touches nothing.
    ///
    /// It lives here rather than on each card because it is one fact about
    /// the whole board. Asked per card it cost nine runs across five cards,
    /// each reading one card to decide something about all of them — and, because it moved the mark the ask cap counted from, it
    /// minted the quota to keep doing so. Asked here it costs one run, and
    /// answering it is a look, so the save after it buys nothing.
    #[test]
    fn a_rule_the_board_changed_makes_the_board_a_question_once() {
        let looked = Utc::now();
        let quiet = DrainMarks {
            looked_at: Some(looked),
            worked_at: Some(looked - Duration::seconds(1)),
        };
        assert!(
            nothing_has_happened_since_the_lead_looked(&quiet, looked - Duration::seconds(1)),
            "the control: a rule changed before the lead looked is a rule it looked under"
        );
        assert!(
            !nothing_has_happened_since_the_lead_looked(&quiet, looked + Duration::seconds(1)),
            "a rule changed since is a board the lead has not read under it"
        );

        let answered = DrainMarks {
            looked_at: Some(looked + Duration::seconds(2)),
            ..quiet
        };
        assert!(
            nothing_has_happened_since_the_lead_looked(&answered, looked + Duration::seconds(1)),
            "and answering it is a look, so the board is quiet again — a burst \
             of saves is one question, not one per save"
        );
    }

    #[test]
    fn the_board_is_told_it_ran_dry_only_when_work_outlived_the_leads_last_look() {
        let looked = Utc::now();
        assert!(
            !nothing_has_happened_since_the_lead_looked(
                &DrainMarks::default(),
                rules_never_changed()
            ),
            "a board nothing ever ran on has never been looked at; cards were \
             filed and nothing came for them"
        );
        assert!(
            nothing_has_happened_since_the_lead_looked(
                &DrainMarks {
                    looked_at: Some(looked),
                    worked_at: None,
                },
                rules_never_changed(),
            ),
            "a lead woken on a board nothing has run on has been shown all of it"
        );
        assert!(
            nothing_has_happened_since_the_lead_looked(
                &DrainMarks {
                    looked_at: Some(looked),
                    worked_at: Some(looked - Duration::seconds(1)),
                },
                rules_never_changed(),
            ),
            "and work that settled before that look is work the look covered"
        );
        assert!(
            !nothing_has_happened_since_the_lead_looked(
                &DrainMarks {
                    looked_at: Some(looked),
                    worked_at: Some(looked + Duration::seconds(1)),
                },
                rules_never_changed(),
            ),
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
            drain_anchor(
                &[ordinary.clone(), urgent.clone()],
                &idle(),
                &the_boards_own([1, 2])
            )
            .map(|i| i.number),
            Some(2),
            "the same order every column is read in"
        );
        assert!(
            drain_anchor(
                &[ordinary.clone(), urgent.clone()],
                &idle(),
                &all_the_operators()
            )
            .is_none(),
            "the same two cards parked by a person are the operator's answer, \
             not the board's to anchor a run on"
        );

        let mut the_leads_own = issue(3, IssueStatus::Review);
        the_leads_own.assignee = Some(lead);
        assert_eq!(
            drain_anchor(
                std::slice::from_ref(&the_leads_own),
                &idle(),
                &all_the_operators()
            )
            .map(|i| i.number),
            Some(3),
            "a board whose only live card is the lead's still has a board to \
             ask about — the question is not about the card, and authorship \
             only ever speaks for Backlog"
        );

        let mut paused = ordinary;
        paused.blocked_reason = Some("waiting on the operator".into());
        let mut also_paused = urgent;
        also_paused.blocked_reason = Some("waiting on the operator".into());
        assert!(
            drain_anchor(&[paused, also_paused], &idle(), &the_boards_own([1, 2])).is_none(),
            "every live card paused is nothing the board may act on, and each \
             block has already been put to the lead once"
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
            !already_asked(&card, &[asked.clone(), worked], RunTrigger::Triage),
            "a work run settling after the lead looked is news the lead has not seen"
        );

        let looked_again = IssueRunRow {
            trigger: RunTrigger::Review,
            settled_at: Some(asked.created_at + Duration::seconds(10)),
            ..triage_run(&card, card.updated_at)
        };
        assert!(
            already_asked(&card, &[asked, looked_again], RunTrigger::Triage),
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
        let opened = the_boards_own([1]);

        assert_eq!(
            numbers(&awaiting_grooming(&board, &busy([]), &opened, &lead)),
            vec![1],
            "the control: this card is the lead's to groom"
        );
        assert!(
            !is_promotable(&parked, &idle(), &no_stage_holds()),
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

        // The two board-scale rules, which this test used not to name — and
        // that omission is the whole of how the board came to start the
        // operator's parked cards through the door underneath grooming.
        assert!(
            ran_dry(&board, &idle(), &opened, 0)
                && drain_anchor(&board, &idle(), &opened).is_some(),
            "the board's own parked card is still work it is stuck on"
        );
        assert!(
            !ran_dry(&board, &idle(), &all_the_operators(), 0)
                && drain_anchor(&board, &idle(), &all_the_operators()).is_none(),
            "and the operator's is not — grooming's rule is the same rule here"
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
            !already_asked(&card, std::slice::from_ref(&failed_ask), RunTrigger::Triage),
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
            already_asked(&card, &[failed_ask, second_failed], RunTrigger::Triage),
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
            already_asked(&card, &runs, RunTrigger::Triage),
            "the machinery's own echo cannot re-raise the question a third time"
        );

        let mut edited = card.clone();
        edited.updated_at = at + Duration::seconds(1);
        assert!(
            !already_asked(&edited, &runs, RunTrigger::Triage),
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
            client_msg_id: None,
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
