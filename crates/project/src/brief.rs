//! The brief a run is handed: the card, plus what has been said on it that
//! the assignee has not already read.

use std::sync::Arc;

use baybo_store::project::{IssueEventBody, IssueRow, IssueRunRow, ProjectStore};

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
    comments: Vec<String>,
}

const SAID_SINCE_LAST_RUN: &str = "\n\nSaid since your last run:\n";

const SAID_ON_THE_CARD: &str = "\n\nSaid on the card so far:\n";

const INHERITED_WORKTREE: &str = r#"

You are picking this card up after another agent worked it. The checkout you
are given is the one it worked in, so any uncommitted change in there is its
work and not yours — read what is already in the tree before you add to it."#;

const COMMENT_BUDGET: usize = 16_000;

const EARLIER_COMMENTS_TRIMMED: &str =
    "(earlier comments are not repeated here — the card itself has all of them)";

fn comment_block(comments: &[String]) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut spent = 0usize;
    for comment in comments.iter().rev() {
        if !kept.is_empty() && spent + comment.len() > COMMENT_BUDGET {
            break;
        }
        spent += comment.len();
        kept.push(comment);
    }
    let mut block = String::new();
    if kept.len() < comments.len() {
        block.push_str(&format!("- {EARLIER_COMMENTS_TRIMMED}\n"));
    }
    for comment in kept.iter().rev() {
        block.push_str(&format!("- {comment}\n"));
    }
    block
}

pub(crate) fn issue_brief(issue: &IssueRow, said: &Said) -> String {
    let mut brief = if issue.description.trim().is_empty() {
        issue.title.clone()
    } else {
        format!("{}\n\n{}", issue.title, issue.description)
    };
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
    brief
}

fn brief_window(run: &IssueRunRow, runs: &[IssueRunRow]) -> BriefWindow {
    match session_run_before(run, runs) {
        Some(previous) => BriefWindow::SinceItsLastRun(previous.created_at),
        None => BriefWindow::WholeCard,
    }
}

fn inherits_a_worktree(run: &IssueRunRow, runs: &[IssueRunRow]) -> bool {
    runs.iter()
        .filter(|candidate| candidate.id != run.id && ever_ran(candidate))
        .max_by_key(|candidate| candidate.attempt)
        .is_some_and(|last| last.agent_id != run.agent_id)
}

pub(crate) async fn comments_for_brief(store: &Arc<dyn ProjectStore>, run: &IssueRunRow) -> Said {
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
        Ok(events) => events
            .into_iter()
            .filter_map(|event| match event.body {
                IssueEventBody::Comment { text } => Some(text),
                _ => None,
            })
            .collect(),
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
                comments: vec!["start with the CSV path".to_owned()],
            },
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
                comments: vec!["also handle the empty case".to_owned()],
            },
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
                    "start with the CSV path".to_owned(),
                    "also handle the empty case".to_owned(),
                ],
            },
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
        let comments: Vec<String> = (0..500)
            .map(|n| format!("comment {n}: {}", "x".repeat(200)))
            .collect();

        let brief = issue_brief(
            &issue,
            &Said {
                window: BriefWindow::WholeCard,
                inherited_worktree: false,
                comments: comments.clone(),
            },
        );

        assert!(
            brief.len() < COMMENT_BUDGET * 2,
            "a hundred thousand characters of card discussion is not a brief: {} chars",
            brief.len()
        );
        assert!(
            brief.contains(comments.last().expect("comments")),
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
                    "start with the CSV path".to_owned(),
                    "also handle the empty case".to_owned(),
                ],
            },
        );
        assert!(brief.contains("- start with the CSV path\n"));
        assert!(brief.contains("- also handle the empty case\n"));
        assert!(!brief.contains(EARLIER_COMMENTS_TRIMMED), "{brief}");
    }
}
