//! Who a timeline entry names.
//!
//! A brief and a rendered timeline both have to answer "who said this", and
//! two spellings of the answer would be two answers — the person reading
//! the card and the agent reading its brief have to be looking at the same
//! somebody.
//!
//! `you` is never one of the answers here: it means *the reader*, and each
//! surface has a different one. The web board says `you` to the person
//! whose card it is. These are the surfaces an **agent** reads, so the
//! person is [`OPERATOR`] and every agent is its `@handle` — including the
//! reader, whose own past words are quoted back to it under the same name
//! its teammates use for it.

use std::sync::Arc;

use baybo_model::{AgentProfileId, ProjectId};
use baybo_store::project::IssueActor;
use baybo_store::{AgentProfileRow, AgentProfileStore};

/// What the person on the other side of the board is called when an agent
/// is being told about them.
pub(crate) const OPERATOR: &str = "the operator";

/// The board acting on its own — today, the budget gate.
pub(crate) const BOARD: &str = "the board";

/// The profile rows for `ids` **on this board**, reaching agents that have
/// left its team. An id belonging to another project resolves to nothing
/// rather than to a row, so a board can never name somebody who was never
/// on it.
pub(crate) async fn profiles(
    agents: &Arc<dyn AgentProfileStore>,
    project: &ProjectId,
    ids: impl IntoIterator<Item = AgentProfileId>,
) -> Vec<AgentProfileRow> {
    let mut rows = Vec::new();
    for id in ids {
        match agents.get(&id).await {
            Ok(Some(row))
                if row
                    .team
                    .as_ref()
                    .is_some_and(|team| &team.project_id == project) =>
            {
                rows.push(row)
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(agent = %id, error = %e, "could not resolve an agent a timeline names")
            }
        }
    }
    rows
}

/// An agent's `@handle`, falling back to its id when the board cannot name
/// it: an id nobody recognises still says the entries came from different
/// people, which dropping the attribution would not.
pub(crate) fn handle_of(known: &[AgentProfileRow], id: &AgentProfileId) -> String {
    known
        .iter()
        .find(|row| &row.id == id)
        .and_then(|row| row.team.as_ref())
        .map(|t| format!("@{}", t.handle))
        .unwrap_or_else(|| id.as_str().to_owned())
}

/// Who did the thing a timeline entry records. The mirror of the web
/// board's `actorLabel`, for the surfaces an agent reads.
pub(crate) fn label(actor: &IssueActor, known: &[AgentProfileRow]) -> String {
    match actor {
        IssueActor::User => OPERATOR.to_owned(),
        IssueActor::System => BOARD.to_owned(),
        IssueActor::Agent(id) => handle_of(known, id),
    }
}

/// Every agent an entry names as its author, for a caller resolving the
/// rows [`label`] needs.
pub(crate) fn named_agent(actor: &IssueActor) -> Option<AgentProfileId> {
    match actor {
        IssueActor::Agent(id) => Some(id.clone()),
        IssueActor::User | IssueActor::System => None,
    }
}
