pub mod budget;
pub mod calibration;
pub mod error;
pub mod strategy;
pub mod tokenizer;

pub use budget::TokenBudget;
pub use calibration::TokenCalibration;
pub use error::ContextError;
pub use strategy::summarize::Summarize;
pub use strategy::truncate::Truncate;
pub use strategy::{CompressOutput, CompressionStrategy};
pub use tokenizer::{TiktokenTokenizer, Tokenizer};

pub type Result<T> = std::result::Result<T, ContextError>;

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use aura_llm::{ChatRequest, LlmResponse};
use aura_model::ChatMessage;
use aura_model::Session;
use aura_model::{ContentBlock, Role};
use aura_skills::render::{render_skill_block, render_skill_reminder};
use aura_skills::{SKILL_INPUT_NAME_FIELD, SKILL_TOOL_NAME, SkillDefinition, SkillRegistry};
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
/// "Nothing changed" is split into three reason-specific variants so
/// callers (notably the `/compact` notice path in the agent loop) can
/// surface *why* nothing was applied instead of a generic message.
///
/// Cost recording is the caller's responsibility — both entry points
/// invoke the supplied chat closure for any LLM call, and that closure
/// is where the agent loop opens its trace span and records cost.
/// Hence the outcome carries no LLM-call provenance.
#[derive(Debug, Clone, Copy)]
pub enum CompressionOutcome {
    /// `session.messages` was replaced with a shorter list. `before`
    /// and `after` are the pre/post token totals — the manager owns
    /// these numbers, so callers don't have to re-read `budget()`.
    Compressed { before: usize, after: usize },
    /// Budget was under the configured compression threshold and the
    /// strategy was not invoked. Only produced by `maybe_compress` —
    /// `force_compress` bypasses the threshold by design.
    BelowThreshold,
    /// The strategy itself returned `NoOp` without invoking the chat
    /// closure (e.g. `Truncate` when the non-system message count is
    /// already at or below `keep_recent`). No LLM call was made.
    StrategyDeclined,
    /// The strategy produced a candidate slice, but its post-tokenise
    /// total was not smaller than the original. The manager refused
    /// to apply it (so the budget stays honest) and `session.messages`
    /// is unchanged.
    NoSavings { before: usize, after: usize },
}

/// Manages session context: appending messages with automatic compression
/// and token budget tracking.
///
/// This is a concrete struct, not a trait. The extension points are the
/// `CompressionStrategy` injected at construction (the management logic
/// — append, budget check — is invariant) and the optional
/// `SkillRegistry` plumbed in via [`Self::with_skill_registry`] which
/// lets the manager re-broadcast the authoritative skill list and
/// re-attach per-skill detail blocks after a summary-style compression
/// has wiped the historical `<skill>` / `<system-reminder>` messages
/// off the front of the conversation.
pub struct ContextManager {
    tokenizer: Arc<dyn Tokenizer>,
    strategy: Box<dyn CompressionStrategy>,
    budget: TokenBudget,
    calibration: Option<Arc<TokenCalibration>>,
    /// Used to render the post-compression skill trailer. `None` keeps
    /// the manager skill-agnostic for callers that don't need the
    /// trailer (Truncate-only flows, isolated tests).
    skill_registry: Option<Arc<SkillRegistry>>,
    /// Skills the model has invoked via the `Skill` tool somewhere in
    /// the current `session.messages`, in first-seen order with
    /// duplicates collapsed. Maintained incrementally by
    /// [`Self::append`] and rebuilt on every compression apply so the
    /// vector always mirrors the current message slice. Plain `Vec`
    /// (not `RwLock`) because every read/write site is on a
    /// `&mut self` path; nothing else looks at it.
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
    /// Per-message token count memo, kept in step with the slice
    /// callers pass to `count_tokens` / `raw_estimate`. Eliminates
    /// the O(N²) re-tokenization the post-baseline suffix loop and
    /// post-compression full sweep would otherwise repeat across a
    /// turn's appends.
    ///
    /// In sync when `len() == messages.len()`. We detect drift via
    /// length mismatch and resync transparently — fast path stays
    /// O(1) per slot; the slow path collapses to one full retokenize
    /// (the same cost the cache-free implementation paid every call).
    ///
    /// Failure mode the length check can't catch: same length /
    /// different content. The only producer of that pattern is
    /// `maybe_compress` apply, which builds its replacement cache
    /// explicitly during the apply branch instead of going through
    /// the read path. No other call site mutates `session.messages`
    /// in place.
    per_message_tokens: RwLock<Vec<usize>>,
}

impl ContextManager {
    pub fn new(
        tokenizer: Arc<dyn Tokenizer>,
        strategy: Box<dyn CompressionStrategy>,
        budget: TokenBudget,
    ) -> Self {
        Self {
            tokenizer,
            strategy,
            budget,
            calibration: None,
            skill_registry: None,
            called_skills: Vec::new(),
            baseline: RwLock::new(None),
            current_model: RwLock::new(None),
            per_message_tokens: RwLock::new(Vec::new()),
        }
    }

    /// Attach a `TokenCalibration` so `count_tokens` scales raw BPE
    /// estimates by the per-model `actual / estimate` ratio fed back
    /// from the agent loop after each main LLM call. Without this, the
    /// budget tracks the unscaled estimate (still correct, just less
    /// accurate vs real `usage.input_tokens`).
    pub fn with_calibration(mut self, calibration: Arc<TokenCalibration>) -> Self {
        self.calibration = Some(calibration);
        self
    }

    /// Attach a `SkillRegistry` so the manager can append a fresh
    /// skill trailer (authoritative `<system-reminder>` skill list +
    /// per-skill detail blocks for skills called during the now-wiped
    /// history) after any compression that flagged
    /// `replaced_full_history`. Without this, the post-compression
    /// message list is whatever the strategy produced, unmodified.
    pub fn with_skill_registry(mut self, registry: Arc<SkillRegistry>) -> Self {
        self.skill_registry = Some(registry);
        self
    }

    /// Settle calibration + baseline post-response from a main LLM
    /// call. Pass `sent_messages` — the exact slice handed to
    /// `LlmCompletion::chat`/`chat_stream` — so the calibrator's
    /// `(estimate, actual)` pair is computed against the same set
    /// the provider billed. Must be called before any subsequent
    /// mutation of `session.messages`; the agent loop honours this
    /// because it sits inside the `with_llm_span` closure that
    /// returns before the assistant message is appended.
    ///
    /// Skipped when `actual_input_tokens == 0` — a hard transport
    /// failure with no usage signal; leaving the prior baseline in
    /// place beats overwriting it with zero.
    ///
    /// Only call this for *main* LLM calls. Compression calls
    /// summarise old non-system messages with no tools schema, so
    /// their `(estimate, actual)` ratio doesn't generalise.
    pub fn record_call_actual(&self, sent_messages: &[ChatMessage], actual_input_tokens: usize) {
        if actual_input_tokens == 0 {
            return;
        }
        if let (Some(cal), Some(model_id)) =
            (&self.calibration, self.current_model.read().as_deref())
        {
            let raw = self.raw_estimate(sent_messages);
            cal.observe(model_id, raw, actual_input_tokens);
        }
        *self.baseline.write() = Some(TokenBaseline {
            actual_tokens: actual_input_tokens,
            message_count_at_call: sent_messages.len(),
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

    fn raw_estimate(&self, messages: &[ChatMessage]) -> usize {
        self.cached_sum(messages, 0..messages.len())
    }

    /// Sum per-message token counts over `range`, using `per_message_tokens`
    /// when in sync (length matches `messages.len()`). On length mismatch
    /// the cache has drifted (compression replaced the slice, the caller
    /// truncated externally, etc.) — re-tokenize the whole slice, install
    /// it as the new cache, and sum the requested range from the fresh
    /// vector. The resync cost is exactly what the cache-free path used
    /// to pay every call; the next call is back on the fast path.
    fn cached_sum(&self, messages: &[ChatMessage], range: std::ops::Range<usize>) -> usize {
        {
            let cache = self.per_message_tokens.read();
            if cache.len() == messages.len() {
                return cache[range].iter().copied().sum();
            }
        }
        let fresh: Vec<usize> = messages
            .iter()
            .map(|m| self.tokenizer.count_message(m))
            .collect();
        let total: usize = fresh[range].iter().copied().sum();
        *self.per_message_tokens.write() = fresh;
        total
    }

    /// Append a message to the session context and update the token
    /// budget. Does **not** trigger compression — the caller (the
    /// agent loop) is responsible for invoking
    /// [`Self::maybe_compress`] at well-defined points where it can
    /// also record the compression LLM call's cost. Auto-compressing
    /// here would silently bypass that cost-recording path.
    ///
    /// Safe because the agent loop runs `maybe_compress` at the top
    /// of every iteration, so any over-budget state from intermediate
    /// `append` calls is resolved before the next LLM request is
    /// built.
    pub fn append(&mut self, session: &mut Session, msg: &ChatMessage) {
        let count = self.tokenizer.count_message(msg);
        record_skill_calls(&mut self.called_skills, msg);
        session.messages.push(msg.clone());
        self.per_message_tokens.write().push(count);
        self.budget.update(self.count_tokens(&session.messages));
    }

    /// Check the token budget and compress if the threshold is exceeded.
    /// Called at the top of every agent-loop iteration before the next
    /// `ChatRequest` is built.
    ///
    /// `model_id` is the LLM the next main call will hit. It's stored
    /// as the calibration key for subsequent `count_tokens` /
    /// `record_call_actual` calls; switching `model_id` invalidates
    /// the baseline (the prior `actual_tokens` was tokenised by the
    /// old provider).
    ///
    /// `chat` is invoked only if the strategy chooses to make an LLM
    /// call (i.e. [`Summarize`]). It performs the request inside a
    /// trace span and records cost against the ledger; the strategy
    /// owns trim + empty-summary checking and falls back to a
    /// Truncate-equivalent slice on transport / sanitize failure or
    /// empty content so a transient summarizer failure never kills
    /// the user's turn. Pure strategies (`Truncate`) ignore `chat`
    /// entirely.
    pub async fn maybe_compress<F, Fut>(
        &mut self,
        session: &mut Session,
        model_id: &str,
        chat: F,
    ) -> crate::Result<CompressionOutcome>
    where
        F: FnOnce(ChatRequest) -> Fut + Send + 'static,
        Fut: Future<Output = std::result::Result<LlmResponse, ContextError>> + Send + 'static,
    {
        self.set_current_model(model_id);

        self.budget.update(self.count_tokens(&session.messages));

        if !self.budget.needs_compression() {
            return Ok(CompressionOutcome::BelowThreshold);
        }

        self.run_compression(session, chat).await
    }

    /// Like [`Self::maybe_compress`] but skips the threshold gate.
    /// A strategy NoOp surfaces as `StrategyDeclined`; a non-shrinking
    /// apply surfaces as `NoSavings`, so a too-small conversation isn't
    /// rewritten as a one-line summary. For caller-initiated passes
    /// (e.g. a user-typed `/compact`).
    pub async fn force_compress<F, Fut>(
        &mut self,
        session: &mut Session,
        model_id: &str,
        chat: F,
    ) -> crate::Result<CompressionOutcome>
    where
        F: FnOnce(ChatRequest) -> Fut + Send + 'static,
        Fut: Future<Output = std::result::Result<LlmResponse, ContextError>> + Send + 'static,
    {
        self.set_current_model(model_id);
        self.budget.update(self.count_tokens(&session.messages));
        self.run_compression(session, chat).await
    }

    async fn run_compression<F, Fut>(
        &mut self,
        session: &mut Session,
        chat: F,
    ) -> crate::Result<CompressionOutcome>
    where
        F: FnOnce(ChatRequest) -> Fut + Send + 'static,
        Fut: Future<Output = std::result::Result<LlmResponse, ContextError>> + Send + 'static,
    {
        let chat_box: crate::strategy::ChatCallback = Box::new(move |req| Box::pin(chat(req)));
        let plan = self.strategy.compress(&session.messages, chat_box).await?;
        let (mut new_messages, replaced_full_history) = match plan {
            CompressOutput::NoOp => return Ok(CompressionOutcome::StrategyDeclined),
            CompressOutput::Replaced {
                messages,
                replaced_full_history,
            } => (messages, replaced_full_history),
        };

        // Strategy with `replaced_full_history` discarded the
        // historical skill_reminder / tool_use trail; the manager
        // re-attaches the authoritative reminder + per-skill detail
        // blocks so the next LLM call still sees the skill set the
        // user expects. `called_skills` was tracked incrementally on
        // every `append` and tells us which skills' details to ship.
        if replaced_full_history && let Some(registry) = &self.skill_registry {
            append_skill_trailer(
                &mut new_messages,
                registry.as_ref(),
                self.tokenizer.as_ref(),
                &self.called_skills,
            );
        }

        let before_tokens = self.budget.current();
        let start = Instant::now();

        // Decide on tokens, not message count: a same-length
        // replacement (one big message → one summary) would slip past
        // a length-only check despite a real token cut. Drop the
        // baseline first — it's anchored to the old slice.
        self.invalidate_baseline();
        // Tokenize directly: the cache still mirrors the OLD slice,
        // and we'll install this exact vec as the new cache after
        // commit, so `cached_sum` would just re-do the work.
        let new_per_message: Vec<usize> = new_messages
            .iter()
            .map(|m| self.tokenizer.count_message(m))
            .collect();
        let after_tokens = self.calibrate(new_per_message.iter().copied().sum());

        if after_tokens >= before_tokens {
            // Don't apply. `session.messages` and the existing cache
            // are still in sync; nothing to undo.
            return Ok(CompressionOutcome::NoSavings {
                before: before_tokens,
                after: after_tokens,
            });
        }

        session.messages = new_messages;
        *self.per_message_tokens.write() = new_per_message;
        // Rebuild called_skills from the freshly-applied slice so the
        // vector mirrors session.messages: a successful summary leaves
        // it empty (the trailer carries plain text, no `ToolUse`), and
        // a Truncate apply scopes it to whatever's still in the kept
        // tail. Either way, the next compression sees the right set.
        self.called_skills = scan_skill_calls(&session.messages);
        self.budget.update(after_tokens);

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

        Ok(CompressionOutcome::Compressed {
            before: before_tokens,
            after: after_tokens,
        })
    }

    /// Read-only access to the token budget.
    pub fn budget(&self) -> &TokenBudget {
        &self.budget
    }

    /// Steady-state cost is `O(suffix)`: the bulk of the count is the
    /// provider's authoritative `actual_tokens` from the last main
    /// call, and only the messages appended since are BPE-encoded.
    /// Falls back to a full calibrated sweep on cold start, or after
    /// compression, or if the message list shrank below the anchor.
    fn count_tokens(&self, messages: &[ChatMessage]) -> usize {
        let snapshot = *self.baseline.read();
        if let Some(b) = snapshot
            && messages.len() >= b.message_count_at_call
        {
            let delta_raw = self.cached_sum(messages, b.message_count_at_call..messages.len());
            return b.actual_tokens + self.calibrate(delta_raw);
        }
        self.calibrate(self.raw_estimate(messages))
    }

    fn calibrate(&self, raw: usize) -> usize {
        match (&self.calibration, self.current_model.read().as_deref()) {
            (Some(cal), Some(model_id)) => cal.adjust(model_id, raw),
            _ => raw,
        }
    }
}

/// Walk one message's `ContentBlock::ToolUse` entries and append
/// every freshly-seen skill name (in the order they appear) to `acc`.
/// Only `ToolUse` blocks for the canonical Skill tool are considered;
/// insertion-order dedup keeps the post-summary trailer deterministic.
fn record_skill_calls(acc: &mut Vec<String>, msg: &ChatMessage) {
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
        if !acc.iter().any(|n| n == skill_name) {
            acc.push(skill_name.to_string());
        }
    }
}

/// Rebuild the called-skills vector from a full message slice.
/// Used after a compression apply to scope the vector to whatever
/// `ToolUse` blocks survived in the new `session.messages`.
fn scan_skill_calls(messages: &[ChatMessage]) -> Vec<String> {
    let mut out = Vec::new();
    for msg in messages {
        record_skill_calls(&mut out, msg);
    }
    out
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

/// Append the post-summary skill trailer to `messages`: always the
/// authoritative reminder, plus a detail block when at least one
/// previously-called skill survives the per-skill / total token caps.
/// Both ride as `Role::User` text so `merge_for_llm` folds them into
/// the leading user message before dispatch; the in-storage messages
/// stay separate for trace clarity.
fn append_skill_trailer(
    messages: &mut Vec<ChatMessage>,
    registry: &SkillRegistry,
    tokenizer: &dyn Tokenizer,
    called_skills: &[String],
) {
    let reminder = render_skill_reminder(&registry.all_summaries_sorted());
    messages.push(ChatMessage {
        role: Role::User,
        content: vec![ContentBlock::Text(reminder)],
    });
    if let Some(detail) = build_skill_detail_payload(registry, tokenizer, called_skills) {
        messages.push(ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text(detail)],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::{ContentBlock, Role};

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
        ChatMessage {
            role,
            content: vec![ContentBlock::Text(text.to_string())],
        }
    }

    fn make_session(messages: Vec<ChatMessage>) -> Session {
        let id = aura_model::SessionId::from("test-session");
        Session {
            id: id.clone(),
            user: aura_model::User {
                id: "user-1".to_string(),
                name: None,
                channel: aura_model::ChannelType::tui(),
            },
            channel: aura_model::ChannelType::tui(),
            messages,
            created_at: chrono::Utc::now(),
            last_active: chrono::Utc::now(),
            state: Default::default(),
            root_session_id: id,
            trigger: aura_model::TriggerSource::User,
            lineage: None,
            bound_soul_version: "soul-test".into(),
        }
    }

    /// Chat closure that panics if invoked. Every in-lib test uses
    /// `Truncate`, which never returns `NeedsLlmCall`, so the closure
    /// should never run. Failing loudly here means a future regression
    /// that wires `Summarize` into these tests can't slip past.
    async fn never_chat(_: ChatRequest) -> std::result::Result<LlmResponse, ContextError> {
        panic!("Truncate-only tests must not invoke the chat closure");
    }

    fn make_ctx(keep_recent: usize, max_tokens: usize, threshold: f64) -> ContextManager {
        ContextManager::new(
            Arc::new(SimpleTokenizer),
            Box::new(Truncate::new(keep_recent)),
            TokenBudget::new(max_tokens, threshold),
        )
    }

    #[tokio::test]
    async fn append_adds_message_without_compression() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        let mut session = make_session(vec![]);

        let msg = make_msg(Role::User, "hello");
        ctx.append(&mut session, &msg);

        assert_eq!(session.messages.len(), 1);
        assert_eq!(session.messages[0].role, Role::User);
        assert!(matches!(
            ctx.maybe_compress(&mut session, "test-model", never_chat)
                .await
                .unwrap(),
            CompressionOutcome::BelowThreshold
        ));
    }

    #[tokio::test]
    async fn maybe_compress_on_token_threshold() {
        // max=50, threshold=0.5 → compress when > 25 tokens
        let mut ctx = make_ctx(2, 50, 0.5);
        let mut session = make_session(vec![]);

        // Build up messages one by one. `append` no longer
        // auto-compresses; the agent loop is responsible for calling
        // `maybe_compress` at well-defined cost-recording points.
        ctx.append(&mut session, &make_msg(Role::System, "You are helpful"));
        ctx.append(&mut session, &make_msg(Role::User, "First message here"));
        ctx.append(&mut session, &make_msg(Role::Assistant, "First reply here"));
        ctx.append(&mut session, &make_msg(Role::User, "Second message here"));

        let outcome = ctx
            .maybe_compress(&mut session, "test-model", never_chat)
            .await
            .unwrap();

        assert!(matches!(outcome, CompressionOutcome::Compressed { .. }));
        // system + 2 most recent non-system
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[0].role, Role::System);
    }

    #[tokio::test]
    async fn no_compress_under_threshold() {
        let mut ctx = make_ctx(10, 100_000, 0.75);
        let mut session = make_session(vec![]);

        ctx.append(&mut session, &make_msg(Role::System, "sys"));
        ctx.append(&mut session, &make_msg(Role::User, "hi"));
        ctx.append(&mut session, &make_msg(Role::Assistant, "hello"));

        let outcome = ctx
            .maybe_compress(&mut session, "test-model", never_chat)
            .await
            .unwrap();

        assert!(matches!(outcome, CompressionOutcome::BelowThreshold));
        assert_eq!(session.messages.len(), 3);
    }

    #[tokio::test]
    async fn force_compress_runs_under_budget() {
        // Plenty of headroom — `maybe_compress` would skip — but
        // `force_compress` runs the strategy regardless. With
        // keep_recent=2 and 3 non-system messages the Truncate
        // strategy shrinks the slice, so the call returns
        // `Compressed`.
        let mut ctx = make_ctx(2, 100_000, 0.75);
        let mut session = make_session(vec![]);

        ctx.append(&mut session, &make_msg(Role::System, "sys"));
        ctx.append(&mut session, &make_msg(Role::User, "first"));
        ctx.append(&mut session, &make_msg(Role::Assistant, "second"));
        ctx.append(&mut session, &make_msg(Role::User, "third"));

        // Sanity: budget-gated path is a no-op here.
        let baseline = ctx
            .maybe_compress(&mut session, "test-model", never_chat)
            .await
            .unwrap();
        assert!(matches!(baseline, CompressionOutcome::BelowThreshold));
        assert_eq!(session.messages.len(), 4);

        let outcome = ctx
            .force_compress(&mut session, "test-model", never_chat)
            .await
            .unwrap();

        assert!(matches!(outcome, CompressionOutcome::Compressed { .. }));
        // system + keep_recent=2 most recent non-system.
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.messages[0].role, Role::System);
    }

    #[tokio::test]
    async fn force_compress_strategy_declined_when_cant_shorten() {
        // keep_recent=5 ≥ non-system count → Truncate returns NoOp,
        // and `force_compress` surfaces it as `StrategyDeclined`
        // (the budget gate was bypassed; the strategy itself bowed out).
        let mut ctx = make_ctx(5, 100_000, 0.75);
        let mut session = make_session(vec![]);

        ctx.append(&mut session, &make_msg(Role::System, "sys"));
        ctx.append(&mut session, &make_msg(Role::User, "hi"));
        ctx.append(&mut session, &make_msg(Role::Assistant, "hello"));

        let outcome = ctx
            .force_compress(&mut session, "test-model", never_chat)
            .await
            .unwrap();

        assert!(matches!(outcome, CompressionOutcome::StrategyDeclined));
        assert_eq!(session.messages.len(), 3);
    }

    #[tokio::test]
    async fn no_compress_when_already_at_keep_recent() {
        // Low threshold triggers compression check, but only 2 non-system
        // messages with keep_recent=5 → strategy can't reduce further.
        // Surfaces as `StrategyDeclined`: the budget gate did fire
        // (`BelowThreshold` would mean we never got to the strategy at all).
        let mut ctx = make_ctx(5, 10, 0.1);
        let mut session = make_session(vec![]);

        ctx.append(&mut session, &make_msg(Role::System, "sys"));
        ctx.append(&mut session, &make_msg(Role::User, "hi"));
        ctx.append(&mut session, &make_msg(Role::Assistant, "hello"));

        let outcome = ctx
            .maybe_compress(&mut session, "test-model", never_chat)
            .await
            .unwrap();

        assert!(matches!(outcome, CompressionOutcome::StrategyDeclined));
        assert_eq!(session.messages.len(), 3);
    }

    #[tokio::test]
    async fn budget_tracks_tokens() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        let mut session = make_session(vec![]);

        assert_eq!(ctx.budget().current(), 0);

        ctx.append(&mut session, &make_msg(Role::User, "hello world"));

        assert!(ctx.budget().current() > 0);
        assert!(ctx.budget().remaining() < 100_000);
    }

    /// Without a baseline, `count_tokens` falls back to a full
    /// tokenizer sweep. Establishes the baseline-vs-fallback contrast
    /// the next test relies on.
    #[tokio::test]
    async fn count_tokens_falls_back_to_full_count_without_baseline() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        let mut session = make_session(vec![]);
        ctx.append(&mut session, &make_msg(Role::User, "alpha"));
        ctx.append(&mut session, &make_msg(Role::Assistant, "beta"));
        ctx.append(&mut session, &make_msg(Role::User, "gamma"));

        let raw_full: usize = session
            .messages
            .iter()
            .map(|m| ctx.tokenizer.count_message(m))
            .sum();
        // No calibration injected → calibrate is identity, full count.
        assert_eq!(ctx.count_tokens(&session.messages), raw_full);
    }

    /// After `record_call_actual`, `count_tokens` returns
    /// `actual + tokenize(suffix)` — only the messages appended since
    /// the call get BPE-encoded.
    #[tokio::test]
    async fn count_tokens_uses_baseline_plus_delta() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        let mut session = make_session(vec![]);
        ctx.append(&mut session, &make_msg(Role::User, "old-1"));
        ctx.append(&mut session, &make_msg(Role::Assistant, "old-2"));
        ctx.append(&mut session, &make_msg(Role::User, "old-3"));
        ctx.record_call_actual(&session.messages, 5_000);

        let new_a = make_msg(Role::Assistant, "new-a");
        let new_b = make_msg(Role::User, "new-b");
        session.messages.push(new_a.clone());
        session.messages.push(new_b.clone());

        let expected_delta =
            ctx.tokenizer.count_message(&new_a) + ctx.tokenizer.count_message(&new_b);
        assert_eq!(ctx.count_tokens(&session.messages), 5_000 + expected_delta);
    }

    /// Compression mutates the message prefix in place, so the
    /// baseline's `message_count_at_call` no longer maps to anything
    /// meaningful. `maybe_compress` must drop the baseline; the next
    /// `count_tokens` falls back to a full sweep.
    #[tokio::test]
    async fn compression_invalidates_baseline() {
        let mut ctx = make_ctx(2, 50, 0.5);
        let mut session = make_session(vec![]);

        ctx.append(&mut session, &make_msg(Role::System, "sys"));
        ctx.append(&mut session, &make_msg(Role::User, "msg-1 with content"));
        ctx.append(&mut session, &make_msg(Role::Assistant, "reply-1 here"));
        ctx.append(&mut session, &make_msg(Role::User, "msg-2 with content"));
        ctx.record_call_actual(&session.messages, 9_999);

        // Pre-compression: baseline applies → big number.
        assert_eq!(ctx.count_tokens(&session.messages), 9_999);

        // Drive compression. With max=50, threshold=0.5 the budget
        // says "compress" once the post-baseline estimate exceeds 25
        // (here it's 9_999 + 0).
        let _ = ctx
            .maybe_compress(&mut session, "test-model", never_chat)
            .await
            .unwrap();

        // Post-compression: baseline cleared → must re-tokenize the
        // (now-shrunken) message list, no 9_999 anywhere.
        let raw: usize = session
            .messages
            .iter()
            .map(|m| ctx.tokenizer.count_message(m))
            .sum();
        assert_eq!(ctx.count_tokens(&session.messages), raw);
        assert!(ctx.count_tokens(&session.messages) < 9_999);
    }

    /// `append` keeps the per-message token cache in step with
    /// `session.messages` so the suffix loop in `count_tokens`
    /// doesn't re-tokenize across appends. Spot-check by appending
    /// after a baseline is set: each `count_tokens` call must agree
    /// with a fresh full retokenize, and the cache vector's length
    /// must track the slice.
    #[tokio::test]
    async fn cache_stays_in_sync_across_appends() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        let mut session = make_session(vec![]);
        ctx.append(&mut session, &make_msg(Role::User, "first"));
        ctx.append(&mut session, &make_msg(Role::Assistant, "second"));
        ctx.record_call_actual(&session.messages, 1_000);

        // Append after baseline: count_tokens uses baseline + cached
        // suffix counts. The expected value is `actual + sum of new
        // message counts`.
        let new_a = make_msg(Role::User, "after-baseline-a");
        let new_b = make_msg(Role::Assistant, "after-baseline-b");
        ctx.append(&mut session, &new_a);
        ctx.append(&mut session, &new_b);

        let expected_delta =
            ctx.tokenizer.count_message(&new_a) + ctx.tokenizer.count_message(&new_b);
        assert_eq!(ctx.count_tokens(&session.messages), 1_000 + expected_delta);
        assert_eq!(ctx.per_message_tokens.read().len(), session.messages.len());
    }

    /// After `maybe_compress` applies a new message list, the cache
    /// must reflect the **new** messages — even when the new length
    /// happens to equal the old (length-only sync would silently
    /// return stale counts). Forces the same-length-replacement
    /// branch by handing the strategy a single huge message and
    /// having Truncate keep it; in practice Summarize would replace
    /// it with `[system, summary]`, also a same-length scenario when
    /// the input is `[system, big_msg]`. We assert via a sanity
    /// recount that count_tokens agrees with a fresh tokenize of
    /// `session.messages`.
    #[tokio::test]
    async fn cache_rebuilt_after_compression_apply() {
        let mut ctx = make_ctx(2, 50, 0.5);
        let mut session = make_session(vec![]);
        ctx.append(&mut session, &make_msg(Role::System, "You are helpful"));
        ctx.append(&mut session, &make_msg(Role::User, "First message here"));
        ctx.append(&mut session, &make_msg(Role::Assistant, "First reply here"));
        ctx.append(&mut session, &make_msg(Role::User, "Second message here"));

        let outcome = ctx
            .maybe_compress(&mut session, "test-model", never_chat)
            .await
            .unwrap();
        assert!(matches!(outcome, CompressionOutcome::Compressed { .. }));

        // Cache must be in lockstep with the post-compression slice.
        assert_eq!(ctx.per_message_tokens.read().len(), session.messages.len());
        let expected: usize = session
            .messages
            .iter()
            .map(|m| ctx.tokenizer.count_message(m))
            .sum();
        let cached: usize = ctx.per_message_tokens.read().iter().copied().sum();
        assert_eq!(cached, expected);
    }

    /// If `session.messages.len()` shrinks below the baseline's
    /// `message_count_at_call` (e.g. the caller mutated messages
    /// outside the normal append/compress flow), the indexed slice
    /// would be invalid. `count_tokens` falls back to a full sweep.
    #[tokio::test]
    async fn shrunken_message_list_falls_back_to_full_count() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        let mut session = make_session(vec![]);
        for i in 0..5 {
            ctx.append(&mut session, &make_msg(Role::User, &format!("m{i}")));
        }
        ctx.record_call_actual(&session.messages, 1_000);

        // Drop the last two messages — count_at_call (5) is now > len (3).
        session.messages.truncate(3);

        let raw: usize = session
            .messages
            .iter()
            .map(|m| ctx.tokenizer.count_message(m))
            .sum();
        assert_eq!(ctx.count_tokens(&session.messages), raw);
    }

    // ---------- Skill-trailer tests ----------

    use aura_model::{ArtifactSource, TrustLevel};
    use aura_skills::{SkillDefinition, SkillRequirements};

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
        ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: format!("call-{skill_name}"),
                name: SKILL_TOOL_NAME.into(),
                input: serde_json::json!({ SKILL_INPUT_NAME_FIELD: skill_name }),
                signature: None,
            }],
        }
    }

    /// `append` records every fresh `Skill` ToolUse it sees, in
    /// first-seen order with insertion-order dedup.
    #[test]
    fn append_records_skill_calls_in_order() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        let mut session = make_session(vec![]);

        ctx.append(&mut session, &skill_call("foo"));
        ctx.append(&mut session, &make_msg(Role::User, "u"));
        ctx.append(&mut session, &skill_call("bar"));
        ctx.append(&mut session, &skill_call("foo")); // duplicate
        ctx.append(&mut session, &skill_call("baz"));

        assert_eq!(ctx.called_skills, vec!["foo", "bar", "baz"]);
    }

    /// `record_skill_calls` must ignore `ToolUse` blocks for non-Skill
    /// tools so we don't accidentally render Bash / WebFetch / etc.
    /// detail blocks at compression time.
    #[test]
    fn append_ignores_non_skill_tool_uses() {
        let mut ctx = make_ctx(5, 100_000, 0.75);
        let mut session = make_session(vec![]);
        let bash_call = ChatMessage {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "1".into(),
                name: "Bash".into(),
                input: serde_json::json!({ "command": "ls" }),
                signature: None,
            }],
        };
        ctx.append(&mut session, &bash_call);
        assert!(ctx.called_skills.is_empty());
    }

    /// Without a skill registry, a `replaced_full_history: true`
    /// compression apply leaves the message vector exactly as the
    /// strategy returned it — no trailer is invented out of thin air.
    #[tokio::test]
    async fn no_skill_registry_means_no_trailer() {
        let mut ctx = ContextManager::new(
            Arc::new(SimpleTokenizer),
            Box::new(crate::strategy::summarize::Summarize::new(2)),
            TokenBudget::new(50, 0.5),
        );
        let mut session = make_session(vec![]);
        // Long enough that compression with the (real, registry-rendered)
        // trailer still wins on tokens — SimpleTokenizer counts text as
        // `len()/4 + 1`, so a few hundred bytes of user text easily out-
        // weighs the ~120-byte reminder + ~150-byte detail trailer.
        ctx.append(&mut session, &make_msg(Role::System, "sys"));
        ctx.append(&mut session, &make_msg(Role::User, &"u1 ".repeat(80)));
        ctx.append(&mut session, &skill_call("foo"));
        ctx.append(&mut session, &make_msg(Role::Assistant, &"a1 ".repeat(80)));
        ctx.append(&mut session, &make_msg(Role::User, &"u2 ".repeat(80)));

        let chat = |_req: ChatRequest| async move {
            Ok(LlmResponse {
                content: "<analysis>x</analysis><summary>S</summary>".into(),
                content_blocks: vec![],
                tool_calls: vec![],
                usage: Default::default(),
                thinking: None,
            })
        };
        let outcome = ctx
            .maybe_compress(&mut session, "test-model", chat)
            .await
            .unwrap();
        assert!(matches!(outcome, CompressionOutcome::Compressed { .. }));

        // [system, summary] only — no reminder, no detail.
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].role, Role::System);
        if let ContentBlock::Text(t) = &session.messages[1].content[0] {
            assert_eq!(t, "<summary>S</summary>");
        } else {
            panic!("expected text content");
        }
    }

    /// With a skill registry attached, after a Summarize compression
    /// the manager appends `[reminder, detail]` (when there are
    /// previously-called skills the registry can render).
    #[tokio::test]
    async fn summarize_apply_appends_skill_trailer() {
        let registry = registry_with(&[("foo", "FOO_BODY")]);
        let mut ctx = ContextManager::new(
            Arc::new(SimpleTokenizer),
            Box::new(crate::strategy::summarize::Summarize::new(2)),
            TokenBudget::new(50, 0.5),
        )
        .with_skill_registry(registry);
        let mut session = make_session(vec![]);
        // Long enough that compression with the (real, registry-rendered)
        // trailer still wins on tokens — SimpleTokenizer counts text as
        // `len()/4 + 1`, so a few hundred bytes of user text easily out-
        // weighs the ~120-byte reminder + ~150-byte detail trailer.
        ctx.append(&mut session, &make_msg(Role::System, "sys"));
        ctx.append(&mut session, &make_msg(Role::User, &"u1 ".repeat(80)));
        ctx.append(&mut session, &skill_call("foo"));
        ctx.append(&mut session, &make_msg(Role::Assistant, &"a1 ".repeat(80)));
        ctx.append(&mut session, &make_msg(Role::User, &"u2 ".repeat(80)));

        let chat = |_req: ChatRequest| async move {
            Ok(LlmResponse {
                content: "<analysis>x</analysis><summary>S</summary>".into(),
                content_blocks: vec![],
                tool_calls: vec![],
                usage: Default::default(),
                thinking: None,
            })
        };
        let outcome = ctx
            .maybe_compress(&mut session, "test-model", chat)
            .await
            .unwrap();
        assert!(matches!(outcome, CompressionOutcome::Compressed { .. }));

        // [system, summary, reminder, detail]
        assert_eq!(session.messages.len(), 4);
        let texts: Vec<&str> = session
            .messages
            .iter()
            .map(|m| match m.content.first() {
                Some(ContentBlock::Text(t)) => t.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(texts[0], "sys");
        assert_eq!(texts[1], "<summary>S</summary>");
        assert!(texts[2].contains("The following skills are available"));
        assert!(texts[2].contains("- foo: desc for foo"));
        assert!(texts[3].contains("<skill name=\"foo\" version=\"0.1.0\">"));
        assert!(texts[3].contains("FOO_BODY"));
    }

    /// Truncate compression (`replaced_full_history: false`) does NOT
    /// trigger trailer attachment — the tail still carries the
    /// historical reminder + tool_use trail, so re-broadcasting
    /// would just duplicate.
    #[tokio::test]
    async fn truncate_apply_skips_skill_trailer() {
        let registry = registry_with(&[("foo", "FOO_BODY")]);
        let mut ctx = ContextManager::new(
            Arc::new(SimpleTokenizer),
            Box::new(Truncate::new(2)),
            TokenBudget::new(50, 0.5),
        )
        .with_skill_registry(registry);
        let mut session = make_session(vec![]);
        // Long enough that compression with the (real, registry-rendered)
        // trailer still wins on tokens — SimpleTokenizer counts text as
        // `len()/4 + 1`, so a few hundred bytes of user text easily out-
        // weighs the ~120-byte reminder + ~150-byte detail trailer.
        ctx.append(&mut session, &make_msg(Role::System, "sys"));
        ctx.append(&mut session, &make_msg(Role::User, &"u1 ".repeat(80)));
        ctx.append(&mut session, &skill_call("foo"));
        ctx.append(&mut session, &make_msg(Role::Assistant, &"a1 ".repeat(80)));
        ctx.append(&mut session, &make_msg(Role::User, &"u2 ".repeat(80)));

        let outcome = ctx
            .maybe_compress(&mut session, "test-model", never_chat)
            .await
            .unwrap();
        assert!(matches!(outcome, CompressionOutcome::Compressed { .. }));

        // No reminder / detail messages were appended; the result is
        // just system + the kept tail (which itself may still have
        // the original Skill ToolUse).
        for msg in &session.messages {
            for block in &msg.content {
                if let ContentBlock::Text(t) = block {
                    assert!(!t.contains("The following skills are available"));
                    assert!(!t.contains("Full definitions for skills"));
                }
            }
        }
    }

    /// After a Summarize apply the called_skills vector is empty: the
    /// trailer is plain text with no `ToolUse`, and the rebuild
    /// re-scans only the new (post-trailer) slice.
    #[tokio::test]
    async fn called_skills_clears_after_summarize_apply() {
        let registry = registry_with(&[("foo", "FOO_BODY")]);
        let mut ctx = ContextManager::new(
            Arc::new(SimpleTokenizer),
            Box::new(crate::strategy::summarize::Summarize::new(2)),
            TokenBudget::new(50, 0.5),
        )
        .with_skill_registry(registry);
        let mut session = make_session(vec![]);
        // Long enough that compression with the (real, registry-rendered)
        // trailer still wins on tokens — SimpleTokenizer counts text as
        // `len()/4 + 1`, so a few hundred bytes of user text easily out-
        // weighs the ~120-byte reminder + ~150-byte detail trailer.
        ctx.append(&mut session, &make_msg(Role::System, "sys"));
        ctx.append(&mut session, &make_msg(Role::User, &"u1 ".repeat(80)));
        ctx.append(&mut session, &skill_call("foo"));
        ctx.append(&mut session, &make_msg(Role::Assistant, &"a1 ".repeat(80)));
        ctx.append(&mut session, &make_msg(Role::User, &"u2 ".repeat(80)));
        assert_eq!(ctx.called_skills, vec!["foo"]);

        let chat = |_req: ChatRequest| async move {
            Ok(LlmResponse {
                content: "<analysis>x</analysis><summary>S</summary>".into(),
                content_blocks: vec![],
                tool_calls: vec![],
                usage: Default::default(),
                thinking: None,
            })
        };
        ctx.maybe_compress(&mut session, "test-model", chat)
            .await
            .unwrap();
        assert!(ctx.called_skills.is_empty());
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
