pub mod budget;
pub mod calibration;
pub mod compressor;
pub mod error;
pub mod prompts;
pub mod tokenizer;
mod transcript_repair;

pub use budget::TokenBudget;
pub use calibration::TokenCalibration;
pub use compressor::{CompressOutput, CompressionStage, parse_summary_response};
pub use error::ContextError;
pub use prompts::compression::SUMMARIZE_INSTRUCTION;
pub use tokenizer::{TiktokenTokenizer, Tokenizer};

// ---------------------------------------------------------------------------
// Recent-slice bounds for the compaction's backward atomic-pair walk.
//
// A compaction keeps the tail of the conversation verbatim under the
// summary, so the model still sees the last tool results and the user's
// own words rather than a paraphrase of them. These bound how much tail.
// See `docs/modules/context.md`.
// ---------------------------------------------------------------------------

/// Absolute ceiling on the verbatim tail a compaction keeps.
pub(crate) const RECENT_SLICE_MAX_TOKENS_ABS: usize = 40_000;

/// Window-relative ceiling on that tail, applied alongside the absolute one.
///
/// **Must stay below `compression_threshold`.** The tail is re-added to the
/// compacted transcript, so a ratio at or above the trigger threshold makes
/// every compaction land back above its own trigger — an unshrinkable
/// transcript that re-compacts on every iteration. At 0.15 against the 0.65
/// default, a compaction lands near 0.2 and leaves most of the window for new
/// material.
pub(crate) const RECENT_SLICE_MAX_TOKENS_RATIO: f64 = 0.15;

/// The walk's soft-stop token floor, as a fraction of the derived cap — so
/// `min <= max` holds structurally at every window size instead of by
/// coincidence at large ones.
pub(crate) const RECENT_SLICE_MIN_RATIO_OF_CAP: f64 = 0.25;

/// The walk's soft-stop message floor: how many text-carrying messages the
/// tail must hold before the walk may stop. A count, not a token quantity —
/// the cap check breaks first and unconditionally, so this can never exceed
/// the cap and needs no scaling.
pub(crate) const RECENT_SLICE_MIN_TEXT_BLOCK_MSGS: usize = 5;

/// `(min_tokens, min_text_block_msgs, max_tokens)` for the backward walk at
/// this context window.
pub(crate) fn recent_slice_bounds(max_tokens: usize) -> (usize, usize, usize) {
    let cap = RECENT_SLICE_MAX_TOKENS_ABS
        .min((max_tokens as f64 * RECENT_SLICE_MAX_TOKENS_RATIO) as usize);
    (
        (cap as f64 * RECENT_SLICE_MIN_RATIO_OF_CAP) as usize,
        RECENT_SLICE_MIN_TEXT_BLOCK_MSGS,
        cap,
    )
}

pub type Result<T> = std::result::Result<T, ContextError>;

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use baybo_llm::{ChatRequest, LlmResponse};
use baybo_model::{ChatMessage, ContentBlock, Role, SessionId};
use baybo_session::{SessionManager, SessionMessageAppendOutcome};
use baybo_skills::render::{render_skill_block, render_skill_reminder};
use baybo_skills::{
    SKILL_INPUT_NAME_FIELD, SKILL_TOOL_NAME, SkillDefinition, SkillRegistry, SkillSummary,
    render_skill_for_slash,
};
use baybo_trace::LlmCallInputs;
use parking_lot::RwLock;
use tracing::{debug, warn};

/// Maximum tokens the rendered detail block of a single previously
/// called skill may take up after compression — anything bigger gets
/// its body truncated (with a marker) so an oversized skill still
/// surfaces enough context to be useful without crowding out the rest.
const PER_SKILL_TOKEN_CAP: usize = 5_000;

/// Cumulative token cap across every skill detail block we attach
/// after a summary. Skills near the end of the called-list get
/// truncated harder to fit whatever budget remains; once nothing fits,
/// further skills are dropped.
const TOTAL_SKILL_TOKEN_CAP: usize = 25_000;

/// Marker appended to a truncated skill body so the model can tell
/// the definition is incomplete.
const TRUNCATION_MARKER: &str = "\n…[truncated]";

/// Anchor for cheap, near-exact token estimation between calls:
/// `actual_tokens` is the provider's `usage.input_tokens` for the
/// request whose `messages.len()` equalled `message_count_at_call`.
/// Subsequent budget queries become
/// `actual_tokens + tokenize(messages[message_count_at_call..])`.
#[derive(Debug, Clone, Copy)]
struct TokenBaseline {
    actual_tokens: usize,
    message_count_at_call: usize,
}

/// Result of a [`ContextManager::maybe_compress`] /
/// [`ContextManager::force_compress`] call.
///
/// "Nothing changed" is split into reason-specific variants so callers
/// (notably the `/compact` notice path in the agent loop) can surface
/// *why* nothing was applied instead of a generic message.
///
/// Cost recording is the caller's responsibility — both entry points
/// invoke the supplied chat closure for any LLM call, and that closure
/// is where the agent loop opens its trace span and records cost.
/// Hence the outcome carries no LLM-call provenance.
#[derive(Debug, Clone, Copy)]
pub enum CompressionOutcome {
    /// The transcript was replaced with a shorter list. `stage` says how —
    /// only [`CompressionStage::LiveSummary`] involved an LLM call, so the
    /// caller needs it to know whether a trace span already covers this
    /// compaction or whether it has to record the event itself.
    Compressed { stage: CompressionStage },
    /// Budget was under the configured compression threshold; the
    /// compressor was not invoked. Only produced by `maybe_compress` —
    /// `force_compress` bypasses the threshold by design.
    BelowThreshold,
    /// The compressor's pre-flight gate fired: the non-system message
    /// count was already at or below `keep_recent`, so even the
    /// truncate fallback couldn't shrink. No LLM call was made.
    StrategyDeclined,
    /// The compressor produced a candidate slice, but its post-tokenise
    /// total was not smaller than the original. The manager refused
    /// to apply it (so the budget stays honest) and the transcript
    /// is unchanged.
    NoSavings,
}

/// Manages a session's context: owns the conversation transcript,
/// tracks the token budget, and runs the compaction flow (live LLM
/// summary, truncate on failure). See [`compressor`] for the contract.
///
/// This is the **single owner** of `messages` for the actor handling
/// one session. `Session` (in `baybo-model`) carries only metadata.
/// The split that previously had `Session` own `messages` and
/// `ContextManager` shadow them via a token cache is folded into one
/// owner here, eliminating the drift-detection logic.
pub struct ContextManager {
    pub(crate) tokenizer: Arc<dyn Tokenizer>,
    /// Source of truth for per-session paths: the JSONL transcript the
    /// continuation-summary message points at, the identity files, and the
    /// tool-spills dir.
    pub(crate) workspace: Arc<baybo_workspace::WorkspacePaths>,
    /// Tail size for the truncate fallback and the pre-flight gate.
    pub(crate) keep_recent: usize,
    pub(crate) budget: TokenBudget,
    calibration: Arc<TokenCalibration>,
    pub(crate) skill_registry: Arc<SkillRegistry>,
    /// Channel of the session this manager serves. Gates which skills
    /// are advertised to the model and which `/command`s expand here —
    /// a skill whose `channels:` frontmatter excludes this channel is
    /// invisible to the session.
    pub(crate) channel: baybo_model::ChannelType,
    /// Owned conversation transcript — the sole source of truth.
    pub(crate) messages: Vec<ChatMessage>,
    /// Per-message token count, kept in lockstep with `messages`.
    /// Both vectors are mutated together on every append / insert /
    /// compression apply, so they cannot drift.
    per_message_tokens: Vec<usize>,
    /// Skills the model has invoked via the `Skill` tool somewhere in
    /// the current transcript, in first-seen order with duplicates
    /// collapsed. Maintained incrementally by [`Self::append`] and
    /// rebuilt on every compression apply so the vector always
    /// mirrors the current message slice.
    called_skills: Vec<String>,
    // Interior-mutable: `record_call_actual` runs from the agent
    // loop's `&self`-only `call_llm` path.
    baseline: RwLock<Option<TokenBaseline>>,
    /// LLM model id used as the calibration key. Set by
    /// `maybe_compress` (which the agent loop calls at the top of
    /// every turn with the current `LlmCompletion::model_info().id`)
    /// and read by `calibrate` / `record_call_actual`. `None` until
    /// the first compression check — cold start passes the raw
    /// tokenizer estimate through unchanged.
    current_model: RwLock<Option<String>>,
    /// Identity of the session this manager mirrors to in
    /// `session_messages`, so a process bounce / actor respawn can
    /// reload via [`Self::restore_from_store`].
    pub(crate) session_id: SessionId,
    /// Cross-session manager for transcript persistence + summary
    /// metadata reads.
    pub(crate) sessions: Arc<SessionManager>,
    /// For a subagent session: `(profile registry, profile name)` — context
    /// resolves the child's system prompt from the registry by name. `None`
    /// for a workspace session (assemble the workspace soul). Resolved by
    /// [`Self::resolve_system_prompt`] for the seed and re-resolved by
    /// `reseed_system_row` after each compaction, so a source edit (workspace
    /// soul *or* subagent profile) lands on the next compaction.
    subagent_profile: Option<(Arc<baybo_subagent::SubagentRegistry>, String)>,
    /// Transcript length at which a compaction last came back with no
    /// savings, so the next threshold check can short-circuit instead of
    /// spending another full-transcript LLM call on the same input.
    ///
    /// The compaction keeps a verbatim tail, so its output is not
    /// guaranteed smaller than its input — and the threshold check runs at
    /// the top of *every* loop iteration with no backoff of its own. Without
    /// this the first `NoSavings` would be followed by one more identical
    /// call per iteration for the rest of the turn. Cleared as soon as the
    /// transcript grows past the recorded length: more material is exactly
    /// the condition under which compaction can start paying again.
    /// `force_compress` (`/compact`) ignores and clears it — the user asked.
    compaction_declined_at_len: Option<usize>,
    /// Transient per-turn planning-checklist reminder (`Task*`),
    /// rendered from the durable `session_tasks` list and refreshed by the
    /// agent loop via [`Self::refresh_task_reminder`]. Kept OUT of
    /// `self.messages` so it is never persisted and survives compaction for
    /// free; [`Self::messages_for_llm`] appends it at the tail through the same
    /// coalescing path as the stored rows. `None` when the session has no tasks.
    task_reminder: Option<ChatMessage>,
    /// Cached raw tokenizer count of `task_reminder` (0 when `None`). The
    /// reminder rides in the real request but is **not** in `self.messages`, so
    /// [`Self::count_tokens`] adds this to the budget estimate (charging the
    /// reminder to the compression decision) and [`Self::record_call_actual`]
    /// subtracts it so the provider-anchored baseline stays messages-only.
    task_reminder_raw: usize,
    /// Request-time retry cue for a background-notification turn (see
    /// [`crate::prompts::background_notification`]). Set for the duration of a
    /// notification delivery and cleared after; like [`Self::task_reminder`] it
    /// is never persisted and never enters `self.messages`. It rides a request
    /// **only when the transcript tail is an assistant row** — that is the sole
    /// case a notification turn needs a synthetic user-role tail (a cancelled
    /// prior attempt's salvage), so the mount is recomputed per request rather
    /// than tracked. Both the request (`messages_for_llm`) and the trace marker
    /// (`build_call_input_marker`) apply the same condition, so replay matches
    /// what the model saw.
    notification_cue: Option<ChatMessage>,
}

/// Required dependencies for [`ContextManager::from_config`]. Plain
/// struct literal at the call site keeps every field visible by name.
pub struct ContextManagerConfig {
    pub tokenizer: Arc<dyn Tokenizer>,
    /// Workspace paths handle. Resolves the JSONL transcript path the
    /// continuation-summary message points at.
    pub workspace: Arc<baybo_workspace::WorkspacePaths>,
    pub keep_recent: usize,
    /// Fraction of the active model's context window at which the
    /// compression gate trips. Sourced from
    /// `agent.context.compression_threshold` in `baybo.json`. The
    /// budget's `max_tokens` is installed later via
    /// [`ContextManager::set_active_model_context_window`] once the
    /// owning `AgentLoop` resolves its LLM client.
    pub compression_threshold: f64,
    pub calibration: Arc<TokenCalibration>,
    pub skill_registry: Arc<SkillRegistry>,
    /// Channel of the session (from the session row). Skills restricted
    /// via `channels:` frontmatter are filtered against it.
    pub channel: baybo_model::ChannelType,
    pub session_id: SessionId,
    pub sessions: Arc<SessionManager>,
    /// For a subagent session: `(profile registry, profile name)` — context
    /// resolves the child's system prompt from the registry by name (and
    /// re-resolves on compaction). `None` for a workspace session (assemble the
    /// workspace soul). The profile *name* is the parent's spawn-time choice;
    /// resolving it to a prompt is context's job, so an edited profile is
    /// picked up like an edited workspace soul.
    pub subagent_profile: Option<(Arc<baybo_subagent::SubagentRegistry>, String)>,
}

impl ContextManager {
    pub fn from_config(config: ContextManagerConfig) -> Self {
        Self {
            tokenizer: config.tokenizer,
            workspace: config.workspace,
            keep_recent: config.keep_recent,
            // `max_tokens` is a placeholder; `AgentLoop::from_config`
            // installs the active model's `context_window` via
            // `set_active_model_context_window` before any compression
            // check runs.
            budget: TokenBudget::new(0, config.compression_threshold),
            calibration: config.calibration,
            skill_registry: config.skill_registry,
            channel: config.channel,
            messages: Vec::new(),
            per_message_tokens: Vec::new(),
            called_skills: Vec::new(),
            baseline: RwLock::new(None),
            current_model: RwLock::new(None),
            session_id: config.session_id,
            sessions: config.sessions,
            subagent_profile: config.subagent_profile,
            compaction_declined_at_len: None,
            task_reminder: None,
            task_reminder_raw: 0,
            notification_cue: None,
        }
    }

    /// Install the active model's context window as the compression
    /// budget cap. Called by `AgentLoop` on construction so the
    /// budget reflects the provider's hard limit.
    pub fn set_active_model_context_window(&mut self, window: usize) {
        self.budget.set_max_tokens(window);
    }

    /// Read-only access to the owned transcript.
    pub fn messages(&self) -> &[ChatMessage] {
        &self.messages
    }

    /// The transcript shaped for an LLM request: mid-turn user interjections and
    /// recalled-memory rows wrapped in their steering envelopes
    /// ([`frame_interjections`] / [`frame_recalled_memories`]), then adjacent
    /// same-role user/assistant rows coalesced ([`merge_for_llm`]) so providers
    /// that require strict alternation accept it. An owned snapshot — the stored
    /// transcript keeps each row separate and unframed.
    pub fn messages_for_llm(&self) -> Vec<ChatMessage> {
        // Skip the framing passes (and their full clones) unless the transcript
        // actually holds a row that needs framing — the common case. The scan is
        // O(n) with no allocation; each framing pass would otherwise clone every
        // row before `merge_for_llm` clones again.
        let needs_framing = self.messages.iter().any(|m| {
            matches!(
                m.source(),
                baybo_model::MessageSource::UserInterjection
                    | baybo_model::MessageSource::RecalledMemory
            )
        });
        // The task reminder is appended at the tail (after framing, before
        // coalescing) so adjacent-role merging applies to it like any other
        // row. The no-framing / no-reminder path stays clone-free.
        let mut base = if needs_framing {
            frame_recalled_memories(&frame_interjections(&self.messages))
        } else if self.task_reminder.is_some() || self.active_notification_cue().is_some() {
            self.messages.clone()
        } else {
            return merge_for_llm(&self.messages);
        };
        if let Some(reminder) = &self.task_reminder {
            base.push(reminder.clone());
        }
        if let Some(cue) = self.active_notification_cue() {
            base.push(cue.clone());
        }
        merge_for_llm(&base)
    }

    /// The notification cue, but only when it should actually ride this
    /// request: it exists AND the persisted transcript tail is an assistant
    /// row (the sole case a notification turn lacks a user-role tail). The
    /// condition is on `self.messages`, not the framed/reminder-appended view,
    /// so the request and the trace marker agree. Recomputed per request, which
    /// is what makes it correct across a mid-turn iteration (once a tool result
    /// or the model's reply lands, the tail is user-role and the cue drops out)
    /// and across a crash-replay (no attempt counter to desync).
    fn active_notification_cue(&self) -> Option<&ChatMessage> {
        let cue = self.notification_cue.as_ref()?;
        let tail_is_assistant = self
            .messages
            .last()
            .is_some_and(|m| m.role == baybo_model::Role::Assistant);
        tail_is_assistant.then_some(cue)
    }

    /// Arm (or clear) the request-time background-notification retry cue. The
    /// actor sets it around a notification delivery turn; it is applied only
    /// when [`Self::active_notification_cue`]'s tail condition holds. Never
    /// persisted, never added to `self.messages`.
    pub fn set_notification_cue(&mut self, armed: bool) {
        self.notification_cue = armed.then(|| {
            ChatMessage::agent_context(crate::prompts::background_notification::build_retry_cue())
        });
    }

    /// Set (or clear) the transient planning-checklist reminder injected at the
    /// tail of every LLM request. Built from the durable `session_tasks` list
    /// the agent loop passes in. An empty `tasks` slice clears it. Never
    /// persisted and never added to `self.messages`, so it survives compaction
    /// for free and keeps the prompt-cache prefix stable.
    pub fn refresh_task_reminder(&mut self, tasks: &[baybo_model::Task]) {
        self.task_reminder = if tasks.is_empty() {
            None
        } else {
            Some(ChatMessage::agent_context(vec![ContentBlock::Text(
                crate::prompts::tasks::render_task_list(tasks),
            )]))
        };
        self.task_reminder_raw = self
            .task_reminder
            .as_ref()
            .map_or(0, |m| self.tokenizer.count_message(m));
        // Charge the (possibly resized) reminder to the budget now so the
        // compression gate this turn sees the real request size.
        self.budget.update(self.count_tokens());
    }

    /// Resolve the system prompt to seed the leading `Role::System` row. The
    /// agent loop awaits this on a fresh session. Falls back to a minimal
    /// prompt only if resolution fails outright (a missing subagent profile or
    /// a workspace I/O error — identity files normally auto-seed) rather than
    /// seeding an empty system row.
    pub async fn resolve_system_prompt(&self) -> String {
        self.try_resolve_system_prompt()
            .await
            .unwrap_or_else(|| crate::prompts::soul::FALLBACK_SYSTEM_PROMPT.to_string())
    }

    /// Resolve the session's system prompt from its source: a subagent profile
    /// (looked up by name in the spawn registry) or the workspace soul. `None`
    /// on a resolution failure (profile not found, or workspace I/O error) so
    /// the caller decides whether to fall back (seed) or keep the prior row
    /// (reseed).
    async fn try_resolve_system_prompt(&self) -> Option<String> {
        match &self.subagent_profile {
            Some((registry, profile_name)) => {
                let resolved = registry.get(profile_name).map(|p| p.system_prompt);
                if resolved.is_none() {
                    tracing::warn!(subagent_type = %profile_name, "subagent profile not found in registry");
                }
                resolved
            }
            None => match crate::prompts::soul::assemble_from_workspace(&self.workspace).await {
                Ok(prompt) => Some(prompt),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to assemble workspace soul");
                    None
                }
            },
        }
    }

    /// Refresh the leading system row after a *committed* compaction so a
    /// source edit (the user's workspace soul, or a subagent profile) lands on
    /// the next compaction rather than carrying the pre-compaction prompt
    /// forward forever. Runs after the savings gate + commit (operating on
    /// `self.messages`, not the candidate) so a grown prompt can't skew the
    /// shrink decision, and a declined apply does no resolution work. Re-reads
    /// the session's source via [`Self::replace_first_message`] (which
    /// re-totals the budget); on a resolution failure the prior row is kept.
    async fn reseed_system_row(&mut self) {
        let first_is_system_text = self.messages.first().is_some_and(|m| {
            m.role == Role::System && matches!(m.content.first(), Some(ContentBlock::Text(_)))
        });
        if !first_is_system_text {
            return;
        }
        match self.try_resolve_system_prompt().await {
            Some(prompt) => {
                self.replace_first_message(ChatMessage::system(vec![ContentBlock::Text(prompt)]));
            }
            None => {
                tracing::warn!(
                    "failed to re-resolve system prompt after compaction; keeping prior row"
                );
            }
        }
    }

    /// Seed the leading `Role::System` row — and a skill reminder when any
    /// skills are invocable — if the transcript doesn't already lead with one.
    /// Each seeded row is persisted and mirrored to the session JSONL log by
    /// [`Self::append`].
    ///
    /// Idempotent and cheap on the hot path — the leading-system check
    /// short-circuits before the (file-reading) prompt resolution, so only a
    /// fresh session pays for the resolve.
    ///
    /// The skill reminder rides as a `Role::User` `agent_context` row, not a
    /// `system` row — some providers reject `system` outside the leading slot;
    /// `merge_for_llm` folds it into the first real user message.
    pub async fn ensure_seeded(&mut self) {
        if self
            .messages
            .first()
            .is_some_and(|m| m.role == Role::System)
        {
            return;
        }
        let prompt = self.resolve_system_prompt().await;
        let skills = self.invocable_skill_summaries();
        self.append(&ChatMessage::system(vec![ContentBlock::Text(prompt)]))
            .await;
        if !skills.is_empty() {
            self.append(&ChatMessage::agent_context(vec![ContentBlock::Text(
                render_skill_reminder(&skills),
            )]))
            .await;
        }
    }

    /// Skills the agent may invoke here: the registry's summaries filtered to
    /// agent-invocable, non-untrusted entries whose `channels:` restriction
    /// (if any) admits this session's channel. Empty when the registry is
    /// empty. The seed reminder and the post-compaction trailer advertise
    /// exactly this set; slash candidates are a *different* set
    /// ([`Self::slash_skill_summaries`]) so a slash-only skill stays
    /// user-invocable without being advertised.
    pub fn invocable_skill_summaries(&self) -> Vec<SkillSummary> {
        if self.skill_registry.is_empty() {
            return Vec::new();
        }
        self.skill_registry
            .all_summaries_sorted()
            .into_iter()
            .filter(|s| {
                s.agent_invocable
                    && !matches!(s.trust_level, baybo_model::TrustLevel::Untrusted)
                    && s.allows_channel(&self.channel)
            })
            .collect()
    }

    /// Skills a user `/command` may expand here: anything carrying a
    /// command, minus untrusted entries and skills whose `channels:`
    /// restriction excludes this session's channel. Deliberately
    /// independent of `agent_invocable`: a slash-only skill
    /// (`disable-model-invocation: true` + `user-invocable: true`) is
    /// hidden from the model's listing yet must keep expanding on the
    /// user's explicit command (docs/modules/skills.md).
    fn slash_skill_summaries(&self) -> Vec<SkillSummary> {
        if self.skill_registry.is_empty() {
            return Vec::new();
        }
        self.skill_registry
            .all_summaries_sorted()
            .into_iter()
            .filter(|s| {
                s.command.is_some()
                    && !matches!(s.trust_level, baybo_model::TrustLevel::Untrusted)
                    && s.allows_channel(&self.channel)
            })
            .collect()
    }

    /// If the trailing message is a user `/command` whose command matches an
    /// invocable skill, expand it: append that skill's body (via
    /// `baybo_skills::render_skill_for_slash`, `{{session_id}}` substituted) as a
    /// hidden agent-context row (`MessageSource::Agent` — the next LLM turn sees
    /// it, but it isn't shown as a user bubble). No-op when the tail isn't a
    /// matching user `/command`. The appended row is persisted + JSONL-logged
    /// by [`Self::append`].
    ///
    /// Unlike an LLM-issued `Skill` tool call this deliberately skips the risk
    /// assessor: an explicit user slash command is treated as authorized, so the
    /// body is injected directly rather than gated. When the skill ships linked
    /// files the injected text still carries their inventory + a hint, so the
    /// model can pull a sub-file with a follow-up `Skill` tool call — that fetch
    /// goes through the normal gate. The original `/command` message stays in
    /// the transcript, so any args remain visible.
    pub async fn expand_slash_command(&mut self) {
        if let Some((skill_name, msg)) = self.slash_expansion_message() {
            // Record eagerly so a compaction *this turn* (before any rebuild)
            // re-broadcasts the definition via the skill trailer — the body
            // row carries no `ToolUse` for `scan_skill_calls` to find, and a
            // compaction folds it into the summary unless it happens to land
            // in the kept tail. Later compactions + cold-start restore re-derive it
            // durably from the persisted `/command` row (`called_skills_in`).
            push_called_skill(&mut self.called_skills, &skill_name);
            self.append(&msg).await;
        }
    }

    /// Build the matched skill's name + agent-context body row for a trailing
    /// `/command`, or `None`. Pure (no append) so
    /// [`Self::expand_slash_command`] keeps the `?`-chain while owning the
    /// `&mut` append and the `called_skills` record.
    fn slash_expansion_message(&self) -> Option<(String, ChatMessage)> {
        let user_text = self
            .messages
            .last()
            .filter(|m| m.source() == baybo_model::MessageSource::User)
            .map(|m| baybo_llm::multimodal::extract_text(&m.content))?;
        let (skill_name, _args) =
            detect_slash_invocation(&user_text, &self.slash_skill_summaries())?;
        let skill = self.skill_registry.get(&skill_name)?;
        let body = render_skill_for_slash(&skill, self.session_id.as_str());
        Some((
            skill_name,
            ChatMessage::agent_context(vec![ContentBlock::Text(body)]),
        ))
    }

    /// Rebuild the called-skills list from a transcript slice: `ToolUse`
    /// skill calls (via [`scan_skill_calls`]) plus slash invocations
    /// re-derived from any surviving `/command` user rows. The latter keeps a
    /// slash-invoked skill tracked as durably as an LLM-issued one — its
    /// agent-context body carries no `ToolUse` for `scan_skill_calls` to find,
    /// but the user's literal `/command` row is persisted, so the skill trailer
    /// can still re-broadcast the definition right up until that row is folded
    /// into a summary. Used by both transcript-replacing paths
    /// ([`Self::restore_messages`], the compaction apply); the in-turn
    /// `expand_slash_command` still records eagerly for a same-turn compaction.
    fn called_skills_in(&self, messages: &[ChatMessage]) -> Vec<String> {
        let mut called = scan_skill_calls(messages);
        let summaries = self.invocable_skill_summaries();
        for msg in messages {
            if msg.source() != baybo_model::MessageSource::User {
                continue;
            }
            let text = baybo_llm::multimodal::extract_text(&msg.content);
            if let Some((name, _)) = detect_slash_invocation(&text, &summaries) {
                push_called_skill(&mut called, &name);
            }
        }
        called
    }

    /// Number of messages currently in the transcript.
    pub fn message_count(&self) -> usize {
        self.messages.len()
    }

    /// Read-only access to the bound tokenizer. Lets agent-side code
    /// reuse the same tokenizer for one-off counts (e.g. the
    /// background-summary prompt's per-section / total budget checks)
    /// without having to wire a separate `Arc<dyn Tokenizer>` through
    /// every layer.
    pub fn tokenizer(&self) -> &Arc<dyn Tokenizer> {
        &self.tokenizer
    }

    /// Replace `messages[0]` in place. Keeps every other row, the
    /// supersede log, and the summary anchor intact — only the first
    /// row's content + per-message token cache are touched, and the
    /// budget is re-totalled.
    ///
    /// Used by [`Self::reseed_system_row`] to refresh the soul system prompt
    /// after a committed compaction when the identity files changed on disk.
    /// The refresh is in-memory only: `persist_compaction` already wrote the
    /// pre-reseed row, and on a cold start `ensure_system_prompt` short-circuits
    /// on the restored leading `System` row without re-reading disk — so a
    /// reaped-then-rehydrated actor carries the persisted prompt until the next
    /// compaction reseeds again. Correct for the live actor's lifetime and
    /// self-healing across compactions; a source edit just isn't durable until
    /// then.
    ///
    /// No-op if the transcript is empty.
    fn replace_first_message(&mut self, msg: ChatMessage) {
        if self.messages.is_empty() {
            return;
        }
        self.per_message_tokens[0] = self.tokenizer.count_message(&msg);
        self.messages[0] = msg;
        // The cached baseline was anchored to the prior message[0]
        // token count; invalidate so the next `count_tokens` recomputes.
        self.invalidate_baseline();
        self.budget.update(self.count_tokens());
    }

    /// Replace the entire transcript. Recomputes the per-message
    /// token cache, the called-skills vector, and the budget; clears
    /// any baseline since the prior `actual_tokens` is anchored to a
    /// slice that no longer exists.
    ///
    /// Used by the actor on cold start to seed the manager from a
    /// persisted snapshot. Don't call this for in-flight mutation —
    /// `append` / `force_compress` already maintain the invariants
    /// incrementally.
    pub fn restore_messages(&mut self, messages: Vec<ChatMessage>) {
        // `message_budget_tokens` (not the raw tokenizer) so a preserved
        // `UserInterjection` row keeps its framed wire size across restart.
        let per_message_tokens: Vec<usize> = messages
            .iter()
            .map(|m| self.message_budget_tokens(m))
            .collect();
        self.per_message_tokens = per_message_tokens;
        self.called_skills = self.called_skills_in(&messages);
        self.messages = messages;
        self.invalidate_baseline();
        self.budget.update(self.calibrate(self.raw_estimate()));
        // The latch described a transcript that has been replaced wholesale;
        // whatever it proved about compactability no longer applies.
        self.compaction_declined_at_len = None;
    }

    /// Settle calibration + baseline post-response from a main LLM
    /// call. Anchors against the current transcript length — must be
    /// called before any subsequent mutation; the agent loop honours
    /// this because it sits inside the `with_llm_span` closure that
    /// returns before the assistant message is appended.
    ///
    /// Skipped when `actual_input_tokens == 0` — a hard transport
    /// failure with no usage signal; leaving the prior baseline in
    /// place beats overwriting it with zero.
    ///
    /// Only call this for *main* LLM calls. Compression calls
    /// summarise old non-system messages with no tools schema, so
    /// their `(estimate, actual)` ratio doesn't generalise.
    pub fn record_call_actual(&self, actual_input_tokens: usize) {
        if actual_input_tokens == 0 {
            return;
        }
        // The provider's count includes the transient task reminder (it's in the
        // request), but `count_tokens` re-adds the *current* reminder on top of
        // the baseline. Subtract it here so the baseline tracks the stored
        // transcript only — otherwise a large checklist is counted twice. Only
        // the main-turn call records actuals; the compression call (no reminder)
        // never reaches here.
        let actual_messages = actual_input_tokens.saturating_sub(self.task_reminder_raw);
        if let Some(model_id) = self.current_model.read().as_deref() {
            let raw = self.raw_estimate();
            self.calibration.observe(model_id, raw, actual_messages);
        }
        *self.baseline.write() = Some(TokenBaseline {
            actual_tokens: actual_messages,
            message_count_at_call: self.messages.len(),
        });
    }

    fn invalidate_baseline(&self) {
        *self.baseline.write() = None;
    }

    fn set_current_model(&self, model_id: &str) {
        if matches!(self.current_model.read().as_deref(), Some(prev) if prev == model_id) {
            return;
        }
        let mut current = self.current_model.write();
        let switching = current.is_some();
        *current = Some(model_id.to_string());
        drop(current);
        if switching {
            self.invalidate_baseline();
        }
    }

    fn raw_estimate(&self) -> usize {
        self.per_message_tokens.iter().copied().sum()
    }

    /// Append a message to the transcript and update the token
    /// budget. Does **not** trigger compression — the caller (the
    /// agent loop) is responsible for invoking
    /// [`Self::maybe_compress`] at well-defined points where it can
    /// also record the compression LLM call's cost. Auto-compressing
    /// here would silently bypass that cost-recording path.
    ///
    /// Returns the persisted `session_messages.ordinal` the store
    /// assigned to the row. `None` means persistence failed and was
    /// logged but the in-memory transcript still has the message —
    /// callers that need the ordinal to stamp it onto an outbound
    /// `Frame::Message` should just skip the stamp in that case (the
    /// client will fall back to the next assistant turn's ordinal to
    /// re-anchor its cursor).
    ///
    /// Safe because the agent loop runs `maybe_compress` at the top
    /// of every iteration, so any over-budget state from intermediate
    /// `append` calls is resolved before the next LLM request is
    /// built.
    pub async fn append(&mut self, msg: &ChatMessage) -> Option<i64> {
        self.push_message(msg);
        self.persist_appended(msg).await
    }

    /// Persist a source-event-backed row atomically and mirror it into the
    /// live window only when the store inserted a new row. A replay returns
    /// the original ordinal without duplicating the in-memory transcript.
    pub async fn append_idempotent(
        &mut self,
        source_event_id: &str,
        msg: &ChatMessage,
    ) -> Option<SessionMessageAppendOutcome> {
        match self
            .sessions
            .append_session_message_idempotent(&self.session_id, source_event_id, msg)
            .await
        {
            Ok(outcome) => {
                if outcome.was_inserted() {
                    self.push_message(msg);
                }
                Some(outcome)
            }
            Err(error) => {
                warn!(
                    session_id = %self.session_id,
                    error = %error,
                    "failed to append idempotent message to session_messages log"
                );
                None
            }
        }
    }

    fn push_message(&mut self, msg: &ChatMessage) {
        let count = self.message_budget_tokens(msg);
        record_skill_calls(&mut self.called_skills, msg);
        self.messages.push(msg.clone());
        self.per_message_tokens.push(count);
        self.budget.update(self.count_tokens());
    }

    /// Append a mid-turn user interjection as a faithful user-bubble row. The
    /// budget is charged the framed wire size via [`Self::message_budget_tokens`]
    /// (the row is sent wrapped in the `<user_interjection>` envelope); this thin
    /// wrapper exists for call-site clarity.
    pub async fn append_user_interjection(&mut self, content: Vec<ContentBlock>) -> Option<i64> {
        self.append(&ChatMessage::user_interjection(content)).await
    }

    pub async fn append_user_interjection_with_platform_msg_id(
        &mut self,
        content: Vec<ContentBlock>,
        platform_msg_id: impl Into<String>,
    ) -> Option<i64> {
        self.append(&ChatMessage::user_interjection(content).with_platform_msg_id(platform_msg_id))
            .await
    }

    /// Append a recalled-memory row as a persisted, framed context entry. The
    /// budget is charged the framed wire size (the row is sent wrapped in the
    /// `<recalled_memory>` envelope by [`Self::messages_for_llm`] /
    /// [`frame_recalled_memories`]); this thin wrapper exists for call-site
    /// clarity, mirroring [`Self::append_user_interjection`].
    pub async fn append_recalled_memory(&mut self, content: Vec<ContentBlock>) -> Option<i64> {
        self.append(&ChatMessage::recalled_memory(content)).await
    }

    /// Tokens to charge the budget for `msg`. A `UserInterjection` or
    /// `RecalledMemory` row is sent wrapped in its steering envelope
    /// (`messages_for_llm` / [`frame_interjections`] / [`frame_recalled_memories`]),
    /// so the budget must count the **framed** size or it under-counts the
    /// request and the compression gate may skip a pass it should run. This is
    /// the single place that knows the framed cost, so it applies on the live
    /// [`Self::append`] path **and** every cache-rebuild path
    /// ([`Self::restore_messages`], the compaction apply) — otherwise a row
    /// preserved across restart/compaction would silently revert to the raw
    /// count. Non-text blocks (images) are preserved; a multi-row run over-counts
    /// by the shared envelope (the safe direction), and the estimate is transient
    /// anyway (`record_call_actual` resets the baseline after the next call).
    /// Everything else is the plain message count.
    fn message_budget_tokens(&self, msg: &ChatMessage) -> usize {
        let text = baybo_llm::multimodal::extract_text(&msg.content);
        let framed_text = match msg.source() {
            baybo_model::MessageSource::UserInterjection => {
                crate::prompts::interjection::wrap_interjections(&[text])
            }
            baybo_model::MessageSource::RecalledMemory => {
                crate::prompts::recalled_memory::wrap_recalled_memories(&[text])
            }
            _ => return self.tokenizer.count_message(msg),
        };
        let mut framed = vec![ContentBlock::Text(framed_text)];
        framed.extend(
            msg.content
                .iter()
                .filter(|b| !matches!(b, ContentBlock::Text(_)))
                .cloned(),
        );
        self.tokenizer.count_message(&ChatMessage::user(framed))
    }

    /// Cap untrusted tool output to the per-result byte budget, spilling the
    /// full payload under the workspace's tool-spills dir so the truncation
    /// notice can point the model back at it. The framing primitives live in
    /// [`crate::prompts::tool_output`]; this method resolves the spill
    /// location from the manager's own workspace handle. Injection scanning
    /// and the `<tool_output>` wrap stay separate — the caller runs the
    /// `baybo-security` scan and calls
    /// [`baybo_model::wrap_tool_output`] with the capped text.
    pub async fn cap_tool_output(&self, content: String) -> String {
        use crate::prompts::tool_output;
        if content.len() <= tool_output::MAX_TOOL_OUTPUT_BYTES {
            return content;
        }
        let spill =
            tool_output::spill_tool_output(&self.workspace.tool_spills_dir(), content.as_bytes())
                .await;
        tool_output::cap_tool_output(content, spill.as_deref())
    }

    async fn persist_appended(&self, msg: &ChatMessage) -> Option<i64> {
        match self
            .sessions
            .append_session_message(&self.session_id, msg)
            .await
        {
            Ok(ordinal) => Some(ordinal),
            Err(e) => {
                warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "failed to append message to session_messages log"
                );
                None
            }
        }
    }

    /// Whether the next [`Self::maybe_compress`] for `model_id` would
    /// actually run a pass rather than short-circuit. Lets the caller
    /// bracket the pass with progress events without re-implementing the
    /// gate. Cheap — the token count is a baseline-plus-delta sum, the same
    /// one `maybe_compress` does.
    pub fn needs_compression(&mut self, model_id: &str) -> bool {
        self.set_current_model(model_id);
        self.budget.update(self.count_tokens());
        self.budget.needs_compression() && !self.compaction_declined()
    }

    /// Whether a compaction already ran at this transcript length and came
    /// back with no savings. Until the transcript grows past that length,
    /// the same call would buy the same answer.
    fn compaction_declined(&self) -> bool {
        self.compaction_declined_at_len
            .is_some_and(|len| self.messages.len() <= len)
    }

    /// Check the token budget and compact if the threshold is exceeded.
    /// Called at the top of every agent-loop iteration before the next
    /// `ChatRequest` is built.
    ///
    /// `model_id` is the LLM the next main call will hit. It's stored
    /// as the calibration key for subsequent `count_tokens` /
    /// `record_call_actual` calls; switching `model_id` invalidates
    /// the baseline (the prior `actual_tokens` was tokenised by the
    /// old provider).
    ///
    /// `chat` performs the summarizer request inside a trace span and
    /// records cost against the ledger; the compressor owns parsing and
    /// falls back to a truncate slice on failure or empty content, so a
    /// transient summarizer failure never kills the user's turn.
    pub async fn maybe_compress<F, Fut>(
        &mut self,
        model_id: &str,
        chat: F,
    ) -> crate::Result<CompressionOutcome>
    where
        F: FnOnce(ChatRequest, LlmCallInputs) -> Fut + Send + 'static,
        Fut: Future<Output = std::result::Result<LlmResponse, ContextError>> + Send + 'static,
    {
        self.set_current_model(model_id);
        self.budget.update(self.count_tokens());
        if !self.budget.needs_compression() {
            return Ok(CompressionOutcome::BelowThreshold);
        }
        if self.compaction_declined() {
            return Ok(CompressionOutcome::NoSavings);
        }

        self.run_compression(chat).await
    }

    /// Like [`Self::maybe_compress`] but skips the threshold gate and the
    /// no-savings latch — the user asked, so re-try even if the last
    /// attempt at this length declined. A strategy NoOp surfaces as
    /// `StrategyDeclined`; a non-shrinking apply surfaces as `NoSavings`,
    /// so a too-small conversation isn't rewritten as a one-line summary.
    pub async fn force_compress<F, Fut>(
        &mut self,
        model_id: &str,
        chat: F,
    ) -> crate::Result<CompressionOutcome>
    where
        F: FnOnce(ChatRequest, LlmCallInputs) -> Fut + Send + 'static,
        Fut: Future<Output = std::result::Result<LlmResponse, ContextError>> + Send + 'static,
    {
        self.set_current_model(model_id);
        self.budget.update(self.count_tokens());
        self.compaction_declined_at_len = None;
        self.run_compression(chat).await
    }

    async fn run_compression<F, Fut>(&mut self, chat: F) -> crate::Result<CompressionOutcome>
    where
        F: FnOnce(ChatRequest, LlmCallInputs) -> Fut + Send + 'static,
        Fut: Future<Output = std::result::Result<LlmResponse, ContextError>> + Send + 'static,
    {
        let chat_box: compressor::ChatCallback =
            Box::new(move |req, marker| Box::pin(chat(req, marker)));
        let plan = self.run_compression_flow(chat_box).await?;
        let (mut new_messages, stage) = match plan {
            CompressOutput::NoOp => return Ok(CompressionOutcome::StrategyDeclined),
            CompressOutput::Replaced { messages, stage } => (messages, stage),
        };

        // Refuse to apply an empty replacement: persist would mark
        // every active row in `session_messages` as superseded with
        // no successor, leaving the active slice empty until the
        // next turn re-seeds the system prompt. Unreachable today
        // (every Replaced branch keeps at least the system block),
        // so treat it as a contract violation rather than a routine
        // outcome.
        if new_messages.is_empty() {
            warn!("compression produced an empty replacement; refusing to apply");
            return Ok(CompressionOutcome::StrategyDeclined);
        }

        // Re-broadcast the authoritative skill list right after the
        // system block. The summary stages discard the historical
        // `<system-reminder>` by construction; the truncate fallback
        // can drop it too when it lands in the dropped middle.
        // Cheaper to always re-insert than to track whether the kept
        // slice still carries one.
        insert_skill_trailer(
            &mut new_messages,
            self.skill_registry.as_ref(),
            self.tokenizer.as_ref(),
            &self.called_skills,
            &self.invocable_skill_summaries(),
        );

        let before_tokens = self.budget.current();
        let start = Instant::now();

        // Decide on tokens, not message count: a same-length
        // replacement (one big message → one summary) would slip past
        // a length-only check despite a real token cut. Drop the
        // baseline first — it's anchored to the old slice.
        self.invalidate_baseline();
        // `message_budget_tokens` so a `UserInterjection` row preserved by the
        // summary keeps its framed wire size rather than reverting to raw.
        let new_per_message: Vec<usize> = new_messages
            .iter()
            .map(|m| self.message_budget_tokens(m))
            .collect();
        let after_tokens = self.calibrate(new_per_message.iter().copied().sum());

        if after_tokens >= before_tokens {
            // Don't apply. The transcript and per-message cache stay
            // in sync; nothing to undo. Latch the length so the next
            // threshold check short-circuits instead of buying the same
            // answer again on every remaining iteration of this turn.
            self.compaction_declined_at_len = Some(self.messages.len());
            return Ok(CompressionOutcome::NoSavings);
        }

        self.messages = new_messages;
        self.per_message_tokens = new_per_message;
        // Rebuild called_skills from the freshly-applied slice: a summary
        // that folds away every `Skill` ToolUse and `/command` row leaves it
        // empty (the trailer carries plain text), and a Truncate apply scopes
        // it to whatever calls — ToolUse or slash — survived in the kept tail.
        let rebuilt = self.called_skills_in(&self.messages);
        self.called_skills = rebuilt;
        self.budget.update(after_tokens);
        self.compaction_declined_at_len = None;

        if after_tokens > self.budget.max_tokens() {
            warn!(
                after_tokens,
                max_tokens = self.budget.max_tokens(),
                "token count still exceeds max_tokens after proactive compression"
            );
        }

        debug!(
            before = before_tokens,
            after = after_tokens,
            latency_ms = start.elapsed().as_millis() as u64,
            "proactive context compression"
        );

        self.persist_compaction().await;
        // Refresh the system row from the workspace soul now that the shrink
        // decision is committed + persisted — kept out of the savings gate
        // above so a grown soul can't veto a real compaction, and after
        // `persist_compaction` so it stays an in-memory-only refresh.
        self.reseed_system_row().await;
        Ok(CompressionOutcome::Compressed { stage })
    }

    /// Failures are logged, not propagated: the in-memory window stays the
    /// source of truth for this actor's lifetime, and hydration re-reads the
    /// (un-compacted) persisted set on the next boot.
    async fn persist_compaction(&self) {
        if let Err(e) = self
            .sessions
            .apply_session_compaction(&self.session_id, &self.messages)
            .await
        {
            warn!(
                session_id = %self.session_id,
                error = %e,
                "failed to persist session compaction"
            );
        }
    }

    /// Pull the persisted active transcript out of the bound
    /// `SessionManager` and seed `messages`. Called by the agent
    /// actor once on cold start so a process bounce / actor respawn
    /// picks up where the prior actor left off. No-ops cleanly when:
    /// - no session is bound (tests, single-shot harnesses);
    /// - the session has no rows yet (fresh session, cron fires,
    ///   subagent spawns).
    ///
    /// Failures log at warn and fall through to a fresh transcript;
    /// startup must not block on a transient store error.
    pub async fn restore_from_store(&mut self) {
        // Clone the bound handles up-front so we can call
        // `&mut self` methods inside the function without holding a
        // borrow of `self.session_id` / `self.sessions`.
        let session_id = self.session_id.clone();
        let sessions = Arc::clone(&self.sessions);

        // A crash mid-tool-batch
        // leaves dangling `ToolUse` rows that strict providers reject on
        // the next request, so normalize pairing before the window goes
        // live: synthetic fills are persisted (append-only) and the
        // repaired order is what the loop builds requests from.
        match sessions.load_active_session_messages(&session_id).await {
            Ok(messages) if !messages.is_empty() => {
                let (repaired, fills) = transcript_repair::repair_tool_pairing(messages);
                if !fills.is_empty() {
                    warn!(
                        session_id = %session_id,
                        fills = fills.len(),
                        "hydration: repaired dangling tool_use rows from an interrupted turn"
                    );
                    for fill in &fills {
                        if let Err(e) = sessions.append_session_message(&session_id, fill).await {
                            warn!(
                                session_id = %session_id,
                                error = %e,
                                "hydration: failed to persist synthetic tool_result fill"
                            );
                        }
                    }
                }
                self.restore_messages(repaired);
            }
            Ok(_) => {}
            Err(e) => warn!(
                session_id = %session_id,
                error = %e,
                "failed to load persisted transcript; starting fresh"
            ),
        }
    }

    /// Build the `LlmCallInputs` an `LlmCall` trace span should
    /// carry for the *current* transcript. When the bound session has
    /// rows, returns `Persisted { last_ordinal }` — the gateway
    /// hydrates this back into a flat slice on read, keeping span
    /// storage constant per call instead of cloning a growing prefix
    /// every turn. Falls back to `Inline(messages)` when the store
    /// has no rows yet (fresh session) or the lookup errors.
    pub async fn build_call_input_marker(&self) -> LlmCallInputs {
        // The notification cue rides the request as a suffix (not a log row),
        // so the marker must carry it too or replay would omit it. Same tail
        // condition as `messages_for_llm`.
        let suffix = self
            .active_notification_cue()
            .map(|cue| vec![cue.clone()])
            .unwrap_or_default();
        self.input_marker_with_suffix(suffix).await
    }

    /// Like [`build_call_input_marker`](Self::build_call_input_marker) but
    /// for callers whose request appends framing messages that are *not*
    /// rows in `session_messages` (the progress observer's prompt, a
    /// compression instruction). The persisted prefix references the
    /// active set by ordinal; `suffix` rides inline so hydration can
    /// rebuild the exact `request.messages` the LLM saw. Falls back to a
    /// fully inline marker (prefix + suffix) when the store has no rows
    /// yet, the lookup errors, or the active set diverges from the
    /// in-memory window (a failed persist that wasn't rolled back).
    pub async fn input_marker_with_suffix(&self, suffix: Vec<ChatMessage>) -> LlmCallInputs {
        // Emit a `Persisted` reference only when the anchor ordinal and the
        // prefix count are both known AND the persisted active set mirrors the
        // in-memory window (`count == messages.len()` — the same invariant
        // `synced_last_ordinal` guards). A divergence means a regular append
        // failed after entering the live window; the tripwire can't catch an
        // under-counted prefix (the reconstructed count would match it), so
        // any miss falls back to a self-contained inline copy instead.
        if let (Ok(Some(last_ordinal)), Ok(prefix_len)) = (
            self.sessions.latest_session_ordinal(&self.session_id).await,
            self.sessions.count_active_messages(&self.session_id).await,
        ) && prefix_len == self.messages.len()
        {
            LlmCallInputs::Persisted {
                last_ordinal,
                prefix_len,
                suffix,
            }
        } else {
            let mut messages = self.messages.clone();
            messages.extend(suffix);
            LlmCallInputs::Inline(messages)
        }
    }

    /// `Some((last_ordinal, active_count))` only when the in-memory
    /// transcript provably mirrors the persisted active set — the same
    /// `active_count == len` invariant [`Compressor`](crate::compressor)'s
    /// the marker requires. A compaction call sends `self.messages` verbatim,
    /// so a `Persisted` trace reference is only safe to emit when hydration
    /// would rebuild exactly that slice; otherwise the caller embeds inline.
    /// The returned count seeds the `prefix_len` tripwire. `None` on any
    /// mismatch or store error.
    async fn synced_last_ordinal(&self) -> Option<(i64, usize)> {
        let last = self
            .sessions
            .latest_session_ordinal(&self.session_id)
            .await
            .ok()??;
        let active_count = self
            .sessions
            .count_active_messages(&self.session_id)
            .await
            .ok()?;
        (active_count == self.messages.len()).then_some((last, active_count))
    }

    /// Read-only access to the token budget.
    pub fn budget(&self) -> &TokenBudget {
        &self.budget
    }

    /// Steady-state cost is `O(suffix)`: the bulk of the count is the
    /// provider's authoritative `actual_tokens` from the last main
    /// call, and only the messages appended since are summed from the
    /// per-message cache. Falls back to a full calibrated sweep on
    /// cold start, or after compression, or if the message list
    /// shrank below the anchor.
    fn count_tokens(&self) -> usize {
        // The transient task reminder rides in the real request but isn't a
        // stored message, so fold its raw count into the estimate (the baseline
        // is kept reminder-free by `record_call_actual`, so this isn't double
        // counted).
        let reminder = self.task_reminder_raw;
        let snapshot = *self.baseline.read();
        if let Some(b) = snapshot
            && self.messages.len() >= b.message_count_at_call
        {
            let delta_raw: usize = self.per_message_tokens[b.message_count_at_call..]
                .iter()
                .copied()
                .sum();
            return b.actual_tokens + self.calibrate(delta_raw + reminder);
        }
        self.calibrate(self.raw_estimate() + reminder)
    }

    fn calibrate(&self, raw: usize) -> usize {
        match self.current_model.read().as_deref() {
            Some(model_id) => self.calibration.adjust(model_id, raw),
            None => raw,
        }
    }
}

/// Walk one message's `ContentBlock::ToolUse` entries and append
/// every freshly-seen skill name (in the order they appear) to `acc`.
/// Only `ToolUse` blocks for the canonical Skill tool are considered;
/// insertion-order dedup keeps the post-summary trailer deterministic.
pub(crate) fn record_skill_calls(acc: &mut Vec<String>, msg: &ChatMessage) {
    for block in &msg.content {
        let ContentBlock::ToolUse { name, input, .. } = block else {
            continue;
        };
        if name != SKILL_TOOL_NAME {
            continue;
        }
        let Some(skill_name) = input.get(SKILL_INPUT_NAME_FIELD).and_then(|v| v.as_str()) else {
            continue;
        };
        push_called_skill(acc, skill_name);
    }
}

/// Append `name` to `acc` unless already present (insertion-order dedup),
/// keeping the post-summary skill trailer deterministic.
pub(crate) fn push_called_skill(acc: &mut Vec<String>, name: &str) {
    if !acc.iter().any(|n| n == name) {
        acc.push(name.to_string());
    }
}

/// Rebuild the called-skills vector from a full message slice.
/// Used after a compression apply to scope the vector to whatever
/// `ToolUse` blocks survived in the new transcript.
pub fn scan_skill_calls(messages: &[ChatMessage]) -> Vec<String> {
    let mut out = Vec::new();
    for msg in messages {
        record_skill_calls(&mut out, msg);
    }
    out
}

/// Coalesce adjacent same-role user/assistant messages into a single
/// message so providers that require strict user/assistant alternation
/// (e.g. some Gemini / Mistral configurations) accept the request.
///
/// `Role::System` and `Role::Tool` are passed through untouched — system
/// messages are typically extracted to a dedicated field by the provider
/// adapter, and tool-result messages must remain individually addressable
/// by their `tool_use_id`.
///
/// When two adjacent user/assistant messages are merged, the merge also
/// flattens trailing/leading `ContentBlock::Text` blocks across the
/// boundary into a single text block (joined with `\n\n`). Non-text
/// blocks (images, tool_use, tool_result, thinking) are appended as-is so
/// signatures, IDs, and modality data are preserved verbatim.
/// Wire-only pass: collapse each maximal run of consecutive
/// [`baybo_model::MessageSource::UserInterjection`] rows into a single
/// `Role::User` message whose text is wrapped in the `<user_interjection>`
/// steering envelope ([`crate::prompts::interjection`]). Re-derived on every
/// LLM call from the source flag, so the framing survives compaction/rebuild
/// and is never persisted. Non-text blocks (e.g. images) ride after the
/// envelope text in the same message. Non-interjection rows pass through
/// untouched.
fn frame_interjections(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    frame_source_runs(
        messages,
        baybo_model::MessageSource::UserInterjection,
        crate::prompts::interjection::wrap_interjections,
    )
}

/// Wire-only twin of [`frame_interjections`] for
/// [`baybo_model::MessageSource::RecalledMemory`] rows: collapses each maximal run
/// into one `Role::User` message wrapped in the `<recalled_memory>` envelope
/// ([`crate::prompts::recalled_memory`]). Run alongside [`frame_interjections`];
/// the two passes touch disjoint sources, so order is irrelevant.
fn frame_recalled_memories(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    frame_source_runs(
        messages,
        baybo_model::MessageSource::RecalledMemory,
        crate::prompts::recalled_memory::wrap_recalled_memories,
    )
}

/// Collapse each maximal run of consecutive rows whose provenance is `source`
/// into a single `Role::User` message whose joined text is wrapped by `wrap`.
/// Non-text blocks ride after the envelope text in the same message; rows of any
/// other source pass through untouched. Shared by [`frame_interjections`] and
/// [`frame_recalled_memories`] — the only difference between the two framings is
/// the source matched and the envelope applied.
fn frame_source_runs(
    messages: &[ChatMessage],
    source: baybo_model::MessageSource,
    wrap: fn(&[String]) -> String,
) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    let mut i = 0;
    while i < messages.len() {
        if messages[i].source() != source {
            out.push(messages[i].clone());
            i += 1;
            continue;
        }
        // Gather the maximal run of consecutive rows so a batch injected at one
        // boundary becomes one envelope, not N merged ones.
        let mut texts: Vec<String> = Vec::new();
        let mut extra_blocks: Vec<ContentBlock> = Vec::new();
        while i < messages.len() && messages[i].source() == source {
            let text = baybo_llm::multimodal::extract_text(&messages[i].content);
            if !text.is_empty() {
                texts.push(text);
            }
            for block in &messages[i].content {
                if !matches!(block, ContentBlock::Text(_)) {
                    extra_blocks.push(block.clone());
                }
            }
            i += 1;
        }
        let mut content = vec![ContentBlock::Text(wrap(&texts))];
        content.append(&mut extra_blocks);
        out.push(ChatMessage::user(content));
    }
    out
}

fn merge_for_llm(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut out: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    for msg in messages {
        let mergeable = matches!(msg.role, Role::User | Role::Assistant);
        match out.last_mut() {
            Some(last) if mergeable && last.role == msg.role => {
                for block in &msg.content {
                    let folded = matches!(block, ContentBlock::Text(_))
                        && matches!(last.content.last(), Some(ContentBlock::Text(_)));
                    if folded {
                        if let (Some(ContentBlock::Text(prev_t)), ContentBlock::Text(cur_t)) =
                            (last.content.last_mut(), block)
                        {
                            if !prev_t.is_empty() && !cur_t.is_empty() {
                                prev_t.push_str("\n\n");
                            }
                            prev_t.push_str(cur_t);
                        }
                    } else {
                        last.content.push(block.clone());
                    }
                }
            }
            _ => out.push(msg.clone()),
        }
    }
    out
}

#[cfg(test)]
mod frame_interjections_tests {
    use super::frame_interjections;
    use baybo_model::{BlobRef, ChatMessage, ContentBlock, Role};

    fn txt(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text(s.into())]
    }

    #[test]
    fn non_interjection_rows_pass_through() {
        let msgs = vec![
            ChatMessage::user(txt("prompt")),
            ChatMessage::assistant(txt("reply")),
        ];
        assert_eq!(frame_interjections(&msgs), msgs);
    }

    #[test]
    fn collapses_a_run_into_one_envelope() {
        let msgs = vec![
            ChatMessage::user_interjection(txt("one")),
            ChatMessage::user_interjection(txt("two")),
        ];
        let out = frame_interjections(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, Role::User);
        let ContentBlock::Text(t) = &out[0].content[0] else {
            panic!("expected text block");
        };
        assert_eq!(t.matches("<user_interjection>").count(), 1);
        assert!(t.contains("one\n\ntwo"));
    }

    #[test]
    fn interjection_after_tool_does_not_fold_into_prompt() {
        // Realistic mid-turn shape: prompt → assistant → tool result → injected
        // interjection. The interjection becomes its own enveloped user message.
        let msgs = vec![
            ChatMessage::user(txt("do X")),
            ChatMessage::assistant(txt("working")),
            ChatMessage::tool(txt("result")),
            ChatMessage::user_interjection(txt("also do Y")),
        ];
        let out = frame_interjections(&msgs);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], msgs[0]);
        let ContentBlock::Text(t) = &out[3].content[0] else {
            panic!("expected text block");
        };
        assert!(t.contains("<user_interjection>") && t.contains("also do Y"));
    }

    #[test]
    fn separate_runs_get_separate_envelopes() {
        let msgs = vec![
            ChatMessage::user_interjection(txt("a")),
            ChatMessage::assistant(txt("mid")),
            ChatMessage::user_interjection(txt("b")),
        ];
        let out = frame_interjections(&msgs);
        assert_eq!(out.len(), 3);
        for idx in [0usize, 2] {
            let ContentBlock::Text(t) = &out[idx].content[0] else {
                panic!("expected text block");
            };
            assert_eq!(t.matches("<user_interjection>").count(), 1);
        }
    }

    #[test]
    fn non_text_blocks_ride_after_envelope() {
        let img = ContentBlock::Image {
            blob: BlobRef {
                blob_id: "sha256:x".into(),
            },
            mime_type: "image/png".into(),
            filename: None,
        };
        let msgs = vec![ChatMessage::user_interjection(vec![
            ContentBlock::Text("see this".into()),
            img.clone(),
        ])];
        let out = frame_interjections(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content.len(), 2);
        assert!(matches!(&out[0].content[0], ContentBlock::Text(t) if t.contains("see this")));
        assert_eq!(out[0].content[1], img);
    }
}

#[cfg(test)]
mod frame_recalled_memories_tests {
    //! Mirrors [`frame_interjections_tests`] for the recalled-memory
    //! framing. Both wrappers delegate to one [`frame_source_runs`] helper,
    //! but the framing body differs (recalled memory carries a "treat as
    //! background you already know" preamble, not "the user just sent");
    //! pinning each independently means a refactor to one envelope can't
    //! silently regress the other.
    use super::frame_recalled_memories;
    use baybo_model::{BlobRef, ChatMessage, ContentBlock, Role};

    fn txt(s: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text(s.into())]
    }

    #[test]
    fn non_recalled_rows_pass_through() {
        let msgs = vec![
            ChatMessage::user(txt("prompt")),
            ChatMessage::assistant(txt("reply")),
        ];
        assert_eq!(frame_recalled_memories(&msgs), msgs);
    }

    #[test]
    fn collapses_a_run_into_one_envelope() {
        let msgs = vec![
            ChatMessage::recalled_memory(txt("the user prefers Rust")),
            ChatMessage::recalled_memory(txt("they dislike YAML")),
        ];
        let out = frame_recalled_memories(&msgs);
        assert_eq!(out.len(), 1);
        // Wire-only framing — the framed row lands as `Role::User` so the
        // LLM payload converter (which doesn't understand
        // `MessageSource::RecalledMemory`) accepts it.
        assert_eq!(out[0].role, Role::User);
        let ContentBlock::Text(t) = &out[0].content[0] else {
            panic!("expected text block");
        };
        assert_eq!(t.matches("<recalled_memory>").count(), 1);
        assert!(t.contains("the user prefers Rust\n\nthey dislike YAML"));
    }

    #[test]
    fn recalled_after_tool_does_not_fold_into_prompt() {
        // Mid-turn recall (e.g. a follow-up interjection triggered another
        // recall pass) lands after a tool row: it should still get its own
        // envelope rather than merging into the original prompt.
        let msgs = vec![
            ChatMessage::user(txt("do X")),
            ChatMessage::assistant(txt("working")),
            ChatMessage::tool(txt("result")),
            ChatMessage::recalled_memory(txt("user is on macOS")),
        ];
        let out = frame_recalled_memories(&msgs);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0], msgs[0]);
        let ContentBlock::Text(t) = &out[3].content[0] else {
            panic!("expected text block");
        };
        assert!(t.contains("<recalled_memory>") && t.contains("user is on macOS"));
    }

    #[test]
    fn separate_runs_get_separate_envelopes() {
        let msgs = vec![
            ChatMessage::recalled_memory(txt("a")),
            ChatMessage::assistant(txt("mid")),
            ChatMessage::recalled_memory(txt("b")),
        ];
        let out = frame_recalled_memories(&msgs);
        assert_eq!(out.len(), 3);
        for idx in [0usize, 2] {
            let ContentBlock::Text(t) = &out[idx].content[0] else {
                panic!("expected text block");
            };
            assert_eq!(t.matches("<recalled_memory>").count(), 1);
        }
    }

    #[test]
    fn non_text_blocks_ride_after_envelope() {
        // RecalledMemory rows are produced from `RecalledMemory.content`
        // (text only today), but the framing helper is content-agnostic —
        // mirror the interjection test to lock that contract.
        let img = ContentBlock::Image {
            blob: BlobRef {
                blob_id: "sha256:y".into(),
            },
            mime_type: "image/png".into(),
            filename: None,
        };
        let msgs = vec![ChatMessage::recalled_memory(vec![
            ContentBlock::Text("snippet about X".into()),
            img.clone(),
        ])];
        let out = frame_recalled_memories(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].content.len(), 2);
        assert!(
            matches!(&out[0].content[0], ContentBlock::Text(t) if t.contains("snippet about X"))
        );
        assert_eq!(out[0].content[1], img);
    }
}

#[cfg(test)]
mod merge_for_llm_tests {
    use super::merge_for_llm;
    use baybo_model::{BlobRef, ChatMessage, ContentBlock, Role};

    fn text(role: Role, body: &str) -> ChatMessage {
        let content = vec![ContentBlock::Text(body.into())];
        match role {
            Role::User => ChatMessage::agent_context(content),
            Role::Assistant => ChatMessage::assistant(content),
            Role::System => ChatMessage::system(content),
            Role::Tool => ChatMessage::tool(content),
        }
    }

    fn img() -> ContentBlock {
        ContentBlock::Image {
            blob: BlobRef {
                blob_id: "sha256:abc".into(),
            },
            mime_type: "image/png".into(),
            filename: None,
        }
    }

    #[test]
    fn passthrough_when_alternating() {
        let msgs = vec![
            text(Role::System, "sys"),
            text(Role::User, "u1"),
            text(Role::Assistant, "a1"),
            text(Role::User, "u2"),
        ];
        let out = merge_for_llm(&msgs);
        assert_eq!(out.len(), 4);
        assert_eq!(out, msgs);
    }

    #[test]
    fn merges_consecutive_user_text() {
        let msgs = vec![text(Role::User, "reminder"), text(Role::User, "hello")];
        let out = merge_for_llm(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].role, Role::User);
        assert_eq!(out[0].content.len(), 1);
        match &out[0].content[0] {
            ContentBlock::Text(t) => assert_eq!(t, "reminder\n\nhello"),
            other => panic!("expected merged text, got {other:?}"),
        }
    }

    #[test]
    fn merges_consecutive_assistant_text() {
        let msgs = vec![text(Role::Assistant, "a"), text(Role::Assistant, "b")];
        let out = merge_for_llm(&msgs);
        assert_eq!(out.len(), 1);
        match &out[0].content[0] {
            ContentBlock::Text(t) => assert_eq!(t, "a\n\nb"),
            other => panic!("expected merged text, got {other:?}"),
        }
    }

    #[test]
    fn keeps_non_text_blocks_separate() {
        let msgs = vec![
            ChatMessage::agent_context(vec![ContentBlock::Text("hi".into()), img()]),
            ChatMessage::agent_context(vec![ContentBlock::Text("more".into())]),
        ];
        let out = merge_for_llm(&msgs);
        assert_eq!(out.len(), 1);
        // hi, image, more — image keeps the text blocks from folding across it.
        assert_eq!(out[0].content.len(), 3);
        match &out[0].content[0] {
            ContentBlock::Text(t) => assert_eq!(t, "hi"),
            other => panic!("unexpected first block {other:?}"),
        }
        assert!(matches!(out[0].content[1], ContentBlock::Image { .. }));
        match &out[0].content[2] {
            ContentBlock::Text(t) => assert_eq!(t, "more"),
            other => panic!("unexpected third block {other:?}"),
        }
    }

    #[test]
    fn does_not_merge_system_or_tool() {
        let msgs = vec![
            text(Role::System, "s1"),
            text(Role::System, "s2"),
            ChatMessage::tool_result("1".into(), "r1".into()),
            ChatMessage::tool_result("2".into(), "r2".into()),
        ];
        let out = merge_for_llm(&msgs);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn preserves_assistant_tool_use_then_tool_result() {
        let assistant = ChatMessage::assistant(vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "Foo".into(),
            input: serde_json::json!({}),
            signature: None,
        }]);
        let tool = ChatMessage::tool_result("t1".into(), "ok".into());
        let msgs = vec![text(Role::User, "hi"), assistant.clone(), tool.clone()];
        let out = merge_for_llm(&msgs);
        assert_eq!(out.len(), 3);
        assert_eq!(out[1], assistant);
        assert_eq!(out[2], tool);
    }
}

/// Parse a leading `/command` against the invocable skill set, returning the
/// matched skill's name + the trailing args (empty when none). Pure; the
/// `command` field is the `/cmd` trigger configured on a skill. Used by
/// [`ContextManager::expand_slash_command`].
fn detect_slash_invocation(user_text: &str, skills: &[SkillSummary]) -> Option<(String, String)> {
    let rest = user_text.trim_start().strip_prefix('/')?;
    let (cmd, args) = match rest.find(char::is_whitespace) {
        Some(idx) => (&rest[..idx], rest[idx..].trim().to_string()),
        None => (rest, String::new()),
    };
    if cmd.is_empty() {
        return None;
    }
    let skill = skills.iter().find(|s| s.command.as_deref() == Some(cmd))?;
    Some((skill.name.clone(), args))
}

/// Render `skill` as a `<skill>` block, truncating the body in place
/// if the full rendering would exceed `cap` tokens. Returns `None` when
/// `cap` is too small to fit even a truncated body — the caller drops
/// the skill entirely rather than ship an empty wrapper.
///
/// Sizing is proportional (`body_chars * cap / full_cost`) with a
/// 10 % safety margin against per-region BPE-ratio drift, then a
/// post-render verification: if the truncated block still costs more
/// than `cap`, return `None`. One pass, no iteration.
fn render_skill_block_capped(
    mut skill: SkillDefinition,
    tokenizer: &dyn Tokenizer,
    cap: usize,
) -> Option<String> {
    let full = render_skill_block(&skill);
    let full_cost = tokenizer.count_text(&full);
    if full_cost <= cap {
        return Some(full);
    }
    let body_chars = skill.prompt_template.chars().count();
    if body_chars == 0 {
        return None;
    }
    // `* 9 / 10`: 10 % headroom so a body with slightly denser BPE
    // tokens than the rest of the rendering still lands under `cap`.
    let target_body_chars = body_chars
        .saturating_mul(cap)
        .saturating_div(full_cost)
        .saturating_mul(9)
        .saturating_div(10);
    if target_body_chars == 0 {
        return None;
    }
    let truncated_body: String = skill
        .prompt_template
        .chars()
        .take(target_body_chars)
        .chain(TRUNCATION_MARKER.chars())
        .collect();
    skill.prompt_template = truncated_body;
    let rendered = render_skill_block(&skill);
    // BPE ratio can still drift past the 10 % margin in pathological
    // cases (heavy emoji, code with rare tokens). Bail rather than
    // ship an over-budget block.
    if tokenizer.count_text(&rendered) > cap {
        return None;
    }
    Some(rendered)
}

/// Render the per-skill detail blocks for `called_skills`, truncating
/// any single block that would exceed [`PER_SKILL_TOKEN_CAP`] and
/// shrinking the effective per-skill budget toward the end of the
/// list so the cumulative payload stays under
/// [`TOTAL_SKILL_TOKEN_CAP`]. Returns `None` when nothing survives so
/// callers can skip emitting an empty wrapper.
fn build_skill_detail_payload(
    registry: &SkillRegistry,
    tokenizer: &dyn Tokenizer,
    called_skills: &[String],
) -> Option<String> {
    let mut total = 0usize;
    let mut blocks: Vec<String> = Vec::new();
    for name in called_skills {
        let Some(skill) = registry.get(name) else {
            continue;
        };
        let remaining = TOTAL_SKILL_TOKEN_CAP.saturating_sub(total);
        if remaining == 0 {
            break;
        }
        // The effective cap shrinks toward the end of the list:
        // earliest skills get up to `PER_SKILL_TOKEN_CAP`, latest ones
        // get whatever's left of the total budget.
        let cap = remaining.min(PER_SKILL_TOKEN_CAP);
        let Some(rendered) = render_skill_block_capped(skill, tokenizer, cap) else {
            continue;
        };
        total = total.saturating_add(tokenizer.count_text(&rendered));
        blocks.push(rendered);
    }
    if blocks.is_empty() {
        return None;
    }
    let mut out = String::from(
        "<system-reminder>\nFull definitions for skills referenced in the conversation summary above:\n\n",
    );
    for (i, b) in blocks.iter().enumerate() {
        if i > 0 {
            out.push_str("\n\n");
        }
        out.push_str(b);
    }
    out.push_str("\n</system-reminder>");
    Some(out)
}

/// Insert the skill trailer right after the system block of
/// `messages`: always the authoritative reminder, plus a detail
/// block when at least one previously-called skill survives the
/// per-skill / total token caps. Slotting it after the system prompt
/// (rather than at the tail) keeps the model's "what tools are
/// available" context adjacent to its instructions and lines up
/// better with prompt caching.
///
/// Both blocks ride as `Role::User` text so `merge_for_llm` folds
/// them into the leading user message before dispatch; the
/// in-storage messages stay separate for trace clarity.
pub(crate) fn insert_skill_trailer(
    messages: &mut Vec<ChatMessage>,
    registry: &SkillRegistry,
    tokenizer: &dyn Tokenizer,
    called_skills: &[String],
    // The session's advertisable set (`invocable_skill_summaries`) — NOT
    // the raw registry listing, which would re-broadcast skills the seed
    // reminder deliberately hid (non-agent-invocable, untrusted, or
    // restricted to another channel). The detail payload below stays
    // keyed on `called_skills` unfiltered: a skill actually invoked in
    // this session must keep its definition re-broadcast.
    advertised: &[SkillSummary],
) -> usize {
    let mut insert_at = 0;
    while insert_at < messages.len() && messages[insert_at].role == Role::System {
        insert_at += 1;
    }
    let mut inserted = 0;
    if !advertised.is_empty() {
        let reminder = render_skill_reminder(advertised);
        messages.insert(
            insert_at,
            ChatMessage::agent_context(vec![ContentBlock::Text(reminder)]),
        );
        insert_at += 1;
        inserted += 1;
    }
    if let Some(detail) = build_skill_detail_payload(registry, tokenizer, called_skills) {
        messages.insert(
            insert_at,
            ChatMessage::agent_context(vec![ContentBlock::Text(detail)]),
        );
        inserted += 1;
    }
    inserted
}

/// Estimate the **token cost** of the skill trailer that
/// [`insert_skill_trailer`] would attach for `called_skills` against
/// the given registry. Used by the compaction's candidate-fit check —
/// the trailer is inserted after the compressor returns, so a candidate
/// that ignored it could be accepted and then land over budget — without
/// committing the trailer to the assembled list. Returns the sum of
/// the rendered reminder + detail payload tokens, or just the
/// reminder if no called_skills carry a renderable definition.
pub(crate) fn estimate_skill_trailer_tokens(
    registry: &SkillRegistry,
    tokenizer: &dyn Tokenizer,
    called_skills: &[String],
    advertised: &[SkillSummary],
) -> usize {
    let mut total = 0;
    if !advertised.is_empty() {
        let reminder = render_skill_reminder(advertised);
        total += tokenizer.count_message(&ChatMessage::agent_context(vec![ContentBlock::Text(
            reminder,
        )]));
    }
    if let Some(detail) = build_skill_detail_payload(registry, tokenizer, called_skills) {
        total += tokenizer.count_message(&ChatMessage::agent_context(vec![ContentBlock::Text(
            detail,
        )]));
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{ContentBlock, Role};

    struct SimpleTokenizer;

    impl Tokenizer for SimpleTokenizer {
        fn count_text(&self, text: &str) -> usize {
            text.len() / 4 + 1
        }
        fn count_image(&self, _w: u32, _h: u32) -> usize {
            100
        }
        fn count_message(&self, msg: &ChatMessage) -> usize {
            let mut tokens = 4;
            for block in &msg.content {
                match block {
                    ContentBlock::Text(text) => tokens += self.count_text(text),
                    ContentBlock::Image { .. } => tokens += 100,
                    _ => tokens += 50,
                }
            }
            tokens
        }
    }

    fn make_msg(role: Role, text: &str) -> ChatMessage {
        let content = vec![ContentBlock::Text(text.to_string())];
        match role {
            Role::User => ChatMessage::agent_context(content),
            Role::Assistant => ChatMessage::assistant(content),
            Role::System => ChatMessage::system(content),
            Role::Tool => ChatMessage::tool(content),
        }
    }

    /// Padded message body so the truncate fallback's savings beat the
    /// post-compression skill trailer overhead in budget-gated tests.
    fn padded(prefix: &str) -> String {
        format!("{prefix} {}", "x".repeat(120))
    }

    /// Chat closure that panics if invoked. Use in tests where
    /// compression must not reach the LLM stage; a panic surfaces any
    /// regression that lets the call slip through.
    async fn never_chat(
        _: ChatRequest,
        _: LlmCallInputs,
    ) -> std::result::Result<LlmResponse, ContextError> {
        panic!("test must not invoke the chat closure");
    }

    /// Chat closure that errors so the compressor falls through to
    /// the truncate stage. Use to exercise truncate fallback
    /// deterministically.
    async fn err_chat(
        _: ChatRequest,
        _: LlmCallInputs,
    ) -> std::result::Result<LlmResponse, ContextError> {
        Err(ContextError::Compression("test: chat unavailable".into()))
    }

    fn test_session_id() -> SessionId {
        SessionId::from("test-session")
    }

    fn test_sessions() -> Arc<baybo_session::SessionManager> {
        let store = Arc::new(baybo_session::test_support::MemorySessionStore::new())
            as Arc<dyn baybo_session::SessionStore>;
        let folder_store = Arc::new(baybo_session::test_support::MemorySessionFolderStore::new())
            as Arc<dyn baybo_session::SessionFolderStore>;
        Arc::new(baybo_session::SessionManager::new(store, folder_store))
    }

    /// Workspace rooted at a non-existent path — nothing under it is read
    /// in these tests, and there is no tempdir to clean up.
    fn test_workspace() -> Arc<baybo_workspace::WorkspacePaths> {
        Arc::new(baybo_workspace::WorkspacePaths::new(
            "/nonexistent-baybo-test-workspace",
        ))
    }

    fn make_ctx_with_sessions(
        sessions: Arc<baybo_session::SessionManager>,
        keep_recent: usize,
        max_tokens: usize,
        threshold: f64,
    ) -> ContextManager {
        let mut ctx = ContextManager::from_config(ContextManagerConfig {
            tokenizer: Arc::new(SimpleTokenizer),
            workspace: test_workspace(),
            keep_recent,
            compression_threshold: threshold,
            calibration: Arc::new(TokenCalibration::new()),
            skill_registry: Arc::new(SkillRegistry::new()),
            channel: baybo_model::ChannelType::owner(),
            session_id: test_session_id(),
            sessions,
            subagent_profile: None,
        });
        ctx.set_active_model_context_window(max_tokens);
        ctx
    }

    fn make_ctx(keep_recent: usize, max_tokens: usize, threshold: f64) -> ContextManager {
        make_ctx_with_sessions(test_sessions(), keep_recent, max_tokens, threshold)
    }

    #[test]
    fn set_active_model_context_window_installs_budget_cap() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        assert_eq!(ctx.budget().max_tokens(), 100_000);
        // Swap to a smaller-context model: budget drops.
        ctx.set_active_model_context_window(8_000);
        assert_eq!(ctx.budget().max_tokens(), 8_000);
        // Swap to a larger one: budget grows to the new model's window.
        ctx.set_active_model_context_window(500_000);
        assert_eq!(ctx.budget().max_tokens(), 500_000);
    }

    #[tokio::test]
    async fn append_adds_message_without_compression() {
        let mut ctx = make_ctx(5, 100_000, 0.75);

        let msg = make_msg(Role::User, "hello");
        ctx.append(&msg).await;

        assert_eq!(ctx.messages().len(), 1);
        assert_eq!(ctx.messages()[0].role, Role::User);
        assert!(matches!(
            ctx.maybe_compress("test-model", never_chat).await.unwrap(),
            CompressionOutcome::BelowThreshold
        ));
    }

    #[tokio::test]
    async fn restore_repairs_dangling_tool_use_and_persists_fills() {
        let sessions = test_sessions();
        let mut ctx = make_ctx_with_sessions(Arc::clone(&sessions), 5, 100_000, 0.75);

        // The wedged shape: crash left an unanswered ToolUse, then a
        // resume nudge landed after it (pre-repair behavior).
        ctx.append(&ChatMessage::assistant(vec![ContentBlock::ToolUse {
            id: "call_dangling".into(),
            name: "Bash".into(),
            input: serde_json::Value::Null,
            signature: None,
        }]))
        .await;
        ctx.append(&make_msg(Role::User, "请立即返回最终结果"))
            .await;

        let mut restored = make_ctx_with_sessions(Arc::clone(&sessions), 5, 100_000, 0.75);
        restored.restore_from_store().await;

        // In-memory: fill sits directly after the assistant row, nudge after.
        assert_eq!(restored.messages().len(), 3);
        assert!(
            matches!(&restored.messages()[1].content[0], ContentBlock::ToolResult { tool_use_id, content, .. }
                if tool_use_id == "call_dangling" && content.contains("interrupted"))
        );
        assert_eq!(restored.messages()[2].role, Role::User);

        // The fill was persisted (append-only), so a SECOND hydration
        // finds pairing complete and synthesizes nothing new.
        let mut again = make_ctx_with_sessions(Arc::clone(&sessions), 5, 100_000, 0.75);
        again.restore_from_store().await;
        assert_eq!(
            again.messages().len(),
            3,
            "no duplicate fills on rehydration"
        );
        assert!(
            matches!(&again.messages()[1].content[0], ContentBlock::ToolResult { tool_use_id, .. }
                if tool_use_id == "call_dangling"),
            "persisted fill must be repositioned adjacent on rehydration"
        );
    }

    #[tokio::test]
    async fn idempotent_append_mirrors_only_new_source_events() {
        let sessions = test_sessions();
        let mut ctx = make_ctx_with_sessions(Arc::clone(&sessions), 5, 100_000, 0.75);
        let original = make_msg(Role::User, "original");
        let replay = make_msg(Role::User, "replay");
        let source_event_id = "background-notification:batch:prompt";

        assert_eq!(
            ctx.append_idempotent(source_event_id, &original).await,
            Some(SessionMessageAppendOutcome::Inserted { ordinal: 0 })
        );
        assert_eq!(
            ctx.append_idempotent(source_event_id, &replay).await,
            Some(SessionMessageAppendOutcome::Existing { ordinal: 0 })
        );
        assert_eq!(ctx.messages(), std::slice::from_ref(&original));

        let mut restored = make_ctx_with_sessions(Arc::clone(&sessions), 5, 100_000, 0.75);
        restored.restore_from_store().await;
        assert_eq!(
            restored.append_idempotent(source_event_id, &replay).await,
            Some(SessionMessageAppendOutcome::Existing { ordinal: 0 })
        );
        assert_eq!(restored.messages(), std::slice::from_ref(&original));
        assert_eq!(
            sessions
                .load_active_session_messages(&test_session_id())
                .await
                .expect("load transcript"),
            restored.messages()
        );
    }

    /// Steady state — every in-memory row is also persisted — so the marker
    /// references the transcript by ordinal: `prefix_len` equals the window
    /// size.
    #[tokio::test]
    async fn input_marker_emits_persisted_when_active_set_mirrors_window() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        ctx.append(&make_msg(Role::System, "sys")).await;
        ctx.append(&make_msg(Role::User, "hi")).await;

        match ctx.build_call_input_marker().await {
            LlmCallInputs::Persisted {
                last_ordinal,
                prefix_len,
                suffix,
            } => {
                assert_eq!(prefix_len, 2, "prefix_len must equal the active-set size");
                assert_eq!(last_ordinal, 1, "MAX ordinal of two 0-based rows");
                assert!(suffix.is_empty());
            }
            other => panic!("expected Persisted, got {other:?}"),
        }
    }

    /// The armed notification cue rides a request ONLY when the transcript
    /// tail is an assistant row (the case a notification retry lacks a
    /// user-role tail), and it never enters `self.messages`.
    #[tokio::test]
    async fn notification_cue_mounts_only_on_an_assistant_tail() {
        let cue_text = "no complete report";
        let has_cue = |msgs: &[ChatMessage]| {
            msgs.iter().any(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text(t) if t.contains(cue_text)))
            })
        };

        let mut ctx = make_ctx(5, 100_000, 0.75);
        ctx.append(&make_msg(Role::System, "sys")).await;
        ctx.append(&make_msg(Role::User, "prompt row")).await; // tail is user-role
        ctx.set_notification_cue(true);

        // User-role tail: armed, but the cue must NOT mount.
        let req = ctx.messages_for_llm();
        assert!(!has_cue(&req), "cue must not mount on a user-role tail");
        assert!(
            !has_cue(ctx.messages()),
            "cue must never enter the persisted window"
        );

        // Assistant tail (a prior attempt's cancelled salvage): the cue mounts.
        ctx.append(&make_msg(Role::Assistant, "partial… [cut short]"))
            .await;
        let req = ctx.messages_for_llm();
        assert!(has_cue(&req), "cue must mount on an assistant tail");
        assert_eq!(
            req.last().map(|m| m.role),
            Some(Role::User),
            "the cue must give the request a user-role tail"
        );
        assert!(
            !has_cue(ctx.messages()),
            "the mounted cue still must not be persisted"
        );

        // Disarmed: no cue regardless of tail.
        ctx.set_notification_cue(false);
        assert!(!has_cue(&ctx.messages_for_llm()), "disarmed → no cue");
    }

    /// The cue rides the request as a marker suffix too, so trace replay
    /// reconstructs exactly what the model saw (unlike `task_reminder`, whose
    /// omission from the marker is a known gap). On an assistant tail the
    /// marker must be `Persisted` with the cue as its sole suffix.
    #[tokio::test]
    async fn notification_cue_rides_the_trace_marker_suffix() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        ctx.append(&make_msg(Role::System, "sys")).await;
        ctx.append(&make_msg(Role::User, "prompt row")).await;
        ctx.append(&make_msg(Role::Assistant, "partial… [cut short]"))
            .await;
        ctx.set_notification_cue(true);

        match ctx.build_call_input_marker().await {
            LlmCallInputs::Persisted {
                prefix_len, suffix, ..
            } => {
                assert_eq!(prefix_len, 3, "the three persisted rows");
                assert_eq!(suffix.len(), 1, "the cue is the sole marker suffix");
                assert_eq!(suffix[0].role, Role::User);
            }
            other => panic!("expected Persisted with a cue suffix, got {other:?}"),
        }
    }

    /// A window/log divergence (a store row the window doesn't have, or vice
    /// versa) means a `Persisted` marker would hydrate the wrong slice AND
    /// slip past the `prefix_len` tripwire, so the marker must fall back to a
    /// self-contained `Inline` copy of the whole window plus the suffix. Every
    /// append persists in lockstep, so this is defense against a store error
    /// that slipped through — simulated here by writing a row behind the
    /// manager's back.
    #[tokio::test]
    async fn input_marker_falls_back_to_inline_on_window_log_divergence() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        ctx.append(&make_msg(Role::System, "sys")).await; // persisted (active = 1)
        ctx.append(&make_msg(Role::User, "hi")).await; // persisted (active = 2)
        ctx.sessions
            .append_session_message(&ctx.session_id, &make_msg(Role::User, "behind the back"))
            .await
            .expect("direct store append"); // active = 3, window = 2

        let suffix = vec![make_msg(Role::User, "observer prompt")];
        match ctx.input_marker_with_suffix(suffix).await {
            LlmCallInputs::Inline(messages) => {
                assert_eq!(
                    messages.len(),
                    3,
                    "Inline must carry the full window (2) + suffix (1) verbatim"
                );
            }
            other => panic!("expected Inline fallback, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn maybe_compress_on_token_threshold() {
        // max=200, threshold=0.25 → compress when > 50 tokens
        let mut ctx = make_ctx(2, 200, 0.25);

        // Build up messages one by one. `append` no longer
        // auto-compresses; the agent loop is responsible for calling
        // `maybe_compress` at well-defined cost-recording points.
        ctx.append(&make_msg(Role::System, "You are helpful")).await;
        ctx.append(&make_msg(Role::User, &padded("First"))).await;
        ctx.append(&make_msg(Role::Assistant, &padded("Reply 1")))
            .await;
        ctx.append(&make_msg(Role::User, &padded("Second"))).await;

        // `err_chat` makes the LLM-summary stage fail, so the
        // compressor falls through to truncate. The test registry is
        // empty, so no trailer reminder is inserted.
        let outcome = ctx.maybe_compress("test-model", err_chat).await.unwrap();

        assert!(matches!(outcome, CompressionOutcome::Compressed { .. }));
        // system + 2 most recent non-system.
        assert_eq!(ctx.messages().len(), 3);
        assert_eq!(ctx.messages()[0].role, Role::System);
    }

    #[tokio::test]
    async fn no_compress_under_threshold() {
        let mut ctx = make_ctx(10, 100_000, 0.75);

        ctx.append(&make_msg(Role::System, "sys")).await;
        ctx.append(&make_msg(Role::User, "hi")).await;
        ctx.append(&make_msg(Role::Assistant, "hello")).await;

        let outcome = ctx.maybe_compress("test-model", never_chat).await.unwrap();

        assert!(matches!(outcome, CompressionOutcome::BelowThreshold));
        assert_eq!(ctx.messages().len(), 3);
    }

    #[tokio::test]
    async fn force_compress_runs_under_budget() {
        // Plenty of headroom — `maybe_compress` would skip — but
        // `force_compress` runs the compressor regardless. With
        // keep_recent=2 and 3 non-system messages the truncate
        // fallback shrinks the slice, so the call returns
        // `Compressed`.
        let mut ctx = make_ctx(2, 100_000, 0.75);

        ctx.append(&make_msg(Role::System, "sys")).await;
        ctx.append(&make_msg(Role::User, &padded("first"))).await;
        ctx.append(&make_msg(Role::Assistant, &padded("second")))
            .await;
        ctx.append(&make_msg(Role::User, &padded("third"))).await;

        // Sanity: budget-gated path is a no-op here.
        let baseline = ctx.maybe_compress("test-model", never_chat).await.unwrap();
        assert!(matches!(baseline, CompressionOutcome::BelowThreshold));
        assert_eq!(ctx.messages().len(), 4);

        // `err_chat` makes the LLM stage fail, falling through to truncate.
        let outcome = ctx.force_compress("test-model", err_chat).await.unwrap();

        assert!(matches!(outcome, CompressionOutcome::Compressed { .. }));
        // system + keep_recent=2 most recent non-system (empty test
        // registry → no trailer reminder).
        assert_eq!(ctx.messages().len(), 3);
        assert_eq!(ctx.messages()[0].role, Role::System);
    }

    #[tokio::test]
    async fn reseed_rereads_grown_workspace_soul_without_vetoing_compaction() {
        // Regression (#1): the soul re-read happens AFTER the savings gate, on
        // committed state — never on the candidate before the shrink decision.
        // A large workspace soul must NOT inflate `after_tokens` and turn a
        // real truncate compaction into a spurious NoSavings.
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Arc::new(baybo_workspace::WorkspacePaths::new(
            dir.path().to_path_buf(),
        ));
        // Distinctive + large: swapping it in before the savings gate (the old
        // bug) would exceed the one-message truncate savings and veto the apply.
        let soul_path = workspace.identity_file(baybo_workspace::IdentityKind::Soul);
        std::fs::create_dir_all(soul_path.parent().expect("profile parent")).expect("profile dir");
        std::fs::write(
            &soul_path,
            format!("DISTINCTIVE_SOUL {}", "soul ".repeat(400)),
        )
        .expect("write soul");

        let mut ctx = ContextManager::from_config(ContextManagerConfig {
            tokenizer: Arc::new(SimpleTokenizer),
            workspace: Arc::clone(&workspace),
            keep_recent: 2,
            compression_threshold: 0.75,
            calibration: Arc::new(TokenCalibration::new()),
            skill_registry: Arc::new(SkillRegistry::new()),
            channel: baybo_model::ChannelType::owner(),
            session_id: test_session_id(),
            sessions: test_sessions(),
            subagent_profile: None,
        });
        ctx.set_active_model_context_window(100_000);
        ctx.append(&make_msg(Role::System, "small seed")).await;
        ctx.append(&make_msg(Role::User, &padded("first"))).await;
        ctx.append(&make_msg(Role::Assistant, &padded("second")))
            .await;
        ctx.append(&make_msg(Role::User, &padded("third"))).await;

        // err_chat → truncate fallback (drops 1 of 3 non-system messages).
        let outcome = ctx.force_compress("test-model", err_chat).await.unwrap();
        // Savings gate compared the old (small) soul on both sides, so the real
        // shrink is honoured rather than vetoed by the large workspace soul.
        assert!(
            matches!(outcome, CompressionOutcome::Compressed { .. }),
            "{outcome:?}"
        );
        // And the system row was refreshed from the workspace after the commit.
        match &ctx.messages()[0].content[0] {
            ContentBlock::Text(t) => assert!(
                t.contains("DISTINCTIVE_SOUL"),
                "system row should be re-read from workspace after compaction"
            ),
            other => panic!("expected system text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn reseed_resolves_subagent_profile_from_registry() {
        // A subagent session resolves its system prompt from the profile
        // registry by name — at seed and re-resolved on every compaction —
        // NOT from the workspace soul.
        let dir = tempfile::tempdir().expect("tempdir");
        let workspace = Arc::new(baybo_workspace::WorkspacePaths::new(
            dir.path().to_path_buf(),
        ));
        // A workspace soul that must NOT leak into a subagent session.
        let soul_path = workspace.identity_file(baybo_workspace::IdentityKind::Soul);
        std::fs::create_dir_all(soul_path.parent().expect("profile parent")).expect("profile dir");
        std::fs::write(&soul_path, "WORKSPACE_SOUL_SHOULD_NOT_APPEAR").expect("write soul");

        let registry = Arc::new(baybo_subagent::SubagentRegistry::new());
        registry.register(baybo_subagent::SubagentProfile {
            name: "test-agent".into(),
            version: "1".into(),
            description: "test".into(),
            system_prompt: "SUBAGENT_PROFILE".into(),
            default_tier: None,
            source: baybo_model::ArtifactSource::Inline,
            trust_level: baybo_model::TrustLevel::Trusted,
            source_path: None,
        });

        let mut ctx = ContextManager::from_config(ContextManagerConfig {
            tokenizer: Arc::new(SimpleTokenizer),
            workspace: Arc::clone(&workspace),
            keep_recent: 2,
            compression_threshold: 0.75,
            calibration: Arc::new(TokenCalibration::new()),
            skill_registry: Arc::new(SkillRegistry::new()),
            channel: baybo_model::ChannelType::owner(),
            session_id: test_session_id(),
            sessions: test_sessions(),
            subagent_profile: Some((Arc::clone(&registry), "test-agent".to_string())),
        });
        ctx.set_active_model_context_window(100_000);
        ctx.append(&make_msg(Role::System, "SUBAGENT_PROFILE"))
            .await;
        ctx.append(&make_msg(Role::User, &padded("first"))).await;
        ctx.append(&make_msg(Role::Assistant, &padded("second")))
            .await;
        ctx.append(&make_msg(Role::User, &padded("third"))).await;

        let outcome = ctx.force_compress("test-model", err_chat).await.unwrap();
        assert!(
            matches!(outcome, CompressionOutcome::Compressed { .. }),
            "{outcome:?}"
        );
        match &ctx.messages()[0].content[0] {
            ContentBlock::Text(t) => {
                assert_eq!(
                    t, "SUBAGENT_PROFILE",
                    "subagent prompt must resolve from the registry"
                );
                assert!(
                    !t.contains("WORKSPACE_SOUL"),
                    "must not use the workspace soul"
                );
            }
            other => panic!("expected system text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn force_compress_strategy_declined_when_cant_shorten() {
        // keep_recent=5 ≥ non-system count → pre-flight gate fires,
        // and `force_compress` surfaces it as `StrategyDeclined` (the
        // budget gate was bypassed; the compressor itself bowed out).
        // No LLM call attempted, so `never_chat` is correct.
        let mut ctx = make_ctx(5, 100_000, 0.75);

        ctx.append(&make_msg(Role::System, "sys")).await;
        ctx.append(&make_msg(Role::User, "hi")).await;
        ctx.append(&make_msg(Role::Assistant, "hello")).await;

        let outcome = ctx.force_compress("test-model", never_chat).await.unwrap();

        assert!(matches!(outcome, CompressionOutcome::StrategyDeclined));
        assert_eq!(ctx.messages().len(), 3);
    }

    #[tokio::test]
    async fn no_compress_when_already_at_keep_recent() {
        // Low threshold triggers compression check, but only 2 non-system
        // messages with keep_recent=5 → pre-flight gate fires.
        // Surfaces as `StrategyDeclined`: the budget gate did fire
        // (`BelowThreshold` would mean we never got to the compressor).
        let mut ctx = make_ctx(5, 10, 0.1);

        ctx.append(&make_msg(Role::System, "sys")).await;
        ctx.append(&make_msg(Role::User, "hi")).await;
        ctx.append(&make_msg(Role::Assistant, "hello")).await;

        let outcome = ctx.maybe_compress("test-model", never_chat).await.unwrap();

        assert!(matches!(outcome, CompressionOutcome::StrategyDeclined));
        assert_eq!(ctx.messages().len(), 3);
    }

    #[tokio::test]
    async fn budget_tracks_tokens() {
        let mut ctx = make_ctx(5, 100_000, 0.75);

        assert_eq!(ctx.budget().current(), 0);

        ctx.append(&make_msg(Role::User, "hello world")).await;

        assert!(ctx.budget().current() > 0);
        assert!(ctx.budget().remaining() < 100_000);
    }

    /// Without a baseline, `count_tokens` falls back to a full
    /// tokenizer sweep. Establishes the baseline-vs-fallback contrast
    /// the next test relies on.
    #[tokio::test]
    async fn count_tokens_falls_back_to_full_count_without_baseline() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        ctx.append(&make_msg(Role::User, "alpha")).await;
        ctx.append(&make_msg(Role::Assistant, "beta")).await;
        ctx.append(&make_msg(Role::User, "gamma")).await;

        let raw_full: usize = ctx
            .messages()
            .iter()
            .map(|m| ctx.tokenizer.count_message(m))
            .sum();
        // No calibration injected → calibrate is identity, full count.
        assert_eq!(ctx.count_tokens(), raw_full);
    }

    /// After `record_call_actual`, `count_tokens` returns
    /// `actual + tokenize(suffix)` — only the messages appended since
    /// the call get BPE-encoded.
    #[tokio::test]
    async fn count_tokens_uses_baseline_plus_delta() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        ctx.append(&make_msg(Role::User, "old-1")).await;
        ctx.append(&make_msg(Role::Assistant, "old-2")).await;
        ctx.append(&make_msg(Role::User, "old-3")).await;
        ctx.record_call_actual(5_000);

        let new_a = make_msg(Role::Assistant, "new-a");
        let new_b = make_msg(Role::User, "new-b");
        ctx.append(&new_a.clone()).await;
        ctx.append(&new_b.clone()).await;

        let expected_delta =
            ctx.tokenizer.count_message(&new_a) + ctx.tokenizer.count_message(&new_b);
        assert_eq!(ctx.count_tokens(), 5_000 + expected_delta);
    }

    fn make_task(subject: &str) -> baybo_model::Task {
        let now = chrono::Utc::now();
        baybo_model::Task {
            id: baybo_model::TaskId::new(),
            subject: subject.into(),
            description: "body".into(),
            status: baybo_model::TaskStatus::Pending,
            depends_on: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }

    /// The transient task reminder rides in the real request, so its tokens
    /// must be charged to the budget estimate (the compression gate would
    /// otherwise under-count and send an over-window request for a big plan).
    #[tokio::test]
    async fn task_reminder_is_charged_to_the_budget() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        ctx.append(&make_msg(Role::User, "plan this")).await;
        let before = ctx.count_tokens();

        ctx.refresh_task_reminder(&[make_task("write the table"), make_task("wire the runtime")]);
        let reminder_raw = ctx.task_reminder_raw;
        assert!(reminder_raw > 0, "a non-empty list produces a reminder");
        assert_eq!(
            ctx.count_tokens(),
            before + reminder_raw,
            "the reminder is charged exactly once"
        );
        assert!(ctx.budget().current() >= before + reminder_raw);

        // Clearing the list drops the charge.
        ctx.refresh_task_reminder(&[]);
        assert_eq!(ctx.task_reminder_raw, 0);
        assert_eq!(ctx.count_tokens(), before);
    }

    /// The provider's actual count includes the reminder, but `count_tokens`
    /// re-adds the current reminder — so `record_call_actual` must strip it from
    /// the baseline, leaving the estimate equal to the real request size.
    #[tokio::test]
    async fn record_call_actual_does_not_double_count_the_reminder() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        ctx.append(&make_msg(Role::User, "plan this")).await;
        ctx.refresh_task_reminder(&[make_task("a"), make_task("b")]);
        assert!(ctx.task_reminder_raw > 0);

        // Provider reported `actual` for a request that included the reminder.
        let actual = 5_000;
        ctx.record_call_actual(actual);

        // No new messages since the call ⇒ the estimate equals `actual`
        // (reminder counted once, not twice).
        assert_eq!(ctx.count_tokens(), actual);
    }

    /// A `UserInterjection` row is sent wrapped in the `<user_interjection>`
    /// envelope (`messages_for_llm`), so `append` must charge the budget the
    /// framed wire size — not the raw text — or the compression gate would
    /// under-count the request. Regression for the wire-only-framing gap.
    #[tokio::test]
    async fn interjection_row_is_budgeted_at_framed_wire_size() {
        let body = "please switch the implementation to TypeScript";

        let mut plain = make_ctx(50, 100_000, 0.95);
        plain
            .append(&ChatMessage::user(vec![ContentBlock::Text(body.into())]))
            .await;
        let plain_tokens = plain.count_tokens();

        let mut interject = make_ctx(50, 100_000, 0.95);
        interject
            .append_user_interjection(vec![ContentBlock::Text(body.into())])
            .await;
        let framed_tokens = interject.count_tokens();

        // Same raw text, but the interjection is charged the envelope on top —
        // its budgeted size is strictly larger, and by more than a stray token
        // or two (the framing preamble is substantial).
        assert!(
            framed_tokens > plain_tokens + 20,
            "interjection must be budgeted at framed wire size: framed={framed_tokens}, plain={plain_tokens}"
        );
    }

    /// The framed wire size must be charged on **rebuild** paths too, not just
    /// the live append — otherwise a `UserInterjection` row preserved across an
    /// actor restart (`restore_messages`) or compaction would silently revert to
    /// the raw count and under-budget the next request.
    #[tokio::test]
    async fn restore_charges_interjection_at_framed_wire_size() {
        let body = "please switch the implementation to TypeScript";

        // Live append charges the framed size.
        let mut live = make_ctx(50, 100_000, 0.95);
        live.append_user_interjection(vec![ContentBlock::Text(body.into())])
            .await;
        let live_tokens = live.count_tokens();

        // Rebuilding the same row via restore_messages must match it.
        let mut restored = make_ctx(50, 100_000, 0.95);
        restored.restore_messages(vec![ChatMessage::user_interjection(vec![
            ContentBlock::Text(body.into()),
        ])]);
        assert_eq!(
            restored.count_tokens(),
            live_tokens,
            "restore must charge a UserInterjection row the same framed size as the live append"
        );
    }

    /// Compression mutates the message prefix in place, so the
    /// baseline's `message_count_at_call` no longer maps to anything
    /// meaningful. `maybe_compress` must drop the baseline; the next
    /// `count_tokens` falls back to a full sweep.
    #[tokio::test]
    async fn compression_invalidates_baseline() {
        let mut ctx = make_ctx(2, 200, 0.25);

        ctx.append(&make_msg(Role::System, "sys")).await;
        ctx.append(&make_msg(Role::User, &padded("msg-1"))).await;
        ctx.append(&make_msg(Role::Assistant, &padded("reply-1")))
            .await;
        ctx.append(&make_msg(Role::User, &padded("msg-2"))).await;
        ctx.record_call_actual(9_999);

        // Pre-compression: baseline applies → big number.
        assert_eq!(ctx.count_tokens(), 9_999);

        // Drive compression. With max=50, threshold=0.5 the budget
        // says "compress" once the post-baseline estimate exceeds 25
        // (here it's 9_999 + 0). `err_chat` forces the LLM stage to
        // fail so the truncate fallback runs deterministically.
        let _ = ctx.maybe_compress("test-model", err_chat).await.unwrap();

        // Post-compression: baseline cleared → must re-tokenize the
        // (now-shrunken) message list, no 9_999 anywhere.
        let raw: usize = ctx
            .messages()
            .iter()
            .map(|m| ctx.tokenizer.count_message(m))
            .sum();
        assert_eq!(ctx.count_tokens(), raw);
        assert!(ctx.count_tokens() < 9_999);
    }

    /// `append` keeps the per-message token cache in step with the
    /// transcript so the suffix loop in `count_tokens` doesn't
    /// re-tokenize across appends. Spot-check by appending after a
    /// baseline is set: each `count_tokens` call must agree with a
    /// fresh full retokenize, and the cache vector's length must
    /// track the slice.
    #[tokio::test]
    async fn cache_stays_in_sync_across_appends() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        ctx.append(&make_msg(Role::User, "first")).await;
        ctx.append(&make_msg(Role::Assistant, "second")).await;
        ctx.record_call_actual(1_000);

        // Append after baseline: count_tokens uses baseline + cached
        // suffix counts. The expected value is `actual + sum of new
        // message counts`.
        let new_a = make_msg(Role::User, "after-baseline-a");
        let new_b = make_msg(Role::Assistant, "after-baseline-b");
        ctx.append(&new_a).await;
        ctx.append(&new_b).await;

        let expected_delta =
            ctx.tokenizer.count_message(&new_a) + ctx.tokenizer.count_message(&new_b);
        assert_eq!(ctx.count_tokens(), 1_000 + expected_delta);
        assert_eq!(ctx.per_message_tokens.len(), ctx.messages().len());
    }

    /// After `maybe_compress` applies a new message list, the cache
    /// must reflect the **new** messages — even when the new length
    /// happens to equal the old (length-only sync would silently
    /// return stale counts). The truncate fallback (driven via
    /// `err_chat`) here keeps `[system, kept tail]`, exercising the
    /// same-prefix replacement branch.
    #[tokio::test]
    async fn cache_rebuilt_after_compression_apply() {
        let mut ctx = make_ctx(2, 200, 0.25);
        ctx.append(&make_msg(Role::System, "You are helpful")).await;
        ctx.append(&make_msg(Role::User, &padded("First"))).await;
        ctx.append(&make_msg(Role::Assistant, &padded("Reply 1")))
            .await;
        ctx.append(&make_msg(Role::User, &padded("Second"))).await;

        let outcome = ctx.maybe_compress("test-model", err_chat).await.unwrap();
        assert!(matches!(outcome, CompressionOutcome::Compressed { .. }));

        // Cache must be in lockstep with the post-compression slice.
        assert_eq!(ctx.per_message_tokens.len(), ctx.messages().len());
        let expected: usize = ctx
            .messages()
            .iter()
            .map(|m| ctx.tokenizer.count_message(m))
            .sum();
        let cached: usize = ctx.per_message_tokens.iter().copied().sum();
        assert_eq!(cached, expected);
    }

    // ---------- Skill-trailer tests ----------

    use baybo_model::{ArtifactSource, TrustLevel};
    use baybo_skills::{SkillDefinition, SkillRequirements};

    /// Build a minimally-populated `SkillDefinition` so tests can
    /// register skills with a chosen body — the registry's renderer
    /// wraps `prompt_template` in `<skill name="…" version="…">…</skill>`,
    /// which is what we assert against downstream.
    fn mk_skill(name: &str, body: &str) -> SkillDefinition {
        SkillDefinition {
            name: name.into(),
            version: "0.1.0".into(),
            description: format!("desc for {name}"),
            command: None,
            agent_invocable: true,
            channels: vec![],
            argument_hint: None,
            prompt_template: body.into(),
            allowed_tools: vec![],
            source: ArtifactSource::Workspace,
            trust_level: TrustLevel::Trusted,
            requirements: SkillRequirements::default(),
            token_budget_hint: 0,
            source_path: None,
            linked_files: Default::default(),
        }
    }

    fn registry_with(skills: &[(&str, &str)]) -> Arc<SkillRegistry> {
        let r = Arc::new(SkillRegistry::new());
        for (name, body) in skills {
            r.register(mk_skill(name, body));
        }
        r
    }

    fn skill_call(skill_name: &str) -> ChatMessage {
        ChatMessage::assistant(vec![ContentBlock::ToolUse {
            id: format!("call-{skill_name}"),
            name: SKILL_TOOL_NAME.into(),
            input: serde_json::json!({ SKILL_INPUT_NAME_FIELD: skill_name }),
            signature: None,
        }])
    }

    /// `append` records every fresh `Skill` ToolUse it sees, in
    /// first-seen order with insertion-order dedup.
    #[tokio::test]
    async fn append_records_skill_calls_in_order() {
        let mut ctx = make_ctx(5, 100_000, 0.75);

        ctx.append(&skill_call("foo")).await;
        ctx.append(&make_msg(Role::User, "u")).await;
        ctx.append(&skill_call("bar")).await;
        ctx.append(&skill_call("foo")).await; // duplicate
        ctx.append(&skill_call("baz")).await;

        assert_eq!(ctx.called_skills, vec!["foo", "bar", "baz"]);
    }

    fn registry_with_slash_skill() -> Arc<SkillRegistry> {
        let registry = Arc::new(SkillRegistry::new());
        let mut skill = mk_skill("weather", "WEATHER_BODY");
        skill.command = Some("wx".into());
        registry.register(skill);
        registry
    }

    /// On cold-start restore a slash skill must be re-derived from its
    /// persisted `/command` row: the injected body carries no `ToolUse`, so
    /// without re-derivation a rehydrated actor drops it from `called_skills`
    /// and a later compaction can't re-broadcast the definition.
    #[tokio::test]
    async fn restore_messages_rederives_slash_skill_from_command_row() {
        let mut ctx = make_ctx_with_registry(registry_with_slash_skill(), 5, 100_000, 0.75);
        ctx.restore_messages(vec![
            make_msg(Role::System, "sys"),
            ChatMessage::user(vec![ContentBlock::Text("/wx today".into())]),
            ChatMessage::agent_context(vec![ContentBlock::Text("WEATHER_BODY".into())]),
        ]);
        assert_eq!(
            ctx.called_skills,
            vec!["weather"],
            "slash skill re-derived from the persisted /command row"
        );
    }

    /// `called_skills_in` unions `ToolUse` skill calls with slash invocations
    /// re-derived from `/command` rows (deduped), so the compaction rebuild
    /// tracks both kinds as long as their call survives in the transcript.
    #[tokio::test]
    async fn called_skills_in_unions_tooluse_and_slash() {
        let ctx = make_ctx_with_registry(registry_with_slash_skill(), 5, 100_000, 0.75);
        let msgs = vec![
            skill_call("foo"),
            ChatMessage::user(vec![ContentBlock::Text("/wx now".into())]),
        ];
        assert_eq!(ctx.called_skills_in(&msgs), vec!["foo", "weather"]);
    }

    /// `record_skill_calls` must ignore `ToolUse` blocks for non-Skill
    /// tools so we don't accidentally render Bash / WebFetch / etc.
    /// detail blocks at compression time.
    #[tokio::test]
    async fn append_ignores_non_skill_tool_uses() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        let bash_call = ChatMessage::assistant(vec![ContentBlock::ToolUse {
            id: "1".into(),
            name: "Bash".into(),
            input: serde_json::json!({ "command": "ls" }),
            signature: None,
        }]);
        ctx.append(&bash_call).await;
        assert!(ctx.called_skills.is_empty());
    }

    /// Helper: build a ContextManager with a custom skill registry and
    /// the test-defaults for everything else.
    fn make_ctx_with_registry(
        registry: Arc<SkillRegistry>,
        keep_recent: usize,
        max_tokens: usize,
        threshold: f64,
    ) -> ContextManager {
        let mut ctx = ContextManager::from_config(ContextManagerConfig {
            tokenizer: Arc::new(SimpleTokenizer),
            workspace: test_workspace(),
            keep_recent,
            compression_threshold: threshold,
            calibration: Arc::new(TokenCalibration::new()),
            skill_registry: registry,
            channel: baybo_model::ChannelType::owner(),
            session_id: test_session_id(),
            sessions: test_sessions(),
            subagent_profile: None,
        });
        ctx.set_active_model_context_window(max_tokens);
        ctx
    }

    #[tokio::test]
    async fn ensure_seeded_seeds_leading_system_row_on_fresh_session() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        ctx.ensure_seeded().await;
        assert_eq!(ctx.message_count(), 1, "no skills → one system row");
        assert_eq!(ctx.messages()[0].role, Role::System);
    }

    #[tokio::test]
    async fn ensure_seeded_is_idempotent() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        ctx.ensure_seeded().await;
        assert_eq!(ctx.message_count(), 1);
        ctx.ensure_seeded().await;
        assert_eq!(ctx.message_count(), 1, "re-seeding is a no-op");
    }

    #[tokio::test]
    async fn ensure_seeded_appends_skill_reminder_when_skills_invocable() {
        let mut ctx = make_ctx_with_registry(registry_with(&[("alpha", "body")]), 5, 100_000, 0.75);
        ctx.ensure_seeded().await;
        assert_eq!(ctx.message_count(), 2, "system row + skill reminder");
        assert_eq!(ctx.messages()[0].role, Role::System);
        let ContentBlock::Text(reminder) = &ctx.messages()[1].content[0] else {
            panic!("reminder should be a text block");
        };
        assert!(
            reminder.contains("alpha"),
            "reminder advertises the invocable skill"
        );
    }

    #[tokio::test]
    async fn expand_slash_command_injects_skill_body_as_agent_context() {
        let registry = Arc::new(SkillRegistry::new());
        let mut skill = mk_skill("weather", "WEATHER INSTRUCTIONS");
        skill.command = Some("wx".into());
        registry.register(skill);
        let mut ctx = make_ctx_with_registry(registry, 5, 100_000, 0.75);
        ctx.append(&ChatMessage::user(vec![ContentBlock::Text(
            "/wx Beijing".into(),
        )]))
        .await;

        ctx.expand_slash_command().await;
        // appended the skill body as a hidden agent-context row after `/wx`
        assert_eq!(ctx.message_count(), 2);
        let appended = ctx.messages().last().expect("appended row");
        assert_eq!(appended.source(), baybo_model::MessageSource::Agent);
        let ContentBlock::Text(body) = &appended.content[0] else {
            panic!("expected a text block");
        };
        assert!(body.contains("WEATHER INSTRUCTIONS"));
    }

    #[tokio::test]
    async fn expand_slash_command_noop_for_plain_text_or_unknown_command() {
        let registry = Arc::new(SkillRegistry::new());
        let mut skill = mk_skill("weather", "BODY");
        skill.command = Some("wx".into());
        registry.register(skill);
        let mut ctx = make_ctx_with_registry(registry, 5, 100_000, 0.75);

        ctx.append(&ChatMessage::user(vec![ContentBlock::Text("hello".into())]))
            .await;
        ctx.expand_slash_command().await;
        assert_eq!(ctx.message_count(), 1, "plain text is not a slash command");

        ctx.append(&ChatMessage::user(vec![ContentBlock::Text(
            "/unknown".into(),
        )]))
        .await;
        ctx.expand_slash_command().await;
        assert_eq!(ctx.message_count(), 2, "unknown command appends nothing");
    }

    /// A slash-only skill (`disable-model-invocation: true` +
    /// `user-invocable: true`) must stay expandable on the user's
    /// explicit `/command` while being hidden from the model's
    /// advertised listing. The candidate sets are deliberately
    /// different — this combination used to be dead because slash
    /// detection reused `invocable_skill_summaries`.
    #[tokio::test]
    async fn slash_expands_a_skill_hidden_from_the_model() {
        let registry = Arc::new(SkillRegistry::new());
        let mut skill = mk_skill("deck", "CARD_BODY");
        skill.command = Some("deck".into());
        skill.agent_invocable = false;
        registry.register(skill);
        let mut ctx = make_ctx_with_registry(registry, 5, 100_000, 0.75);

        assert!(
            ctx.invocable_skill_summaries().is_empty(),
            "not advertised to the model"
        );

        ctx.append(&ChatMessage::user(vec![ContentBlock::Text(
            "/deck quota monitor".into(),
        )]))
        .await;
        ctx.expand_slash_command().await;
        let last = ctx.messages().last().unwrap();
        match last.content.first() {
            Some(ContentBlock::Text(t)) => assert!(t.contains("CARD_BODY")),
            other => panic!("expected injected body, got {other:?}"),
        }
    }

    /// A `channels:`-restricted skill is invisible on other channels:
    /// not listed, and its `/command` falls through as plain text.
    #[tokio::test]
    async fn channel_restricted_skill_is_inert_off_channel() {
        let mk_registry = || {
            let registry = Arc::new(SkillRegistry::new());
            let mut skill = mk_skill("deck", "CARD_BODY");
            skill.command = Some("deck".into());
            skill.channels = vec![baybo_model::ChannelType::owner()];
            registry.register(skill);
            registry
        };

        let mut on_owner = make_ctx_with_registry(mk_registry(), 5, 100_000, 0.75);
        assert_eq!(on_owner.invocable_skill_summaries().len(), 1);
        on_owner
            .append(&ChatMessage::user(vec![ContentBlock::Text("/deck".into())]))
            .await;
        on_owner.expand_slash_command().await;
        assert_eq!(on_owner.message_count(), 2, "owner session expands");

        let mut on_telegram = make_ctx_with_registry(mk_registry(), 5, 100_000, 0.75);
        on_telegram.channel = baybo_model::ChannelType::telegram();
        assert!(
            on_telegram.invocable_skill_summaries().is_empty(),
            "hidden from a telegram session's listing"
        );
        on_telegram
            .append(&ChatMessage::user(vec![ContentBlock::Text("/deck".into())]))
            .await;
        on_telegram.expand_slash_command().await;
        assert_eq!(
            on_telegram.message_count(),
            1,
            "telegram session must not expand an owner-only skill"
        );
    }

    /// The trailer's reminder block advertises the caller-supplied
    /// (seed-filtered) set — not the raw registry — and is skipped
    /// entirely when that set is empty; the called-skill detail block
    /// stays keyed on `called_skills` regardless.
    #[test]
    fn skill_trailer_respects_the_advertised_set() {
        let registry = registry_with(&[("visible", "V_BODY"), ("hidden", "H_BODY")]);
        let advertised: Vec<SkillSummary> = registry
            .all_summaries_sorted()
            .into_iter()
            .filter(|s| s.name == "visible")
            .collect();

        let mut messages = vec![make_msg(Role::System, "sys")];
        insert_skill_trailer(
            &mut messages,
            &registry,
            &SimpleTokenizer,
            &["hidden".to_string()],
            &advertised,
        );
        let texts: Vec<&str> = messages
            .iter()
            .map(|m| match m.content.first() {
                Some(ContentBlock::Text(t)) => t.as_str(),
                _ => "",
            })
            .collect();
        assert!(texts[1].contains("- visible"));
        assert!(
            !texts[1].contains("- hidden"),
            "reminder leaks hidden skill"
        );
        assert!(texts[2].contains("H_BODY"), "called skill keeps its detail");

        let mut bare = vec![make_msg(Role::System, "sys")];
        insert_skill_trailer(&mut bare, &registry, &SimpleTokenizer, &[], &[]);
        assert_eq!(bare.len(), 1, "empty advertised set inserts no reminder");
    }

    // ---------- compaction shape ----------

    #[test]
    fn recent_slice_cap_scales_with_the_window() {
        // The absolute ceiling only binds on a large window.
        assert_eq!(recent_slice_bounds(1_000_000).2, 40_000);
        assert_eq!(recent_slice_bounds(272_000).2, 40_000);
        assert_eq!(recent_slice_bounds(100_000).2, 15_000);
        assert_eq!(recent_slice_bounds(8_192).2, 1_228);
    }

    /// The walk takes the floor as a soft stop and the cap as a hard one, so a
    /// floor above the cap would mean "never stop" — the slice would swallow
    /// the transcript it was meant to trim.
    #[test]
    fn recent_slice_floor_never_exceeds_the_cap() {
        for window in [0, 1, 200, 1_000, 8_192, 50_000, 272_000, 1_000_000] {
            let (min_tokens, _, cap) = recent_slice_bounds(window);
            assert!(
                min_tokens <= cap,
                "window {window}: floor {min_tokens} above cap {cap}"
            );
        }
    }

    /// A compaction keeps the tail verbatim. Losing that is what would turn the
    /// last tool results and the user's own words into a paraphrase of
    /// themselves the moment the threshold trips.
    #[tokio::test]
    async fn compaction_keeps_a_verbatim_recent_slice_after_the_summary() {
        let mut ctx = make_ctx(2, 10_000, 0.5);
        ctx.append(&make_msg(Role::System, "sys")).await;
        for i in 0..12 {
            ctx.append(&make_msg(
                Role::User,
                &format!("m{i} {}", "x".repeat(2_400)),
            ))
            .await;
        }
        let last = ctx.messages().last().cloned().expect("a last message");

        let outcome = ctx
            .maybe_compress("test-model", ok_summary_chat)
            .await
            .unwrap();
        assert!(matches!(outcome, CompressionOutcome::Compressed { .. }));

        let applied = ctx.messages();
        assert!(
            applied.len() > 2,
            "expected system + summary + a tail, got {}",
            applied.len()
        );
        assert_eq!(
            applied.last().map(|m| &m.content),
            Some(&last.content),
            "the newest message must survive byte-identical, not paraphrased"
        );
    }

    /// The slice is cut in atomic units, so it can never start between an
    /// `assistant{tool_use}` and its `user{tool_result}` — both Anthropic and
    /// OpenAI reject that array outright.
    #[tokio::test]
    async fn compaction_slice_never_splits_a_tool_use_result_pair() {
        let mut ctx = make_ctx(2, 10_000, 0.5);
        ctx.append(&make_msg(Role::System, "sys")).await;
        for i in 0..10 {
            ctx.append(&make_msg(
                Role::User,
                &format!("m{i} {}", "x".repeat(2_400)),
            ))
            .await;
        }
        ctx.append(&ChatMessage::assistant(vec![ContentBlock::ToolUse {
            id: "tu1".into(),
            name: "bash".into(),
            input: serde_json::Value::Null,
            signature: None,
        }]))
        .await;
        ctx.append(&ChatMessage::agent_context(vec![
            ContentBlock::ToolResult {
                tool_use_id: "tu1".into(),
                content: "ok".into(),
                meta: None,
            },
        ]))
        .await;

        ctx.maybe_compress("test-model", ok_summary_chat)
            .await
            .unwrap();

        let has_result = ctx.messages().iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "tu1"))
        });
        let has_use = ctx.messages().iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == "tu1"))
        });
        assert!(
            !has_result || has_use,
            "a kept tool_result must keep its tool_use"
        );
    }

    /// On a window too small to afford one, the tail is dropped — a slice
    /// sized by an absolute constant would exceed the very threshold that
    /// triggered the compaction. And having compacted, the turn must not keep
    /// buying the same call: on a window this small the continuation framing
    /// alone outweighs the ceiling, so what stops the loop is the pre-flight
    /// gate, not the budget.
    #[tokio::test]
    async fn small_window_compaction_degrades_to_summary_only() {
        let mut ctx = make_ctx(1, 200, 0.5);
        ctx.append(&make_msg(Role::System, "sys")).await;
        for i in 0..5 {
            ctx.append(&make_msg(Role::User, &format!("m{i} {}", "x".repeat(240))))
                .await;
        }

        let outcome = ctx
            .maybe_compress("test-model", ok_summary_chat)
            .await
            .unwrap();
        assert!(matches!(outcome, CompressionOutcome::Compressed { .. }));

        let surviving_originals = ctx
            .messages()
            .iter()
            .filter(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Text(t) if t.starts_with('m')))
            })
            .count();
        assert_eq!(
            surviving_originals, 0,
            "no window-relative tail fits here, so none should have been kept"
        );

        // `never_chat` panics if the summarizer is reached a second time.
        let second = ctx.maybe_compress("test-model", never_chat).await.unwrap();
        assert!(matches!(second, CompressionOutcome::StrategyDeclined));
    }

    /// The candidate pick spends no second LLM call: the summary is already in
    /// hand, so dropping the slice is a re-assembly, not a round-trip.
    #[tokio::test]
    async fn dropping_the_slice_costs_no_second_call() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);

        let mut ctx = make_ctx(1, 1_000, 0.1);
        ctx.append(&make_msg(Role::System, "sys")).await;
        for i in 0..3 {
            ctx.append(&make_msg(Role::User, &format!("m{i} {}", "x".repeat(240))))
                .await;
        }

        ctx.maybe_compress("test-model", move |req, marker| {
            seen.fetch_add(1, Ordering::SeqCst);
            ok_summary_chat(req, marker)
        })
        .await
        .unwrap();

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the summarizer must be called exactly once per compaction"
        );
    }

    /// Once a compaction has come back with no savings, repeating it at the
    /// same transcript length buys the same answer — and the threshold check
    /// runs at the top of every loop iteration, so without the latch the rest
    /// of the turn is one full-transcript LLM call per iteration.
    #[tokio::test]
    async fn declined_compaction_does_not_refire_until_the_transcript_grows() {
        let mut ctx = make_ctx(1, 100, 0.1);
        ctx.append(&make_msg(Role::System, "sys")).await;
        for i in 0..3 {
            ctx.append(&make_msg(Role::User, &format!("m{i}"))).await;
        }

        let first = ctx
            .maybe_compress("test-model", ok_summary_chat)
            .await
            .unwrap();
        assert!(
            matches!(first, CompressionOutcome::NoSavings),
            "fixture must actually produce NoSavings, got {first:?}"
        );

        // `never_chat` panics if the summarizer is reached.
        let second = ctx.maybe_compress("test-model", never_chat).await.unwrap();
        assert!(matches!(second, CompressionOutcome::NoSavings));

        // Growth is exactly the condition under which compaction can start
        // paying again, so the latch releases.
        for i in 0..40 {
            ctx.append(&make_msg(
                Role::User,
                &format!("grown{i} {}", "x".repeat(400)),
            ))
            .await;
        }
        let third = ctx
            .maybe_compress("test-model", ok_summary_chat)
            .await
            .unwrap();
        assert!(
            !matches!(third, CompressionOutcome::NoSavings),
            "the latch must release once the transcript grows past it"
        );
    }

    /// Chat closure returning a well-formed `<summary>S</summary>` so the
    /// LLM-summary stage produces a usable summary message.
    async fn ok_summary_chat(
        _: ChatRequest,
        _: LlmCallInputs,
    ) -> std::result::Result<LlmResponse, ContextError> {
        Ok(LlmResponse {
            content: "<analysis>x</analysis><summary>S</summary>".into(),
            content_blocks: vec![],
            tool_calls: vec![],
            usage: Default::default(),
            thinking: None,
        })
    }

    /// With a skill registry attached, after the LLM-summary stage
    /// produces a usable response the manager inserts `[reminder,
    /// detail]` right after the system block (when there are
    /// previously-called skills the registry can render).
    #[tokio::test]
    async fn summarize_apply_inserts_skill_trailer_after_system() {
        let registry = registry_with(&[("foo", "FOO_BODY")]);
        let mut ctx = make_ctx_with_registry(registry, 2, 50, 0.5);
        // Long enough that compression with the (real, registry-rendered)
        // trailer still wins on tokens — SimpleTokenizer counts text as
        // `len()/4 + 1`, so a few hundred bytes of user text easily out-
        // weighs the ~120-byte reminder + ~150-byte detail trailer.
        ctx.append(&make_msg(Role::System, "sys")).await;
        ctx.append(&make_msg(Role::User, &"u1 ".repeat(800))).await;
        ctx.append(&skill_call("foo")).await;
        ctx.append(&make_msg(Role::Assistant, &"a1 ".repeat(800)))
            .await;
        ctx.append(&make_msg(Role::User, &"u2 ".repeat(800))).await;

        let outcome = ctx
            .maybe_compress("test-model", ok_summary_chat)
            .await
            .unwrap();
        assert!(matches!(outcome, CompressionOutcome::Compressed { .. }));

        // [system, reminder, detail, summary]
        assert_eq!(ctx.messages().len(), 4);
        let texts: Vec<&str> = ctx
            .messages()
            .iter()
            .map(|m| match m.content.first() {
                Some(ContentBlock::Text(t)) => t.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(texts[0], "sys");
        assert!(texts[1].contains("The following skills are available"));
        assert!(texts[1].contains("- foo: desc for foo"));
        assert!(texts[2].contains("<skill name=\"foo\" version=\"0.1.0\">"));
        assert!(texts[2].contains("FOO_BODY"));
        // The summary message is the continuation-style block: the
        // intro + the parsed summary body (label-prefixed for the LLM
        // path) + the transcript pointer + the footer.
        assert!(texts[3].contains("This session is being continued"));
        assert!(texts[3].contains("Summary:\nS"));
        assert!(texts[3].contains("read the full transcript at:"));
    }

    /// After a successful LLM-summary apply the called_skills vector
    /// is empty: the trailer is plain text with no `ToolUse`, and the
    /// rebuild re-scans only the new (post-trailer) slice.
    #[tokio::test]
    async fn called_skills_clears_after_summarize_apply() {
        let registry = registry_with(&[("foo", "FOO_BODY")]);
        let mut ctx = make_ctx_with_registry(registry, 2, 50, 0.5);
        ctx.append(&make_msg(Role::System, "sys")).await;
        ctx.append(&make_msg(Role::User, &"u1 ".repeat(800))).await;
        ctx.append(&skill_call("foo")).await;
        ctx.append(&make_msg(Role::Assistant, &"a1 ".repeat(800)))
            .await;
        ctx.append(&make_msg(Role::User, &"u2 ".repeat(800))).await;
        assert_eq!(ctx.called_skills, vec!["foo"]);

        ctx.maybe_compress("test-model", ok_summary_chat)
            .await
            .unwrap();
        assert!(ctx.called_skills.is_empty());
    }

    /// A slash-invoked skill must survive a live-LLM-summary compression.
    /// `expand_slash_command` records the match in `called_skills`, so when
    /// the summary stage drops the body row (it keeps no recent slice) the
    /// skill trailer still re-broadcasts the full definition. Without the
    /// record the model would act on the `/command` with only the generic
    /// one-line reminder.
    #[tokio::test]
    async fn slash_invoked_skill_survives_summarize_via_trailer() {
        let registry = Arc::new(SkillRegistry::new());
        let mut skill = mk_skill("weather", "WEATHER_BODY");
        skill.command = Some("wx".into());
        registry.register(skill);
        let mut ctx = make_ctx_with_registry(registry, 2, 50, 0.5);

        ctx.append(&make_msg(Role::System, "sys")).await;
        ctx.append(&make_msg(Role::User, &"u1 ".repeat(800))).await;
        ctx.append(&make_msg(Role::Assistant, &"a1 ".repeat(800)))
            .await;
        ctx.append(&ChatMessage::user(vec![ContentBlock::Text(
            "/wx today".into(),
        )]))
        .await;

        ctx.expand_slash_command().await;
        assert_eq!(
            ctx.called_skills,
            vec!["weather"],
            "slash invocation is recorded so the trailer can re-broadcast it"
        );

        let outcome = ctx
            .maybe_compress("test-model", ok_summary_chat)
            .await
            .unwrap();
        assert!(matches!(outcome, CompressionOutcome::Compressed { .. }));

        // The agent-context body row was folded into the summary; the full
        // definition is back only because the trailer detail block carries it.
        let has_body = ctx.messages().iter().any(|m| {
            matches!(m.content.first(), Some(ContentBlock::Text(t)) if t.contains("WEATHER_BODY"))
        });
        assert!(
            has_body,
            "skill body survives compression via the trailer detail block"
        );
    }

    // ---------- render_skill_block_capped / build_skill_detail_payload ----------
    //
    // The end-to-end `maybe_compress` path is hard to drive against
    // these caps because compression also has to *win* on tokens
    // before the manager applies the new slice. Unit-test the helpers
    // directly so the truncation contract is exercised without the
    // budget-comparison gate getting in the way.

    #[test]
    fn render_skill_block_capped_returns_full_when_under_cap() {
        let skill = mk_skill("foo", "short body");
        let rendered = render_skill_block_capped(skill.clone(), &SimpleTokenizer, 10_000)
            .expect("must render");
        // Identical to the un-capped rendering — no truncation marker.
        assert_eq!(rendered, render_skill_block(&skill));
        assert!(!rendered.contains("[truncated]"));
    }

    #[test]
    fn render_skill_block_capped_truncates_oversized_body() {
        // SimpleTokenizer: text.len()/4 + 1. A 24_000-byte body alone
        // costs ~6_001 tokens, so the full block lands well past
        // PER_SKILL_TOKEN_CAP (5_000).
        let body = "x".repeat(24_000);
        let skill = mk_skill("big", &body);
        let rendered = render_skill_block_capped(skill, &SimpleTokenizer, PER_SKILL_TOKEN_CAP)
            .expect("must render");

        assert!(rendered.contains("name=\"big\""));
        assert!(rendered.contains("[truncated]"));
        assert!(rendered.ends_with("</skill>"));
        assert!(SimpleTokenizer.count_text(&rendered) <= PER_SKILL_TOKEN_CAP);
        // Body shrank — way fewer 'x's than the original 24_000.
        assert!(rendered.matches('x').count() < 24_000);
    }

    #[test]
    fn render_skill_block_capped_returns_none_when_cap_too_small() {
        let skill = mk_skill("foo", &"x".repeat(1_000));
        // 10 tokens wouldn't fit even the wrapper, never mind the
        // truncation marker — the proportional sizing rounds to 0
        // and the helper bails.
        assert!(render_skill_block_capped(skill, &SimpleTokenizer, 10).is_none());
    }

    #[test]
    fn build_skill_detail_payload_truncates_only_oversized_entries() {
        let big = "x".repeat(24_000);
        let registry = registry_with(&[("big", big.as_str()), ("small", "SMALL_BODY")]);
        let payload = build_skill_detail_payload(
            &registry,
            &SimpleTokenizer,
            &["big".to_string(), "small".to_string()],
        )
        .expect("payload");

        assert!(payload.contains("name=\"big\""));
        assert!(payload.contains("[truncated]"));
        assert!(payload.contains("name=\"small\""));
        assert!(payload.contains("SMALL_BODY"));
        // Small skill rendered untouched — no marker on its body.
        let small_block_start = payload.find("name=\"small\"").unwrap();
        let small_block = &payload[small_block_start..];
        assert!(!small_block.contains("[truncated]"));
    }

    #[test]
    fn build_skill_detail_payload_keeps_total_under_cap() {
        // Eight ~24_000-char bodies, each rendering at ~6_000 tokens
        // when uncapped → far past the 25_000 total. The routine must
        // shrink the effective per-skill budget toward the end of the
        // list (and drop entries once nothing fits) so the final
        // payload stays under TOTAL_SKILL_TOKEN_CAP regardless.
        let body = "z".repeat(24_000);
        let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let entries: Vec<(&str, &str)> = names.iter().map(|n| (*n, body.as_str())).collect();
        let registry = registry_with(&entries);

        let payload = build_skill_detail_payload(
            &registry,
            &SimpleTokenizer,
            &names.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
        .expect("payload");

        let cost = SimpleTokenizer.count_text(&payload);
        // Wrapper adds ~100 chars of fixed overhead — allow a small slack.
        assert!(
            cost <= TOTAL_SKILL_TOKEN_CAP + 100,
            "trailer cost {cost} exceeded total cap"
        );
        // First skills always make it in.
        assert!(payload.contains("name=\"a\""));
        // Truncation marker proves at least one entry was shrunk
        // rather than rendered full.
        assert!(payload.contains("[truncated]"));
    }

    #[test]
    fn build_skill_detail_payload_drops_skills_when_budget_zero() {
        // Three ~24_000-char bodies fed into a registry where the
        // first occupies almost the full total cap. The trailing
        // skills get a vanishing per-skill cap and the final entry
        // ends up dropped entirely.
        let body = "w".repeat(24_000);
        let names = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
        let entries: Vec<(&str, &str)> = names.iter().map(|n| (*n, body.as_str())).collect();
        let registry = registry_with(&entries);

        let payload = build_skill_detail_payload(
            &registry,
            &SimpleTokenizer,
            &names.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        )
        .expect("payload");

        let cost = SimpleTokenizer.count_text(&payload);
        assert!(cost <= TOTAL_SKILL_TOKEN_CAP + 100);
        // Last skill in the list cannot survive — by then the
        // remaining budget is at or near zero and `render_skill_block_capped`
        // refuses to ship an empty wrapper.
        assert!(!payload.contains("name=\"j\""));
    }

    #[test]
    fn build_skill_detail_payload_returns_none_when_nothing_fits() {
        // All skills are missing from the registry → payload is None,
        // so the trailer-emitting caller skips the message entirely.
        let registry = Arc::new(SkillRegistry::new());
        assert!(
            build_skill_detail_payload(&registry, &SimpleTokenizer, &["ghost".to_string()])
                .is_none()
        );
    }
}
