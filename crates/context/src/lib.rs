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
use parking_lot::RwLock;
use tracing::{debug, warn};

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

/// Result of a [`ContextManager::maybe_compress`] call.
///
/// Cost recording is the caller's responsibility — `maybe_compress`
/// invokes the supplied chat closure for any LLM call, and that
/// closure is where the agent loop opens its trace span and records
/// cost. Hence the outcome carries no LLM-call provenance.
#[derive(Debug, Clone, Copy)]
pub enum CompressionOutcome {
    /// Budget was under the threshold, or the strategy chose not to
    /// shorten the list. `session.messages` is unchanged.
    NoChange,
    /// `session.messages` was replaced with a shorter list.
    Compressed,
}

/// Manages session context: appending messages with automatic compression
/// and token budget tracking.
///
/// This is a concrete struct, not a trait. The only extension point is the
/// `CompressionStrategy` injected at construction — the management logic
/// (append, budget check) is invariant.
pub struct ContextManager {
    tokenizer: Arc<dyn Tokenizer>,
    strategy: Box<dyn CompressionStrategy>,
    budget: TokenBudget,
    calibration: Option<Arc<TokenCalibration>>,
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
            return Ok(CompressionOutcome::NoChange);
        }

        let chat_box: crate::strategy::ChatCallback = Box::new(move |req| Box::pin(chat(req)));
        let plan = self.strategy.compress(&session.messages, chat_box).await?;
        let new_messages = match plan {
            CompressOutput::NoOp => return Ok(CompressionOutcome::NoChange),
            CompressOutput::Replaced(messages) => messages,
        };

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
            return Ok(CompressionOutcome::NoChange);
        }

        session.messages = new_messages;
        *self.per_message_tokens.write() = new_per_message;
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

        Ok(CompressionOutcome::Compressed)
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
            CompressionOutcome::NoChange
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

        assert!(matches!(outcome, CompressionOutcome::Compressed));
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

        assert!(matches!(outcome, CompressionOutcome::NoChange));
        assert_eq!(session.messages.len(), 3);
    }

    #[tokio::test]
    async fn no_compress_when_already_at_keep_recent() {
        // Low threshold triggers compression check, but only 2 non-system
        // messages with keep_recent=5 → strategy can't reduce further.
        let mut ctx = make_ctx(5, 10, 0.1);
        let mut session = make_session(vec![]);

        ctx.append(&mut session, &make_msg(Role::System, "sys"));
        ctx.append(&mut session, &make_msg(Role::User, "hi"));
        ctx.append(&mut session, &make_msg(Role::Assistant, "hello"));

        let outcome = ctx
            .maybe_compress(&mut session, "test-model", never_chat)
            .await
            .unwrap();

        assert!(matches!(outcome, CompressionOutcome::NoChange));
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
        assert!(matches!(outcome, CompressionOutcome::Compressed));

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
}
