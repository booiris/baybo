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
    RECENT_SLICE_MIN_TEXT_BLOCK_MSGS, RECENT_SLICE_MIN_TOKENS, Tokenizer,
    estimate_skill_trailer_tokens, scan_skill_calls,
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

        let (system_msgs, non_system) = partition_system(messages);

        let tokenize_msg = |m: &ChatMessage| self.tokenizer.count_message(m);
        let cut = walk_backward_atomic(
            &non_system,
            RECENT_SLICE_MIN_TOKENS,
            RECENT_SLICE_MIN_TEXT_BLOCK_MSGS,
            RECENT_SLICE_MAX_TOKENS,
            tokenize_msg,
        );
        let recent_slice = non_system[cut..].to_vec();

        // Recent-slice tokens are excluded from this budget — recent
        // is already hard-capped at `RECENT_SLICE_MAX_TOKENS`. The
        // guard catches a summary + skill_trailer combo that would
        // leave no room for the next user turn.
        let summary_msg = self.build_summary_message(&summary_content);
        let summary_tokens = self.tokenizer.count_message(&summary_msg);
        let called = scan_skill_calls(&recent_slice);
        let skill_trailer_tokens = estimate_skill_trailer_tokens(
            self.skill_registry.as_ref(),
            self.tokenizer.as_ref(),
            &called,
        );
        let fallthrough_budget =
            (self.max_tokens as f64 * FAST_PATH_FALLTHROUGH_THRESHOLD_RATIO) as usize;
        if summary_tokens + skill_trailer_tokens > fallthrough_budget {
            warn!(
                session_id = %self.session_id,
                summary_tokens,
                skill_trailer_tokens,
                fallthrough_budget,
                "fast-path: summary + skill_trailer exceeds fall-through threshold; falling through"
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
            recent_msg_count = (non_system.len() - cut),
            summary_tokens,
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
        let manager = Arc::new(
            SessionManager::new(session_store.clone(), chrono::Duration::minutes(30))
                .with_summary_store(summary_store),
        );

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
        let store = mgr.summary_store().unwrap();
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

    /// Both metadata + file present, totals fit: fast-path applies.
    /// `replaced_full_history = true` so the manager will attach the
    /// skill trailer at the end. Recent slice is the tail of the
    /// non-system messages, walked atomically.
    #[tokio::test]
    async fn fast_path_assembles_summary_plus_recent_slice() {
        let (wrapper, loader, id, mgr) = wired_wrapper(200_000, 2).await;
        let store = mgr.summary_store().unwrap();
        store
            .upsert_success(&id, 10, 100, "m", "span", Utc::now())
            .await
            .unwrap();
        loader.put(id.as_str(), "PRIOR SUMMARY CONTENT");

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
                // Recent slice present.
                assert!(m.len() >= 3, "expected system + summary + ≥1 recent msg");
            }
            _ => panic!("fast-path must succeed when metadata + file present"),
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
        let store = mgr.summary_store().unwrap();
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
