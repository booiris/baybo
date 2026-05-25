//! Bounded, single-consumer **priority** mailbox for the actor.
//!
//! Replaces the plain FIFO `tokio::mpsc` the actor used, so a live
//! `UserInput` is served ahead of a queued `SubagentFinished` while a
//! lowest-priority `ActorStop` only runs once all real work has drained.
//! It mirrors the slice of the `mpsc` API the actor + supervisor rely on
//! (`send` / `try_send` / `recv` / `try_recv`, `Clone`, and
//! close-on-last-sender-drop).
//!
//! Ordering: messages pop highest-priority-first; within one priority the
//! earliest-enqueued pops first (FIFO), using a monotonic insertion
//! sequence as the heap tiebreaker. Backpressure: a `Semaphore` carries
//! `capacity` permits — `send` holds one permit per queued message
//! (stored in the entry) and it is released when the message is received,
//! freeing a slot for a blocked sender.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering as AtomicOrdering};

use parking_lot::Mutex;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, TryAcquireError};

/// Default mailbox capacity the assembly layer uses for production
/// per-session actors. Generous so a burst of background-subagent terminals
/// or user messages backpressures the sender rather than stalling the actor;
/// the mailbox is bounded (it never drops — `send` awaits a slot), so this
/// only bounds queue depth.
pub const DEFAULT_CAPACITY: usize = 4096;

/// Priority class of a mailbox message; higher is served first. The
/// derived `Ord` follows declaration order, so `Trigger > SubagentFinished
/// > Stop`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum MessagePriority {
    /// Lowest — drain every real message first, then stop the actor.
    Stop,
    /// Autonomous reaction to a finished background subagent.
    SubagentFinished,
    /// Triggering work: live user input, cron fire, subagent spawn,
    /// background-compression dispatch.
    Trigger,
}

/// Implemented by the payload so priority is intrinsic to the message
/// kind — senders never pass a priority explicitly.
pub trait Prioritized {
    fn priority(&self) -> MessagePriority;
}

struct Entry<T> {
    priority: MessagePriority,
    seq: u64,
    msg: T,
    /// Released when this entry is received (or dropped), returning one
    /// capacity slot to the semaphore for a blocked sender.
    _permit: OwnedSemaphorePermit,
}

impl<T> PartialEq for Entry<T> {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.seq == other.seq
    }
}
impl<T> Eq for Entry<T> {}
impl<T> PartialOrd for Entry<T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for Entry<T> {
    fn cmp(&self, other: &Self) -> Ordering {
        // Max-heap pops the greatest: higher priority first, then the
        // smaller seq (earlier enqueue) so equal priority is FIFO.
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

struct Shared<T> {
    queue: Mutex<BinaryHeap<Entry<T>>>,
    /// Notified on every push and when the last sender drops, so a parked
    /// receiver re-checks the queue / observes disconnection.
    signal: Notify,
    /// Permits == free capacity slots. Closed when the receiver drops, so
    /// in-flight and future sends fail rather than block forever.
    capacity: Arc<Semaphore>,
    sender_count: AtomicUsize,
    next_seq: AtomicU64,
}

/// Cloneable sender half. Each clone keeps the channel open; the channel
/// disconnects (so `recv` yields `None`) only once every sender drops.
pub struct MailboxSender<T> {
    shared: Arc<Shared<T>>,
}

/// Single-consumer receiver half.
pub struct MailboxReceiver<T> {
    shared: Arc<Shared<T>>,
}

impl<T> fmt::Debug for MailboxSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailboxSender").finish_non_exhaustive()
    }
}
impl<T> fmt::Debug for MailboxReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailboxReceiver").finish_non_exhaustive()
    }
}

/// Create a bounded priority channel holding at most `capacity` messages.
pub fn channel<T>(capacity: usize) -> (MailboxSender<T>, MailboxReceiver<T>) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(BinaryHeap::new()),
        signal: Notify::new(),
        capacity: Arc::new(Semaphore::new(capacity)),
        sender_count: AtomicUsize::new(1),
        next_seq: AtomicU64::new(0),
    });
    (
        MailboxSender {
            shared: Arc::clone(&shared),
        },
        MailboxReceiver { shared },
    )
}

impl<T: Prioritized> MailboxSender<T> {
    /// Enqueue `msg`, awaiting a free capacity slot. Fails only once the
    /// receiver has been dropped.
    pub async fn send(&self, msg: T) -> Result<(), SendError<T>> {
        match Arc::clone(&self.shared.capacity).acquire_owned().await {
            Ok(permit) => {
                self.enqueue(permit, msg);
                Ok(())
            }
            Err(_) => Err(SendError(msg)),
        }
    }

    /// Non-blocking enqueue: `Full` when at capacity, `Closed` once the
    /// receiver is gone.
    pub fn try_send(&self, msg: T) -> Result<(), TrySendError<T>> {
        match Arc::clone(&self.shared.capacity).try_acquire_owned() {
            Ok(permit) => {
                self.enqueue(permit, msg);
                Ok(())
            }
            Err(TryAcquireError::NoPermits) => Err(TrySendError::Full(msg)),
            Err(TryAcquireError::Closed) => Err(TrySendError::Closed(msg)),
        }
    }

    fn enqueue(&self, permit: OwnedSemaphorePermit, msg: T) {
        let seq = self.shared.next_seq.fetch_add(1, AtomicOrdering::Relaxed);
        let priority = msg.priority();
        self.shared.queue.lock().push(Entry {
            priority,
            seq,
            msg,
            _permit: permit,
        });
        self.shared.signal.notify_one();
    }
}

impl<T> Clone for MailboxSender<T> {
    fn clone(&self) -> Self {
        self.shared
            .sender_count
            .fetch_add(1, AtomicOrdering::Relaxed);
        MailboxSender {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Drop for MailboxSender<T> {
    fn drop(&mut self) {
        // On the last sender dropping, wake a parked receiver so it
        // observes disconnection and returns `None`.
        if self
            .shared
            .sender_count
            .fetch_sub(1, AtomicOrdering::AcqRel)
            == 1
        {
            self.shared.signal.notify_one();
        }
    }
}

impl<T> MailboxReceiver<T> {
    /// Await the highest-priority message. Returns `None` once the queue
    /// is empty and every sender has dropped.
    pub async fn recv(&mut self) -> Option<T> {
        loop {
            if let Some(entry) = self.shared.queue.lock().pop() {
                return Some(entry.msg);
            }
            if self.shared.sender_count.load(AtomicOrdering::Acquire) == 0 {
                // A sender may have pushed in the window before its drop;
                // re-check before declaring disconnection.
                if let Some(entry) = self.shared.queue.lock().pop() {
                    return Some(entry.msg);
                }
                return None;
            }
            // `notify_one` stores a permit even with no waiter parked, so a
            // push (or last-sender drop) in the gap above is not lost.
            self.shared.signal.notified().await;
        }
    }

    /// Priority of the highest-priority queued message, without removing
    /// it; `None` when the queue is empty. Lets the actor decide whether
    /// to drain its pending-subagent buffer now or defer to higher work.
    pub fn peek_priority(&self) -> Option<MessagePriority> {
        self.shared.queue.lock().peek().map(|e| e.priority)
    }

    /// Conditionally receive: pop the highest-priority queued message **only
    /// if** `pred` accepts it; otherwise leave it queued and return `None`.
    /// Only the single highest-priority entry is examined — a non-matching top
    /// blocks the pop even when a matching entry sits lower, which is exactly
    /// what the agent loop wants: it drains the *leading run* of injectable user
    /// interjections at a tool boundary and stops at the first non-injectable
    /// message (a queued slash command, a `SubagentFinished`, an `ActorStop`),
    /// leaving everything behind it for the actor's normal dispatch.
    pub fn try_recv_if<F: FnOnce(&T) -> bool>(&mut self, pred: F) -> Option<T> {
        let mut queue = self.shared.queue.lock();
        let matches = queue.peek().is_some_and(|e| pred(&e.msg));
        if matches {
            queue.pop().map(|e| e.msg)
        } else {
            None
        }
    }

    /// Non-blocking receive.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        if let Some(entry) = self.shared.queue.lock().pop() {
            return Ok(entry.msg);
        }
        if self.shared.sender_count.load(AtomicOrdering::Acquire) == 0 {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }
}

impl<T> Drop for MailboxReceiver<T> {
    fn drop(&mut self) {
        // Fail any pending and future sends instead of blocking forever.
        self.shared.capacity.close();
    }
}

/// Returned by [`MailboxSender::send`] when the receiver has been dropped;
/// carries the message back to the caller.
pub struct SendError<T>(pub T);

/// Returned by [`MailboxSender::try_send`].
pub enum TrySendError<T> {
    /// The channel is at capacity.
    Full(T),
    /// The receiver has been dropped.
    Closed(T),
}

/// Returned by [`MailboxReceiver::try_recv`].
#[derive(Debug, PartialEq, Eq)]
pub enum TryRecvError {
    /// No message currently queued.
    Empty,
    /// Queue is empty and every sender has dropped.
    Disconnected,
}

// Display/Debug deliberately omit the payload `T` (matching `tokio::mpsc`)
// so the error types stay usable with `tracing`'s `%e` without a `T:
// Display` bound.
impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sending on a closed mailbox")
    }
}
impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SendError(..)")
    }
}
impl<T> std::error::Error for SendError<T> {}

impl<T> fmt::Display for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrySendError::Full(_) => f.write_str("mailbox is full"),
            TrySendError::Closed(_) => f.write_str("sending on a closed mailbox"),
        }
    }
}
impl<T> fmt::Debug for TrySendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrySendError::Full(_) => f.write_str("TrySendError::Full(..)"),
            TrySendError::Closed(_) => f.write_str("TrySendError::Closed(..)"),
        }
    }
}
impl<T> std::error::Error for TrySendError<T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Msg {
        pri: MessagePriority,
        id: u32,
    }
    impl Msg {
        fn new(pri: MessagePriority, id: u32) -> Self {
            Self { pri, id }
        }
    }
    impl Prioritized for Msg {
        fn priority(&self) -> MessagePriority {
            self.pri
        }
    }

    use MessagePriority::{Stop, SubagentFinished, Trigger};

    #[tokio::test]
    async fn pops_highest_priority_first() {
        let (tx, mut rx) = channel::<Msg>(8);
        tx.send(Msg::new(Stop, 1)).await.unwrap();
        tx.send(Msg::new(Trigger, 2)).await.unwrap();
        tx.send(Msg::new(SubagentFinished, 3)).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().id, 2); // Trigger
        assert_eq!(rx.recv().await.unwrap().id, 3); // SubagentFinished
        assert_eq!(rx.recv().await.unwrap().id, 1); // Stop
    }

    #[tokio::test]
    async fn fifo_within_a_priority() {
        let (tx, mut rx) = channel::<Msg>(8);
        for id in 0..4 {
            tx.send(Msg::new(Trigger, id)).await.unwrap();
        }
        for id in 0..4 {
            assert_eq!(rx.recv().await.unwrap().id, id);
        }
    }

    #[tokio::test]
    async fn recv_yields_none_after_all_senders_drop() {
        let (tx, mut rx) = channel::<Msg>(8);
        drop(tx);
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn queued_messages_drain_before_disconnect() {
        let (tx, mut rx) = channel::<Msg>(8);
        tx.send(Msg::new(Trigger, 7)).await.unwrap();
        drop(tx);
        assert_eq!(rx.recv().await.unwrap().id, 7);
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn try_send_reports_full_then_closed() {
        let (tx, mut rx) = channel::<Msg>(1);
        tx.try_send(Msg::new(Trigger, 1)).unwrap();
        assert!(matches!(
            tx.try_send(Msg::new(Trigger, 2)),
            Err(TrySendError::Full(_))
        ));
        // Receiving frees the single slot.
        assert_eq!(rx.recv().await.unwrap().id, 1);
        tx.try_send(Msg::new(Trigger, 3)).unwrap();
        // Drain so the receiver is empty, then drop it: sends now close.
        assert_eq!(rx.recv().await.unwrap().id, 3);
        drop(rx);
        assert!(matches!(
            tx.try_send(Msg::new(Trigger, 4)),
            Err(TrySendError::Closed(_))
        ));
    }

    #[tokio::test]
    async fn try_recv_if_pops_only_on_match() {
        let (tx, mut rx) = channel::<Msg>(8);
        tx.send(Msg::new(Trigger, 1)).await.unwrap();
        // Predicate rejects -> leaves it queued.
        assert!(rx.try_recv_if(|m| m.id == 99).is_none());
        // Predicate accepts -> pops it.
        assert_eq!(rx.try_recv_if(|m| m.id == 1).unwrap().id, 1);
        // Empty -> None.
        assert!(rx.try_recv_if(|_| true).is_none());
    }

    #[tokio::test]
    async fn try_recv_if_only_considers_the_top_priority_entry() {
        let (tx, mut rx) = channel::<Msg>(8);
        // A lower-priority SubagentFinished queued behind a Trigger.
        tx.send(Msg::new(SubagentFinished, 1)).await.unwrap();
        tx.send(Msg::new(Trigger, 2)).await.unwrap();
        // Top is the Trigger: a Trigger-only predicate pops it.
        assert_eq!(rx.try_recv_if(|m| m.pri == Trigger).unwrap().id, 2);
        // Now the top is the SubagentFinished: a Trigger-only predicate leaves it.
        assert!(rx.try_recv_if(|m| m.pri == Trigger).is_none());
        // Still queued, served by a normal recv.
        assert_eq!(rx.recv().await.unwrap().id, 1);
    }

    #[tokio::test]
    async fn try_recv_distinguishes_empty_from_disconnected() {
        let (tx, mut rx) = channel::<Msg>(8);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
        tx.send(Msg::new(Trigger, 1)).await.unwrap();
        assert_eq!(rx.try_recv().unwrap().id, 1);
        drop(tx);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Disconnected));
    }

    #[tokio::test]
    async fn send_blocks_until_capacity_frees() {
        let (tx, mut rx) = channel::<Msg>(1);
        tx.send(Msg::new(Trigger, 1)).await.unwrap();
        let tx2 = tx.clone();
        let blocked = tokio::spawn(async move { tx2.send(Msg::new(Trigger, 2)).await });
        // The second send cannot complete until we receive the first.
        assert_eq!(rx.recv().await.unwrap().id, 1);
        blocked.await.unwrap().unwrap();
        assert_eq!(rx.recv().await.unwrap().id, 2);
    }
}
