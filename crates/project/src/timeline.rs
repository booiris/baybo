//! What an edit is worth saying on the timeline.

use baybo_store::project::{IssueEventBody, IssueRow};

/// The entries one edit deserves, in the order they should read.
pub(crate) fn diff_events(before: &IssueRow, after: &IssueRow) -> Vec<IssueEventBody> {
    let mut out = Vec::new();
    if before.status != after.status {
        out.push(IssueEventBody::Moved {
            from: before.status,
            to: after.status,
        });
    }
    if before.assignee != after.assignee {
        out.push(IssueEventBody::Assigned {
            from: before.assignee.clone(),
            to: after.assignee.clone(),
        });
    }
    // A changed reason is a fresh block, not a silent amendment: the reason
    // is the whole content of the entry, so replacing one leaves the
    // timeline saying something that is no longer true.
    match (&before.blocked_reason, &after.blocked_reason) {
        (_, Some(reason)) if before.blocked_reason.as_ref() != Some(reason) => {
            out.push(IssueEventBody::Blocked {
                reason: reason.clone(),
            });
        }
        (Some(_), None) => out.push(IssueEventBody::Unblocked),
        _ => {}
    }
    if before.cancelled_at.is_none() && after.cancelled_at.is_some() {
        out.push(IssueEventBody::Cancelled);
    }
    out
}

/// Whether this event changed the card rather than only recording run bookkeeping.
pub(crate) fn left_a_mark(body: &IssueEventBody) -> bool {
    matches!(
        body,
        IssueEventBody::Comment { .. }
            | IssueEventBody::Moved { .. }
            | IssueEventBody::Assigned { .. }
            | IssueEventBody::Blocked { .. }
            | IssueEventBody::Unblocked
            | IssueEventBody::Cancelled
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{AgentProfileId, IssueId, ProjectId};
    use baybo_store::project::{IssuePriority, IssueStatus};

    fn issue() -> IssueRow {
        let now = chrono::Utc::now();
        IssueRow {
            id: IssueId::generate(),
            project_id: ProjectId::parse("proj-a".to_owned()).expect("id"),
            number: 1,
            title: "Wire it".into(),
            description: String::new(),
            attachments: Vec::new(),
            status: IssueStatus::Backlog,
            priority: IssuePriority::None,
            assignee: None,
            position: 0,
            pinned: false,
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

    fn agent(id: &str) -> AgentProfileId {
        AgentProfileId::parse(id.to_owned()).expect("agent id")
    }

    #[test]
    fn an_edit_that_changes_nothing_visible_says_nothing() {
        let before = issue();
        let mut after = before.clone();
        after.title = "Wire it properly".into();
        after.description = "with tests".into();
        after.priority = IssuePriority::Urgent;
        assert!(
            diff_events(&before, &after).is_empty(),
            "prose edits are not events; the current text is on the page"
        );
    }

    #[test]
    fn a_pin_is_not_something_that_happened_to_the_card() {
        let before = issue();
        let mut after = before.clone();
        after.pinned = true;
        assert!(
            diff_events(&before, &after).is_empty(),
            "the pin is how one person reads the column, and priority — which \
             actually decides what the board starts — is silent too"
        );
    }

    #[test]
    fn a_move_and_an_assignment_in_one_edit_are_two_entries() {
        let before = issue();
        let mut after = before.clone();
        after.status = IssueStatus::InProgress;
        after.assignee = Some(agent("dev-1"));
        assert_eq!(
            diff_events(&before, &after),
            vec![
                IssueEventBody::Moved {
                    from: IssueStatus::Backlog,
                    to: IssueStatus::InProgress,
                },
                IssueEventBody::Assigned {
                    from: None,
                    to: Some(agent("dev-1")),
                },
            ],
            "the move reads first: it is why the assignment happened"
        );
    }

    #[test]
    fn a_reason_that_changes_is_a_fresh_block() {
        let mut before = issue();
        before.blocked_reason = Some("waiting on tmux".into());
        let mut after = before.clone();
        after.blocked_reason = Some("waiting on the operator".into());
        assert_eq!(
            diff_events(&before, &after),
            vec![IssueEventBody::Blocked {
                reason: "waiting on the operator".into(),
            }],
            "otherwise the timeline keeps stating a reason that is no longer why"
        );
    }

    #[test]
    fn unblocking_and_cancelling_each_read_once() {
        let mut before = issue();
        before.blocked_reason = Some("waiting".into());
        let mut after = before.clone();
        after.blocked_reason = None;
        assert_eq!(
            diff_events(&before, &after),
            vec![IssueEventBody::Unblocked]
        );

        let before = issue();
        let mut after = before.clone();
        after.cancelled_at = Some(chrono::Utc::now());
        assert_eq!(
            diff_events(&before, &after),
            vec![IssueEventBody::Cancelled]
        );
        assert!(diff_events(&after, &after).is_empty());
    }
}
