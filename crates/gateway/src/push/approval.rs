//! Approval-gate push relay.
//!
//! A tool call that trips the approval gate parks for
//! [`APPROVAL_TIMEOUT`](crate::channel::boot::APPROVAL_TIMEOUT) and then
//! **denies itself**. Nothing on the [`JobLifecycle`](baybo_job::JobLifecycle)
//! bus fires while it waits — that bus only speaks in completed turns — so the
//! dispatcher's existing subscription cannot see the one moment the user's
//! attention is load-bearing. This relay is the second source feeding
//! [`PushDispatcher`](super::PushDispatcher).
//!
//! It hangs off the approval queue's per-session
//! [`PendingEdge`] stream rather than the gate's waker, which buys two things
//! the waker cannot: the edge is already deduplicated (a second prompt on an
//! already-waiting conversation is silent), and the queue distinguishes a
//! prompt somebody *answered* from one that was simply *abandoned* — the
//! difference between "the user is here, interrupt them again" and "nobody
//! came, stop shouting".
//!
//! The relay is sync and non-blocking end to end: its watcher runs inline on
//! the blocked tool's future, under the queue lock, so it only ever stamps a
//! `DashMap` and `try_send`s onto a bounded channel. All the real work — store
//! reads, crypto, HTTP — happens on the push task that drains the receiver.

use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use baybo_channels::Channel;
use baybo_model::SessionId;
use baybo_tools::PendingEdge;
use dashmap::DashMap;
use tokio::sync::mpsc;

/// Bounded so a pathological prompt storm sheds load instead of growing the
/// queue without limit. Push is best-effort everywhere else in this module
/// (a lagged lifecycle bus already just drops), so an overflow drops too.
const APPROVAL_PUSH_QUEUE: usize = 64;

/// How long to stay quiet about a conversation whose last approval prompt
/// nobody answered.
///
/// A denied call does not end the turn: the agent is told not to retry that
/// tool and commonly reaches for an alternative that is *also* gated. Left
/// alone, an unattended overnight run announces itself once per gate timeout —
/// roughly twelve times an hour, all night. Backing off after an abandoned
/// prompt bounds that at two an hour, while a conversation the user actually
/// answers resets to zero and interrupts again immediately.
const RENAG_AFTER_ABANDONED: Duration = Duration::from_secs(30 * 60);

/// The title of an approval push. Distinct from the reply push's `"Baybo"`
/// because with "Show Previews: When Unlocked" this is the *only* line a
/// locked phone renders, and it has to read as something to act on.
pub(crate) const APPROVAL_TITLE: &str = "Baybo needs approval";

/// One buzz-worthy approval prompt, handed to the push dispatcher.
pub struct ApprovalPush {
    pub(crate) session_id: SessionId,
    /// The prompt's own id — rechecked against the live queue immediately
    /// before the POST, because everything between here and there (store
    /// reads, vault reads, two TLS round-trips) is time in which someone at a
    /// desk can answer it.
    pub(crate) call_id: String,
    /// Tool name, and deliberately nothing else. See [`approval_push_body`].
    pub(crate) tool: String,
    /// The channel the prompt is parked on, for that recheck. `Weak` so a
    /// queued push can never keep a torn-down channel alive.
    pub(crate) channel: Weak<Channel>,
}

/// The body of an approval push.
///
/// **The tool's arguments are deliberately absent.** The obvious richer
/// sources are both unusable here: `Tool::call_label` is implemented by two
/// tools in the workspace (and Bash's is a fixed warning paragraph that never
/// names the command), while `params_preview` is unredacted JSON — a
/// `Bash {"command":"curl -H 'Authorization: Bearer …'}` would render that
/// credential on a lock screen and leave it sitting in Notification Center.
/// A notification only has to be actionable; the card inside the conversation
/// is what is informative.
pub(crate) fn approval_push_body(tool: &str) -> String {
    if tool.is_empty() {
        return "A tool call is waiting for your approval".to_string();
    }
    format!("{tool} is waiting for your approval")
}

/// Receiving half, drained by the push task's `select!` loop.
pub struct ApprovalPushStream(mpsc::Receiver<ApprovalPush>);

impl ApprovalPushStream {
    pub async fn recv(&mut self) -> Option<ApprovalPush> {
        self.0.recv().await
    }
}

/// Per-conversation nag state. Present only for conversations that have
/// actually pushed, and dropped the moment someone answers one.
struct SessionNag {
    last_push: Instant,
    /// The most recent prompt here went away with nobody deciding it.
    abandoned: bool,
}

/// Watches approval-queue edges and forwards the ones worth a notification.
pub struct ApprovalPushRelay {
    tx: mpsc::Sender<ApprovalPush>,
    nag: DashMap<SessionId, SessionNag>,
}

impl ApprovalPushRelay {
    pub fn new() -> (Arc<Self>, ApprovalPushStream) {
        let (tx, rx) = mpsc::channel(APPROVAL_PUSH_QUEUE);
        (
            Arc::new(Self {
                tx,
                nag: DashMap::new(),
            }),
            ApprovalPushStream(rx),
        )
    }

    /// Attach to `channel`'s approval queue. Call once, on the channel whose
    /// clients can actually answer a prompt — a push that deep-links into a
    /// conversation whose gate lives on some other channel's queue would open
    /// a chat with no card in it.
    pub fn install(self: &Arc<Self>, channel: &Arc<Channel>) {
        let relay = Arc::clone(self);
        let weak = Arc::downgrade(channel);
        channel.add_pending_approval_watcher(Arc::new(move |session_id, edge| {
            relay.observe(&session_id, &edge, &weak);
        }));
    }

    /// Watcher body. Runs under the approval queue's lock — see
    /// [`PendingEdge`]. `pub(crate)` so tests drive it without a channel.
    pub(crate) fn observe(
        &self,
        session_id: &SessionId,
        edge: &PendingEdge,
        channel: &Weak<Channel>,
    ) {
        let PendingEdge::Raised { call_id, tool } = edge else {
            // Somebody decided → they are present, and the next prompt in this
            // conversation earns a fresh interruption. Nobody came → keep the
            // timestamp and remember why.
            match edge {
                PendingEdge::Answered => {
                    self.nag.remove(session_id);
                }
                PendingEdge::Abandoned => {
                    if let Some(mut nag) = self.nag.get_mut(session_id) {
                        nag.abandoned = true;
                    }
                }
                PendingEdge::Raised { .. } => unreachable!(),
            }
            return;
        };

        // Somebody already has this conversation open, so the card is on their
        // screen — and on iOS a push that arrives while the app is frontmost
        // presents nothing anyway. This is also what keeps a user answering
        // prompts at their desk from buzzing the phone in their pocket once
        // per prompt.
        if let Some(channel) = channel.upgrade()
            && channel.has_subscribers(session_id)
        {
            tracing::debug!(
                session = %session_id,
                "push: approval prompt has a live subscriber; not pushing"
            );
            return;
        }
        if !self.may_nag(session_id) {
            tracing::debug!(
                session = %session_id,
                "push: last approval prompt here went unanswered; holding off"
            );
            return;
        }

        let push = ApprovalPush {
            session_id: session_id.clone(),
            call_id: call_id.clone(),
            tool: tool.clone(),
            channel: channel.clone(),
        };
        match self.tx.try_send(push) {
            Ok(()) => {
                self.nag.insert(
                    session_id.clone(),
                    SessionNag {
                        last_push: Instant::now(),
                        abandoned: false,
                    },
                );
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    session = %session_id,
                    "push: approval push queue is full; this prompt will not buzz"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    /// False while a conversation is inside its post-abandonment quiet period.
    fn may_nag(&self, session_id: &SessionId) -> bool {
        match self.nag.get(session_id) {
            Some(nag) => !nag.abandoned || nag.last_push.elapsed() >= RENAG_AFTER_ABANDONED,
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raised(call_id: &str, tool: &str) -> PendingEdge {
        PendingEdge::Raised {
            call_id: call_id.into(),
            tool: tool.into(),
        }
    }

    /// No channel: `Weak::new()` never upgrades, so the subscriber check is
    /// skipped and `observe` exercises the nag policy alone.
    fn detached() -> Weak<Channel> {
        Weak::new()
    }

    #[test]
    fn body_names_the_tool_and_nothing_else() {
        // The whole point: no arguments reach a lock screen.
        assert_eq!(
            approval_push_body("Bash"),
            "Bash is waiting for your approval"
        );
        assert!(!approval_push_body("Bash").contains('{'));
        assert_eq!(
            approval_push_body(""),
            "A tool call is waiting for your approval"
        );
    }

    #[test]
    fn a_raise_pushes_and_carries_the_prompt_identity() {
        let (relay, mut stream) = ApprovalPushRelay::new();
        relay.observe(&SessionId::from("s1"), &raised("c1", "Bash"), &detached());
        let push = stream.0.try_recv().expect("a push was queued");
        assert_eq!(push.session_id, SessionId::from("s1"));
        assert_eq!(push.call_id, "c1");
        assert_eq!(push.tool, "Bash");
    }

    #[test]
    fn an_answered_prompt_lets_the_next_one_interrupt_immediately() {
        // The sequential shape: gated tools are Exclusive, so an approval-heavy
        // turn prompts one at a time. Each prompt the user answers is a real
        // block on the turn, and must buzz.
        let (relay, mut stream) = ApprovalPushRelay::new();
        let s = SessionId::from("s1");
        relay.observe(&s, &raised("c1", "Bash"), &detached());
        relay.observe(&s, &PendingEdge::Answered, &detached());
        relay.observe(&s, &raised("c2", "Write"), &detached());
        assert_eq!(stream.0.try_recv().expect("first").call_id, "c1");
        assert_eq!(stream.0.try_recv().expect("second").call_id, "c2");
    }

    #[test]
    fn an_abandoned_prompt_silences_the_conversation_for_a_while() {
        // The unattended shape: nobody answered, the gate self-denied, and the
        // agent reached for another gated approach. Re-announcing every cycle
        // is what turns one blocked turn into an all-night stream.
        let (relay, mut stream) = ApprovalPushRelay::new();
        let s = SessionId::from("s1");
        relay.observe(&s, &raised("c1", "Bash"), &detached());
        assert!(stream.0.try_recv().is_ok());
        relay.observe(&s, &PendingEdge::Abandoned, &detached());
        relay.observe(&s, &raised("c2", "Write"), &detached());
        assert!(
            stream.0.try_recv().is_err(),
            "a re-nag inside the quiet period is suppressed"
        );

        // …and the quiet period is per-conversation, not global.
        relay.observe(&SessionId::from("s2"), &raised("d1", "Bash"), &detached());
        assert_eq!(
            stream.0.try_recv().expect("other session").session_id,
            SessionId::from("s2")
        );
    }

    #[test]
    fn the_quiet_period_expires() {
        let (relay, mut stream) = ApprovalPushRelay::new();
        let s = SessionId::from("s1");
        relay.observe(&s, &raised("c1", "Bash"), &detached());
        assert!(stream.0.try_recv().is_ok());
        relay.observe(&s, &PendingEdge::Abandoned, &detached());
        // Backdate past the window rather than sleeping half an hour.
        if let Some(mut nag) = relay.nag.get_mut(&s) {
            nag.last_push = Instant::now() - RENAG_AFTER_ABANDONED - Duration::from_secs(1);
        }
        relay.observe(&s, &raised("c2", "Write"), &detached());
        assert_eq!(stream.0.try_recv().expect("re-nag allowed").call_id, "c2");
    }

    #[test]
    fn a_full_queue_drops_rather_than_blocking() {
        let (relay, _stream) = ApprovalPushRelay::new();
        // Distinct sessions so the nag policy never suppresses; only capacity
        // can stop these. The watcher runs under the gate's lock — it must
        // return no matter what.
        for i in 0..(APPROVAL_PUSH_QUEUE * 2) {
            relay.observe(
                &SessionId::from(format!("s{i}").as_str()),
                &raised("c", "Bash"),
                &detached(),
            );
        }
    }
}
