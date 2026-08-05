use std::collections::HashMap;
use std::sync::Arc;

use baybo_context::prompts::cron::{DreamDigestGroup, DreamDigestSession, frame_dream_digest};
use baybo_context::prompts::soul::{PersonaSources, PromptBudget, assemble_for};
use baybo_context::tokenizer::TiktokenTokenizer;
use baybo_cron::{CronTriggerEvent, ExecutionCompletion};
use baybo_model::{
    AgentBinding, AgentFramework, AgentProfileId, BuiltinCronJob, CronExecution, ExecutionOutcome,
    PendingCronResult, Session, SessionId, TriggerSource, User,
};
use baybo_session::SessionManager;
use baybo_store::CronStore;
use baybo_turn::{TurnInputKind, TurnLifecycle, TurnLifecycleEvent, TurnPhase, TurnStatusKind};
use chrono::Utc;
use chrono_tz::Tz;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::actor::supervisor::AgentSupervisor;
use crate::actor::{AgentMessage, BuiltinFireContext, CronDelivery};

use super::{ActorSpawner, Router};

/// How far back a **first** dream pass looks. There is no previous fire to
/// bound the window, and "all of history" would hand the model every
/// conversation the deployment has ever had; a fortnight is enough to make
/// the first pass useful without making it a migration.
const DREAM_FIRST_PASS_LOOKBACK_DAYS: i64 = 14;

/// Most conversations one fire's digest will name.
///
/// The digest is one line per conversation and the model is invited to read
/// each — so an unbounded list is an unbounded prompt and an unbounded number
/// of `Read`s. The first pass after an upgrade is where this bites: it looks
/// back [`DREAM_FIRST_PASS_LOOKBACK_DAYS`] over a store that has been running
/// for months, and would otherwise hand over every conversation in a
/// fortnight at once.
///
/// Newest first, and the overflow is **not** dropped: the cursor only
/// advances over what was listed, so what does not fit here is offered again
/// next pass. The digest says how many it is holding back so the omission is
/// visible rather than inferred.
const DREAM_MAX_SESSIONS_PER_FIRE: usize = 40;

/// One agent's share of a dream pass: the digest its fire is given, and the
/// conversations that digest names.
///
/// `offered` is carried separately from the rendered digest because the
/// cursor must advance over exactly what was listed, and only once the fire
/// is dispatched — the digest is a string by then.
struct DreamGroup {
    binding: AgentBinding,
    context: BuiltinFireContext,
    /// `(session, highest ordinal listed)`, in digest order.
    offered: Vec<(SessionId, i64)>,
}

/// The two halves of a group as it is being accumulated, before the agent is
/// known to be one that dreams at all.
#[derive(Default)]
struct DreamAgentWork {
    sessions: Vec<DreamDigestSession>,
    offered: Vec<(SessionId, i64)>,
    /// Conversations past [`DREAM_MAX_SESSIONS_PER_FIRE`]. They are neither
    /// listed nor marked offered, so the next pass sees them again.
    held_back: usize,
}

impl Router {
    /// Handle a cron trigger by minting a fresh session and routing a
    /// `CronTrigger` message into a one-shot actor.
    ///
    /// Each fire creates an isolated session so the trigger sees a
    /// clean transcript and a fresh `SessionState` (no leaked
    /// `approved_resources` or compression state from prior fires).
    /// Continuity across fires belongs to memory + skill loading, not to a
    /// shared mutable transcript.
    ///
    /// What the user *sees* — and how the actor is managed — depends on the
    /// schedule:
    ///
    /// - **Recurring** — the fire session is a first-class conversation
    ///   (`conversation: true`), titled after the job, listed in the sidebar,
    ///   and its reply dispatches out through the channel as usual. Each fire
    ///   is its own conversation. Because the user can reply in it, its actor
    ///   is **registered** with the supervisor, so a reply routes to that actor
    ///   rather than forking a second one over the same transcript.
    /// - **One-shot** — the fire session is a private workspace: invisible,
    ///   unopenable, dispatching nothing, and its actor deliberately
    ///   **unregistered** (no follow-up traffic will ever arrive, so a handle
    ///   would just dangle in the supervisor's map). A waiter — spawned here,
    ///   before the trigger is sent, so the terminal event cannot be missed —
    ///   picks the result off the fire job's terminal lifecycle edge and
    ///   delivers it into the conversation that scheduled the job.
    ///
    /// Either way the actor gets `CronTrigger` followed by `ActorStop`; the
    /// priority mailbox serves the trigger first (`ActorStop` is the lowest
    /// tier), so the actor runs the fire and then exits.
    pub(super) async fn handle_cron_trigger(
        &mut self,
        event: CronTriggerEvent,
    ) -> anyhow::Result<()> {
        // A runtime-owned job may need fire-time material an ordinary one
        // does not — and the dream pass is not even one fire but one per
        // agent that has something to think about, so it takes its own path
        // entirely.
        match BuiltinCronJob::from_id(&event.job_id) {
            Some(BuiltinCronJob::Dream) => return self.fan_out_dream(&event).await,
            None => {}
        }

        // A recurring fire's session IS the notification, so it is a listed,
        // titled conversation; a one-shot's is a private workspace whose result
        // is reported elsewhere. Everything below forks on that one fact.
        let conversation = !event.one_shot;
        let session = self.mint_fire_session(&event, conversation).await?;

        debug!(
            session_id = %session.id,
            job_id = %event.job_id,
            one_shot = event.one_shot,
            "routing cron trigger to fresh session"
        );

        let trigger = cron_trigger(&event, None);
        if conversation {
            self.title_cron_conversation(&session.id, &event).await;
            // An ordinary fire has nothing downstream of delivery: the warn
            // inside is the whole report.
            let _routed = self.run_conversation_fire(session, trigger).await;
        } else {
            let waiter = self.cron_result_waiter(&event, session.id.clone());
            self.run_oneshot_fire(session, trigger, waiter).await;
        }
        Ok(())
    }

    /// Open one dream conversation per agent that has something to tend.
    ///
    /// Each agent keeps its own memory and its own identity files, and the
    /// write tier refuses one agent's writes into another's — so a single
    /// fire could only ever tend whichever agent it happened to run as. The
    /// pass is therefore per agent: each fire is bound to that agent and is
    /// shown only its own conversations. (Shown, not granted — the
    /// transcript resolver enforces nothing; see `SessionTranscriptReader`.)
    /// They share one job, one schedule and one cron group, so what the user
    /// sees is still a single pass.
    ///
    /// An agent with no activity gets no fire — an idle deployment spends
    /// nothing and leaves no empty conversation behind. When *no* agent has
    /// any, nothing is minted at all. The execution row is already recorded
    /// and `next_trigger_at` already advanced by the time we run, so
    /// skipping costs the ledger nothing.
    async fn fan_out_dream(&self, event: &CronTriggerEvent) -> anyhow::Result<()> {
        let groups = match self.dream_groups(event).await {
            Ok(groups) => groups,
            // The window is lost, not retried: the scheduler has already
            // advanced the job, so the next pass starts after this one.
            // Named as a lost window rather than an idle one, because the
            // two look identical from outside and only this one is a problem.
            Err(e) => {
                warn!(
                    job_id = %event.job_id,
                    error = %e,
                    "dream skipped: could not read what is unconsolidated; \
                     nothing is lost, the next pass sees the same work"
                );
                return Ok(());
            }
        };
        if groups.is_empty() {
            info!(
                job_id = %event.job_id,
                "dream skipped: nobody has said anything since the last pass"
            );
            return Ok(());
        }

        for group in groups {
            let binding = group.binding;
            // Degrade like every other failure in this pass: one agent's bad
            // luck must not cost the others their pass. Nothing is lost by
            // continuing — this agent's cursor stays where it is, so the next
            // pass offers the same conversations again.
            let session = match self.mint_dream_session(event, binding.clone()).await {
                Ok(session) => session,
                Err(e) => {
                    warn!(
                        agent_id = %binding.agent_id,
                        error = %e,
                        "dream skipped for this agent: could not open its conversation; \
                         its conversations stay queued for the next pass"
                    );
                    continue;
                }
            };
            debug!(
                session_id = %session.id,
                agent_id = %binding.agent_id,
                offered = group.offered.len(),
                "opening a dream conversation"
            );
            self.title_dream_conversation(&session.id, event, &binding.agent_id)
                .await;
            // Only a fire that actually reached a mailbox may move the
            // cursor. `route_or_spawn` returning false — a closed mailbox, a
            // spawn that failed — means this pass never happened for these
            // conversations, and stepping over them would be the one failure
            // in this path that loses content instead of costing a re-read.
            if self
                .run_conversation_fire(session, cron_trigger(event, Some(group.context)))
                .await
            {
                self.advance_dream_cursor(&group.offered).await;
            } else {
                warn!(
                    agent_id = %binding.agent_id,
                    offered = group.offered.len(),
                    "dream trigger was not delivered; its conversations stay queued for the next pass"
                );
            }
        }
        Ok(())
    }

    /// Title a dream conversation with the agent it tends, because one pass
    /// opens several and they would otherwise be N identical rows in the
    /// cron group the spec sells as a browsable journal.
    ///
    /// The name comes from the agent's own `IDENTITY.md` — the one source
    /// for what an agent is called — falling back to its id when it has not
    /// named itself yet.
    async fn title_dream_conversation(
        &self,
        session_id: &SessionId,
        event: &CronTriggerEvent,
        agent: &AgentProfileId,
    ) {
        let name = tokio::fs::read_to_string(
            agent.identity_file(&self.workspace, baybo_workspace::IdentityKind::Identity),
        )
        .await
        .ok()
        .and_then(|body| baybo_workspace::display_name(&body))
        .unwrap_or_else(|| agent.as_str().to_string());
        let titled = format!("{} · {name}", event.title);
        let title = cron_conversation_title(&titled, &event.timezone);
        if let Err(e) = self
            .session_manager
            .set_title(session_id, Some(&title))
            .await
        {
            warn!(session_id = %session_id, error = %e, "failed to title dream conversation");
        }
    }

    /// A dream fire's session, bound to the agent whose memory it tends.
    ///
    /// Unlike an ordinary fire this binding is not inherited from an origin
    /// conversation — the job has none — it *is* the thing being fanned out
    /// over.
    async fn mint_dream_session(
        &self,
        event: &CronTriggerEvent,
        binding: AgentBinding,
    ) -> anyhow::Result<Session> {
        let user = User {
            id: event.user_id.clone(),
            name: None,
            channel: event.channel.clone(),
        };
        Ok(self
            .session_manager
            .create_bound_session_with_trigger(
                user,
                event.channel.clone(),
                TriggerSource::Cron {
                    cron_job_id: event.job_id.clone(),
                    origin_session_id: None,
                    conversation: true,
                    job_title: Some(event.title.clone()),
                },
                Some(binding),
            )
            .await?)
    }

    /// What each agent has to work with this time round: one entry per
    /// agent with unconsolidated conversations, in no particular order.
    /// Empty when there is nothing anywhere.
    ///
    /// Selection is [`SessionStore::dream_candidates`]: an ordinal cursor per
    /// session, falling back to a time window for one never offered before.
    /// The window's lower bound is [`CronTriggerEvent::previous_fire_at`],
    /// which rides the execution row precisely because the job row's own
    /// `last_triggered_at` has already been advanced to *this* fire by the
    /// time we get here. A first-ever pass has no previous one, so it looks
    /// back over [`DREAM_FIRST_PASS_LOOKBACK_DAYS`] rather than over all of
    /// history.
    ///
    /// A session whose turn is still writing is left for the next pass.
    /// Reading it now would consolidate half an exchange and then advance
    /// past the rest — and the rest is `MessageSource::Agent`, which no
    /// human-message predicate would ever surface again. Deferring is only
    /// safe because the cursor is an ordinal this pass simply does not
    /// advance; a time cursor could not express "not yet".
    async fn dream_groups(&self, event: &CronTriggerEvent) -> anyhow::Result<Vec<DreamGroup>> {
        let until = Utc::now();
        let since = event
            .previous_fire_at
            .unwrap_or_else(|| until - chrono::Duration::days(DREAM_FIRST_PASS_LOOKBACK_DAYS));
        let candidates = self
            .session_manager
            .store()
            .dream_candidates(since, until)
            .await?;
        // Failing this query defers everything rather than reading anyway:
        // an unknown turn state plus a cursor that advances is how rows go
        // missing, and a deferral costs one skipped pass.
        let busy = self.turn_lifecycle.sessions_with_live_turns().await?;

        // Group by owning agent, preserving the store's newest-first order
        // within each group. An undecodable binding was already dropped by
        // the store — filing somebody's conversation under the wrong agent is
        // exactly what the partition exists to prevent.
        let mut order: Vec<AgentProfileId> = Vec::new();
        let mut by_agent: HashMap<AgentProfileId, DreamAgentWork> = HashMap::new();
        let mut deferred: Vec<SessionId> = Vec::new();
        for candidate in candidates {
            if busy.contains(&candidate.session_id) {
                // Deferral has no timeout on purpose — the safe direction is
                // to wait — but a turn that never settles would then skip
                // this conversation for the life of the process, silently.
                // Say so once per pass: a stuck turn is an operator problem,
                // and this is the only place that can see it as one.
                debug!(
                    session_id = %candidate.session_id,
                    "dream deferring a conversation whose turn is still writing"
                );
                deferred.push(candidate.session_id);
                continue;
            }
            let owner = candidate
                .agent_id
                .clone()
                .unwrap_or_else(AgentProfileId::builtin);
            let work = by_agent.entry(owner.clone()).or_insert_with(|| {
                order.push(owner.clone());
                DreamAgentWork::default()
            });
            if work.sessions.len() >= DREAM_MAX_SESSIONS_PER_FIRE {
                work.held_back += 1;
                continue;
            }
            work.sessions.push(DreamDigestSession {
                title: candidate
                    .title
                    .unwrap_or_else(|| candidate.session_id.as_str().to_string()),
                // Starting where this pass's reading starts, so a
                // conversation that has been running for months costs it
                // only what is new. The composer collapses ordinal 0 back to
                // the plain path, so a conversation with nothing consolidated
                // before it reads whole.
                transcript_path: self
                    .workspace
                    .session_log_file_from(
                        candidate.session_id.as_str(),
                        candidate.read_from_ordinal,
                    )
                    .display()
                    .to_string(),
                earlier_messages: candidate.read_from_ordinal,
                user_message_count: candidate.human_message_count,
                last_spoken_on: candidate.last_activity_at.format("%Y-%m-%d").to_string(),
            });
            work.offered
                .push((candidate.session_id, candidate.latest_ordinal));
        }

        if !deferred.is_empty() {
            warn!(
                count = deferred.len(),
                sessions = ?deferred,
                "dream deferred conversations that were mid-turn; a turn that never \
                 settles will keep deferring its conversation until the next restart"
            );
        }

        let mut groups = Vec::new();
        for agent in order {
            let Some(framework) = self.dream_framework(&agent).await else {
                continue;
            };
            let Some(work) = by_agent.remove(&agent) else {
                continue;
            };
            let digest = DreamDigestGroup {
                // Absolutised like every other producer of this string (the
                // prompt's own `<memory>` section, and the tools'
                // `ManagedRoots`), so a symlinked workspace root does not
                // hand the model a path that fails the tools' own prefix
                // test.
                memory_dir: baybo_workspace::absolutise(&agent.memory_dir(&self.workspace))
                    .display()
                    .to_string(),
                agent_label: agent.as_str().to_string(),
                sessions: work.sessions,
                held_back: work.held_back,
                budget: self.dream_prompt_budget(&agent).await,
            };
            let Some(digest) = frame_dream_digest(&digest) else {
                continue;
            };
            groups.push(DreamGroup {
                binding: AgentBinding {
                    agent_id: agent,
                    framework,
                },
                context: BuiltinFireContext {
                    prompt_context: digest,
                },
                offered: work.offered,
            });
        }
        Ok(groups)
    }

    /// Advance the dream cursor over everything a dispatched fire was shown.
    ///
    /// After dispatch, never before: every earlier failure — the digest, the
    /// mint, the store — then leaves the cursor untouched, so the next pass
    /// re-offers those conversations instead of stepping over them. What it
    /// records is "offered", not "consumed": the model chooses which of the
    /// listed transcripts to read, exactly as the time cursor advanced
    /// whether or not it read anything.
    async fn advance_dream_cursor(&self, offered: &[(SessionId, i64)]) {
        for (session_id, ordinal) in offered {
            if let Err(e) = self
                .session_manager
                .store()
                .set_dreamed_through_ordinal(session_id, *ordinal)
                .await
            {
                // Costs a re-read next pass, which is the direction this
                // whole cursor errs in anyway.
                warn!(
                    session_id = %session_id,
                    error = %e,
                    "failed to advance the dream cursor; this conversation will be offered again"
                );
            }
        }
    }

    /// What `agent`'s system prompt costs per call, for the pass to trim
    /// against. `None` when the assembly fails — the pass still has work to do
    /// without it, so a bad read degrades the instruction rather than the fire.
    ///
    /// Built-in memory is taken as on: the dream job is only ever seeded when
    /// it is (`ensure_dream_job`), so a fire reaching here has a memory tree.
    ///
    /// The count is an estimate — the encoding is picked here, not by the model
    /// the fire will actually run on. That is the right trade: this number
    /// steers how hard the pass trims, and being 5% out never changes that
    /// answer, while threading the fire's model down here would.
    async fn dream_prompt_budget(&self, agent: &AgentProfileId) -> Option<PromptBudget> {
        let sources = PersonaSources::for_agent(&self.workspace, agent, true);
        match assemble_for(&self.workspace, &sources).await {
            Ok(prompt) => Some(prompt.budget(&TiktokenTokenizer::for_model(""))),
            Err(e) => {
                warn!(agent_id = %agent, error = %e, "dream: cannot price the identity files; firing without a budget");
                None
            }
        }
    }

    /// The framework a dream fire for `agent` would run under, or `None`
    /// when it should not run at all.
    ///
    /// Only `baybo` agents dream. An external framework runs its own CLI
    /// with its own tools and never sees the `<memory>` injection at all, so
    /// firing the pass at one would spend a turn asking a stranger to tidy a
    /// room it cannot see.
    async fn dream_framework(&self, agent: &AgentProfileId) -> Option<AgentFramework> {
        if agent.is_builtin() {
            return Some(AgentFramework::default());
        }
        match self.agent_profiles.get(agent).await {
            Ok(Some(row)) if row.framework == AgentFramework::Baybo => Some(row.framework),
            Ok(Some(row)) => {
                debug!(
                    agent_id = %agent, framework = %row.framework.as_str(),
                    "skipping dream for a non-baybo agent: it never sees the memory injection"
                );
                None
            }
            // A profile row that is gone takes its memory tree out of reach
            // with it: the tier is keyed on the id, and nothing would read
            // what the pass wrote.
            Ok(None) => {
                debug!(agent_id = %agent, "skipping dream for an agent whose profile is gone");
                None
            }
            Err(e) => {
                warn!(agent_id = %agent, error = %e, "skipping dream: cannot read the agent profile");
                None
            }
        }
    }

    /// The isolated session this fire runs in.
    async fn mint_fire_session(
        &self,
        event: &CronTriggerEvent,
        conversation: bool,
    ) -> anyhow::Result<Session> {
        let user = User {
            id: event.user_id.clone(),
            name: None,
            channel: event.channel.clone(),
        };
        let session = self
            .session_manager
            .create_bound_session_with_trigger(
                user,
                event.channel.clone(),
                TriggerSource::Cron {
                    cron_job_id: event.job_id.clone(),
                    origin_session_id: event.origin_session_id.clone(),
                    conversation,
                    job_title: Some(event.title.clone()),
                },
                self.inherited_binding(event.origin_session_id.as_ref())
                    .await?,
            )
            .await?;
        Ok(session)
    }

    /// The agent a fire runs as: the one bound to the conversation that
    /// scheduled the job.
    ///
    /// Derived at fire time from the origin session rather than stored on the
    /// job — the same rule as cron grouping (`docs/cron-groups.md`), and it
    /// means re-binding is impossible to get out of step because there is
    /// nothing to keep in step.
    ///
    /// `Ok(None)` covers the two ways a fire legitimately has no agent: the
    /// job recorded no origin (created before origins were stamped), or the
    /// origin is itself unbound and so reads as the built-in.
    ///
    /// A recorded origin that **cannot be read** is an error, not a third
    /// flavour of "no agent". Session rows are never deleted, so a missing
    /// one means the store is inconsistent — and running the fire unbound
    /// would answer in the wrong persona, write into the wrong memory
    /// partition, and leave nothing but a `warn!` to say so. Failing the
    /// mint surfaces it and leaves the schedule to retry.
    async fn inherited_binding(
        &self,
        origin: Option<&SessionId>,
    ) -> anyhow::Result<Option<AgentBinding>> {
        let Some(origin) = origin else {
            return Ok(None);
        };
        let session = self
            .session_manager
            .get(origin)
            .await
            .map_err(|e| anyhow::anyhow!("read cron origin session {origin}: {e}"))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "cron origin session {origin} is missing; session rows are never deleted, \
                     so refusing to fire this job as some other agent"
                )
            })?;
        Ok(session.state.agent_id.map(|agent_id| AgentBinding {
            agent_id,
            framework: session.state.agent_framework.unwrap_or_default(),
        }))
    }

    /// The task that will report a one-shot's result to the conversation that
    /// scheduled it.
    ///
    /// Built — and therefore **subscribed** — before the trigger is sent: the
    /// fire's terminal event is published from inside its own turn, so a
    /// subscription opened afterwards could miss it entirely.
    fn cron_result_waiter(
        &self,
        event: &CronTriggerEvent,
        fire_session_id: SessionId,
    ) -> CronResultWaiter {
        CronResultWaiter {
            event: event.clone(),
            fire_session_id,
            lifecycle: Arc::clone(&self.turn_lifecycle),
            terminal_rx: self.turn_lifecycle.subscribe_lifecycle_events(),
            cron_store: Arc::clone(&self.cron_store),
            delivery: self.cron_result_delivery(),
        }
    }

    /// A **recurring** fire: it owns a conversation the user can reply in, so
    /// its actor is **registered** with the supervisor and **not stopped**.
    ///
    /// Registration is what guarantees the session has exactly one actor: a
    /// reply routes to this one (`route_or_spawn` finds the entry) instead of
    /// forking a second over the same transcript, and the actor deregisters the
    /// id it registered rather than evicting somebody else's handle.
    ///
    /// No `ActorStop` rides behind the trigger, unlike a one-shot's. Stopping
    /// the actor the instant the fire ends is worst exactly when it matters
    /// most — the moment a notification lands is when a user is most likely to
    /// reply — and a reply that raced the stop would be routed into a mailbox
    /// nobody is reading, reported as delivered, and dropped. The actor stays
    /// resident and is reclaimed by the idle reaper, like every conversation's.
    ///
    /// Returns whether the trigger actually reached a mailbox. A dropped one
    /// means this fire never happened, which the dream pass has to know: its
    /// cursor may only step over conversations a fire was really handed.
    #[must_use]
    async fn run_conversation_fire(&self, session: Session, trigger: AgentMessage) -> bool {
        let session_id = session.id.clone();
        // A fresh cron session has no `last_llm` of its own, so without this
        // a bound agent's scheduled work runs on the pool default rather than
        // the model its profile pins.
        let pins = super::resolve_spawn_pins(&session, &self.agent_profiles).await;
        let response_tx = self.supervisor.response_tx().clone();
        let parent_token = self.actor_parent_token.clone();
        let actor_spawner = self.actor_spawner.as_ref();
        let routed = self
            .supervisor
            .route_or_spawn(&session_id, trigger, || {
                let actor_token = parent_token.child_token();
                actor_spawner(
                    session,
                    pins.llm,
                    pins.model,
                    pins.effort,
                    response_tx,
                    actor_token,
                )
            })
            .await;
        if !routed {
            warn!(session_id = %session_id, "failed to deliver cron trigger to its conversation");
        }
        routed
    }

    /// A **one-shot** fire: it runs in a private workspace nobody will ever
    /// message again, so its actor is deliberately **unregistered** (a handle in
    /// the supervisor's map would only dangle) and is stopped as soon as the
    /// turn ends. `waiter` reports the result into the conversation that
    /// scheduled the job.
    ///
    /// The waiter is spawned **whatever happens here**, including when the actor
    /// is already gone. The scheduler has recorded this execution as dispatched,
    /// so nothing else will ever retry it: a fire abandoned here would be a
    /// reminder lost in silence. Tripping the actor's token instead makes the
    /// waiter resolve immediately and report the fire as failed.
    async fn run_oneshot_fire(
        &self,
        session: Session,
        trigger: AgentMessage,
        waiter: CronResultWaiter,
    ) {
        let session_id = session.id.clone();
        let pins = super::resolve_spawn_pins(&session, &self.agent_profiles).await;
        let response_tx = self.supervisor.response_tx().clone();
        let (mailbox, actor_token) = self.spawn_oneshot_actor(
            session,
            pins.llm,
            pins.model,
            pins.effort,
            response_tx,
            &self.actor_parent_token,
        );

        match mailbox.send(trigger).await {
            Ok(()) => {
                // Lowest mailbox priority, so it is served after the fire: the
                // workspace exits as soon as its one turn is done.
                if let Err(e) = mailbox.send(AgentMessage::ActorStop).await {
                    warn!(
                        session_id = %session_id,
                        error = %e,
                        "post-fire shutdown was not queued; the actor still exits when its mailbox drops",
                    );
                }
            }
            Err(e) => {
                warn!(
                    session_id = %session_id,
                    error = %e,
                    "cron fire could not reach its actor; reporting it as a failed fire",
                );
                actor_token.cancel();
            }
        }

        tokio::spawn(waiter.run(actor_token));
    }

    /// Re-deliver one-shot results whose fire completed but whose delivery
    /// never resolved — the process died between the origin's transcript
    /// append and the ledger stamp, or before the append happened at all.
    ///
    /// Idempotent by construction: the origin actor drops an execution it has
    /// already appended (and re-stamps the ledger), so a replay of a delivery
    /// that *did* land costs one no-op message rather than a duplicate row.
    /// Run at router start, before any live traffic.
    pub(super) async fn redrive_cron_deliveries(&self) {
        let awaiting = match self.cron_store.list_executions_awaiting_delivery().await {
            Ok(rows) => rows,
            Err(e) => {
                warn!(error = %e, "failed to scan for undelivered cron results");
                return;
            }
        };
        let pending: Vec<CronExecution> = awaiting
            .into_iter()
            .filter(CronExecution::is_one_shot)
            .collect();
        if pending.is_empty() {
            return;
        }
        info!(
            count = pending.len(),
            "re-driving cron results that never reached their conversation"
        );
        let delivery = self.cron_result_delivery();
        for exec in pending {
            let Some(result) = pending_result_from_execution(&exec) else {
                // No fire session recorded — nothing to read a result out of.
                // Resolve it rather than rescanning this row on every boot.
                warn!(
                    execution_id = %exec.id,
                    "undeliverable cron execution has no fire session; dropping"
                );
                delivery.resolve_ledger(&exec.id).await;
                continue;
            };
            delivery
                .deliver(exec.origin_session_id.clone(), result)
                .await;
        }
    }

    /// The handles a one-shot's result needs to reach its conversation.
    fn cron_result_delivery(&self) -> CronResultDelivery {
        CronResultDelivery {
            session_manager: Arc::clone(&self.session_manager),
            supervisor: self.supervisor.clone(),
            cron_store: Arc::clone(&self.cron_store),
            actor_spawner: Arc::clone(&self.actor_spawner),
            agent_profiles: Arc::clone(&self.agent_profiles),
            actor_parent_token: self.actor_parent_token.clone(),
        }
    }

    /// Name a recurring fire's conversation `{title} · {M/d}` — the fire date
    /// in the job's own timezone, so a job that fires at 09:00 Shanghai is
    /// dated by its local day rather than by UTC. Deterministic, so no LLM
    /// title pass is needed (and none would run: the titler is gated on a
    /// `User` trigger).
    async fn title_cron_conversation(&self, session_id: &SessionId, event: &CronTriggerEvent) {
        let title = cron_conversation_title(&event.title, &event.timezone);
        if let Err(e) = self
            .session_manager
            .set_title(session_id, Some(&title))
            .await
        {
            warn!(
                session_id = %session_id,
                error = %e,
                "failed to title cron conversation"
            );
        }
    }
}

/// Watches one one-shot fire to its terminal state, records the outcome on the
/// execution, and hands the result to the conversation that scheduled the job.
///
/// Mirrors the subagent waiter (`crate::actor::subagent`): a detached task on
/// the lifecycle bus, with a store reconcile for the cases the bus can't cover
/// (a lagged subscriber, or an actor that died before opening a job at all).
struct CronResultWaiter {
    event: CronTriggerEvent,
    fire_session_id: SessionId,
    lifecycle: Arc<TurnLifecycle>,
    terminal_rx: broadcast::Receiver<TurnLifecycleEvent>,
    cron_store: Arc<dyn CronStore>,
    delivery: CronResultDelivery,
}

impl CronResultWaiter {
    async fn run(mut self, actor_token: CancellationToken) {
        let outcome = self.await_fire(actor_token).await;
        let completed_at = Utc::now();

        // Record the outcome BEFORE delivering it: a crash in the delivery
        // window then leaves a durable "completed, not yet delivered" row for
        // the boot re-drive, instead of a result nobody remembers.
        if let Err(e) = self
            .cron_store
            .record_execution_completion(
                &self.event.execution_id,
                ExecutionCompletion {
                    fire_session_id: self.fire_session_id.clone(),
                    outcome: outcome.outcome,
                    reply_ordinal: outcome.reply_ordinal,
                    completed_at,
                },
            )
            .await
        {
            warn!(
                execution_id = %self.event.execution_id,
                error = %e,
                "failed to record cron fire completion; delivering anyway"
            );
        }

        let result = PendingCronResult {
            execution_id: self.event.execution_id.clone(),
            cron_job_id: self.event.job_id.clone(),
            job_title: self.event.title.clone(),
            fire_session_id: self.fire_session_id.clone(),
            reply_ordinal: outcome.reply_ordinal,
            outcome: outcome.outcome,
            failure_reason: outcome.failure_reason,
            completed_at,
        };

        self.delivery
            .deliver(self.event.origin_session_id.clone(), result)
            .await;
    }

    /// Wait for the fire's job to reach a terminal state.
    ///
    /// Three exits, all of which notify (silence is the one outcome a
    /// scheduled task must never have):
    /// - the job's terminal lifecycle event — the normal path;
    /// - a lagged bus — reconcile against the store;
    /// - the fire actor stopping without a terminal event (it died before
    ///   opening a job, e.g. the framed prompt could not be appended) — one
    ///   last store reconcile, then report a failure.
    async fn await_fire(&mut self, actor_token: CancellationToken) -> FireOutcome {
        loop {
            tokio::select! {
                event = self.terminal_rx.recv() => match event {
                    Ok(ev) if self.is_our_fire(&ev) => {
                        let Some(kind) = ev.phase.terminal_status() else {
                            continue;
                        };
                        return self.outcome_for(kind, reply_ordinal(&ev.phase)).await;
                    }
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(
                            session_id = %self.fire_session_id,
                            skipped = n,
                            "cron waiter lagged on the lifecycle bus; reconciling via store"
                        );
                        if let Some(outcome) = self.reconcile_via_store().await {
                            return outcome;
                        }
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        return FireOutcome::failed("job lifecycle bus closed before the fire finished");
                    }
                },
                _ = actor_token.cancelled() => {
                    // The actor is gone. Either it finished (and we simply
                    // haven't drained its terminal event yet) or it died
                    // before opening a job — the store tells us which.
                    if let Some(outcome) = self.reconcile_via_store().await {
                        return outcome;
                    }
                    return FireOutcome::failed(
                        "the scheduled run stopped before producing a result",
                    );
                }
            }
        }
    }

    fn is_our_fire(&self, ev: &TurnLifecycleEvent) -> bool {
        ev.session_id == self.fire_session_id && ev.kind == TurnInputKind::Cron
    }

    /// The fire's outcome from the store, or `None` if it has no terminal job
    /// yet (the caller keeps waiting).
    async fn reconcile_via_store(&self) -> Option<FireOutcome> {
        let jobs = match self
            .lifecycle
            .list_by_session(&self.fire_session_id, None)
            .await
        {
            Ok(jobs) => jobs,
            Err(e) => {
                warn!(
                    session_id = %self.fire_session_id,
                    error = %e,
                    "cron waiter store reconcile failed"
                );
                return None;
            }
        };
        let job = jobs
            .into_iter()
            .find(|j| j.input_kind() == TurnInputKind::Cron && j.is_terminal())?;
        let ordinal = match &job.final_result {
            Some(baybo_turn::TurnOutput::Message { ordinal, .. }) => *ordinal,
            _ => None,
        };
        Some(self.outcome_for(job.status.kind(), ordinal).await)
    }

    /// Classify a terminal job state, reading the failure reason off the job
    /// row so the notification can say *why* the scheduled task failed.
    async fn outcome_for(&self, kind: TurnStatusKind, reply_ordinal: Option<i64>) -> FireOutcome {
        match kind {
            TurnStatusKind::Completed => match reply_ordinal {
                // A completed fire with no persisted reply produced nothing to
                // report — still delivered, as an explicit "no output".
                None => FireOutcome {
                    outcome: ExecutionOutcome::Blank,
                    reply_ordinal: None,
                    failure_reason: None,
                },
                Some(ordinal) => FireOutcome {
                    outcome: ExecutionOutcome::Success,
                    reply_ordinal: Some(ordinal),
                    failure_reason: None,
                },
            },
            _ => FireOutcome {
                outcome: ExecutionOutcome::Failed,
                reply_ordinal: None,
                failure_reason: Some(self.failure_reason().await),
            },
        }
    }

    /// Why the fire failed, in the user's words rather than the job's: the
    /// stored reason when there is one, a generic line otherwise.
    async fn failure_reason(&self) -> String {
        let jobs = self
            .lifecycle
            .list_by_session(&self.fire_session_id, None)
            .await
            .unwrap_or_default();
        jobs.into_iter()
            .filter(|j| j.input_kind() == TurnInputKind::Cron)
            .find_map(|j| match j.status {
                baybo_turn::TurnStatus::Failed { reason } => Some(reason),
                baybo_turn::TurnStatus::Cancelled { reason, .. } => {
                    Some(format!("cancelled ({reason:?})"))
                }
                _ => None,
            })
            .unwrap_or_else(|| "the scheduled run did not finish".to_string())
    }
}

/// How a fire ended, as the ledger and the notification both need it.
struct FireOutcome {
    outcome: ExecutionOutcome,
    reply_ordinal: Option<i64>,
    failure_reason: Option<String>,
}

impl FireOutcome {
    fn failed(reason: &str) -> Self {
        Self {
            outcome: ExecutionOutcome::Failed,
            reply_ordinal: None,
            failure_reason: Some(reason.to_string()),
        }
    }
}

/// The reply ordinal a terminal phase carries (`Completed` only).
fn reply_ordinal(phase: &TurnPhase) -> Option<i64> {
    match phase {
        TurnPhase::Completed { reply_ordinal } => *reply_ordinal,
        _ => None,
    }
}

/// Rebuild the delivery payload from a persisted execution — the boot re-drive
/// path. Produces exactly what the waiter would have handed over, so a replayed
/// delivery is identical to the live one.
fn pending_result_from_execution(exec: &CronExecution) -> Option<PendingCronResult> {
    Some(PendingCronResult {
        execution_id: exec.id.clone(),
        cron_job_id: exec.job_id.clone(),
        job_title: exec.display_title(),
        fire_session_id: exec.fire_session_id.clone()?,
        reply_ordinal: exec.reply_ordinal,
        outcome: exec.outcome.unwrap_or(ExecutionOutcome::Blank),
        // The reason was never persisted (only the outcome is); the fire's own
        // session still carries the failed job for anyone who looks.
        failure_reason: None,
        completed_at: exec.completed_at.unwrap_or_else(Utc::now),
    })
}

/// Everything needed to hand a finished one-shot's result to the conversation
/// that scheduled it. Held by the waiter (live path) and rebuilt by the boot
/// re-drive, so both deliver through exactly the same code.
#[derive(Clone)]
pub(super) struct CronResultDelivery {
    session_manager: Arc<SessionManager>,
    supervisor: AgentSupervisor,
    cron_store: Arc<dyn CronStore>,
    actor_spawner: ActorSpawner,
    /// The origin conversation may be bound to an agent, so hydrating it has
    /// the same pin resolution as any other cold spawn.
    agent_profiles: Arc<dyn baybo_store::agent_profile::AgentProfileStore>,
    actor_parent_token: CancellationToken,
}

impl CronResultDelivery {
    /// Deliver `result` into its origin conversation, hydrating that
    /// conversation's actor if the idle reaper already reclaimed it.
    ///
    /// Drops the delivery — resolving the ledger, so it is not retried forever
    /// — when the origin cannot receive it:
    /// - no origin recorded (a job created before origins were stamped);
    /// - the origin session no longer resolves;
    /// - the origin is itself a cron fire session. Chained creation normally
    ///   collapses onto the real conversation at *creation* time (`CronCreate`
    ///   inherits the fire's origin), so this is the backstop for rows created
    ///   before that rule — delivering into an invisible fire session would
    ///   notify nobody.
    async fn deliver(&self, origin_session_id: Option<SessionId>, result: PendingCronResult) {
        let execution_id = result.execution_id.clone();
        let Some(origin) = resolve_origin(&self.session_manager, origin_session_id.as_ref()).await
        else {
            warn!(
                execution_id = %execution_id,
                cron_job_id = %result.cron_job_id,
                origin_session_id = ?origin_session_id,
                "one-shot cron result has no conversation to report to; dropping it"
            );
            self.resolve_ledger(&execution_id).await;
            return;
        };

        let origin_id = origin.id.clone();
        let response_tx = self.supervisor.response_tx().clone();
        let parent_token = self.actor_parent_token.clone();
        let actor_spawner = self.actor_spawner.as_ref();
        let pins = super::resolve_spawn_pins(&origin, &self.agent_profiles).await;
        let delivered = self
            .supervisor
            .route_or_spawn(
                &origin_id,
                AgentMessage::CronResultReady(Box::new(result)),
                || {
                    let actor_token = parent_token.child_token();
                    actor_spawner(
                        origin,
                        pins.llm,
                        pins.model,
                        pins.effort,
                        response_tx,
                        actor_token,
                    )
                },
            )
            .await;
        if !delivered {
            // The mailbox is gone (a shutting-down actor). Leave the ledger
            // open: the next boot re-drives it rather than losing the result.
            warn!(
                session_id = %origin_id,
                execution_id = %execution_id,
                "failed to hand cron result to its conversation; leaving it for re-drive"
            );
        }
    }

    /// Stamp an execution's delivery as resolved without notifying anyone —
    /// the result has nowhere to go. Keeps the re-drive scan converging.
    async fn resolve_ledger(&self, execution_id: &str) {
        if let Err(e) = self
            .cron_store
            .mark_execution_notified(execution_id, Utc::now())
            .await
        {
            warn!(
                execution_id = %execution_id,
                error = %e,
                "failed to resolve cron delivery ledger"
            );
        }
    }
}

/// The message that carries a fire to its actor.
fn cron_trigger(event: &CronTriggerEvent, builtin: Option<BuiltinFireContext>) -> AgentMessage {
    AgentMessage::CronTrigger {
        job_id: event.job_id.clone(),
        title: event.title.clone(),
        prompt: event.prompt.clone(),
        delivery: if event.one_shot {
            CronDelivery::OriginSession
        } else {
            CronDelivery::Channel
        },
        builtin,
    }
}

/// The conversation a one-shot's result belongs to, or `None` when it has none
/// that can receive it: no origin recorded, the session is gone, or the origin
/// is a **one-shot fire's own workspace** — invisible and unopenable, so a
/// notification there would reach nobody. The caller then drops the delivery
/// rather than reporting into the void.
///
/// A *recurring* fire's session is a legitimate target: it is a listed,
/// replyable conversation, and a job can genuinely be scheduled from inside one
/// (by the fire, or by a user replying in it). The rule is "can the user open
/// this and read it", not "was it started by cron".
async fn resolve_origin(
    session_manager: &SessionManager,
    origin_session_id: Option<&SessionId>,
) -> Option<Session> {
    let origin_id = origin_session_id?;
    let session = match session_manager.get(origin_id).await {
        Ok(session) => session?,
        Err(e) => {
            warn!(session_id = %origin_id, error = %e, "failed to load cron origin session");
            return None;
        }
    };
    let unreadable_fire_workspace = matches!(session.trigger, TriggerSource::Cron { .. })
        && !session.trigger.is_cron_conversation();
    if unreadable_fire_workspace {
        return None;
    }
    Some(session)
}

/// `{title} · {M/d}` — the fire's date in the job's own timezone. An
/// unparseable zone (a hand-edited row) falls back to UTC rather than dropping
/// the title.
fn cron_conversation_title(title: &str, timezone: &str) -> String {
    let now = Utc::now();
    let local = match timezone.parse::<Tz>() {
        Ok(tz) => now.with_timezone(&tz).format("%-m/%-d").to_string(),
        Err(_) => now.format("%-m/%-d").to_string(),
    };
    format!("{title} · {local}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::mailbox::{self, MailboxReceiver};
    use crate::actor::router::{LiveRateLimit, RouterConfig};
    use crate::security::SecurityGateway;
    use baybo_channels::ChannelRegistry;
    use baybo_cost::test_support::MemoryCostStore;
    use baybo_cost::{CostManager, SpendingLimits};
    use baybo_cron::test_support::InMemoryCronStore;
    use baybo_model::{ChannelType, CronJob, CronSchedule, CronStatus, User};
    use baybo_security::test_support::MemorySecretStore;
    use baybo_security::{EncryptionKey, LeakDetector, SecretVault};
    use baybo_session::test_support::{MemorySessionFolderStore, MemorySessionStore};
    use baybo_turn::test_support::MemoryTurnStore;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::mpsc;

    /// Mailboxes the fake spawner handed out, in spawn order.
    type SpawnedActors = Arc<Mutex<Vec<(SessionId, MailboxReceiver<AgentMessage>)>>>;

    /// Router wired to memory stores and a **fake** actor spawner: the spawner
    /// hands back a live mailbox without starting an agent loop, so a test can
    /// drive `handle_cron_trigger` and inspect exactly what the Router did —
    /// which sessions it minted, what it put on the mailbox, and (the point of
    /// these tests) whether it registered the actor.
    struct RouterHarness {
        router: Router,
        sessions: Arc<SessionManager>,
        agent_profiles: Arc<baybo_store::test_support::MemoryAgentProfileStore>,
        supervisor: AgentSupervisor,
        cron_store: Arc<InMemoryCronStore>,
        /// The lifecycle the router reads live turns from; a test drives it
        /// to put a session mid-turn.
        turn_lifecycle: Arc<TurnLifecycle>,
        spawned: SpawnedActors,
        /// When set, the fake spawner drops each mailbox receiver immediately,
        /// so the sender it hands back is already closed — the state a
        /// shutdown race leaves behind.
        drop_mailboxes: Arc<AtomicBool>,
        _trigger_tx: mpsc::Sender<CronTriggerEvent>,
        _response_rx: mpsc::Receiver<baybo_channels::AgentOutput>,
    }

    impl RouterHarness {
        /// A harness whose store already holds the origin conversation every
        /// [`Self::event`] names — the ordinary case, since session rows are
        /// never deleted.
        fn new() -> Self {
            Self::build(true)
        }

        /// A harness whose store does *not* hold that origin: the broken
        /// invariant a fire must refuse rather than paper over.
        fn without_origin_session() -> Self {
            Self::build(false)
        }

        fn build(seed_origin: bool) -> Self {
            let (response_tx, response_rx) = mpsc::channel(64);
            let supervisor = AgentSupervisor::new(response_tx);
            let sessions = Arc::new(SessionManager::new(
                Arc::new(MemorySessionStore::new()),
                Arc::new(MemorySessionFolderStore::new()),
            ));

            let agent_profiles =
                Arc::new(baybo_store::test_support::MemoryAgentProfileStore::new());
            let spawned: SpawnedActors = Arc::new(Mutex::new(Vec::new()));
            let spawned_for_closure = Arc::clone(&spawned);
            let drop_mailboxes = Arc::new(AtomicBool::new(false));
            let drop_for_closure = Arc::clone(&drop_mailboxes);
            let actor_spawner: ActorSpawner = Arc::new(
                move |session: Session, _llm, _model, _effort, _tx, _token| {
                    let (sender, receiver) = mailbox::channel(16);
                    if drop_for_closure.load(Ordering::Relaxed) {
                        drop(receiver);
                    } else {
                        spawned_for_closure.lock().push((session.id, receiver));
                    }
                    sender
                },
            );

            let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
            let vault = Arc::new(SecretVault::new(key, Arc::new(MemorySecretStore::new())));
            let security_gateway = Arc::new(SecurityGateway::new(
                Arc::new(LeakDetector::with_default_rules()),
                vault,
            ));

            if seed_origin {
                futures::executor::block_on(sessions.store().save(&Session {
                    id: SessionId::from("sess-user"),
                    user: User {
                        id: "u1".into(),
                        name: None,
                        channel: ChannelType::tui(),
                    },
                    channel: ChannelType::tui(),
                    created_at: Utc::now(),
                    last_active: Utc::now(),
                    state: Default::default(),
                    root_session_id: SessionId::from("sess-user"),
                    trigger: TriggerSource::User,
                    lineage: None,
                    hidden: false,
                    pinned: false,
                    archived: false,
                    folder_id: None,
                    title: None,
                }))
                .expect("seed the origin conversation");
            }

            let turn_lifecycle = Arc::new(TurnLifecycle::new(Arc::new(MemoryTurnStore::new())));
            let cron_store = Arc::new(InMemoryCronStore::new());
            let (trigger_tx, cron_trigger_rx) = mpsc::channel(16);
            let router = Router::from_config(RouterConfig {
                agent_profiles: Arc::clone(&agent_profiles)
                    as Arc<dyn baybo_store::AgentProfileStore>,
                workspace: Arc::new(baybo_workspace::WorkspacePaths::new(
                    "/tmp/baybo-router-test",
                )),
                session_manager: Arc::clone(&sessions),
                supervisor: supervisor.clone(),
                channels: Arc::new(ChannelRegistry::new()),
                security_gateway,
                cost_manager: CostManager::new(
                    Arc::new(MemoryCostStore::new()),
                    HashMap::new(),
                    SpendingLimits::default(),
                ),
                actor_spawner,
                turn_lifecycle: Arc::clone(&turn_lifecycle),
                cron_store: Arc::clone(&cron_store) as Arc<dyn CronStore>,
                cron_trigger_rx,
                issue_run_rx: None,
                project_store: None,
                project_events: None,
                actor_parent_token: CancellationToken::new(),
                rate_limit: LiveRateLimit::new(100, std::time::Duration::from_secs(60)),
            });

            Self {
                router,
                sessions,
                agent_profiles,
                supervisor,
                cron_store,
                turn_lifecycle,
                spawned,
                drop_mailboxes,
                _trigger_tx: trigger_tx,
                _response_rx: response_rx,
            }
        }

        fn event(one_shot: bool) -> CronTriggerEvent {
            CronTriggerEvent {
                job_id: "cj-1".into(),
                execution_id: "ce-1".into(),
                user_id: "u1".into(),
                channel: ChannelType::tui(),
                title: "每日新闻".into(),
                timezone: "UTC".into(),
                prompt: "Summarise the news".into(),
                one_shot,
                origin_session_id: Some(SessionId::from("sess-user")),
                previous_fire_at: None,
            }
        }

        /// Make every subsequently-spawned actor's mailbox already closed.
        fn close_spawned_mailboxes(&self) {
            self.drop_mailboxes.store(true, Ordering::Relaxed);
        }

        /// Record the execution the fire event refers to, as the scheduler
        /// would have before dispatching it — the waiter stamps its outcome
        /// onto this row.
        async fn record_execution(&self) {
            let mut exec = CronExecution::pending(&job("每日新闻"), Utc::now(), Utc::now());
            exec.id = "ce-1".into();
            self.cron_store
                .record_execution(&exec)
                .await
                .expect("record execution");
        }

        /// Put `session` mid-turn, as a live reply would.
        async fn open_turn(&self, session: &SessionId) -> baybo_model::TurnId {
            let turn = self
                .turn_lifecycle
                .start_turn(
                    session.clone(),
                    baybo_model::TriggerKind::User,
                    baybo_turn::TurnInput::UserChat {
                        content: Vec::new(),
                    },
                    None,
                )
                .await
                .expect("open a turn");
            self.turn_lifecycle
                .start(&turn.id)
                .await
                .expect("start the turn");
            turn.id
        }

        async fn settle_turn(&self, turn_id: &baybo_model::TurnId) {
            self.turn_lifecycle
                .complete(
                    turn_id,
                    baybo_turn::TurnOutput::Structured {
                        value: serde_json::Value::Null,
                    },
                )
                .await
                .expect("settle the turn");
        }

        /// The session the fake spawner was handed, plus its mailbox.
        fn fire(&self) -> (SessionId, MailboxReceiver<AgentMessage>) {
            self.spawned
                .lock()
                .pop()
                .expect("the router must have spawned an actor for the fire")
        }
    }

    /// The event carries an origin the store does not have. Session rows are
    /// never deleted, so that is a broken invariant — and firing anyway would
    /// answer in the wrong persona and write into the wrong memory partition
    /// with nothing but a log line to show for it.
    #[tokio::test]
    async fn a_fire_whose_origin_is_missing_fails_instead_of_running_unbound() {
        let mut h = RouterHarness::without_origin_session();
        let err = h
            .router
            .handle_cron_trigger(RouterHarness::event(false))
            .await
            .expect_err("a missing origin must not be treated as 'no agent'");
        assert!(err.to_string().contains("never deleted"), "got: {err:#}");
        assert!(
            h.supervisor.registered_session_ids().is_empty(),
            "nothing may be minted for a fire that cannot resolve its agent"
        );
    }

    #[tokio::test]
    async fn a_dream_fire_with_nothing_to_read_mints_no_session_at_all() {
        let mut h = RouterHarness::new();
        let mut event = RouterHarness::event(false);
        event.job_id = BuiltinCronJob::Dream.id().to_string();

        h.router
            .handle_cron_trigger(event)
            .await
            .expect("the skip is not an error");

        // No conversation, no actor, no tokens: an idle deployment must not
        // pay for a pass over an empty window, nor leave an empty row in the
        // user's chat list.
        assert!(
            h.spawned.lock().is_empty(),
            "a skipped dream fire must spawn no actor"
        );
        assert!(
            h.supervisor.registered_session_ids().is_empty(),
            "a skipped dream fire must register nothing"
        );
    }

    #[tokio::test]
    async fn a_dream_pass_opens_one_conversation_per_agent_that_has_activity() {
        let h = RouterHarness::new();
        let scout = AgentProfileId::parse("01JSCOUT").expect("valid id");

        // Two conversations a human spoke in: one the built-in's, one a
        // custom agent's. Each agent keeps its own memory, so each needs its
        // own fire — a single one could only ever tend whichever it ran as.
        for (id, agent) in [("sess-builtin", None), ("sess-scout", Some(scout.clone()))] {
            let mut session = h
                .sessions
                .create_bound_session_with_trigger(
                    User {
                        id: "u1".into(),
                        name: None,
                        channel: ChannelType::tui(),
                    },
                    ChannelType::tui(),
                    TriggerSource::User,
                    agent.clone().map(|agent_id| AgentBinding {
                        agent_id,
                        framework: AgentFramework::Baybo,
                    }),
                )
                .await
                .expect("mint");
            session.id = SessionId::from(id);
            h.sessions.store().save(&session).await.expect("save");
            h.sessions
                .store()
                .append_session_message(
                    &session.id,
                    &baybo_model::ChatMessage::user(vec![baybo_model::ContentBlock::Text(
                        "hello".into(),
                    )]),
                )
                .await
                .expect("append");
        }
        baybo_store::AgentProfileStore::create(
            h.agent_profiles.as_ref(),
            &baybo_store::AgentProfileRow {
                id: scout.clone(),
                description: String::new(),
                avatar_blob_id: None,
                framework: AgentFramework::Baybo,
                llm: None,
                builtin: false,
                team: None,
                hired_by: None,
                deleted_at: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        )
        .await
        .expect("seed profile");

        let mut event = RouterHarness::event(false);
        event.job_id = BuiltinCronJob::Dream.id().to_string();
        let mut h = h;
        h.router.handle_cron_trigger(event).await.expect("fan out");

        let spawned = h.spawned.lock();
        assert_eq!(spawned.len(), 2, "one dream conversation per agent");
        drop(spawned);

        // The partition is the whole point of fanning out: each fire is told
        // about its own agent's conversations and no one else's. Naming
        // another agent's would invite a refused write, and point the pass
        // at material it has no business consolidating.
        let mut digests: Vec<String> = Vec::new();
        for (_, mailbox) in h.spawned.lock().iter_mut() {
            let Ok(AgentMessage::CronTrigger { builtin, .. }) = mailbox.try_recv() else {
                panic!("each dream fire must carry a trigger")
            };
            digests.push(
                builtin
                    .expect("a dream fire carries its digest")
                    .prompt_context,
            );
        }
        let builtin = digests
            .iter()
            .find(|d| d.contains("sess-builtin"))
            .expect("the built-in's fire");
        let scouts = digests
            .iter()
            .find(|d| d.contains("sess-scout"))
            .expect("the custom agent's fire");
        assert!(
            !builtin.contains("sess-scout"),
            "the built-in's digest named another agent's conversation: {builtin}"
        );
        assert!(
            !scouts.contains("sess-builtin"),
            "the custom agent's digest named another agent's conversation: {scouts}"
        );
    }

    #[tokio::test]
    async fn a_dream_window_starts_where_the_previous_fire_left_off() {
        // The scheduler advances the job row *before* dispatching, so the
        // row's own `last_triggered_at` is useless downstream — it already
        // reads as this fire. The window therefore rests entirely on the
        // value the execution snapshotted.
        let h = RouterHarness::new();
        let long_ago = Utc::now() - chrono::Duration::days(30);
        let mut session = h
            .sessions
            .create_session_with_trigger(
                User {
                    id: "u1".into(),
                    name: None,
                    channel: ChannelType::tui(),
                },
                ChannelType::tui(),
                TriggerSource::User,
            )
            .await
            .expect("mint");
        session.id = SessionId::from("sess-old");
        h.sessions.store().save(&session).await.expect("save");
        h.sessions
            .store()
            .append_session_message(
                &session.id,
                &baybo_model::ChatMessage::user(vec![baybo_model::ContentBlock::Text("hi".into())]),
            )
            .await
            .expect("append");

        let mut h = h;
        let mut event = RouterHarness::event(false);
        event.job_id = BuiltinCronJob::Dream.id().to_string();
        // A cursor *after* the conversation: nothing new to think about.
        event.previous_fire_at = Some(Utc::now() + chrono::Duration::seconds(1));
        h.router
            .handle_cron_trigger(event.clone())
            .await
            .expect("skip");
        assert!(
            h.spawned.lock().is_empty(),
            "a window that starts after the activity must see nothing"
        );

        // The same conversation, with the cursor moved back before it.
        event.previous_fire_at = Some(long_ago);
        h.router.handle_cron_trigger(event).await.expect("fan out");
        assert_eq!(
            h.spawned.lock().len(),
            1,
            "the window's lower bound is the previous fire, not the job row"
        );
    }

    /// A conversation that predates the window is listed with a transcript
    /// path anchored at its *new* messages. Handing out the plain path would
    /// make every pass re-render — and re-pay for — the whole history, and a
    /// default-paginated read of a long one returns its opening pages rather
    /// than the activity that got it listed.
    #[tokio::test]
    async fn a_long_running_conversation_is_listed_from_its_new_messages_on() {
        let h = RouterHarness::new();
        let say = |s: &str| {
            baybo_model::ChatMessage::user(vec![baybo_model::ContentBlock::Text(s.into())])
        };
        let mut session = h
            .sessions
            .create_session_with_trigger(
                User {
                    id: "u1".into(),
                    name: None,
                    channel: ChannelType::tui(),
                },
                ChannelType::tui(),
                TriggerSource::User,
            )
            .await
            .expect("mint");
        session.id = SessionId::from("sess-long");
        h.sessions.store().save(&session).await.expect("save");
        for old in ["first", "second", "third"] {
            h.sessions
                .store()
                .append_session_message(&session.id, &say(old))
                .await
                .expect("append");
        }
        let cursor = Utc::now();
        h.sessions
            .store()
            .append_session_message(&session.id, &say("new since the last pass"))
            .await
            .expect("append");

        let mut h = h;
        let mut event = RouterHarness::event(false);
        event.job_id = BuiltinCronJob::Dream.id().to_string();
        event.previous_fire_at = Some(cursor);
        h.router.handle_cron_trigger(event).await.expect("fan out");

        let (_, mut mailbox) = h.fire();
        let Ok(AgentMessage::CronTrigger { builtin, .. }) = mailbox.try_recv() else {
            panic!("a dream fire must carry a trigger")
        };
        let digest = builtin
            .expect("a dream fire carries its digest")
            .prompt_context;
        assert!(digest.contains("sess-long@3"), "{digest}");
        assert!(digest.contains("3 earlier"), "{digest}");
    }

    /// The deferral. Reading a conversation while its turn is still writing
    /// consolidates half an exchange and then steps over the rest — and the
    /// rest is `MessageSource::Agent`, which nothing that selects on human
    /// messages would ever offer again. Leaving the cursor alone is what
    /// makes "not yet" expressible at all.
    #[tokio::test]
    async fn a_conversation_whose_turn_is_still_writing_waits_for_the_next_pass() {
        let h = RouterHarness::new();
        let say = |s: &str| {
            baybo_model::ChatMessage::user(vec![baybo_model::ContentBlock::Text(s.into())])
        };
        let mut session = h
            .sessions
            .create_session_with_trigger(
                User {
                    id: "u1".into(),
                    name: None,
                    channel: ChannelType::tui(),
                },
                ChannelType::tui(),
                TriggerSource::User,
            )
            .await
            .expect("mint");
        session.id = SessionId::from("sess-busy");
        h.sessions.store().save(&session).await.expect("save");
        h.sessions
            .store()
            .append_session_message(&session.id, &say("mid-conversation"))
            .await
            .expect("append");
        let live = h.open_turn(&session.id).await;

        let mut h = h;
        let mut event = RouterHarness::event(false);
        event.job_id = BuiltinCronJob::Dream.id().to_string();
        h.router
            .handle_cron_trigger(event.clone())
            .await
            .expect("the deferral is not an error");
        assert!(
            h.spawned.lock().is_empty(),
            "a session mid-turn is the only activity, so there is nothing to dream about yet"
        );

        // The turn settles; the same conversation is still on offer, from
        // the same left edge. Nothing had to remember that it was deferred —
        // the cursor simply never moved.
        h.settle_turn(&live).await;
        h.router.handle_cron_trigger(event).await.expect("fan out");
        let (_, mut mailbox) = h.fire();
        let Ok(AgentMessage::CronTrigger { builtin, .. }) = mailbox.try_recv() else {
            panic!("a dream fire must carry a trigger")
        };
        let digest = builtin
            .expect("a dream fire carries its digest")
            .prompt_context;
        assert!(digest.contains("sess-busy"), "{digest}");
    }

    /// A capped digest must read as capped. The pass prunes memories it
    /// believes are superseded, so a list that silently stops at the cap
    /// invites it to conclude a conversation no longer exists.
    #[tokio::test]
    async fn a_capped_digest_says_how_many_it_is_holding_back() {
        let h = RouterHarness::new();
        let say = |s: &str| {
            baybo_model::ChatMessage::user(vec![baybo_model::ContentBlock::Text(s.into())])
        };
        for i in 0..DREAM_MAX_SESSIONS_PER_FIRE + 3 {
            let mut session = h
                .sessions
                .create_session_with_trigger(
                    User {
                        id: "u1".into(),
                        name: None,
                        channel: ChannelType::tui(),
                    },
                    ChannelType::tui(),
                    TriggerSource::User,
                )
                .await
                .expect("mint");
            session.id = SessionId::from(format!("sess-{i:03}"));
            h.sessions.store().save(&session).await.expect("save");
            h.sessions
                .store()
                .append_session_message(&session.id, &say("hello"))
                .await
                .expect("append");
        }

        let mut h = h;
        let mut event = RouterHarness::event(false);
        event.job_id = BuiltinCronJob::Dream.id().to_string();
        h.router.handle_cron_trigger(event).await.expect("fan out");

        let (_, mut mailbox) = h.fire();
        let Ok(AgentMessage::CronTrigger { builtin, .. }) = mailbox.try_recv() else {
            panic!("a dream fire must carry a trigger")
        };
        let digest = builtin
            .expect("a dream fire carries its digest")
            .prompt_context;
        assert_eq!(
            digest.lines().filter(|l| l.starts_with("- ")).count(),
            DREAM_MAX_SESSIONS_PER_FIRE,
            "{digest}"
        );
        assert!(digest.contains("3 more not shown"), "{digest}");
    }

    /// A fire whose trigger never reached a mailbox must not move the cursor.
    /// Every other failure in this pass costs a re-read; this one would lose
    /// the content outright, because nothing else will ever offer those
    /// conversations again.
    #[tokio::test]
    async fn an_undelivered_fire_leaves_its_conversations_queued() {
        let h = RouterHarness::new();
        let say = |s: &str| {
            baybo_model::ChatMessage::user(vec![baybo_model::ContentBlock::Text(s.into())])
        };
        let mut session = h
            .sessions
            .create_session_with_trigger(
                User {
                    id: "u1".into(),
                    name: None,
                    channel: ChannelType::tui(),
                },
                ChannelType::tui(),
                TriggerSource::User,
            )
            .await
            .expect("mint");
        session.id = SessionId::from("sess-undelivered");
        h.sessions.store().save(&session).await.expect("save");
        h.sessions
            .store()
            .append_session_message(&session.id, &say("hello"))
            .await
            .expect("append");

        // Every mailbox the spawner hands out is already closed — the state a
        // shutdown race leaves behind.
        h.close_spawned_mailboxes();
        let mut h = h;
        let mut event = RouterHarness::event(false);
        event.job_id = BuiltinCronJob::Dream.id().to_string();
        h.router
            .handle_cron_trigger(event.clone())
            .await
            .expect("an undelivered fire is not an error");

        // The next pass must still see it. If the cursor had advanced, this
        // conversation would be gone for good.
        h.drop_mailboxes.store(false, Ordering::Relaxed);
        h.router.handle_cron_trigger(event).await.expect("fan out");
        let (_, mut mailbox) = h.fire();
        let Ok(AgentMessage::CronTrigger { builtin, .. }) = mailbox.try_recv() else {
            panic!("a dream fire must carry a trigger")
        };
        let digest = builtin
            .expect("a dream fire carries its digest")
            .prompt_context;
        assert!(digest.contains("sess-undelivered"), "{digest}");
    }

    /// The cursor advances only over what a *dispatched* fire was shown, and
    /// then that conversation stops being offered. Without the second half a
    /// consolidated conversation would ride every subsequent digest forever.
    #[tokio::test]
    async fn a_dispatched_pass_advances_the_cursor_and_stops_re_offering() {
        let h = RouterHarness::new();
        let say = |s: &str| {
            baybo_model::ChatMessage::user(vec![baybo_model::ContentBlock::Text(s.into())])
        };
        let mut session = h
            .sessions
            .create_session_with_trigger(
                User {
                    id: "u1".into(),
                    name: None,
                    channel: ChannelType::tui(),
                },
                ChannelType::tui(),
                TriggerSource::User,
            )
            .await
            .expect("mint");
        session.id = SessionId::from("sess-once");
        h.sessions.store().save(&session).await.expect("save");
        h.sessions
            .store()
            .append_session_message(&session.id, &say("hello"))
            .await
            .expect("append");

        let mut h = h;
        let mut event = RouterHarness::event(false);
        event.job_id = BuiltinCronJob::Dream.id().to_string();
        h.router
            .handle_cron_trigger(event.clone())
            .await
            .expect("fan out");
        assert_eq!(h.spawned.lock().len(), 1, "the first pass sees it");

        h.router
            .handle_cron_trigger(event.clone())
            .await
            .expect("the second pass has nothing to do");
        assert_eq!(
            h.spawned.lock().len(),
            1,
            "a consolidated conversation must not ride the next digest"
        );

        // A machine row lands afterwards — a turn finishing late, or a
        // background delivery. The human never comes back, so only the
        // ordinal cursor can surface this.
        h.sessions
            .store()
            .append_session_message(
                &session.id,
                &baybo_model::ChatMessage::assistant(vec![baybo_model::ContentBlock::Text(
                    "the tail nobody asked for".into(),
                )]),
            )
            .await
            .expect("append");
        h.router.handle_cron_trigger(event).await.expect("fan out");
        assert_eq!(
            h.spawned.lock().len(),
            2,
            "rows appended after the pass read must be offered next time"
        );
    }

    #[tokio::test]
    async fn an_external_framework_agent_is_not_dreamt_for() {
        // It runs its own CLI with its own tools and never sees the
        // `<memory>` injection, so a fire would ask a stranger to tidy a
        // room it cannot see.
        let h = RouterHarness::new();
        let codex = AgentProfileId::parse("01JCODEX").expect("valid id");
        let mut session = h
            .sessions
            .create_bound_session_with_trigger(
                User {
                    id: "u1".into(),
                    name: None,
                    channel: ChannelType::tui(),
                },
                ChannelType::tui(),
                TriggerSource::User,
                Some(AgentBinding {
                    agent_id: codex.clone(),
                    framework: AgentFramework::Codex,
                }),
            )
            .await
            .expect("mint");
        session.id = SessionId::from("sess-codex");
        h.sessions.store().save(&session).await.expect("save");
        h.sessions
            .store()
            .append_session_message(
                &session.id,
                &baybo_model::ChatMessage::user(vec![baybo_model::ContentBlock::Text("hi".into())]),
            )
            .await
            .expect("append");
        baybo_store::AgentProfileStore::create(
            h.agent_profiles.as_ref(),
            &baybo_store::AgentProfileRow {
                id: codex,
                description: String::new(),
                avatar_blob_id: None,
                framework: AgentFramework::Codex,
                llm: None,
                builtin: false,
                team: None,
                hired_by: None,
                deleted_at: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            },
        )
        .await
        .expect("seed profile");

        let mut h = h;
        let mut event = RouterHarness::event(false);
        event.job_id = BuiltinCronJob::Dream.id().to_string();
        h.router.handle_cron_trigger(event).await.expect("skip");

        assert!(h.spawned.lock().is_empty(), "no fire for an external agent");
    }

    #[tokio::test]
    async fn an_agent_whose_profile_is_gone_is_not_dreamt_for() {
        // The write tier is keyed on the id, so nothing would read what the
        // pass wrote — and the profile row is what says the agent exists.
        let h = RouterHarness::new();
        let ghost = AgentProfileId::parse("01JGHOST").expect("valid id");
        let mut session = h
            .sessions
            .create_bound_session_with_trigger(
                User {
                    id: "u1".into(),
                    name: None,
                    channel: ChannelType::tui(),
                },
                ChannelType::tui(),
                TriggerSource::User,
                Some(AgentBinding {
                    agent_id: ghost,
                    framework: AgentFramework::Baybo,
                }),
            )
            .await
            .expect("mint");
        session.id = SessionId::from("sess-ghost");
        h.sessions.store().save(&session).await.expect("save");
        h.sessions
            .store()
            .append_session_message(
                &session.id,
                &baybo_model::ChatMessage::user(vec![baybo_model::ContentBlock::Text("hi".into())]),
            )
            .await
            .expect("append");

        let mut h = h;
        let mut event = RouterHarness::event(false);
        event.job_id = BuiltinCronJob::Dream.id().to_string();
        h.router.handle_cron_trigger(event).await.expect("skip");

        assert!(h.spawned.lock().is_empty(), "no fire without a profile row");
    }

    #[tokio::test]
    async fn an_ordinary_job_never_takes_the_dream_path() {
        // The skip is keyed on the built-in job id alone; a user's own job
        // must fire whether or not anyone has spoken lately.
        let mut h = RouterHarness::new();
        h.router
            .handle_cron_trigger(RouterHarness::event(false))
            .await
            .expect("route the fire");

        let (_, mut mailbox) = h.fire();
        assert!(matches!(
            mailbox.try_recv(),
            Ok(AgentMessage::CronTrigger { builtin: None, .. })
        ));
    }

    /// A recurring fire's session is a conversation the user can reply in, so
    /// its actor MUST be registered with the supervisor. If it were not, a
    /// reply would find no entry and fork a *second* actor over the same
    /// transcript — and the fire actor's registry guard would then evict the
    /// user's actor on its way out.
    #[tokio::test]
    async fn a_recurring_fires_actor_is_registered_so_a_reply_reaches_it() {
        let mut h = RouterHarness::new();
        h.router
            .handle_cron_trigger(RouterHarness::event(false))
            .await
            .expect("route the fire");

        let (fire_session_id, mut mailbox) = h.fire();
        assert_eq!(
            h.supervisor.registered_session_ids(),
            vec![fire_session_id.clone()],
            "the fire's actor must be in the supervisor's registry"
        );

        assert!(matches!(
            mailbox.try_recv(),
            Ok(AgentMessage::CronTrigger {
                delivery: CronDelivery::Channel,
                ..
            })
        ));
        // And NOT an `ActorStop`: stopping the actor the instant the fire ends
        // is exactly when the user is most likely to reply to the notification
        // they just got, and a reply that raced the stop would be routed into a
        // mailbox nobody is reading and dropped. The idle reaper reclaims this
        // actor like any other conversation's.
        assert!(
            matches!(mailbox.try_recv(), Err(mailbox::TryRecvError::Empty)),
            "a replyable conversation's actor must not be stopped behind its own fire"
        );

        // A user replying in the conversation reaches THAT actor rather than
        // spawning a rival one.
        let before = h.spawned.lock().len();
        assert!(
            h.supervisor
                .route(&fire_session_id, AgentMessage::ActorStop)
                .await,
            "a message for the fire's conversation must route to its live actor"
        );
        assert_eq!(
            h.spawned.lock().len(),
            before,
            "routing to the registered actor must not spawn a second one"
        );

        // The session is a listed, titled conversation.
        let session = h
            .sessions
            .get(&fire_session_id)
            .await
            .unwrap()
            .expect("fire session");
        assert!(session.trigger.is_cron_conversation());
        assert!(
            session
                .title
                .as_deref()
                .is_some_and(|t| t.starts_with("每日新闻 · ")),
            "got {:?}",
            session.title
        );
    }

    /// A one-shot's session is a private workspace with no follow-up traffic:
    /// leaving it unregistered is what keeps the supervisor's map from filling
    /// with dangling handles. It also dispatches nothing — its result is
    /// reported into the conversation that scheduled it.
    #[tokio::test]
    async fn a_one_shot_fires_actor_stays_unregistered_and_silent() {
        let mut h = RouterHarness::new();
        h.router
            .handle_cron_trigger(RouterHarness::event(true))
            .await
            .expect("route the fire");

        let (fire_session_id, mut mailbox) = h.fire();
        assert!(
            h.supervisor.is_empty(),
            "a one-shot's workspace must not be registered"
        );
        assert!(matches!(
            mailbox.try_recv(),
            Ok(AgentMessage::CronTrigger {
                delivery: CronDelivery::OriginSession,
                ..
            })
        ));

        let session = h
            .sessions
            .get(&fire_session_id)
            .await
            .unwrap()
            .expect("fire session");
        assert!(
            !session.trigger.is_cron_conversation(),
            "a one-shot's workspace is not a conversation"
        );
        assert!(session.title.is_none(), "and it is not titled");
        assert_eq!(
            session.trigger.cron_origin_session_id().map(|s| s.as_str()),
            Some("sess-user"),
            "it carries the origin so a chained CronCreate can inherit it",
        );
    }

    /// A one-shot whose actor is already gone when the trigger is handed over
    /// (a shutdown race) must still be reported. The scheduler has recorded the
    /// execution as dispatched, so nothing else will ever retry it: if the
    /// Router bailed here, the user's reminder would vanish in silence. The
    /// waiter runs anyway, on a tripped token, so the fire is stamped failed and
    /// the origin conversation is told.
    #[tokio::test]
    async fn a_one_shot_whose_actor_is_gone_still_reports_a_failure() {
        let mut h = RouterHarness::new();
        h.record_execution().await;
        // The fake spawner hands back a mailbox whose receiver it immediately
        // drops — the closed-mailbox state the race produces.
        h.close_spawned_mailboxes();

        h.router
            .handle_cron_trigger(RouterHarness::event(true))
            .await
            .expect("a dead fire actor is not an error the Router propagates");

        // The waiter resolved the fire rather than leaving it hanging: the
        // execution is stamped, and the ledger records a failure to deliver.
        let execution = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(exec) = h.cron_store.execution("ce-1")
                    && exec.completed_at.is_some()
                {
                    return exec;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the waiter must resolve a fire whose actor never ran");

        assert_eq!(
            execution.outcome,
            Some(ExecutionOutcome::Failed),
            "a fire that never ran is a failed fire, not a silent one",
        );
    }

    fn job(title: &str) -> CronJob {
        CronJob {
            id: "cj-1".into(),
            user_id: "u1".into(),
            channel: ChannelType::tui(),
            title: title.into(),
            schedule: CronSchedule::at(Utc::now()),
            prompt: "do the thing".into(),
            timezone: "UTC".into(),
            status: CronStatus::Enabled,
            last_triggered_at: None,
            next_trigger_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            origin_session_id: None,
            deleted_at: None,
            pinned: false,
            builtin: false,
        }
    }

    fn sessions() -> Arc<SessionManager> {
        Arc::new(SessionManager::new(
            Arc::new(MemorySessionStore::new()),
            Arc::new(MemorySessionFolderStore::new()),
        ))
    }

    fn user() -> User {
        User {
            id: "u1".into(),
            name: None,
            channel: ChannelType::tui(),
        }
    }

    /// A result reported into a session nobody reads is a lost notification, so
    /// the origin must resolve to a real conversation or the delivery is
    /// dropped outright (and its ledger resolved, so it isn't retried forever).
    #[tokio::test]
    async fn origin_resolves_only_to_a_real_conversation() {
        let sessions = sessions();

        // No origin recorded (a job created before origins were stamped).
        assert!(resolve_origin(&sessions, None).await.is_none());

        // An origin that no longer exists.
        let ghost = SessionId::from("sess-gone");
        assert!(resolve_origin(&sessions, Some(&ghost)).await.is_none());

        // A genuine user conversation — the one deliverable case.
        let convo = sessions
            .create_session(user(), ChannelType::tui())
            .await
            .expect("create user session");
        assert_eq!(
            resolve_origin(&sessions, Some(&convo.id))
                .await
                .map(|s| s.id),
            Some(convo.id.clone()),
        );

        // A recurring fire's session IS a conversation — listed and replyable —
        // so a job scheduled inside one (by the fire, or by a user replying in
        // it) reports back there like anywhere else.
        let recurring = sessions
            .create_session_with_trigger(
                user(),
                ChannelType::tui(),
                TriggerSource::Cron {
                    cron_job_id: "cj-news".into(),
                    origin_session_id: Some(convo.id.clone()),
                    conversation: true,
                    job_title: Some("Daily news".into()),
                },
            )
            .await
            .expect("create recurring fire conversation");
        assert_eq!(
            resolve_origin(&sessions, Some(&recurring.id))
                .await
                .map(|s| s.id),
            Some(recurring.id.clone()),
            "a recurring fire's conversation is a legitimate delivery target",
        );

        // A one-shot fire's workspace is not: it is invisible and unopenable,
        // so a notification there would reach nobody. The delivery is dropped
        // instead of reported into the void.
        let workspace = sessions
            .create_session_with_trigger(
                user(),
                ChannelType::tui(),
                TriggerSource::Cron {
                    cron_job_id: "cj-1".into(),
                    origin_session_id: Some(convo.id.clone()),
                    conversation: false,
                    job_title: None,
                },
            )
            .await
            .expect("create one-shot fire session");
        assert!(
            resolve_origin(&sessions, Some(&workspace.id))
                .await
                .is_none(),
            "a one-shot fire's private workspace must never be a delivery target"
        );
    }

    #[test]
    fn conversation_title_dates_the_fire_in_the_jobs_timezone() {
        // 23:30 UTC on the 1st is already the 2nd in Shanghai — a job that
        // fires "daily at 07:30 Shanghai" must be dated by its own day.
        let title = cron_conversation_title("每日新闻", "Asia/Shanghai");
        assert!(title.starts_with("每日新闻 · "), "{title}");
        // An unparseable zone still yields a titled conversation.
        let fallback = cron_conversation_title("每日新闻", "Mars/Olympus");
        assert!(fallback.starts_with("每日新闻 · "), "{fallback}");
    }

    /// The re-drive rebuilds the delivery payload from the persisted ledger, so
    /// a replayed notification is byte-identical to the one the waiter would
    /// have produced.
    #[test]
    fn redrive_payload_round_trips_from_the_execution_row() {
        let mut exec = CronExecution::pending(&job("晚饭提醒"), Utc::now(), Utc::now());
        exec.fire_session_id = Some("cron-fire".into());
        exec.reply_ordinal = Some(9);
        exec.outcome = Some(ExecutionOutcome::Success);
        exec.completed_at = Some(Utc::now());

        let result = pending_result_from_execution(&exec).expect("has a fire session");
        assert_eq!(result.execution_id, exec.id);
        assert_eq!(result.job_title, "晚饭提醒");
        assert_eq!(result.reply_ordinal, Some(9));
        assert_eq!(result.outcome, ExecutionOutcome::Success);
    }

    /// A fire that never recorded its session has no result to read; the
    /// re-drive must not try to build a delivery out of it.
    #[test]
    fn redrive_payload_absent_without_a_fire_session() {
        let mut exec = CronExecution::pending(&job("x"), Utc::now(), Utc::now());
        exec.completed_at = Some(Utc::now());
        assert!(pending_result_from_execution(&exec).is_none());
    }
}
