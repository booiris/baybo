//! `TurnLifecycle` — thin orchestration layer over `TurnStore` for the
//! Turn state machine. See `docs/modules/turn.md` for the design.

use std::sync::Arc;

#[cfg(test)]
use crate::TurnStatus;
use crate::cancellation_registry::{TurnCancellationGuard, TurnCancellationRegistry};
use crate::{
    CancelReason, Turn, TurnError, TurnInput, TurnInputKind, TurnStatusKind, TurnTransition,
};
use baybo_model::{SessionId, SpanId, TriggerKind, TurnId};
use baybo_store::TurnStore;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

type Result<T> = std::result::Result<T, TurnError>;

/// Process-wide capacity for the lifecycle-event broadcast bus.
///
/// Sized to absorb the burst a multi-turn subagent fan-out can produce
/// without lagging the parent's wait loop. Subscribers that lag still
/// reconcile against the store (`list_by_session` /
/// `active_turn_started_at`) so dropped events don't cause lost
/// terminations or stale turn state — capacity is a latency knob, not
/// correctness.
const TURN_LIFECYCLE_EVENT_CAPACITY: usize = 256;

/// Which lifecycle transition a [`TurnLifecycleEvent`] marks. The bus
/// publishes the `Pending → InProgress` **start** edge and the three
/// terminal edges; the intermediate `stuck` / `recover` transitions are
/// not published (rare maintenance moves; the store stays the source of
/// truth for them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnPhase {
    Started,
    /// `reply_ordinal` is the persisted `session_messages.ordinal` of the
    /// completing turn's reply, carried so the push dispatcher reads exactly that
    /// row off the event (no read-after-write poll). `None` when the completed
    /// turn produced no message reply (maintenance / subagent) or ran with no
    /// durable store — the ordinal only ever rides this edge, so the absence
    /// lives on the variant, not as a field on every other phase.
    Completed {
        reply_ordinal: Option<i64>,
    },
    Failed,
    Cancelled,
}

impl TurnPhase {
    /// The terminal turn status this phase denotes, or `None` for
    /// [`TurnPhase::Started`] — lets a terminal-only consumer (the
    /// subagent waiter) filter out the start edge in one match.
    pub fn terminal_status(&self) -> Option<TurnStatusKind> {
        match self {
            TurnPhase::Started => None,
            TurnPhase::Completed { .. } => Some(TurnStatusKind::Completed),
            TurnPhase::Failed => Some(TurnStatusKind::Failed),
            TurnPhase::Cancelled => Some(TurnStatusKind::Cancelled),
        }
    }
}

/// A turn lifecycle transition published by `TurnLifecycle` on the
/// broadcast bus: the `Pending → InProgress` start edge and the three
/// terminal edges. Carries the minimum identifiers a subscriber needs to
/// filter without going back to the store: the `TurnId`, its session, and
/// the optional parent for hierarchy-scoped waits (subagent path).
///
/// Two consumers today: the subagent runtime waits for a child's terminal
/// edge (`phase.terminal_status()`), and the turn-state projector
/// recomputes a session's turn activity on any edge (it ignores `phase` —
/// every edge just means "recompute this session now").
#[derive(Debug, Clone)]
pub struct TurnLifecycleEvent {
    pub turn_id: TurnId,
    pub session_id: SessionId,
    pub parent_turn_id: Option<TurnId>,
    pub phase: TurnPhase,
    /// Input class of the turn. Lets the push dispatcher filter to real user
    /// turns (`kind == UserChat`) without re-fetching the Turn.
    pub kind: TurnInputKind,
}

/// Owns the turn state machine + persistence orchestration. Pure
/// passthrough to `Turn`'s internal transition validation —
/// `TurnLifecycle` itself contains no state machine logic.
pub struct TurnLifecycle {
    store: Arc<dyn TurnStore>,
    /// Process-wide registry of `TurnId → CancellationToken` for
    /// in-flight turns. `cancel()` trips the matching token (if any)
    /// in addition to flipping the DB row.
    cancellation: Arc<TurnCancellationRegistry>,
    /// Fire-and-forget bus for lifecycle transitions (start + terminal).
    /// The subagent runtime subscribes to unblock the parent on a child's
    /// terminal edge; the turn-state projector subscribes to drive the
    /// web chat's per-session turn activity off both edges.
    lifecycle_events: broadcast::Sender<TurnLifecycleEvent>,
}

impl TurnLifecycle {
    pub fn new(store: Arc<dyn TurnStore>) -> Self {
        let (lifecycle_events, _rx) = broadcast::channel(TURN_LIFECYCLE_EVENT_CAPACITY);
        Self {
            store,
            cancellation: Arc::new(TurnCancellationRegistry::new()),
            lifecycle_events,
        }
    }

    /// Subscribe to lifecycle events (start + terminal). Subscribers must
    /// reconcile against the store on
    /// `broadcast::error::RecvError::Lagged` — a dropped event is not
    /// re-published (the subagent waiter re-checks via the store; the
    /// turn-state projector recomputes on the next event, with the
    /// Subscribe snapshot covering the gap).
    pub fn subscribe_lifecycle_events(&self) -> broadcast::Receiver<TurnLifecycleEvent> {
        self.lifecycle_events.subscribe()
    }

    /// Register an in-flight turn's cancellation token. The returned
    /// guard unregisters on drop, so the agent loop's early-`?` paths
    /// can't leak entries. `cancel(turn_id, ...)` while the guard is
    /// alive will trip the token (and only afterwards flip the DB
    /// row), so the in-flight execution sees the cancel before
    /// terminal-state observers do.
    pub fn register_running(
        &self,
        turn_id: TurnId,
        token: CancellationToken,
    ) -> TurnCancellationGuard {
        self.cancellation.register(turn_id, token);
        TurnCancellationGuard::new(Arc::clone(&self.cancellation), turn_id)
    }

    /// Create a new turn in `Pending`.
    ///
    /// `origin` is the root trigger kind of the owning session — stored
    /// on the turn as-is (see `baybo_turn::kind`), independent of `input`.
    ///
    /// **Production code does not call this directly** — it goes
    /// through `agent::scope::with_turn` (which builds a `TurnSpec`
    /// and owns the full create→start→run→complete chain) so the
    /// cancel state machine can't be skipped. The method is left
    /// `pub` only for tests in this crate (and downstream test
    /// fixtures) that need to construct turns in arbitrary states.
    pub async fn start_turn(
        &self,
        session_id: SessionId,
        origin: TriggerKind,
        input: TurnInput,
        parent_turn_id: Option<TurnId>,
    ) -> Result<Turn> {
        let turn = Turn::new(session_id, origin, input, parent_turn_id);
        self.store.create(&turn.to_row()?).await?;
        Ok(turn)
    }

    /// Move `Pending → InProgress`. Publishes a [`TurnPhase::Started`]
    /// event — the start edge the turn-state projector turns into the
    /// web chat's "turn began".
    pub async fn start(&self, turn_id: &TurnId) -> Result<()> {
        let turn = self.apply(turn_id, |j| j.start()).await?;
        self.persist_and_publish(turn, TurnPhase::Started).await
    }

    /// Move `InProgress → Completed` with a final output.
    pub async fn complete(&self, turn_id: &TurnId, output: crate::TurnOutput) -> Result<()> {
        // The reply's persisted ordinal rides the Completed edge so push reads
        // exactly that row. `None` for a non-message output (maintenance /
        // subagent) or an unpersisted reply.
        let reply_ordinal = match &output {
            crate::TurnOutput::Message { ordinal, .. } => *ordinal,
            crate::TurnOutput::Structured { .. } => None,
        };
        let turn = self.apply(turn_id, |j| j.complete(output)).await?;
        self.persist_and_publish(turn, TurnPhase::Completed { reply_ordinal })
            .await
    }

    /// Move to `Failed { reason }`.
    pub async fn fail(&self, turn_id: &TurnId, reason: impl Into<String>) -> Result<()> {
        let reason = reason.into();
        let turn = self.apply(turn_id, |j| j.fail(&reason)).await?;
        self.persist_and_publish(turn, TurnPhase::Failed).await
    }

    /// Move to `Cancelled { reason, partial_artifacts }`.
    ///
    /// Trips the in-flight cancellation token (if any) before flipping
    /// the DB row, so the running execution observes the cancel and
    /// can short-circuit instead of running to completion.
    ///
    /// Idempotent on terminal turns: a cancel that races against natural
    /// completion returns `Ok(())` without touching the row, so admin
    /// callers don't see a 500 for a turn that finished a moment earlier.
    pub async fn cancel(
        &self,
        turn_id: &TurnId,
        reason: CancelReason,
        partial_artifacts: Vec<SpanId>,
    ) -> Result<()> {
        self.cancellation.cancel(turn_id);
        let turn = self.load_turn(turn_id).await?;
        if turn.is_terminal() {
            return Ok(());
        }
        let turn = self
            .apply(turn_id, |j| j.cancel(reason, partial_artifacts.clone()))
            .await?;
        self.persist_and_publish(turn, TurnPhase::Cancelled).await
    }

    /// Wait (bounded by `timeout`) until `turn_id`'s in-flight run has fully
    /// unwound — its `with_turn` scope exited, so any partial row the cancelled
    /// turn persists is already durable. `/stop` calls this after `cancel` so
    /// its control events anchor after the cancelled turn's last row instead
    /// of racing it. No-op once the run has deregistered.
    pub async fn wait_until_idle(&self, turn_id: &TurnId, timeout: std::time::Duration) {
        self.cancellation.wait_until_idle(turn_id, timeout).await;
    }

    /// Boot-time recovery cancel. Same state-machine semantics as
    /// [`Self::cancel`], but `ended_at` is set to the supplied `at`
    /// (typically `max(child_step.ended_at)`) instead of `Utc::now()` —
    /// the process may have crashed long before the next start, so using
    /// the wall-clock at recovery time would distort duration metrics.
    ///
    /// No registered token to trip (the turn is by definition no longer
    /// running), so this skips the `cancellation.cancel` call.
    pub async fn cancel_at(
        &self,
        turn_id: &TurnId,
        reason: CancelReason,
        partial_artifacts: Vec<SpanId>,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        let turn = self.load_turn(turn_id).await?;
        if turn.is_terminal() {
            return Ok(());
        }
        let turn = self
            .apply(turn_id, |j| {
                j.cancel_at(reason, partial_artifacts.clone(), at)
            })
            .await?;
        self.persist_and_publish(turn, TurnPhase::Cancelled).await
    }

    /// Move to `Stuck { reason }` from `InProgress`.
    pub async fn stuck(&self, turn_id: &TurnId, reason: impl Into<String>) -> Result<()> {
        let reason = reason.into();
        let turn = self.apply(turn_id, |j| j.stuck(&reason)).await?;
        self.persist(turn).await
    }

    /// Move `Stuck → InProgress`.
    pub async fn recover(&self, turn_id: &TurnId, reason: impl Into<String>) -> Result<()> {
        let reason = reason.into();
        let turn = self.apply(turn_id, |j| j.recover(&reason)).await?;
        self.persist(turn).await
    }

    pub async fn get(&self, turn_id: &TurnId) -> Result<Option<Turn>> {
        self.store
            .get(turn_id)
            .await?
            .map(Turn::from_row)
            .transpose()
    }

    /// List turns, optionally filtered by status discriminator. Newest
    /// `created_at` first.
    pub async fn list(&self, status: Option<TurnStatusKind>) -> Result<Vec<Turn>> {
        let mut rows = match status {
            Some(k) => self.store.list_by_status_kind(k.as_snake_case()).await?,
            None => self.store.list_all().await?,
        };
        rows.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        rows.into_iter().map(Turn::from_row).collect()
    }

    /// List every non-terminal turn (`Pending` / `InProgress` / `Stuck`)
    /// in one query. One trip to the store instead of three —
    /// `QueryApi::find_recoverable_turns` used to call `list(Some(k))`
    /// for each kind serially.
    pub async fn list_recoverable(&self) -> Result<Vec<Turn>> {
        self.store
            .list_recoverable()
            .await?
            .into_iter()
            .map(Turn::from_row)
            .collect()
    }

    /// List turns scoped to one session. Hits the `idx_turns_session`
    /// index instead of scanning the full table. Newest first.
    pub async fn list_by_session(
        &self,
        session_id: &baybo_model::SessionId,
        status: Option<TurnStatusKind>,
    ) -> Result<Vec<Turn>> {
        let mut turns = self
            .store
            .list_by_session(session_id)
            .await?
            .into_iter()
            .map(Turn::from_row)
            .collect::<Result<Vec<_>>>()?;
        if let Some(k) = status {
            turns.retain(|j| j.status.kind() == k);
        }
        turns.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        Ok(turns)
    }

    /// Per-session turn aggregates (count + latest status kind) in one
    /// grouped store query — list surfaces use this instead of a
    /// `list_by_session` fan-out across every session.
    pub async fn session_turn_stats(&self) -> Result<Vec<baybo_store::SessionTurnStats>> {
        Ok(self.store.session_turn_stats().await?)
    }

    /// Number of turns in the given status — a SQL COUNT, no rows
    /// materialised.
    pub async fn count_by_status(&self, status: TurnStatusKind) -> Result<usize> {
        Ok(self
            .store
            .count_by_status_kind(status.as_snake_case())
            .await?)
    }

    /// One page of turns (newest `created_at` first) plus the total
    /// matching count — paging lives in SQL, so a page never
    /// materialises the whole table.
    pub async fn list_page(
        &self,
        status: Option<TurnStatusKind>,
        limit: usize,
        offset: usize,
    ) -> Result<(Vec<Turn>, usize)> {
        let (rows, total) = self
            .store
            .list_page(status.map(|k| k.as_snake_case()), limit, offset)
            .await?;
        let turns = rows
            .into_iter()
            .map(Turn::from_row)
            .collect::<Result<Vec<_>>>()?;
        Ok((turns, total))
    }

    /// Non-terminal turns for one session, status-filtered at the store rather
    /// than loaded whole and retained. Used by `/stop` to find the in-flight
    /// turn + background children without a full-history load on a long-lived
    /// session.
    pub async fn list_active_by_session(
        &self,
        session_id: &baybo_model::SessionId,
    ) -> Result<Vec<Turn>> {
        self.store
            .list_active_by_session(session_id)
            .await?
            .into_iter()
            .map(Turn::from_row)
            .collect()
    }

    /// Every session with a turn still writing, as one grouped query.
    ///
    /// The dream pass asks this to leave those conversations alone: reading
    /// one now consolidates half an exchange, and the rows the turn appends
    /// afterwards are `MessageSource::Agent`, so no human-message predicate
    /// would ever offer them again. Unfiltered by kind on purpose — a
    /// `/compact` or a background-notification turn appends rows just as a
    /// chat turn does, and it is the appending, not the visibility, that
    /// makes reading early lossy.
    pub async fn sessions_with_live_turns(
        &self,
    ) -> Result<std::collections::HashSet<baybo_model::SessionId>> {
        Ok(self
            .store
            .sessions_with_live_turns()
            .await?
            .into_iter()
            .collect())
    }

    /// Non-terminal turns for one session that represent user-visible
    /// turn activity. This centralizes the `Turn::is_chat_turn` filter so
    /// callers such as `/stop`, crash recovery, and TurnState projection
    /// do not each re-define "active reply" for themselves. A `/compact`
    /// has its own input kind, so it is correctly excluded here.
    pub async fn list_active_chat_turns_by_session(
        &self,
        session_id: &baybo_model::SessionId,
    ) -> Result<Vec<Turn>> {
        Ok(self
            .list_active_by_session(session_id)
            .await?
            .into_iter()
            .filter(|j| j.is_chat_turn())
            .collect())
    }

    /// When the session has a turn in flight (a non-terminal chat turn — see
    /// [`Turn::is_chat_turn`]), the instant it started. Chat
    /// surfaces use this to tell a late-joining client "a reply is being
    /// produced since T"; `None` means the session is idle. A still-
    /// `Pending` turn reports its `created_at` (queued counts as in
    /// flight from the user's point of view).
    pub async fn active_turn_started_at(
        &self,
        session_id: &baybo_model::SessionId,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
        let turns = self.list_active_chat_turns_by_session(session_id).await?;
        Ok(turns
            .into_iter()
            .map(|j| j.started_at.unwrap_or(j.created_at))
            .max())
    }

    /// Turn bounds for a bounded list of sessions, one grouped store query —
    /// see [`baybo_store::TurnStore::session_turn_bounds`]. A pass-through:
    /// this carries no lifecycle state of its own, but the store handle lives
    /// here and callers (the subagent list surface) hold a `TurnLifecycle`.
    pub async fn session_turn_bounds(
        &self,
        session_ids: &[baybo_model::SessionId],
    ) -> Result<Vec<baybo_store::SessionTurnBounds>> {
        Ok(self.store.session_turn_bounds(session_ids).await?)
    }

    /// Direct children of `parent_turn_id` (one level). Used by `/stop`'s
    /// subtree walk to stamp `UserStopped` on (and back-stop the cancellation
    /// of) in-flight descendant turns such as foreground subagents. Foreground
    /// children are descendants of the turn's loop cancel token, so cancelling
    /// the turn already cascades into them — this walk wins the
    /// `UserStopped`-vs-`ParentCancelled` reason race and guards any future
    /// child that re-anchors off the actor token.
    pub async fn list_children(&self, parent_turn_id: &TurnId) -> Result<Vec<Turn>> {
        self.store
            .list_children(parent_turn_id)
            .await?
            .into_iter()
            .map(Turn::from_row)
            .collect()
    }

    async fn apply<F>(&self, turn_id: &TurnId, f: F) -> Result<Turn>
    where
        F: FnOnce(&mut Turn) -> Result<TurnTransition>,
    {
        let mut turn = self.load_turn(turn_id).await?;
        // The state machine returns a legality receipt for the edge; the
        // edge has already mutated `turn`, and the per-transition audit
        // table (`turn_transitions`) was retired in the 2026-07
        // unused-column audit, so the receipt is dropped here.
        let _transition = f(&mut turn)?;
        Ok(turn)
    }

    async fn persist(&self, turn: Turn) -> Result<()> {
        self.store.save(&turn.to_row()?).await?;
        Ok(())
    }

    /// Persist a transition, then fire its lifecycle event on the
    /// broadcast bus. Publish happens **after** the store write succeeds
    /// so a subscriber that races back into the lifecycle (e.g. to look
    /// the turn up by id, or to recompute `active_turn_started_at`) is
    /// guaranteed to see the new row. Send errors are dropped on the
    /// floor — broadcast `send` only fails when there are no subscribers,
    /// which is the normal state for a process with no live chat tab and
    /// no in-flight subagent.
    async fn persist_and_publish(&self, turn: Turn, phase: TurnPhase) -> Result<()> {
        let event = TurnLifecycleEvent {
            turn_id: turn.id,
            session_id: turn.session_id.clone(),
            parent_turn_id: turn.parent_turn_id,
            phase,
            kind: turn.input_kind(),
        };
        self.persist(turn).await?;
        let _ = self.lifecycle_events.send(event);
        Ok(())
    }

    async fn load_turn(&self, turn_id: &TurnId) -> Result<Turn> {
        self.store
            .get(turn_id)
            .await?
            .map(Turn::from_row)
            .transpose()?
            .ok_or_else(|| TurnError::NotFound(format!("turn {turn_id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::MemoryTurnStore;
    use baybo_model::ContentBlock;

    fn user_chat_input() -> TurnInput {
        TurnInput::UserChat {
            content: vec![ContentBlock::Text("hi".into())],
        }
    }

    fn dummy_output() -> crate::TurnOutput {
        crate::TurnOutput::Message {
            content: vec![ContentBlock::Text("ok".into())],
            ordinal: None,
        }
    }

    fn make_lifecycle() -> TurnLifecycle {
        TurnLifecycle::new(Arc::new(MemoryTurnStore::new()))
    }

    #[tokio::test]
    async fn full_success_path() {
        let lc = make_lifecycle();
        let turn = lc
            .start_turn(
                SessionId::from("s1"),
                TriggerKind::User,
                user_chat_input(),
                None,
            )
            .await
            .unwrap();
        assert!(matches!(turn.status, TurnStatus::Pending));

        lc.start(&turn.id).await.unwrap();
        lc.complete(&turn.id, dummy_output()).await.unwrap();

        let done = lc.get(&turn.id).await.unwrap().expect("turn exists");
        assert!(matches!(done.status, TurnStatus::Completed));
    }

    #[tokio::test]
    async fn cancel_trips_registered_token_and_unregisters_via_guard() {
        let lc = make_lifecycle();
        let turn = lc
            .start_turn(
                SessionId::from("s1"),
                TriggerKind::User,
                user_chat_input(),
                None,
            )
            .await
            .unwrap();
        lc.start(&turn.id).await.unwrap();

        let token = CancellationToken::new();
        let guard = lc.register_running(turn.id, token.clone());
        assert!(!token.is_cancelled());

        // External cancel observes the token before flipping the row.
        lc.cancel(&turn.id, CancelReason::UserPreempt, vec![])
            .await
            .unwrap();
        assert!(token.is_cancelled(), "registered token must be tripped");

        // Drop the guard; subsequent cancels for the same turn_id find
        // no live token (the registry is empty).
        drop(guard);
        let unrelated_token = CancellationToken::new();
        // Cancelling an already-terminal turn is rejected by the state
        // machine, but we just want to assert the registry didn't keep
        // the entry alive past the guard's drop.
        let turn2 = lc
            .start_turn(
                SessionId::from("s2"),
                TriggerKind::User,
                user_chat_input(),
                None,
            )
            .await
            .unwrap();
        lc.start(&turn2.id).await.unwrap();
        let _turn2_guard = lc.register_running(turn2.id, unrelated_token.clone());
        // Cancelling turn (already terminal) should not trip turn2's token.
        let _ = lc.cancel(&turn.id, CancelReason::UserPreempt, vec![]).await;
        assert!(
            !unrelated_token.is_cancelled(),
            "cancel of unrelated turn_id must not trip our token"
        );
    }

    #[tokio::test]
    async fn origin_recorded_as_is_for_any_input_under_any_trigger() {
        // origin is the session's root trigger, recorded honestly and
        // independent of the input payload. The subagent dispatch path
        // relies on a Spawned input working under any inherited root
        // trigger; more broadly, no input/trigger pairing is rejected.
        let lc = make_lifecycle();
        for trigger in [TriggerKind::User, TriggerKind::Cron] {
            let turn = lc
                .start_turn(
                    SessionId::from(format!("child-of-{trigger:?}")),
                    trigger,
                    TurnInput::Spawned {
                        initial_prompt: vec![ContentBlock::Text("task".into())],
                    },
                    None,
                )
                .await
                .expect("any input is allowed under any root trigger");
            assert_eq!(turn.origin, trigger);
            assert_eq!(turn.input_kind(), crate::TurnInputKind::Spawned);
        }
    }

    #[tokio::test]
    async fn user_chat_input_under_cron_session_records_cron_origin() {
        // Input kind and origin are recorded side by side and independently:
        // a UserChat payload in a Cron-trigger session yields input kind =
        // UserChat, origin = Cron, with no payload/trigger constraint.
        let lc = make_lifecycle();
        let turn = lc
            .start_turn(
                SessionId::from("cron-session"),
                TriggerKind::Cron,
                TurnInput::UserChat {
                    content: vec![ContentBlock::Text("hi".into())],
                },
                None,
            )
            .await
            .expect("input no longer constrained by trigger");
        assert_eq!(turn.origin, TriggerKind::Cron);
        assert_eq!(turn.input_kind(), crate::TurnInputKind::UserChat);
    }

    #[tokio::test]
    async fn active_turns_exclude_compact_turns() {
        let lc = make_lifecycle();
        let session_id = SessionId::from("s1");
        let turn = lc
            .start_turn(
                session_id.clone(),
                TriggerKind::User,
                user_chat_input(),
                None,
            )
            .await
            .unwrap();
        lc.start(&turn.id).await.unwrap();
        let compact = lc
            .start_turn(
                session_id.clone(),
                TriggerKind::User,
                TurnInput::Compact,
                None,
            )
            .await
            .unwrap();
        lc.start(&compact.id).await.unwrap();

        let all_active = lc.list_active_by_session(&session_id).await.unwrap();
        assert_eq!(all_active.len(), 2);

        let turns = lc
            .list_active_chat_turns_by_session(&session_id)
            .await
            .unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].id, turn.id);
        assert_eq!(
            lc.active_turn_started_at(&session_id).await.unwrap(),
            turns[0].started_at
        );
    }

    #[tokio::test]
    async fn cancel_on_terminal_turn_is_noop() {
        let lc = make_lifecycle();
        let turn = lc
            .start_turn(
                SessionId::from("s1"),
                TriggerKind::User,
                user_chat_input(),
                None,
            )
            .await
            .unwrap();
        lc.start(&turn.id).await.unwrap();
        lc.complete(&turn.id, dummy_output()).await.unwrap();
        // Race: an admin cancel arriving after natural completion must
        // not return InvalidTransition — it's a benign "already done".
        lc.cancel(&turn.id, CancelReason::OperatorCancel, vec![])
            .await
            .expect("cancel of terminal turn is no-op");
        let loaded = lc.get(&turn.id).await.unwrap().unwrap();
        assert!(matches!(loaded.status, TurnStatus::Completed));
    }

    #[tokio::test]
    async fn cancel_with_partial_artifacts() {
        let lc = make_lifecycle();
        let turn = lc
            .start_turn(
                SessionId::from("s1"),
                TriggerKind::User,
                user_chat_input(),
                None,
            )
            .await
            .unwrap();
        lc.start(&turn.id).await.unwrap();
        let span = SpanId::new();
        lc.cancel(&turn.id, CancelReason::UserPreempt, vec![span])
            .await
            .unwrap();
        let loaded = lc.get(&turn.id).await.unwrap().unwrap();
        match loaded.status {
            TurnStatus::Cancelled {
                reason,
                partial_artifacts,
            } => {
                assert_eq!(reason, CancelReason::UserPreempt);
                assert_eq!(partial_artifacts, vec![span]);
            }
            _ => panic!("expected Cancelled"),
        }
    }
}
