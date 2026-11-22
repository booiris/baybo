//! Ordered approval and question cards for the currently subscribed session.

use std::collections::VecDeque;
use std::sync::Arc;

use baybo_channels::wire::{ApprovalCard, Frame, QuestionCard, QuestionReply};
use baybo_model::SessionId;
use baybo_model::approval::{ApprovalDecision, ApprovalRequest};
use baybo_model::human_ask::{AskId, Question};
use parking_lot::Mutex;
use tokio::sync::mpsc;

#[derive(Clone, Debug)]
pub enum PendingCard {
    Approval(ApprovalRequest),
    Question {
        ask_id: AskId,
        questions: Vec<Question>,
        submitting: bool,
    },
}

impl PendingCard {
    fn id(&self) -> &AskId {
        match self {
            Self::Approval(card) => &card.ask_id,
            Self::Question { ask_id, .. } => ask_id,
        }
    }

    pub(crate) fn question(ask_id: AskId, questions: Vec<Question>) -> Self {
        Self::Question {
            ask_id,
            questions,
            submitting: false,
        }
    }
}

#[derive(Clone)]
pub struct PendingAsks {
    cards: Arc<Mutex<VecDeque<PendingCard>>>,
    answers: mpsc::UnboundedSender<Frame>,
}

impl PendingAsks {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Frame>) {
        let (answers, rx) = mpsc::unbounded_channel();
        (
            Self {
                cards: Arc::default(),
                answers,
            },
            rx,
        )
    }

    pub fn show(&self, card: PendingCard) {
        if matches!(&card, PendingCard::Question { questions, .. } if questions.is_empty()) {
            return;
        }
        let mut cards = self.cards.lock();
        if let Some(existing) = cards.iter_mut().find(|entry| entry.id() == card.id()) {
            // A duplicate question must not unlock a reply still awaiting confirmation.
            if matches!(card, PendingCard::Approval(_)) {
                *existing = card;
            }
        } else {
            cards.push_back(card);
        }
    }

    pub(crate) fn replace(
        &self,
        session: &SessionId,
        approvals: Vec<ApprovalCard>,
        questions: Vec<QuestionCard>,
    ) {
        let mut cards: VecDeque<_> = approvals
            .into_iter()
            .map(|c| {
                PendingCard::Approval(ApprovalRequest {
                    ask_id: c.ask_id,
                    tool_call_id: c.tool_call_id.unwrap_or_default(),
                    session_id: session.clone(),
                    user_id: c.user_id,
                    tool: c.tool,
                    accesses: c.accesses,
                    params_preview: c.params_preview,
                    description: c.description,
                })
            })
            .collect();
        // The gateway emits questions in ascending queue_index among both kinds.
        for c in questions.into_iter().filter(|c| !c.questions.is_empty()) {
            cards.insert(
                c.queue_index.min(cards.len()),
                PendingCard::question(c.ask_id, c.questions),
            );
        }
        *self.cards.lock() = cards;
    }

    pub(crate) fn clear(&self) {
        self.cards.lock().clear();
    }

    pub(crate) fn close(&self, id: &AskId) {
        self.cards.lock().retain(|card| card.id() != id);
    }

    pub fn head(&self) -> Option<PendingCard> {
        self.cards.lock().front().cloned()
    }

    pub fn answer_approval(&self, id: &AskId, decision: ApprovalDecision) -> bool {
        let mut cards = self.cards.lock();
        if !matches!(cards.front(), Some(PendingCard::Approval(card)) if card.ask_id == *id) {
            return false;
        }
        if self
            .answers
            .send(Frame::ResolveApproval {
                ask_id: id.clone(),
                decision,
            })
            .is_err()
        {
            return false;
        }
        cards.pop_front();
        true
    }

    pub fn answer_question(&self, id: &AskId, reply: QuestionReply) -> bool {
        let mut cards = self.cards.lock();
        let Some(PendingCard::Question {
            ask_id, submitting, ..
        }) = cards.front_mut()
        else {
            return false;
        };
        if ask_id != id || *submitting {
            return false;
        }
        *submitting = self
            .answers
            .send(Frame::AnswerQuestion {
                ask_id: id.clone(),
                reply,
            })
            .is_ok();
        *submitting
    }

    pub(crate) fn send_failed(&self, frame: &Frame) {
        if let Frame::AnswerQuestion { ask_id, .. } = frame
            && let Some(PendingCard::Question {
                ask_id: head,
                submitting,
                ..
            }) = self.cards.lock().front_mut()
            && head == ask_id
        {
            *submitting = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approval(id: &str, tool: &str) -> PendingCard {
        PendingCard::Approval(ApprovalRequest {
            ask_id: id.parse().unwrap(),
            tool_call_id: format!("call-{id}"),
            session_id: "session".into(),
            user_id: String::new(),
            tool: tool.into(),
            accesses: vec![],
            params_preview: String::new(),
            description: None,
        })
    }

    #[test]
    fn a_remote_close_racing_an_approval_key_never_decides_the_next_card() {
        let (queue, mut answers) = PendingAsks::new();
        let first = approval("first", "Read");
        let second = approval("second", "Write");
        queue.show(first.clone());
        queue.show(second.clone());
        queue.show(approval("first", "Bash"));
        assert!(matches!(queue.head(), Some(PendingCard::Approval(card)) if card.tool == "Bash"));
        queue.close(first.id());
        assert!(!queue.answer_approval(first.id(), ApprovalDecision::ApproveAlways));
        assert!(queue.answer_approval(second.id(), ApprovalDecision::Deny));
        assert_eq!(
            answers.try_recv().unwrap(),
            Frame::ResolveApproval {
                ask_id: second.id().clone(),
                decision: ApprovalDecision::Deny
            }
        );
        assert!(queue.head().is_none());
        assert!(answers.try_recv().is_err());
    }
}
