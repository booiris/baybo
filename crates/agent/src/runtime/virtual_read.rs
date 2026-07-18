use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::{ChatMessage, ContentBlock, MessageSource, Role, SessionId, ThinkingContent};
use baybo_tools::{VirtualReadAccess, VirtualReadResolver, VirtualReadWindow};
use baybo_workspace::WorkspacePaths;
use baybo_workspace::paths::SESSION_LOG_EXTENSION;
use tracing::warn;

/// Data source for [`SessionTranscriptReader`]: the full durable transcript in
/// ordinal order, **including rows compaction has since superseded** (the
/// detail folded into a summary). Implemented by [`crate::SessionManager`];
/// kept a trait so the reader is unit-testable with a fake.
#[async_trait]
pub trait SessionTranscript: Send + Sync {
    async fn full_transcript(&self, session_id: &SessionId) -> anyhow::Result<Vec<ChatMessage>>;
}

#[async_trait]
impl SessionTranscript for crate::SessionManager {
    async fn full_transcript(&self, session_id: &SessionId) -> anyhow::Result<Vec<ChatMessage>> {
        crate::SessionManager::full_transcript(self, session_id)
            .await
            .map_err(|e| anyhow::anyhow!("load full transcript: {e}"))
    }
}

/// [`VirtualReadResolver`] that serves the per-session transcript recovery path
/// (`<root>/logs/sessions/<id>.jsonl`, embedded in the compaction summary as a
/// `read the full transcript at <path>` pointer) from the durable store
/// instead of a file — no file is ever written there.
///
/// **Access control:** the content is keyed to the *caller*
/// ([`VirtualReadAccess::session_id`]), never to the session id encoded in the
/// requested path. A session can therefore only ever read its **own**
/// transcript; a `Read` aimed at another session's transcript path is denied
/// and audited, not served (and not a silent `ENOENT`).
pub struct SessionTranscriptReader {
    transcript: Arc<dyn SessionTranscript>,
    workspace: WorkspacePaths,
}

impl SessionTranscriptReader {
    pub fn new(transcript: Arc<dyn SessionTranscript>, workspace: WorkspacePaths) -> Self {
        Self {
            transcript,
            workspace,
        }
    }

    /// Whether `path` has the shape of a transcript path —
    /// `<sessions_log_dir>/*.jsonl` — regardless of which session it names.
    fn is_transcript_path(&self, path: &Path) -> bool {
        path.parent() == Some(self.workspace.sessions_log_dir().as_path())
            && path.extension().and_then(|e| e.to_str()) == Some(SESSION_LOG_EXTENSION)
    }
}

#[async_trait]
impl VirtualReadResolver for SessionTranscriptReader {
    async fn resolve(
        &self,
        path: &Path,
        access: &VirtualReadAccess<'_>,
        window: &VirtualReadWindow,
    ) -> Option<Result<String, String>> {
        if !self.is_transcript_path(path) {
            return None;
        }
        // Access control: serve only the caller's OWN transcript. Keying off
        // `access.session_id` (the authenticated caller) rather than an id
        // parsed from `path` means a fabricated path naming another session can
        // never be served — it is denied and audited.
        if path != self.workspace.session_log_file(access.session_id.as_str()) {
            warn!(
                caller = %access.session_id,
                requested = %path.display(),
                "denied cross-session transcript read"
            );
            return Some(Err(
                "access denied: a session may only read its own transcript".to_string(),
            ));
        }
        Some(
            match self.transcript.full_transcript(access.session_id).await {
                // Render only up to the window's last line — a paged read
                // of a long transcript stops materialising text at the
                // page boundary instead of rendering the whole log per
                // page — then number + slice with the shared paginator so
                // line numbers match a real file read. The end line comes
                // from the paginator itself so its offset/limit defaulting
                // has a single source of truth.
                Ok(messages) => {
                    let end_line = baybo_tools::paginate_end_line(window.offset, window.limit);
                    let rendered = render_transcript(&messages, end_line);
                    Ok(baybo_tools::paginate_numbered(
                        &rendered,
                        window.offset,
                        window.limit,
                    ))
                }
                Err(e) => Err(format!("failed to load session transcript: {e}")),
            },
        )
    }
}

/// Cap on the rendered transcript, mirroring `ReadTool`'s 16 MiB filesystem
/// scan cap so a pathologically long-lived session can't render tens of MiB
/// into one allocation on a recovery read. (The underlying `full_transcript`
/// row load is still O(all rows) — an accepted trade-off on this rare path.)
const MAX_RENDER_BYTES: usize = 16 * 1024 * 1024;

/// Flatten a transcript into readable, line-oriented text for the
/// post-compaction recovery read. Each message gets an ordinal/provenance
/// header and its blocks expanded verbatim (text, tool calls + args, tool
/// results, thinking) so the model can recover exact pre-compaction detail.
/// Rendering stops once `max_lines` lines exist — the caller paginates by
/// line, so text past the requested window would be thrown away anyway.
fn render_transcript(messages: &[ChatMessage], max_lines: usize) -> String {
    let mut out = String::new();
    let mut lines = 0usize;
    for (i, msg) in messages.iter().enumerate() {
        if lines >= max_lines {
            break;
        }
        if out.len() >= MAX_RENDER_BYTES {
            out.push_str(&format!(
                "… [transcript truncated at {} MiB]\n",
                MAX_RENDER_BYTES / 1024 / 1024
            ));
            break;
        }
        let rendered_before = out.len();
        // Label by role, except for synthesized/framed rows: recalled-memory,
        // cron, and mid-turn-interjection rows ride the wire as `Role::User`
        // wrapped in a steering envelope, so labelling them by role alone would
        // misrepresent them as genuine user turns on the recovery path.
        let label = match msg.source() {
            MessageSource::User | MessageSource::Agent => match msg.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            },
            framed => framed.as_str(),
        };
        out.push_str(&format!("====== [{i}] {label} ======\n"));
        for block in &msg.content {
            match block {
                ContentBlock::Text(text) => {
                    out.push_str(text);
                    out.push('\n');
                }
                ContentBlock::Thinking { content, .. } => {
                    out.push_str("<thinking>\n");
                    for item in content {
                        match item {
                            ThinkingContent::Text { text, .. }
                            | ThinkingContent::Summary { text } => out.push_str(text),
                            ThinkingContent::Redacted { .. } => {
                                out.push_str("[redacted reasoning]")
                            }
                        }
                        out.push('\n');
                    }
                    out.push_str("</thinking>\n");
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    let args = serde_json::to_string(input).unwrap_or_default();
                    out.push_str(&format!("→ tool_use {name} {args}\n"));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    out.push_str(&format!("← tool_result ({tool_use_id})\n"));
                    out.push_str(content);
                    out.push('\n');
                }
                ContentBlock::Image { mime_type, .. } => {
                    out.push_str(&format!("[image {mime_type}]\n"));
                }
                ContentBlock::Audio { mime_type, .. } => {
                    out.push_str(&format!("[audio {mime_type}]\n"));
                }
                ContentBlock::File {
                    filename,
                    mime_type,
                    ..
                } => out.push_str(&format!("[file {filename} {mime_type}]\n")),
            }
        }
        out.push('\n');
        // Whole messages render atomically (simpler than clipping inside a
        // block); the line count only gates whether the NEXT message starts.
        lines += out[rendered_before..].matches('\n').count();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::{ChannelType, User};

    struct FakeTranscript(Vec<ChatMessage>);
    struct FailingTranscript;

    #[async_trait]
    impl SessionTranscript for FakeTranscript {
        async fn full_transcript(&self, _sid: &SessionId) -> anyhow::Result<Vec<ChatMessage>> {
            Ok(self.0.clone())
        }
    }

    #[async_trait]
    impl SessionTranscript for FailingTranscript {
        async fn full_transcript(&self, _sid: &SessionId) -> anyhow::Result<Vec<ChatMessage>> {
            Err(anyhow::anyhow!("store unavailable"))
        }
    }

    fn paths() -> WorkspacePaths {
        WorkspacePaths::new(std::path::PathBuf::from("/tmp/baybo-vread-test"))
    }

    fn user() -> User {
        User {
            id: "u".into(),
            name: None,
            channel: ChannelType::tui(),
        }
    }

    fn own_path(sid: &SessionId) -> std::path::PathBuf {
        paths().session_log_file(sid.as_str())
    }

    #[test]
    fn render_transcript_expands_every_block_kind() {
        let messages = vec![
            ChatMessage::user(vec![ContentBlock::Text("hello".into())]),
            ChatMessage::assistant(vec![
                ContentBlock::Thinking {
                    id: None,
                    content: vec![ThinkingContent::Text {
                        text: "ponder".into(),
                        signature: None,
                    }],
                },
                ContentBlock::Text("answer".into()),
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({ "file_path": "/x" }),
                    signature: None,
                },
            ]),
            ChatMessage::agent_context(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "body line A\nbody line B".into(),
                meta: None,
            }]),
        ];
        let out = render_transcript(&messages, usize::MAX);
        assert!(out.contains("[0] user"));
        assert!(out.contains("hello"));
        assert!(out.contains("<thinking>"));
        assert!(out.contains("ponder"));
        assert!(out.contains("answer"));
        assert!(out.contains("→ tool_use Read"));
        assert!(out.contains("← tool_result (t1)"));
        // A multi-line tool result stays across separate lines so line-based
        // pagination pages over it instead of the per-line cap clipping it.
        assert!(out.contains("body line A\nbody line B"));
    }

    #[test]
    fn render_labels_synthesized_rows_by_provenance() {
        // Recalled-memory and mid-turn-interjection rows ride the wire as
        // `Role::User`; the recovery render must mark their provenance so they
        // aren't read back as genuine user turns.
        let messages = vec![
            ChatMessage::user(vec![ContentBlock::Text("genuine".into())]),
            ChatMessage::recalled_memory(vec![ContentBlock::Text("recalled note".into())]),
            ChatMessage::user_interjection(vec![ContentBlock::Text("steer left".into())]),
        ];
        let out = render_transcript(&messages, usize::MAX);
        assert!(out.contains("[0] user"));
        assert!(out.contains("[1] recalled_memory"));
        assert!(out.contains("[2] user_interjection"));
        // The genuine user row is NOT relabelled.
        assert!(!out.contains("[0] recalled_memory"));
    }

    #[tokio::test]
    async fn serves_callers_own_transcript() {
        let sid = SessionId::from("sess-own");
        let reader = SessionTranscriptReader::new(
            Arc::new(FakeTranscript(vec![ChatMessage::user(vec![
                ContentBlock::Text("recovered detail".into()),
            ])])),
            paths(),
        );
        let u = user();
        let access = VirtualReadAccess {
            session_id: &sid,
            user: &u,
        };
        let Some(Ok(text)) = reader
            .resolve(&own_path(&sid), &access, &VirtualReadWindow::default())
            .await
        else {
            panic!("expected Some(Ok)");
        };
        assert!(text.contains("recovered detail"));
    }

    #[tokio::test]
    async fn denies_cross_session_transcript() {
        // Caller is `sess-a`; the requested path is `sess-b`'s transcript — a
        // valid transcript-shaped path, but not the caller's own.
        let caller = SessionId::from("sess-a");
        let other = SessionId::from("sess-b");
        let reader = SessionTranscriptReader::new(Arc::new(FakeTranscript(Vec::new())), paths());
        let u = user();
        let access = VirtualReadAccess {
            session_id: &caller,
            user: &u,
        };
        let Some(Err(reason)) = reader
            .resolve(&own_path(&other), &access, &VirtualReadWindow::default())
            .await
        else {
            panic!("cross-session read must be denied");
        };
        assert!(reason.contains("own transcript"));
    }

    #[tokio::test]
    async fn unclaimed_for_non_transcript_path() {
        let sid = SessionId::from("s");
        let reader = SessionTranscriptReader::new(Arc::new(FakeTranscript(Vec::new())), paths());
        let u = user();
        let access = VirtualReadAccess {
            session_id: &sid,
            user: &u,
        };
        let elsewhere = std::path::Path::new("/tmp/baybo-vread-test/work/foo.txt");
        assert!(
            reader
                .resolve(elsewhere, &access, &VirtualReadWindow::default())
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn failed_when_store_errors() {
        let sid = SessionId::from("s");
        let reader = SessionTranscriptReader::new(Arc::new(FailingTranscript), paths());
        let u = user();
        let access = VirtualReadAccess {
            session_id: &sid,
            user: &u,
        };
        let Some(Err(reason)) = reader
            .resolve(&own_path(&sid), &access, &VirtualReadWindow::default())
            .await
        else {
            panic!("expected Some(Err)");
        };
        assert!(reason.contains("failed to load"));
    }

    // End-to-end through the real `SessionManager`: a compaction supersedes the
    // original turn with a summary, yet the reader recovers the exact
    // pre-compaction content for the owning session.
    #[tokio::test]
    async fn recovers_compacted_away_content_via_real_manager() {
        use baybo_session::test_support::{
            MemorySessionFolderStore, MemorySessionStore, MemorySessionSummaryStore,
        };
        use baybo_session::{SessionFolderStore, SessionStore, SessionSummaryStore};

        let now = chrono::Utc::now();
        let sid = SessionId::from("recover-sess");
        let session = baybo_model::Session {
            id: sid.clone(),
            user: user(),
            channel: ChannelType::tui(),
            created_at: now,
            last_active: now,
            state: baybo_model::SessionState::default(),
            root_session_id: sid.clone(),
            trigger: baybo_model::TriggerSource::User,
            lineage: None,
            hidden: false,
            pinned: false,
            archived: false,
            folder_id: None,
            title: None,
        };
        let store = Arc::new(MemorySessionStore::new());
        store.seed_session(&session);
        store
            .append_session_message(
                &sid,
                &ChatMessage::user(vec![ContentBlock::Text(
                    "exact pre-compaction detail".into(),
                )]),
            )
            .await
            .unwrap();
        store
            .apply_session_compaction(
                &sid,
                &[ChatMessage::agent_context(vec![ContentBlock::Text(
                    "summary".into(),
                )])],
            )
            .await
            .unwrap();
        let mgr = crate::SessionManager::new(
            store as Arc<dyn SessionStore>,
            Arc::new(MemorySessionSummaryStore::new()) as Arc<dyn SessionSummaryStore>,
            Arc::new(MemorySessionFolderStore::new()) as Arc<dyn SessionFolderStore>,
        );

        let reader = SessionTranscriptReader::new(Arc::new(mgr), paths());
        let u = user();
        let access = VirtualReadAccess {
            session_id: &sid,
            user: &u,
        };
        let Some(Ok(text)) = reader
            .resolve(&own_path(&sid), &access, &VirtualReadWindow::default())
            .await
        else {
            panic!("expected Some(Ok)");
        };
        assert!(
            text.contains("exact pre-compaction detail"),
            "superseded original must be recoverable: {text}"
        );
        assert!(text.contains("summary"));
    }
}
