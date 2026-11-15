//! The approval cards this client shows.
//!
//! The gateway owns every ask: its queue entry, its ledger row and the tool
//! call parked on it. A client only keeps the cards the gateway told it
//! about, oldest first, so the modal can show the head, and sends each
//! decision back as a [`Frame::ResolveApproval`](baybo_channels::wire::Frame::ResolveApproval).
//! A card leaves the list when this client answers it, or when an
//! `AskResolved` says it closed somewhere else.

use std::collections::VecDeque;
use std::sync::Arc;

use baybo_model::approval::{ApprovalDecision, ApprovalRequest};
use baybo_model::human_ask::AskId;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::debug;

/// One decision on its way to the gateway.
pub type ApprovalAnswer = (AskId, ApprovalDecision);

/// Pending approval cards, shared by the frame pump that fills it and the
/// UI that answers it.
#[derive(Clone)]
pub struct PendingApprovals {
    cards: Arc<Mutex<VecDeque<ApprovalRequest>>>,
    answers: mpsc::UnboundedSender<ApprovalAnswer>,
}

impl PendingApprovals {
    /// An empty list, and the receiver its answers come out of, in the order
    /// they were given.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<ApprovalAnswer>) {
        let (answers, rx) = mpsc::unbounded_channel();
        let list = Self {
            cards: Arc::new(Mutex::new(VecDeque::new())),
            answers,
        };
        (list, rx)
    }

    /// Show `card`. A card already shown under the same `ask_id` is
    /// replaced in place, so a re-sent card neither duplicates nor jumps the
    /// line.
    pub fn show(&self, card: ApprovalRequest) {
        let mut cards = self.cards.lock();
        match cards.iter_mut().find(|c| c.ask_id == card.ask_id) {
            Some(slot) => *slot = card,
            None => cards.push_back(card),
        }
    }

    /// Drop the card for `ask_id`: it closed somewhere else.
    pub fn close(&self, ask_id: &AskId) {
        self.cards.lock().retain(|c| &c.ask_id != ask_id);
    }

    /// The card the modal shows.
    pub fn head(&self) -> Option<ApprovalRequest> {
        self.cards.lock().front().cloned()
    }

    /// Answer the head card with `decision` and take it off the list.
    /// `None` when there is nothing to answer.
    pub fn answer_head(&self, decision: ApprovalDecision) -> Option<ApprovalRequest> {
        let card = self.cards.lock().pop_front()?;
        if self.answers.send((card.ask_id.clone(), decision)).is_err() {
            debug!(ask_id = %card.ask_id, "approval answered after the connection closed");
        }
        Some(card)
    }

    pub fn is_empty(&self) -> bool {
        self.cards.lock().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(ask_id: &str, tool: &str) -> ApprovalRequest {
        ApprovalRequest {
            ask_id: ask_id.parse().unwrap(),
            tool_call_id: format!("use-{ask_id}"),
            session_id: "s1".into(),
            user_id: "u".into(),
            tool: tool.into(),
            accesses: vec![],
            params_preview: String::new(),
            description: None,
        }
    }

    #[test]
    fn answering_takes_the_head_and_sends_its_decision() {
        let (list, mut answers) = PendingApprovals::new();
        list.show(card("a", "Bash"));
        list.show(card("b", "Read"));
        let answered = list.answer_head(ApprovalDecision::Deny).expect("head");
        assert_eq!(answered.ask_id, "a");
        assert_eq!(list.head().map(|c| c.ask_id), Some("b".parse().unwrap()));
        assert_eq!(
            answers.try_recv().expect("sent"),
            ("a".parse().unwrap(), ApprovalDecision::Deny)
        );
    }

    #[test]
    fn a_resent_card_replaces_itself_in_place() {
        let (list, _answers) = PendingApprovals::new();
        list.show(card("a", "Bash"));
        list.show(card("b", "Read"));
        list.show(card("a", "Write"));
        assert_eq!(list.head().map(|c| c.tool), Some("Write".to_owned()));
        list.close(&"a".parse().unwrap());
        list.close(&"b".parse().unwrap());
        assert!(list.is_empty());
    }

    #[test]
    fn nothing_to_answer_sends_nothing() {
        let (list, mut answers) = PendingApprovals::new();
        assert!(list.answer_head(ApprovalDecision::Approve).is_none());
        assert!(answers.try_recv().is_err());
    }
}
