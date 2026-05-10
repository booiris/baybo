//! Compression-time fast-path strategy: swaps in a precomputed
//! `summary.md` plus an atomic-pair-preserved recent slice when both
//! are available, falling through to the inner [`CompressionStrategy`]
//! (typically [`crate::Summarize`]) when they aren't or when the
//! assembled total would still be too large. See
//! `docs/background-compression.md`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use aura_model::{ChatMessage, ContentBlock, Role, SessionId};
use aura_session::SessionManager;
use aura_skills::SkillRegistry;
use tracing::{debug, warn};

use super::{
    ChatCallback, CompressOutput, CompressionStrategy, partition_system, walk_backward_atomic,
};
use crate::{
    FAST_PATH_FALLTHROUGH_THRESHOLD_RATIO, RECENT_SLICE_MAX_TOKENS,
    RECENT_SLICE_MIN_TEXT_BLOCK_MSGS, RECENT_SLICE_MIN_TOKENS, SUMMARY_REFRESH_WAIT_POLL_INTERVAL,
    SUMMARY_REFRESH_WAIT_TIMEOUT, Tokenizer, estimate_skill_trailer_tokens, scan_skill_calls,
};

/// Preamble framing the summary as established context for the LLM
/// rather than fresh input.
const CONTEXT_SUMMARY_WRAPPER_PREAMBLE: &str = "The conversation prior to this point has been compressed for context-window \
management. The summary below was produced from the full prior conversation and \
represents its substantive content. Treat it as established context for the user's \
current request; the recent messages that follow are the only unsummarized exchanges.";

/// Abstraction over loading per-session summary content. Implemented
/// by [`FsSummaryLoader`] (reads
/// `<base_dir>/<session_id>/<file_name>`) and by test doubles. Kept
/// as a trait so `aura-context` doesn't depend on `aura-workspace`
/// directly — the bootstrap layer constructs the FS-backed loader
/// using `WorkspacePaths`.
#[async_trait]
pub trait SummaryLoader: Send + Sync {
    /// `Ok(None)` when the file does not exist (fresh session, never
    /// crossed the trigger threshold). `Err` only for genuine I/O
    /// faults; the wrapper logs and falls through.
    async fn load(&self, session_id: &SessionId) -> std::io::Result<Option<String>>;
}

/// Filename under each per-session state directory. Mirrors
/// `aura_workspace::SUMMARY_FILE`; kept as a local const so this crate
/// stays free of `aura-workspace`.
const SUMMARY_FILE_NAME: &str = "summary.md";

/// Filesystem-backed [`SummaryLoader`]. Reads
/// `<base_dir>/<session_id>/summary.md`. Construct `base_dir` from
/// `WorkspacePaths::state_sessions_dir()`.
pub struct FsSummaryLoader {
    base_dir: PathBuf,
}

impl FsSummaryLoader {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }
}

#[async_trait]
impl SummaryLoader for FsSummaryLoader {
    async fn load(&self, session_id: &SessionId) -> std::io::Result<Option<String>> {
        let path = self
            .base_dir
            .join(session_id.as_str())
            .join(SUMMARY_FILE_NAME);
        match tokio::fs::read_to_string(&path).await {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Compression-time fast-path strategy. See module docs.
pub struct SummaryAwareWrapper {
    inner: Box<dyn CompressionStrategy>,
    summary_loader: Arc<dyn SummaryLoader>,
    sessions: Arc<SessionManager>,
    skill_registry: Arc<SkillRegistry>,
    tokenizer: Arc<dyn Tokenizer>,
    session_id: SessionId,
    /// Cached `TokenBudget::max_tokens` so the wrapper can compute
    /// the absolute fall-through threshold
    /// (`max_tokens × FAST_PATH_FALLTHROUGH_THRESHOLD_RATIO`)
    /// without a back-reference to `ContextManager`.
    max_tokens: usize,
}

impl SummaryAwareWrapper {
    pub fn new(
        inner: Box<dyn CompressionStrategy>,
        summary_loader: Arc<dyn SummaryLoader>,
        sessions: Arc<SessionManager>,
        skill_registry: Arc<SkillRegistry>,
        tokenizer: Arc<dyn Tokenizer>,
        session_id: SessionId,
        max_tokens: usize,
    ) -> Self {
        Self {
            inner,
            summary_loader,
            sessions,
            skill_registry,
            tokenizer,
            session_id,
            max_tokens,
        }
    }

    fn build_summary_message(&self, summary_content: &str) -> ChatMessage {
        let body = format!(
            "<context-summary>\n{}\n\n{}\n</context-summary>",
            CONTEXT_SUMMARY_WRAPPER_PREAMBLE,
            summary_content.trim()
        );
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text(body)],
        }
    }
}

#[async_trait]
impl CompressionStrategy for SummaryAwareWrapper {
    async fn compress(
        &self,
        messages: &[ChatMessage],
        chat: ChatCallback,
    ) -> crate::Result<CompressOutput> {
        // Wait up to `SUMMARY_REFRESH_WAIT_TIMEOUT` for any in-flight
        // background refresh pass to land before reading metadata.
        // Mirrors Claude Code's `waitForSessionMemoryExtraction`: a
        // pass that's about to land can move the cursor forward, and
        // adopting the fresher state here keeps the assembled
        // recent-slice from re-summarizing content the background
        // already covered. If the timeout expires we proceed with the
        // last successfully recorded metadata (stale-by-one tolerated
        // per the design's ζ-i guarantee). `tokio::time::timeout`
        // tracks the tokio clock so paused-time tests behave
        // identically to production wall-clock waits.
        let wait_future = async {
            loop {
                match self.sessions.summary_metadata(&self.session_id).await {
                    Ok(Some(meta)) if meta.in_flight => {
                        tokio::time::sleep(SUMMARY_REFRESH_WAIT_POLL_INTERVAL).await;
                    }
                    _ => return,
                }
            }
        };
        if tokio::time::timeout(SUMMARY_REFRESH_WAIT_TIMEOUT, wait_future)
            .await
            .is_err()
        {
            warn!(
                session_id = %self.session_id,
                "fast-path: in-flight refresh did not settle within timeout; \
                 proceeding with last recorded metadata"
            );
        }

        let metadata = match self.sessions.summary_metadata(&self.session_id).await {
            Ok(Some(m)) => m,
            Ok(None) => {
                debug!(
                    session_id = %self.session_id,
                    "fast-path: no summary metadata; falling through to inner strategy"
                );
                return self.inner.compress(messages, chat).await;
            }
            Err(e) => {
                warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "fast-path: summary metadata read failed; falling through"
                );
                return self.inner.compress(messages, chat).await;
            }
        };
        let summary_content = match self.summary_loader.load(&self.session_id).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                // Metadata + file mismatch is an orphan condition;
                // the startup reaper reconciles on next boot.
                warn!(
                    session_id = %self.session_id,
                    cursor = metadata.cursor,
                    "fast-path: metadata exists but summary.md missing; falling through"
                );
                return self.inner.compress(messages, chat).await;
            }
            Err(e) => {
                warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "fast-path: summary.md read failed; falling through"
                );
                return self.inner.compress(messages, chat).await;
            }
        };

        // Map `metadata.cursor` (a `session_messages.ordinal`) back to
        // its position in the in-memory active list. Without this
        // mapping the recent slice could be cut to a position later
        // than the cursor, silently dropping unsummarized middle
        // history. Walk the supersede log identically to the
        // cold-start anchor reconstruction in `restore_from_store`:
        // count active rows in ordinal order until we hit the cursor
        // ordinal, that count is the in-memory index. Any mismatch
        // between the live active row count and the in-memory
        // `messages.len()` (compaction in flight, snapshot drift,
        // unpersisted system prompt) collapses the fast-path safely
        // — we cannot prove cursor coverage so we hand off to inner.
        let cursor_idx_in_active = match self
            .sessions
            .load_session_messages_with_supersede(&self.session_id)
            .await
        {
            Ok(rows) => {
                let mut active_count: usize = 0;
                let mut cursor_idx: Option<usize> = None;
                for row in &rows {
                    if row.superseded_by.is_some() {
                        continue;
                    }
                    if row.ordinal == metadata.cursor && cursor_idx.is_none() {
                        cursor_idx = Some(active_count);
                    }
                    active_count += 1;
                }
                if active_count != messages.len() {
                    debug!(
                        session_id = %self.session_id,
                        active_count,
                        in_memory_len = messages.len(),
                        "fast-path: active log / in-memory length mismatch; falling through"
                    );
                    return self.inner.compress(messages, chat).await;
                }
                match cursor_idx {
                    Some(idx) => idx,
                    None => {
                        debug!(
                            session_id = %self.session_id,
                            cursor = metadata.cursor,
                            "fast-path: cursor ordinal not present in active log; falling through"
                        );
                        return self.inner.compress(messages, chat).await;
                    }
                }
            }
            Err(e) => {
                warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "fast-path: supersede log read failed; falling through"
                );
                return self.inner.compress(messages, chat).await;
            }
        };

        let (system_msgs, non_system) = partition_system(messages);

        // Translate the full-frame cursor index into the `non_system`
        // frame the recent slice operates over. A cursor that lands
        // inside the system block is degenerate (summary covers only
        // soul prompt content) and we can't reason about the
        // post-cursor tail; fall through.
        let cursor_idx_in_non_system = if cursor_idx_in_active >= system_msgs.len() {
            cursor_idx_in_active - system_msgs.len()
        } else {
            debug!(
                session_id = %self.session_id,
                cursor_idx_in_active,
                system_count = system_msgs.len(),
                "fast-path: cursor falls inside the system block; falling through"
            );
            return self.inner.compress(messages, chat).await;
        };

        // The recent slice must include every message after the
        // cursor — those are unsummarized originals — and may extend
        // *past* the cursor when the post-cursor tail alone is too
        // small to satisfy the structural minimums (atomic-pair
        // preservation, `RECENT_SLICE_MIN_*` thresholds). The walk
        // produces the natural tail; we then clamp the cut to be no
        // later than the cursor so the post-cursor span is always
        // covered. `RECENT_SLICE_MAX_TOKENS` is the *forward-extension*
        // ceiling — it never trims unsummarized content.
        let tokenize_msg = |m: &ChatMessage| self.tokenizer.count_message(m);
        let walk_cut = walk_backward_atomic(
            &non_system,
            RECENT_SLICE_MIN_TOKENS,
            RECENT_SLICE_MIN_TEXT_BLOCK_MSGS,
            RECENT_SLICE_MAX_TOKENS,
            tokenize_msg,
        );
        let cut = walk_cut.min(cursor_idx_in_non_system);
        let recent_slice = non_system[cut..].to_vec();

        let summary_msg = self.build_summary_message(&summary_content);
        let summary_tokens = self.tokenizer.count_message(&summary_msg);
        let called = scan_skill_calls(&recent_slice);
        let skill_trailer_tokens = estimate_skill_trailer_tokens(
            self.skill_registry.as_ref(),
            self.tokenizer.as_ref(),
            &called,
        );
        let recent_slice_tokens: usize = recent_slice
            .iter()
            .map(|m| self.tokenizer.count_message(m))
            .sum();
        let fallthrough_budget =
            (self.max_tokens as f64 * FAST_PATH_FALLTHROUGH_THRESHOLD_RATIO) as usize;
        // Total includes the recent slice now: when the cursor is far
        // back, post-cursor content can dwarf summary + trailer, and
        // we must hand off to inner rather than apply a fast-path
        // assembly that would re-trip the budget on the next turn.
        let assembled_tokens = summary_tokens + skill_trailer_tokens + recent_slice_tokens;
        if assembled_tokens > fallthrough_budget {
            warn!(
                session_id = %self.session_id,
                summary_tokens,
                skill_trailer_tokens,
                recent_slice_tokens,
                fallthrough_budget,
                "fast-path: assembled total exceeds fall-through threshold; falling through"
            );
            return self.inner.compress(messages, chat).await;
        }

        // ContextManager auto-attaches the skill trailer at the end
        // via the `replaced_full_history = true` branch.
        let mut new_messages = system_msgs;
        new_messages.push(summary_msg);
        new_messages.extend(recent_slice);

        debug!(
            session_id = %self.session_id,
            cursor = metadata.cursor,
            cursor_idx_in_non_system,
            cut,
            recent_msg_count = (non_system.len() - cut),
            summary_tokens,
            recent_slice_tokens,
            skill_trailer_tokens,
            "fast-path: assembled list with precomputed summary"
        );

        Ok(CompressOutput::Replaced {
            messages: new_messages,
            replaced_full_history: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Truncate;
    use aura_llm::{LlmResponse, TokenUsage};
    use aura_model::{ChannelType, ContentBlock, Role, Session, SessionId, TriggerSource, User};
    use aura_skills::SkillRegistry;
    use aura_storage::libsql::{LibsqlPool, LibsqlSessionStore, LibsqlSessionSummaryStore};
    use aura_storage::{SessionStore, SessionSummaryStore};
    use chrono::Utc;
    use std::sync::Arc;

    /// Token estimator: 4 chars / token, +2 structural per message.
    /// Mirrors the cl100k-style "rough but consistent" mapping the
    /// strategy module's other tests use.
    struct CountingTokenizer;
    impl Tokenizer for CountingTokenizer {
        fn count_text(&self, text: &str) -> usize {
            text.len() / 4 + 1
        }
        fn count_image(&self, _w: u32, _h: u32) -> usize {
            100
        }
        fn count_message(&self, msg: &ChatMessage) -> usize {
            let mut tokens = 2;
            for block in &msg.content {
                match block {
                    ContentBlock::Text(text) => tokens += self.count_text(text),
                    ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => tokens += 5,
                    _ => tokens += 5,
                }
            }
            tokens
        }
    }

    /// In-memory `SummaryLoader` keyed by session_id; `None` means
    /// the file does not exist.
    struct InMemoryLoader {
        contents: parking_lot::Mutex<std::collections::HashMap<String, String>>,
    }
    impl InMemoryLoader {
        fn new() -> Self {
            Self {
                contents: parking_lot::Mutex::new(std::collections::HashMap::new()),
            }
        }
        fn put(&self, session_id: &str, body: &str) {
            self.contents
                .lock()
                .insert(session_id.to_string(), body.to_string());
        }
    }
    #[async_trait]
    impl SummaryLoader for InMemoryLoader {
        async fn load(&self, session_id: &SessionId) -> std::io::Result<Option<String>> {
            Ok(self.contents.lock().get(session_id.as_str()).cloned())
        }
    }

    fn make_session(id: &str) -> Session {
        let user = User {
            id: format!("u-{id}"),
            name: None,
            channel: ChannelType::tui(),
        };
        let now = Utc::now();
        Session {
            id: SessionId::from(id),
            user,
            channel: ChannelType::tui(),
            created_at: now,
            last_active: now,
            state: aura_model::SessionState::default(),
            root_session_id: SessionId::from(id),
            trigger: TriggerSource::User,
            lineage: None,
            bound_soul_version: "soul".into(),
        }
    }

    fn text(role: Role, t: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: vec![ContentBlock::Text(t.to_string())],
        }
    }

    /// `chat` callback that returns a Summarize-style response —
    /// used to verify fall-through actually invokes the inner
    /// strategy. The wrapper itself never invokes `chat` on the
    /// fast-path success branch, so getting a response here means
    /// the inner ran.
    fn ok_chat(content: &'static str) -> ChatCallback {
        Box::new(move |_req| {
            Box::pin(async move {
                Ok(LlmResponse {
                    content: content.to_string(),
                    content_blocks: vec![],
                    tool_calls: vec![],
                    usage: TokenUsage::default(),
                    thinking: None,
                })
            })
        })
    }

    fn never_chat() -> ChatCallback {
        Box::new(|_req| {
            Box::pin(
                async move { panic!("fast-path success path must not invoke the chat closure") },
            )
        })
    }

    /// Hand-build a fully-wired wrapper + the bits its inner strategy
    /// needs. Returns the wrapper, the in-memory loader (so tests
    /// can pre-populate it), the session id, and the session manager
    /// (so tests can write summary metadata).
    async fn wired_wrapper(
        max_tokens: usize,
        keep_recent_for_inner: usize,
    ) -> (
        SummaryAwareWrapper,
        Arc<InMemoryLoader>,
        SessionId,
        Arc<SessionManager>,
    ) {
        let pool = LibsqlPool::open_in_memory().await.unwrap();
        let session_store: Arc<dyn SessionStore> = Arc::new(LibsqlSessionStore::new(pool.clone()));
        let summary_store: Arc<dyn SessionSummaryStore> =
            Arc::new(LibsqlSessionSummaryStore::new(pool));
        let manager = Arc::new(SessionManager::new(
            session_store.clone(),
            summary_store,
            chrono::Duration::minutes(30),
        ));

        let session = make_session("ctx-test");
        session_store.save(&session).await.unwrap();

        let loader = Arc::new(InMemoryLoader::new());
        let registry = Arc::new(SkillRegistry::new());
        let inner: Box<dyn CompressionStrategy> = Box::new(Truncate::new(keep_recent_for_inner));
        let wrapper = SummaryAwareWrapper::new(
            inner,
            loader.clone() as Arc<dyn SummaryLoader>,
            manager.clone(),
            registry,
            Arc::new(CountingTokenizer),
            session.id.clone(),
            max_tokens,
        );
        (wrapper, loader, session.id, manager)
    }

    /// No metadata, no file → fall through to inner. Inner is
    /// `Truncate(2)`, which returns its keep-tail unchanged.
    #[tokio::test]
    async fn falls_through_when_no_summary_metadata() {
        let (wrapper, _loader, _id, _mgr) = wired_wrapper(200_000, 2).await;
        let messages = vec![
            text(Role::System, "system"),
            text(Role::User, "msg1"),
            text(Role::Assistant, "reply1"),
            text(Role::User, "msg2"),
            text(Role::Assistant, "reply2"),
            text(Role::User, "msg3"),
        ];
        let out = wrapper.compress(&messages, never_chat()).await.unwrap();
        match out {
            CompressOutput::Replaced { messages: m, .. } => {
                // Truncate(2) keeps system + last 2 non-system.
                assert_eq!(m.len(), 3);
                assert_eq!(m[0].role, Role::System);
            }
            _ => panic!("expected Replaced from inner Truncate fall-through"),
        }
    }

    /// Metadata exists but file is missing — orphan condition. Falls
    /// through to inner. Inner is Truncate(2) over a 4-message
    /// non-system tail so it actually shortens the slice rather
    /// than returning NoOp.
    #[tokio::test]
    async fn falls_through_when_file_missing_despite_metadata() {
        let (wrapper, _loader, id, mgr) = wired_wrapper(200_000, 2).await;
        let store = mgr.summary_store();
        store
            .upsert_success(&id, 5, 100, "m", "span", Utc::now())
            .await
            .unwrap();
        // Note: loader has no entry for `id`; it returns Ok(None).

        let messages = vec![
            text(Role::System, "system"),
            text(Role::User, "u1"),
            text(Role::Assistant, "a1"),
            text(Role::User, "u2"),
            text(Role::Assistant, "a2"),
        ];
        let out = wrapper.compress(&messages, never_chat()).await.unwrap();
        match out {
            CompressOutput::Replaced { messages: m, .. } => {
                // Truncate(2) keeps system + last 2 non-system.
                assert_eq!(m.len(), 3);
                // Crucially, the body has no `<context-summary>` —
                // the fast-path didn't install its assembly.
                let has_wrapper = m.iter().any(|msg| {
                    msg.content.iter().any(
                        |b| matches!(b, ContentBlock::Text(t) if t.contains("<context-summary>")),
                    )
                });
                assert!(
                    !has_wrapper,
                    "fast-path must not assemble when summary file is missing"
                );
            }
            _ => panic!("expected Replaced via inner Truncate fall-through"),
        }
    }

    /// Persist `messages` to the session's log so the wrapper's
    /// supersede-log walk can map `metadata.cursor` to an in-memory
    /// index. Each message lands at a fresh ordinal (1-based,
    /// contiguous) — same shape the live agent loop produces via
    /// `append_session_message`.
    async fn persist_messages(mgr: &Arc<SessionManager>, id: &SessionId, messages: &[ChatMessage]) {
        for m in messages {
            mgr.append_session_message(id, m).await.unwrap();
        }
    }

    /// Both metadata + file present, totals fit, and `messages` are
    /// persisted so the cursor maps cleanly: fast-path applies.
    /// `replaced_full_history = true` so the manager will attach the
    /// skill trailer at the end. Recent slice's left edge is clamped
    /// to the cursor's in-memory position so cursor-after content is
    /// always preserved.
    #[tokio::test]
    async fn fast_path_assembles_summary_plus_recent_slice() {
        let (wrapper, loader, id, mgr) = wired_wrapper(200_000, 2).await;

        // Build a transcript with enough text-block messages to
        // satisfy `RECENT_SLICE_MIN_TEXT_BLOCK_MSGS = 5` and enough
        // tokens to satisfy `RECENT_SLICE_MIN_TOKENS = 10_000`.
        let mut messages = vec![text(Role::System, "soul prompt")];
        for i in 0..20 {
            messages.push(text(
                Role::User,
                &format!(
                    "user message number {i} with some text content {}",
                    "x".repeat(2_000)
                ),
            ));
            messages.push(text(
                Role::Assistant,
                &format!("assistant reply {i} {}", "y".repeat(2_000)),
            ));
        }
        persist_messages(&mgr, &id, &messages).await;

        let store = mgr.summary_store();
        // Cursor=10 — ordinal of the 10th persisted message (an
        // assistant reply mid-conversation), so the wrapper's recent
        // slice must extend back at least to in-memory index 9.
        store
            .upsert_success(&id, 10, 100, "m", "span", Utc::now())
            .await
            .unwrap();
        loader.put(id.as_str(), "PRIOR SUMMARY CONTENT");

        let out = wrapper.compress(&messages, never_chat()).await.unwrap();
        match out {
            CompressOutput::Replaced {
                messages: m,
                replaced_full_history,
            } => {
                assert!(
                    replaced_full_history,
                    "fast-path must request trailer attach"
                );
                // Layout: [system, summary_blob, recent...]. System
                // is at index 0; summary at index 1; recent from
                // index 2 onward.
                assert_eq!(m[0].role, Role::System);
                if let ContentBlock::Text(t) = &m[1].content[0] {
                    assert!(t.starts_with("<context-summary>"));
                    assert!(t.contains("PRIOR SUMMARY CONTENT"));
                    assert!(t.ends_with("</context-summary>"));
                } else {
                    panic!("summary message body must be text");
                }
                // Cursor-coverage invariant: recent slice covers
                // every message after the cursor. The persisted
                // transcript has 41 entries (1 system + 40 non-system).
                // Cursor=10 is the 10th total → 9th non-system index.
                // `messages[10..]` is everything strictly after the
                // cursor; all of it must appear in the assembled
                // output, in order.
                let post_cursor_originals = &messages[10..];
                let assembled_tail = &m[2..];
                assert!(
                    assembled_tail.len() >= post_cursor_originals.len(),
                    "recent slice ({} msgs) must cover at least the {} post-cursor messages",
                    assembled_tail.len(),
                    post_cursor_originals.len()
                );
                let assembled_suffix =
                    &assembled_tail[assembled_tail.len() - post_cursor_originals.len()..];
                assert_eq!(
                    assembled_suffix, post_cursor_originals,
                    "post-cursor messages must appear verbatim at the tail of the assembled output"
                );
            }
            _ => panic!("fast-path must succeed when metadata + file present"),
        }
    }

    /// Regression guard for "stale cursor": the prior summary covers
    /// only the early prefix, and the post-cursor span is enormous —
    /// summary + cursor-after + skill_trailer would blow past the
    /// fall-through budget. The wrapper must hand off to inner instead
    /// of producing an assembly that silently drops the post-cursor
    /// middle to fit. The bug this guards against was: walk-cap
    /// (40K) silently truncated cursor-after messages without checking
    /// total size.
    #[tokio::test]
    async fn fast_path_falls_through_when_post_cursor_exceeds_budget() {
        // max_tokens=20_000 → fall-through budget = 12_000. A small
        // summary plus a 30-message tail of ~600 tokens each (≈ 18_000
        // tokens after cursor) overshoots the budget on its own.
        let (wrapper, loader, id, mgr) = wired_wrapper(20_000, 2).await;

        let mut messages = vec![text(Role::System, "soul prompt")];
        for i in 0..30 {
            messages.push(text(Role::User, &format!("user-{i} {}", "x".repeat(2_000))));
            messages.push(text(
                Role::Assistant,
                &format!("asst-{i} {}", "y".repeat(2_000)),
            ));
        }
        persist_messages(&mgr, &id, &messages).await;

        let store = mgr.summary_store();
        // Cursor=2 — only the first turn is summarized; the entire
        // rest of the transcript is post-cursor.
        store
            .upsert_success(&id, 2, 100, "m", "span", Utc::now())
            .await
            .unwrap();
        loader.put(id.as_str(), "TINY SUMMARY");

        let out = wrapper
            .compress(
                &messages,
                ok_chat("<analysis>x</analysis><summary>S</summary>"),
            )
            .await
            .unwrap();
        match out {
            CompressOutput::Replaced { messages: m, .. } => {
                // Fall-through to inner Truncate(2): only system + 2
                // tail messages, NO `<context-summary>` blob.
                let has_wrapper = m.iter().any(|msg| {
                    msg.content.iter().any(
                        |b| matches!(b, ContentBlock::Text(t) if t.contains("<context-summary>")),
                    )
                });
                assert!(
                    !has_wrapper,
                    "fast-path must fall through, not silently drop post-cursor middle"
                );
                assert_eq!(m.len(), 3, "Truncate(2) keeps system + 2 messages");
            }
            _ => panic!("expected Replaced via fall-through"),
        }
    }

    /// Cursor maps to an ordinal that no longer exists in the active
    /// log (a compression has rewritten the transcript since the
    /// summary was written). The wrapper cannot prove cursor coverage,
    /// so it falls through.
    #[tokio::test]
    async fn fast_path_falls_through_when_cursor_not_in_active_log() {
        let (wrapper, loader, id, mgr) = wired_wrapper(200_000, 2).await;

        let messages = vec![
            text(Role::System, "system"),
            text(Role::User, "u1"),
            text(Role::Assistant, "a1"),
            text(Role::User, "u2"),
            text(Role::Assistant, "a2"),
        ];
        persist_messages(&mgr, &id, &messages).await;

        let store = mgr.summary_store();
        // Cursor=99 — no such ordinal in the active log.
        store
            .upsert_success(&id, 99, 100, "m", "span", Utc::now())
            .await
            .unwrap();
        loader.put(id.as_str(), "OLD SUMMARY");

        let out = wrapper
            .compress(
                &messages,
                ok_chat("<analysis>x</analysis><summary>S</summary>"),
            )
            .await
            .unwrap();
        match out {
            CompressOutput::Replaced { messages: m, .. } => {
                let has_wrapper = m.iter().any(|msg| {
                    msg.content.iter().any(
                        |b| matches!(b, ContentBlock::Text(t) if t.contains("<context-summary>")),
                    )
                });
                assert!(
                    !has_wrapper,
                    "fast-path must not assemble when cursor is missing from the active log"
                );
                assert_eq!(m.len(), 3, "Truncate(2) keeps system + last 2");
            }
            _ => panic!("expected Replaced via fall-through"),
        }
    }

    /// `waitForSessionMemoryExtraction` analogue: when a background
    /// pass marked `in_flight = true` lands during compression, the
    /// wrapper waits up to `SUMMARY_REFRESH_WAIT_TIMEOUT` for the
    /// flag to clear before reading metadata, so it picks up the
    /// freshest cursor. Drive with `tokio::time::pause` so the 15s
    /// budget doesn't slow the test suite.
    #[tokio::test(start_paused = true)]
    async fn fast_path_waits_for_in_flight_to_settle_then_proceeds() {
        let (wrapper, loader, id, mgr) = wired_wrapper(200_000, 2).await;

        let messages = vec![
            text(Role::System, "system"),
            text(Role::User, "u1"),
            text(Role::Assistant, "a1"),
        ];
        persist_messages(&mgr, &id, &messages).await;

        // Mark the parent in-flight, then schedule the background
        // refresh "completion" for a couple of (virtual) seconds out.
        // The wrapper's compress() must observe the cleared flag and
        // proceed once the spawned task lands the success record.
        mgr.mark_summary_in_flight(&id).await.unwrap();
        loader.put(id.as_str(), "FRESH SUMMARY");

        let mgr_clone = Arc::clone(&mgr);
        let id_for_task = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            // The runner's normal terminal handler — clears in_flight
            // as part of recording the new cursor.
            mgr_clone
                .record_summary_success(&id_for_task, 1, 1, "m", "span", Utc::now())
                .await
                .unwrap();
        });

        let out = wrapper.compress(&messages, never_chat()).await.unwrap();
        match out {
            CompressOutput::Replaced { messages: m, .. } => {
                let has_wrapper = m.iter().any(|msg| {
                    msg.content.iter().any(
                        |b| matches!(b, ContentBlock::Text(t) if t.contains("<context-summary>")),
                    )
                });
                assert!(
                    has_wrapper,
                    "fast-path must apply once in_flight clears, using the just-landed summary"
                );
            }
            _ => panic!("expected Replaced from fast-path"),
        }
        // After the wait the in_flight flag is cleared by record_summary_success.
        assert!(!mgr.summary_metadata(&id).await.unwrap().unwrap().in_flight);
    }

    /// Timeout path: if the in-flight pass never settles within
    /// `SUMMARY_REFRESH_WAIT_TIMEOUT`, the wrapper proceeds with
    /// whatever metadata is on file (stale-by-one tolerated). The
    /// compression must not hang past the timeout budget.
    #[tokio::test(start_paused = true)]
    async fn fast_path_proceeds_after_in_flight_wait_timeout() {
        let (wrapper, loader, id, mgr) = wired_wrapper(200_000, 2).await;

        let messages = vec![
            text(Role::System, "system"),
            text(Role::User, "u1"),
            text(Role::Assistant, "a1"),
        ];
        persist_messages(&mgr, &id, &messages).await;

        // Seed a successful prior pass first, then re-mark as
        // in-flight without scheduling any clear. Wrapper should wait
        // out the timeout, then assemble using the prior cursor=1
        // metadata.
        mgr.summary_store()
            .upsert_success(&id, 1, 1, "m", "span", Utc::now())
            .await
            .unwrap();
        mgr.mark_summary_in_flight(&id).await.unwrap();
        loader.put(id.as_str(), "STALE BUT ACCEPTABLE");

        let start = tokio::time::Instant::now();
        let out = wrapper.compress(&messages, never_chat()).await.unwrap();
        let elapsed = tokio::time::Instant::now().duration_since(start);

        // Should have waited ≈ SUMMARY_REFRESH_WAIT_TIMEOUT, no longer.
        assert!(
            elapsed >= SUMMARY_REFRESH_WAIT_TIMEOUT,
            "wait must reach the timeout when in_flight never clears (got {elapsed:?})"
        );
        assert!(
            elapsed < SUMMARY_REFRESH_WAIT_TIMEOUT + std::time::Duration::from_secs(2),
            "wait must not exceed the timeout by more than one poll cycle (got {elapsed:?})"
        );
        match out {
            CompressOutput::Replaced { messages: m, .. } => {
                // Stale metadata still served — fast-path applied.
                let has_wrapper = m.iter().any(|msg| {
                    msg.content.iter().any(
                        |b| matches!(b, ContentBlock::Text(t) if t.contains("<context-summary>")),
                    )
                });
                assert!(
                    has_wrapper,
                    "after timeout the wrapper must serve the stale summary, not hang"
                );
            }
            _ => panic!("expected Replaced from fast-path with stale metadata"),
        }
    }

    /// When `summary + skill_trailer > 0.6 × max_tokens`, the wrapper
    /// falls through to the inner Summarize strategy. Smallest way
    /// to drive this: tiny `max_tokens` so even a small summary
    /// payload exceeds the fall-through budget.
    #[tokio::test]
    async fn falls_through_when_summary_exceeds_threshold() {
        // max=1000 → fall-through budget = 600 tokens. A summary
        // body of ~3000 chars (~750 tokens) blows past it.
        let (wrapper, loader, id, mgr) = wired_wrapper(1_000, 2).await;
        let store = mgr.summary_store();
        store
            .upsert_success(&id, 5, 1, "m", "span", Utc::now())
            .await
            .unwrap();
        loader.put(id.as_str(), &"X".repeat(3_000));

        let messages = vec![
            text(Role::System, "system"),
            text(Role::User, "u1"),
            text(Role::Assistant, "a1"),
            text(Role::User, "u2"),
            text(Role::Assistant, "a2"),
        ];
        let out = wrapper
            .compress(
                &messages,
                ok_chat("<analysis>x</analysis><summary>S</summary>"),
            )
            .await
            .unwrap();
        match out {
            CompressOutput::Replaced { messages: m, .. } => {
                // Inner Truncate(2) kept system + last 2 non-system.
                // The fast-path's giant summary must not have been
                // installed; the body must NOT carry the
                // <context-summary> wrapper.
                let has_wrapper = m.iter().any(|msg| {
                    msg.content.iter().any(
                        |b| matches!(b, ContentBlock::Text(t) if t.contains("<context-summary>")),
                    )
                });
                assert!(
                    !has_wrapper,
                    "wrapper must not produce its own assembly when threshold exceeded"
                );
                assert_eq!(m.len(), 3, "Truncate(2) should keep system + 2 messages");
            }
            _ => panic!("expected Replaced via fall-through"),
        }
    }

    /// FsSummaryLoader basic round trip via tempdir: write a file,
    /// load it back; missing file maps to Ok(None).
    #[tokio::test]
    async fn fs_loader_round_trips_and_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let session_id = SessionId::from("abc");
        let session_dir = dir.path().join(session_id.as_str());
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("summary.md"), "hello world").unwrap();

        let loader = FsSummaryLoader::new(dir.path().to_path_buf());
        assert_eq!(
            loader.load(&session_id).await.unwrap().as_deref(),
            Some("hello world")
        );

        let missing = SessionId::from("does-not-exist");
        assert_eq!(loader.load(&missing).await.unwrap(), None);
    }
}
