//! The brief a run is handed: the card, plus what has been said on it that
//! the assignee has not already read.

use std::sync::Arc;

use baybo_model::{AgentProfileId, MediaBlock};
use baybo_store::project::{
    IssueAttachment, IssueEventBody, IssueEventRow, IssueRow, IssueRunRow, ProjectStore, RunTrigger,
};
use baybo_store::{AgentProfileStore, BlobStore};

use crate::attachments::describe;

use crate::actors;
use crate::runs::{ever_ran, session_run_before};

/// How far back the comments in a run's brief reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BriefWindow {
    /// This run opens an empty transcript: nobody has run the card, or the
    /// runs before it were this agent's but never got as far as executing,
    /// or they were somebody else's. It gets the whole conversation.
    WholeCard,
    /// A previous run of this agent's put its turn in the transcript this
    /// one opens. Everything up to the moment that run was enqueued has
    /// been read.
    SinceItsLastRun(chrono::DateTime<chrono::Utc>),
}

pub(crate) struct Said {
    window: BriefWindow,
    /// The checkout this run is handed was last worked in by another agent,
    /// so it may hold that agent's uncommitted changes. Tracked apart from
    /// [`BriefWindow`]: a card handed *back* to its first agent is bounded
    /// by that agent's own last run and still arrives holding whatever the
    /// agent in between left behind.
    inherited_worktree: bool,
    comments: Vec<Comment>,
}

/// One thing said on the card, and who said it.
///
/// The name is not decoration: an unattributed block reads as one voice, so
/// an operator's instruction, a teammate's aside and the agent's own note
/// from its last run all arrive with the same authority. Named, they can be
/// weighed — and an agent asked something can tell who is waiting on the
/// answer. [`crate::actors`] owns the spelling of the name.
pub(crate) struct Comment {
    by: String,
    text: String,
    attachments: Vec<IssueAttachment>,
}

const BLOCKED_ON: &str = "\n\nThis card is blocked. The reason on it reads:\n";

const SAID_SINCE_LAST_RUN: &str = "\n\nSaid since your last run:\n";

const SAID_ON_THE_CARD: &str = "\n\nSaid on the card so far:\n";

const INHERITED_WORKTREE: &str = r#"

You are picking this card up after another agent worked it. The checkout you
are given is the one it worked in, so any uncommitted change in there is its
work and not yours — read what is already in the tree before you add to it."#;

const COMMENT_BUDGET: usize = 16_000;

const FILES_ON_THE_CARD: &str = "\n\nFiles on this card:\n";

const SOME_FILES_NOT_SHOWN: &str = r#"

Not every file above could be shown to you — the ones listed but not
attached are on the card, and the operator can point you at any of them."#;

const FILES_ALREADY_SEEN: &str = r#"

The card's own files are unchanged since your last run, so they are not
attached again — they are already earlier in this conversation."#;

/// How many files a brief may carry as real content.
///
/// A separate rule from [`COMMENT_BUDGET`], because the two measure
/// different things: that one counts the BYTES of rendered text, and a
/// picture contributes almost none of those while costing thousands of
/// real tokens (a provider bills an image per tile of its pixel grid). A
/// card with forty screenshots is under the text budget and over every
/// context window. The neighbouring number is the agent loop's own
/// `MAX_LLM_IMAGES_PER_ITERATION`.
const MAX_BRIEF_MEDIA: usize = 8;

const EARLIER_COMMENTS_TRIMMED: &str =
    "(earlier comments are not repeated here — the card itself has all of them)";

const TRIAGE_PREAMBLE: &str = r#"You are this board's lead, woken because this card sits in Todo with nobody on it. Staff it — assign a teammate with the board's tools, or take it yourself — or deliberately leave it unstaffed. Do not do the card's own work in this run."#;

const REVIEW_PREAMBLE: &str = r#"You are this board's lead, woken because this card sits in Review with nothing running on it. Arrange the review: hand it to a reviewer (reassign and say what to check), or check it yourself, and once the verdict is in move the card onward — Done, or back for fixes with a comment saying what to fix."#;

const STALLED_PREAMBLE: &str = r#"You are this board's lead, woken because this card sits in In Progress with no run working it and nothing queued. Decide what happens to it: wake its assignee with a comment asking for a status or the next step, restaff it, move it back to Todo, or block it with a reason. Do not do the card's own work in this run."#;

const BLOCKED_PREAMBLE: &str = r#"You are this board's lead, woken because this card is blocked and its reason is a question, not a status. Read the reason and decide: answer it and unblock the card, hand it back with a comment saying what to do instead, escalate it to the operator in a comment, or cancel the card. Do not do the card's own work in this run."#;

/// The framing a coordination run opens with. An ordinary run is briefed by
/// the card alone; the lead's wakes carry *why* it was woken, because the
/// card itself does not say — the brief has no status line, and "staff
/// this", "review this" and "this stalled" are three different asks over
/// the same card.
fn coordination_preamble(trigger: RunTrigger) -> Option<&'static str> {
    match trigger {
        RunTrigger::Triage => Some(TRIAGE_PREAMBLE),
        RunTrigger::Review => Some(REVIEW_PREAMBLE),
        RunTrigger::Stalled => Some(STALLED_PREAMBLE),
        RunTrigger::Blocked => Some(BLOCKED_PREAMBLE),
        RunTrigger::Started
        | RunTrigger::Assigned
        | RunTrigger::Retry
        | RunTrigger::Comment
        | RunTrigger::Promoted
        | RunTrigger::StageBarrier => None,
    }
}

/// One comment as the brief reads it. Newlines inside it are indented, so
/// a line of somebody's prose can never be read as the next speaker — which
/// is also why a file list is appended as one flat clause rather than as
/// its own lines.
fn comment_line(comment: &Comment) -> String {
    let mut said = comment.text.clone();
    if !comment.attachments.is_empty() {
        let named = comment
            .attachments
            .iter()
            .map(describe)
            .collect::<Vec<_>>()
            .join(", ");
        if said.is_empty() {
            said = format!("[attached {named}]");
        } else {
            said.push_str(&format!(" [attached {named}]"));
        }
    }
    format!("- {}: {}\n", comment.by, said.replace('\n', "\n  "))
}

/// How many of the newest comments fit the prose budget.
///
/// Extracted so the *files* can be selected from the same set the prose is:
/// a comment trimmed out of the block is a comment whose attachment nothing
/// names, and delivering that attachment anyway put a picture in front of
/// the model with no sentence to say where it came from.
fn kept_comments(comments: &[Comment]) -> usize {
    let mut kept = 0usize;
    let mut spent = 0usize;
    for line in comments
        .iter()
        .map(comment_line)
        .collect::<Vec<_>>()
        .iter()
        .rev()
    {
        if kept > 0 && spent + line.len() > COMMENT_BUDGET {
            break;
        }
        spent += line.len();
        kept += 1;
    }
    kept
}

fn comment_block(comments: &[Comment]) -> String {
    let kept = kept_comments(comments);
    let mut block = String::new();
    if kept < comments.len() {
        block.push_str(&format!("- {EARLIER_COMMENTS_TRIMMED}\n"));
    }
    for comment in &comments[comments.len() - kept..] {
        block.push_str(&comment_line(comment));
    }
    block
}

pub(crate) fn issue_brief(issue: &IssueRow, said: &Said, trigger: RunTrigger) -> String {
    let mut brief = String::new();
    if let Some(preamble) = coordination_preamble(trigger) {
        brief.push_str(preamble);
        brief.push_str("\n\n");
    }
    if issue.description.trim().is_empty() {
        brief.push_str(&issue.title);
    } else {
        brief.push_str(&format!("{}\n\n{}", issue.title, issue.description));
    };
    if let Some(reason) = issue.blocked_reason.as_deref() {
        brief.push_str(BLOCKED_ON);
        // Indentation distinguishes quoted prose from the brief's voice.
        brief.push_str(&format!("  {}\n", reason.replace('\n', "\n  ")));
    }
    if !issue.attachments.is_empty() {
        brief.push_str(FILES_ON_THE_CARD);
        for attachment in &issue.attachments {
            brief.push_str(&format!("- {}\n", describe(attachment)));
        }
    }
    if said.inherited_worktree {
        brief.push_str(INHERITED_WORKTREE);
    }
    if !said.comments.is_empty() {
        brief.push_str(match said.window {
            BriefWindow::SinceItsLastRun(_) => SAID_SINCE_LAST_RUN,
            BriefWindow::WholeCard => SAID_ON_THE_CARD,
        });
        brief.push_str(&comment_block(&said.comments));
    }
    // Two different reasons a named file is not attached below, and they must
    // not be reported as one: files the model already has from an earlier run
    // of this same conversation, and files the budget could not fit.
    let already_seen = if card_files_are_new(issue, said) {
        0
    } else {
        issue.attachments.len()
    };
    if already_seen > 0 {
        brief.push_str(FILES_ALREADY_SEEN);
    }
    if delivered(issue, said).len() + already_seen < named(issue, said).len() {
        brief.push_str(SOME_FILES_NOT_SHOWN);
    }
    brief
}

/// The comments this brief actually prints, newest-first budget applied.
fn spoken(said: &Said) -> &[Comment] {
    let kept = kept_comments(&said.comments);
    &said.comments[said.comments.len() - kept..]
}

/// Every file this brief names, card first and then the conversation in
/// reading order. Only the comments that survived the prose budget count:
/// a file nothing names is a file this brief did not mention.
fn named<'a>(issue: &'a IssueRow, said: &'a Said) -> Vec<&'a IssueAttachment> {
    issue
        .attachments
        .iter()
        .chain(spoken(said).iter().flat_map(|c| c.attachments.iter()))
        .collect()
}

/// Whether the card's own files are new to the transcript this run opens.
///
/// A run continues a session, so everything an earlier run of the same agent
/// was shown is **still in front of the model**. Re-sending the card's
/// screenshots on every run does not remind it of anything; it pays the
/// image price again, per run, in one conversation.
///
/// The signal is the row's own `updated_at` against the window's boundary,
/// because nothing records which files a previous run delivered. It errs
/// towards sending: any edit to the card re-sends its files, so a mockup
/// added between two runs always arrives, and only a genuinely unchanged
/// card is skipped.
fn card_files_are_new(issue: &IssueRow, said: &Said) -> bool {
    match said.window {
        BriefWindow::WholeCard => true,
        BriefWindow::SinceItsLastRun(since) => issue.updated_at > since,
    }
}

/// The files this brief carries as real content, out of everything it
/// names.
///
/// The card's own files come first and in full: they are the specification,
/// and a run that cannot see the mockup it was asked to build is not
/// cheaper, it is wrong. What is left of the budget goes to the
/// conversation **newest-first**, because the useful screenshot on a busy
/// card is the one somebody just posted, not the one from nine comments
/// ago — then reading order is restored, so the agent sees them in the
/// order they were said.
///
/// Nothing is ever *hidden* by this: `issue_brief` names every file either
/// way, and says plainly when it could not show them all.
fn delivered<'a>(issue: &'a IssueRow, said: &'a Said) -> Vec<&'a IssueAttachment> {
    let from_card: Vec<&IssueAttachment> = if card_files_are_new(issue, said) {
        issue.attachments.iter().take(MAX_BRIEF_MEDIA).collect()
    } else {
        Vec::new()
    };
    let room_left = MAX_BRIEF_MEDIA.saturating_sub(from_card.len());
    let said_files: Vec<&IssueAttachment> = spoken(said)
        .iter()
        .flat_map(|c| c.attachments.iter())
        .collect();
    let kept_from = said_files.len().saturating_sub(room_left);
    from_card
        .into_iter()
        .chain(said_files[kept_from..].iter().copied())
        .collect()
}

/// The files a brief carries as real content, each priced from its own
/// bytes.
///
/// Returned apart from the prose rather than as one `Vec` whose first
/// element happens to be text: the prompt framing wraps the prose and only
/// the prose, and a shape that has to *find* the text block is a shape that
/// will one day frame a picture.
pub(crate) async fn issue_brief_media(
    blobs: &Arc<dyn BlobStore>,
    issue: &IssueRow,
    said: &Said,
) -> Vec<MediaBlock> {
    let mut blocks = Vec::new();
    for attachment in delivered(issue, said) {
        blocks.push(
            baybo_tools::blob_media::probed_block(
                blobs.as_ref(),
                attachment.blob_id.clone(),
                attachment.mime_type.clone(),
                Some(crate::attachments::name_of(attachment)),
            )
            .await,
        );
    }
    blocks
}

fn brief_window(run: &IssueRunRow, runs: &[IssueRunRow]) -> BriefWindow {
    match session_run_before(run, runs) {
        Some(previous) => BriefWindow::SinceItsLastRun(previous.created_at),
        None => BriefWindow::WholeCard,
    }
}

fn inherits_a_worktree(run: &IssueRunRow, runs: &[IssueRunRow]) -> bool {
    runs.iter()
        // Coordination runs do not count as somebody having *worked* the
        // tree: the lead is briefed not to touch the card's own work, and
        // counting its look would tell the assignee that the uncommitted
        // changes in the checkout — its own WIP — belong to another agent.
        .filter(|candidate| {
            candidate.id != run.id && ever_ran(candidate) && !candidate.trigger.is_coordination()
        })
        .max_by_key(|candidate| candidate.attempt)
        .is_some_and(|last| last.agent_id != run.agent_id)
}

pub(crate) async fn comments_for_brief(
    store: &Arc<dyn ProjectStore>,
    agents: &Arc<dyn AgentProfileStore>,
    run: &IssueRunRow,
) -> Said {
    let (window, inherited_worktree) = match store.list_runs(&run.issue_id).await {
        Ok(runs) => (brief_window(run, &runs), inherits_a_worktree(run, &runs)),
        Err(e) => {
            tracing::warn!(run = %run.id, error = %e, "could not read prior runs for the brief");
            return Said {
                window: BriefWindow::WholeCard,
                inherited_worktree: false,
                comments: Vec::new(),
            };
        }
    };
    let events = match window {
        BriefWindow::SinceItsLastRun(since) => store.events_since(&run.issue_id, since).await,
        BriefWindow::WholeCard => store.list_events(&run.issue_id).await,
    };
    let comments = match events {
        Ok(events) => attributed(agents, run, events).await,
        Err(e) => {
            tracing::warn!(run = %run.id, error = %e, "could not read comments for the brief");
            Vec::new()
        }
    };
    Said {
        window,
        inherited_worktree,
        comments,
    }
}

/// The comments among `events`, each under the name the board knows its
/// author by.
async fn attributed(
    agents: &Arc<dyn AgentProfileStore>,
    run: &IssueRunRow,
    events: Vec<IssueEventRow>,
) -> Vec<Comment> {
    let spoken: Vec<IssueEventRow> = events
        .into_iter()
        .filter(|event| matches!(event.body, IssueEventBody::Comment { .. }))
        .collect();
    let mut ids: Vec<AgentProfileId> = Vec::new();
    for id in spoken
        .iter()
        .filter_map(|event| actors::named_agent(&event.actor))
    {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    let known = actors::profiles(agents, &run.project_id, ids).await;
    spoken
        .into_iter()
        .filter_map(|event| match event.body {
            IssueEventBody::Comment { text, attachments } => Some(Comment {
                by: actors::label(&event.actor, &known),
                text,
                attachments,
            }),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{AgentProfileId, IssueId, IssueRunId, ProjectId, SessionId};
    use baybo_store::project::{IssuePriority, IssueStatus, RunStatus, RunTrigger};
    use chrono::{Duration, Utc};

    fn card() -> IssueRow {
        let now = Utc::now();
        IssueRow {
            id: IssueId::generate(),
            project_id: ProjectId::generate(),
            number: 7,
            title: "wire the importer".to_owned(),
            description: "it should skip rows with no id".to_owned(),
            attachments: Vec::new(),
            status: IssueStatus::InProgress,
            priority: IssuePriority::None,
            assignee: None,
            position: 0,
            blocked_reason: None,
            branch: None,
            parent_issue_id: None,
            stage: 0,
            source_key: None,
            cancelled_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    fn said(by: &str, text: &str) -> Comment {
        Comment {
            by: by.to_owned(),
            text: text.to_owned(),
            attachments: Vec::new(),
        }
    }

    fn file(name: &str, mime: &str) -> IssueAttachment {
        IssueAttachment {
            blob_id: format!("sha256:{name}.token"),
            mime_type: mime.to_owned(),
            size: 1_024,
            filename: Some(name.to_owned()),
        }
    }

    fn run(issue: &IssueRow, agent: &AgentProfileId, attempt: i64) -> IssueRunRow {
        IssueRunRow {
            id: IssueRunId::generate(),
            issue_id: issue.id.clone(),
            project_id: issue.project_id.clone(),
            number: issue.number,
            agent_id: agent.clone(),
            session_id: None,
            trigger: RunTrigger::Assigned,
            status: RunStatus::Queued,
            attempt,
            resumes: 0,
            error: None,
            created_at: issue.created_at + Duration::minutes(attempt),
            started_at: None,
            settled_at: None,
        }
    }

    fn ran(issue: &IssueRow, agent: &AgentProfileId, attempt: i64) -> IssueRunRow {
        IssueRunRow {
            session_id: Some(SessionId::from(format!(
                "sess-{}-{attempt}",
                agent.as_str()
            ))),
            status: RunStatus::Done,
            settled_at: Some(issue.created_at + Duration::minutes(attempt) + Duration::seconds(30)),
            ..run(issue, agent, attempt)
        }
    }

    #[test]
    fn a_cards_first_run_reads_the_whole_card_as_its_own() {
        let issue = card();
        let dev_1 = AgentProfileId::parse("dev-1").expect("agent id");
        let first = run(&issue, &dev_1, 1);
        let runs = [first.clone()];

        assert_eq!(brief_window(&first, &runs), BriefWindow::WholeCard);
        assert!(!inherits_a_worktree(&first, &runs));
        let brief = issue_brief(
            &issue,
            &Said {
                window: BriefWindow::WholeCard,
                inherited_worktree: false,
                comments: vec![said(actors::OPERATOR, "start with the CSV path")],
            },
            RunTrigger::Started,
        );
        assert!(
            !brief.contains(INHERITED_WORKTREE),
            "there is nobody to pick it up after:\n{brief}"
        );
        assert!(brief.contains(SAID_ON_THE_CARD.trim()));
    }

    #[test]
    fn a_second_run_by_the_same_agent_is_bounded_by_its_own_previous_one() {
        let issue = card();
        let dev_1 = AgentProfileId::parse("dev-1").expect("agent id");
        let first = ran(&issue, &dev_1, 1);
        let second = run(&issue, &dev_1, 2);
        let runs = [first.clone(), second.clone()];

        assert_eq!(
            brief_window(&second, &runs),
            BriefWindow::SinceItsLastRun(first.created_at)
        );
        assert!(
            !inherits_a_worktree(&second, &runs),
            "the checkout holds nobody's work but its own"
        );
        let brief = issue_brief(
            &issue,
            &Said {
                window: BriefWindow::SinceItsLastRun(first.created_at),
                inherited_worktree: false,
                comments: vec![said(actors::OPERATOR, "also handle the empty case")],
            },
            RunTrigger::Started,
        );
        assert!(
            brief.contains(SAID_SINCE_LAST_RUN.trim()),
            "an agent that has read the rest is told only what is new:\n{brief}"
        );
    }

    #[test]
    fn a_same_agent_run_that_never_opened_a_session_does_not_bound_the_window() {
        let issue = card();
        let dev_1 = AgentProfileId::parse("dev-1").expect("agent id");
        let never_started = IssueRunRow {
            status: RunStatus::Cancelled,
            ..run(&issue, &dev_1, 1)
        };
        let second = run(&issue, &dev_1, 2);
        let runs = [never_started, second.clone()];

        assert_eq!(
            brief_window(&second, &runs),
            BriefWindow::WholeCard,
            "the run this one would be bounded by never opened the transcript it is bounded against"
        );
    }

    #[test]
    fn a_card_picked_up_by_another_agent_gets_the_whole_conversation() {
        let issue = card();
        let dev_1 = AgentProfileId::parse("dev-1").expect("agent id");
        let dev_2 = AgentProfileId::parse("dev-2").expect("agent id");
        let theirs = ran(&issue, &dev_1, 1);
        let handover = run(&issue, &dev_2, 2);
        let runs = [theirs.clone(), handover.clone()];

        assert_eq!(
            brief_window(&handover, &runs),
            BriefWindow::WholeCard,
            "dev-2's brief must not be bounded by a run of dev-1's"
        );
        assert!(
            inherits_a_worktree(&handover, &runs),
            "and the checkout it is given is the one dev-1 worked in"
        );

        let brief = issue_brief(
            &issue,
            &Said {
                window: BriefWindow::WholeCard,
                inherited_worktree: true,
                comments: vec![
                    said(actors::OPERATOR, "start with the CSV path"),
                    said("@dev-1", "also handle the empty case"),
                ],
            },
            RunTrigger::Started,
        );
        assert!(
            brief.contains(SAID_ON_THE_CARD.trim()),
            "and must not be told they are the comments since its own last run:\n{brief}"
        );
        assert!(!brief.contains(SAID_SINCE_LAST_RUN.trim()), "{brief}");
        assert!(
            brief.contains("start with the CSV path") && brief.contains("also handle the empty"),
            "everything said on the card is new to it:\n{brief}"
        );
        assert!(
            brief.contains(INHERITED_WORKTREE),
            "and it arrives holding somebody else's uncommitted work:\n{brief}"
        );
    }

    #[test]
    fn an_agent_handed_the_card_back_is_bounded_by_its_own_last_run_and_still_warned() {
        let issue = card();
        let dev_1 = AgentProfileId::parse("dev-1").expect("agent id");
        let dev_2 = AgentProfileId::parse("dev-2").expect("agent id");
        let first = ran(&issue, &dev_1, 1);
        let handover = ran(&issue, &dev_2, 2);
        let back = run(&issue, &dev_1, 3);
        let runs = [first.clone(), handover, back.clone()];

        assert_eq!(
            brief_window(&back, &runs),
            BriefWindow::SinceItsLastRun(first.created_at),
        );
        assert!(
            inherits_a_worktree(&back, &runs),
            "dev-2 had the checkout last, and may have left work in it"
        );
        let brief = issue_brief(
            &issue,
            &Said {
                window: BriefWindow::SinceItsLastRun(first.created_at),
                inherited_worktree: true,
                comments: Vec::new(),
            },
            RunTrigger::Started,
        );
        assert!(
            brief.contains(INHERITED_WORKTREE),
            "or it commits dev-2's changes as its own, or reverts them as stray:\n{brief}"
        );
    }

    #[test]
    fn a_run_that_never_started_does_not_count_as_having_had_the_checkout() {
        let issue = card();
        let dev_1 = AgentProfileId::parse("dev-1").expect("agent id");
        let dev_2 = AgentProfileId::parse("dev-2").expect("agent id");
        let theirs = ran(&issue, &dev_1, 1);
        let cancelled_before_it_started = run(&issue, &dev_2, 2);
        let mine = run(&issue, &dev_1, 3);
        let runs = [theirs, cancelled_before_it_started, mine.clone()];

        assert!(
            !inherits_a_worktree(&mine, &runs),
            "dev-2 never opened the tree; what is in it is dev-1's own"
        );
    }

    #[test]
    fn a_very_long_conversation_still_makes_a_bounded_brief() {
        let issue = card();
        let comments: Vec<Comment> = (0..500)
            .map(|n| {
                said(
                    actors::OPERATOR,
                    &format!("comment {n}: {}", "x".repeat(200)),
                )
            })
            .collect();
        let newest = comments.last().expect("comments").text.clone();

        let brief = issue_brief(
            &issue,
            &Said {
                window: BriefWindow::WholeCard,
                inherited_worktree: false,
                comments,
            },
            RunTrigger::Started,
        );

        assert!(
            brief.len() < COMMENT_BUDGET * 2,
            "a hundred thousand characters of card discussion is not a brief: {} chars",
            brief.len()
        );
        assert!(
            brief.contains(&newest),
            "the newest instruction is the one a run must not miss:\n{brief}"
        );
        assert!(
            !brief.contains("comment 0:"),
            "and the oldest is what gives way for it"
        );
        assert!(
            brief.contains(EARLIER_COMMENTS_TRIMMED),
            "a silent trim reads as the whole story:\n{brief}"
        );
    }

    #[test]
    fn a_short_conversation_arrives_whole_and_unannotated() {
        let issue = card();
        let brief = issue_brief(
            &issue,
            &Said {
                window: BriefWindow::WholeCard,
                inherited_worktree: false,
                comments: vec![
                    said(actors::OPERATOR, "start with the CSV path"),
                    said("@qa", "also handle the empty case"),
                ],
            },
            RunTrigger::Started,
        );
        assert!(brief.contains("- the operator: start with the CSV path\n"));
        assert!(brief.contains("- @qa: also handle the empty case\n"));
        assert!(!brief.contains(EARLIER_COMMENTS_TRIMMED), "{brief}");
    }

    #[test]
    fn every_comment_says_who_said_it() {
        let issue = card();
        let brief = issue_brief(
            &issue,
            &Said {
                window: BriefWindow::WholeCard,
                inherited_worktree: false,
                comments: vec![
                    said(actors::OPERATOR, "ship the CSV path first"),
                    said("@qa", "@dev-1 does it skip a row with no id?"),
                    said("@dev-1", "not yet — doing that now"),
                    said(actors::BOARD, "held the run: the project is out of budget"),
                ],
            },
            RunTrigger::Started,
        );

        // Who is asking is what says whether an answer is owed, and to whom:
        // an unattributed block turns the operator's instruction, a
        // teammate's question and the agent's own note into one voice.
        for line in [
            "- the operator: ship the CSV path first\n",
            "- @qa: @dev-1 does it skip a row with no id?\n",
            "- @dev-1: not yet — doing that now\n",
            "- the board: held the run: the project is out of budget\n",
        ] {
            assert!(brief.contains(line), "{line:?} is missing from:\n{brief}");
        }
    }

    #[test]
    fn a_comment_of_several_lines_stays_under_one_name() {
        let issue = card();
        let brief = issue_brief(
            &issue,
            &Said {
                window: BriefWindow::WholeCard,
                inherited_worktree: false,
                comments: vec![
                    said(
                        actors::OPERATOR,
                        "two things:\n- the header row\n- the id column",
                    ),
                    said("@qa", "agreed"),
                ],
            },
            RunTrigger::Started,
        );

        assert!(
            brief.contains("- the operator: two things:\n  - the header row\n  - the id column\n"),
            "a line inside somebody's comment must not read as the next speaker:\n{brief}"
        );
        assert!(brief.contains("- @qa: agreed\n"), "{brief}");
    }

    fn with_files(comments: Vec<Comment>) -> Said {
        Said {
            window: BriefWindow::WholeCard,
            inherited_worktree: false,
            comments,
        }
    }

    #[test]
    fn a_coordination_brief_opens_with_why_the_lead_was_woken() {
        let issue = card();
        for (trigger, preamble) in [
            (RunTrigger::Triage, TRIAGE_PREAMBLE),
            (RunTrigger::Review, REVIEW_PREAMBLE),
            (RunTrigger::Stalled, STALLED_PREAMBLE),
        ] {
            let brief = issue_brief(&issue, &with_files(Vec::new()), trigger);
            assert!(
                brief.starts_with(preamble),
                "a {trigger:?} brief must open with its own preamble: {brief}"
            );
        }
        let ordinary = issue_brief(&issue, &with_files(Vec::new()), RunTrigger::Started);
        assert!(
            ordinary.starts_with(&issue.title),
            "an ordinary run is briefed by the card alone: {ordinary}"
        );
    }

    #[test]
    fn a_blocked_card_puts_its_reason_in_the_brief() {
        let mut issue = card();
        issue.blocked_reason =
            Some("the card asks for behaviour the Go spec forbids — which wins?".to_owned());

        let woken = issue_brief(&issue, &with_files(Vec::new()), RunTrigger::Blocked);
        assert!(
            woken.starts_with(BLOCKED_PREAMBLE),
            "the wake still says why it happened: {woken}"
        );
        assert!(
            woken.contains("the card asks for behaviour the Go spec forbids — which wins?"),
            "and the one field the whole question is about is in it: {woken}"
        );

        let ordinary = issue_brief(&issue, &with_files(Vec::new()), RunTrigger::Started);
        assert!(
            ordinary.contains("This card is blocked."),
            "a block is a standing fact about the card, not a fact about one trigger: {ordinary}"
        );
        assert!(
            !issue_brief(&card(), &with_files(Vec::new()), RunTrigger::Started)
                .contains("This card is blocked."),
            "and a card nothing has stopped says nothing about a block"
        );
    }

    #[test]
    fn the_cards_own_files_are_named_with_their_type_and_weight() {
        let mut issue = card();
        issue.attachments = vec![file("mockup.png", "image/png")];
        let brief = issue_brief(&issue, &with_files(Vec::new()), RunTrigger::Started);
        assert!(
            brief.contains("Files on this card:\n- mockup.png (image/png, 1 KB)"),
            "{brief}"
        );
    }

    #[test]
    fn a_comment_that_is_only_a_file_still_says_something() {
        let issue = card();
        let mut only_a_file = said(actors::OPERATOR, "");
        only_a_file.attachments = vec![file("trace.log", "text/plain")];
        let brief = issue_brief(&issue, &with_files(vec![only_a_file]), RunTrigger::Started);
        assert!(
            brief.contains("- the operator: [attached trace.log (text/plain, 1 KB)]"),
            "an attachment-only comment must not read as an empty line:\n{brief}"
        );
    }

    #[test]
    fn the_cards_files_keep_their_place_when_the_conversation_is_busy() {
        let mut issue = card();
        issue.attachments = vec![file("spec.png", "image/png")];
        // Ten comment files against a budget of eight, with one already
        // spent by the card: the card's own file survives and the newest
        // seven of theirs come with it.
        let comments: Vec<Comment> = (0..10)
            .map(|i| {
                let mut c = said("@dev-1", "here");
                c.attachments = vec![file(&format!("shot-{i}.png"), "image/png")];
                c
            })
            .collect();
        let said = with_files(comments);

        let carried: Vec<&str> = delivered(&issue, &said)
            .iter()
            .filter_map(|a| a.filename.as_deref())
            .collect();
        assert_eq!(
            carried,
            vec![
                "spec.png",
                "shot-3.png",
                "shot-4.png",
                "shot-5.png",
                "shot-6.png",
                "shot-7.png",
                "shot-8.png",
                "shot-9.png"
            ],
            "the card's own file is the specification and is never the one dropped; \
             what is dropped is the OLDEST of the conversation"
        );
        assert_eq!(named(&issue, &said).len(), 11);

        let brief = issue_brief(&issue, &said, RunTrigger::Started);
        assert!(
            brief.contains("Not every file above could be shown to you"),
            "a brief that quietly showed eight of eleven files would be lying:\n{brief}"
        );
        assert!(
            brief.contains("shot-0.png"),
            "a file that could not be carried is still NAMED, so the agent knows it exists:\n{brief}"
        );
    }

    #[test]
    fn an_unchanged_cards_files_are_not_sent_twice_into_one_transcript() {
        let mut issue = card();
        issue.attachments = vec![file("mockup.png", "image/png")];
        // A follow-up run continues the SAME session, so what the first run
        // was shown is still in front of the model. Re-attaching it pays the
        // image price a second time and reminds it of nothing.
        let follow_up = Said {
            window: BriefWindow::SinceItsLastRun(issue.updated_at + Duration::seconds(1)),
            inherited_worktree: false,
            comments: Vec::new(),
        };
        assert!(
            delivered(&issue, &follow_up).is_empty(),
            "the card's files were already delivered into this transcript"
        );
        let brief = issue_brief(&issue, &follow_up, RunTrigger::Started);
        assert!(brief.contains("mockup.png"), "still named: {brief}");
        assert!(
            brief.contains("already earlier in this conversation"),
            "{brief}"
        );
        assert!(
            !brief.contains("Not every file above could be shown"),
            "a file the model already has was not withheld, and saying so would be wrong:\n{brief}"
        );

        // But an edit to the card re-sends them: nothing records which files
        // a previous run delivered, so a changed card errs towards sending.
        let after_an_edit = Said {
            window: BriefWindow::SinceItsLastRun(issue.updated_at - Duration::seconds(1)),
            inherited_worktree: false,
            comments: Vec::new(),
        };
        assert_eq!(delivered(&issue, &after_an_edit).len(), 1);
    }

    #[test]
    fn a_file_on_a_comment_the_budget_dropped_is_not_delivered_unannounced() {
        let issue = card();
        // One comment big enough to spend the whole prose budget, so the
        // older one is trimmed out of the block entirely.
        let mut old = said("@dev-1", "here is the first screenshot");
        old.attachments = vec![file("old.png", "image/png")];
        let mut huge = said("@dev-2", &"x".repeat(COMMENT_BUDGET + 1));
        huge.attachments = vec![file("new.png", "image/png")];
        let said = with_files(vec![old, huge]);

        let brief = issue_brief(&issue, &said, RunTrigger::Started);
        assert!(
            !brief.contains("old.png"),
            "its whole line was trimmed, so nothing in the prose names it:\n{brief}"
        );
        let carried: Vec<&str> = delivered(&issue, &said)
            .iter()
            .filter_map(|a| a.filename.as_deref())
            .collect();
        assert_eq!(
            carried,
            vec!["new.png"],
            "a picture delivered with no sentence to say where it came from is worse \
             than one left out"
        );
    }

    #[test]
    fn a_card_within_the_budget_says_nothing_about_trimming() {
        let mut issue = card();
        issue.attachments = vec![file("one.png", "image/png")];
        let brief = issue_brief(&issue, &with_files(Vec::new()), RunTrigger::Started);
        assert!(!brief.contains("Not every file"), "{brief}");
    }
}
