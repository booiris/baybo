use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use baybo_model::{ChatMessage, ContentBlock, MessageSource, Role, SessionId, ThinkingContent};
use baybo_store::StoredMessage;
use baybo_tools::{VirtualReadAccess, VirtualReadResolver, VirtualReadWindow};
use baybo_workspace::WorkspacePaths;
use baybo_workspace::paths::{SESSION_LOG_EXTENSION, SESSION_ORDINAL_SEPARATOR};
use tracing::warn;

/// Data source for [`SessionTranscriptReader`]: the durable transcript from
/// `from_ordinal` on, in ordinal order, **including rows compaction has since
/// superseded** (the detail folded into a summary). Rows rather than bare
/// messages, so the reader can number by real ordinal and tell an original
/// from compaction's copy of it. Implemented by [`crate::SessionManager`];
/// kept a trait so the reader is unit-testable with a fake.
#[async_trait]
pub trait SessionTranscript: Send + Sync {
    async fn transcript_from(
        &self,
        session_id: &SessionId,
        from_ordinal: i64,
    ) -> anyhow::Result<Vec<StoredMessage>>;
}

#[async_trait]
impl SessionTranscript for crate::SessionManager {
    async fn transcript_from(
        &self,
        session_id: &SessionId,
        from_ordinal: i64,
    ) -> anyhow::Result<Vec<StoredMessage>> {
        crate::SessionManager::transcript_from(self, session_id, from_ordinal)
            .await
            .map_err(|e| anyhow::anyhow!("load full transcript: {e}"))
    }
}

/// [`VirtualReadResolver`] that serves the per-session transcript recovery path
/// (`<root>/logs/sessions/<id>.jsonl`, embedded in the compaction summary as a
/// `read the full transcript at <path>` pointer) from the durable store
/// instead of a file — no file is ever written there.
///
/// A path may also name where to start — `<id>@<ordinal>.jsonl`, composed by
/// [`WorkspacePaths::session_log_file_from`] — which is what the dream digest
/// hands out so a recurring pass over a long-lived conversation renders only
/// what is new to it. See [`Self::named_session`].
///
/// **Access control: none beyond the path shape.** Any session may read any
/// session's transcript.
///
/// This was own-session-only, and briefly same-person-only, and neither
/// survived contact with real data: one human routinely holds several
/// `user.id`s on a single channel — the gateway's chat path collapses to
/// `owner` while a paired phone stamps its own `device-…` id, and the two
/// interleave daily — so keying on the caller's identity denied the dream
/// pass most of the very conversations it exists to consolidate. Identity is
/// not a boundary this system can currently draw;
/// see `docs/todo/user-identity.md`.
///
/// Nothing replaces it, because today there is nothing to separate: the
/// deployment serves one person. Drawing the boundary again is part of
/// modelling identity properly, not something to re-approximate here.
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

    /// Which session's transcript `path` names, and which ordinal it starts
    /// at (`0` for the whole conversation).
    ///
    /// Resolving it matters even with nothing to authorise: serving
    /// `access.session_id` for another session's path would hand the caller
    /// its own transcript for every conversation it asked about — wrong
    /// content, and no error to notice it by.
    ///
    /// `sanitize_session_id` is lossy, so the id cannot simply be parsed
    /// back out of the path — but a candidate that *round-trips* through the
    /// same composer names itself and nothing else. An id that would need
    /// sanitising is unaddressable this way, which costs nothing: every id
    /// this system mints is already safe. The `@<ordinal>` form round-trips
    /// through its own composer for the same reason, which is also what
    /// rejects an unparsable or negative ordinal without a separate check.
    fn named_session(
        &self,
        path: &Path,
        access: &VirtualReadAccess<'_>,
    ) -> Option<(SessionId, i64)> {
        if path == self.workspace.session_log_file(access.session_id.as_str()) {
            return Some((access.session_id.clone(), 0));
        }
        let stem = path.file_stem()?.to_str()?;
        if let Some((id, ordinal)) = stem.rsplit_once(SESSION_ORDINAL_SEPARATOR) {
            let from: i64 = ordinal.parse().ok()?;
            let candidate = SessionId::from(id);
            return (path
                == self
                    .workspace
                    .session_log_file_from(candidate.as_str(), from))
            .then_some((candidate, from));
        }
        let candidate = SessionId::from(stem);
        (path == self.workspace.session_log_file(candidate.as_str())).then_some((candidate, 0))
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
        let Some((serve, from_ordinal)) = self.named_session(path, access) else {
            warn!(
                caller = %access.session_id,
                requested = %path.display(),
                "transcript path names no session this composer could have produced"
            );
            return Some(Err("no such session transcript".to_string()));
        };
        Some(
            match self.transcript.transcript_from(&serve, from_ordinal).await {
                // Render only up to the window's last line — a paged read
                // of a long transcript stops materialising text at the
                // page boundary instead of rendering the whole log per
                // page — then number + slice with the shared paginator so
                // line numbers match a real file read. The end line comes
                // from the paginator itself so its offset/limit defaulting
                // has a single source of truth.
                Ok(rows) => {
                    // Compaction's own rows are dropped, not rendered. This
                    // read exists to serve the pre-compaction detail, and the
                    // originals it replaced are all still here — so the
                    // verbatim re-injections would show the recent exchange
                    // twice, and the summary head would present a *lossy*
                    // retelling beside the thing it retold. (It is also the
                    // one row here that exists nowhere else, which is why
                    // this is a judgement rather than a tautology: a reader
                    // who wants the summary already has it in context, and a
                    // reader who wants the detail wants the originals.) The
                    // summary would also mislabel: it rides as `Role::User`
                    // with `MessageSource::Agent`, which the render calls
                    // `user`.
                    let messages: Vec<_> = rows
                        .into_iter()
                        .filter(|row| !row.compaction_inserted)
                        .map(|row| (row.ordinal, row.message))
                        .collect();
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
/// into one allocation on a recovery read. (The underlying row load is still
/// O(all rows) even for an ordinal-anchored read — the slice happens after
/// the load; an accepted trade-off on this rare path.)
const MAX_RENDER_BYTES: usize = 16 * 1024 * 1024;

/// Flatten a transcript into readable, line-oriented text for the
/// post-compaction recovery read. Each message gets an ordinal/provenance
/// header and its blocks expanded verbatim (text, tool calls + args, tool
/// results, thinking) so the model can recover exact pre-compaction detail.
/// Rendering stops once `max_lines` lines exist — the caller paginates by
/// line, so text past the requested window would be thrown away anyway.
///
/// Headers carry each row's **stored ordinal**, not its index in `messages`:
/// a read that starts mid-conversation would otherwise renumber from zero
/// and read as a whole conversation.
fn render_transcript(messages: &[(i64, ChatMessage)], max_lines: usize) -> String {
    let mut out = String::new();
    let mut lines = 0usize;
    for (i, msg) in messages.iter() {
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

    /// Sessions keyed by id, each with its own rows — so a test can tell
    /// "served the right session" from "served *a* transcript".
    struct PerSessionTranscript(std::collections::HashMap<String, Vec<ChatMessage>>);

    impl PerSessionTranscript {
        fn of(entries: [(&str, Vec<ChatMessage>); 2]) -> Self {
            Self(
                entries
                    .into_iter()
                    .map(|(id, rows)| (id.to_string(), rows))
                    .collect(),
            )
        }
    }

    /// Ordinals as the store hands them out: 0-based and contiguous, so a
    /// fake's `Vec` position IS its ordinal.
    fn numbered(rows: &[ChatMessage], from_ordinal: i64) -> Vec<StoredMessage> {
        rows.iter()
            .enumerate()
            .filter(|(i, _)| *i as i64 >= from_ordinal)
            .map(|(i, m)| StoredMessage {
                ordinal: i as i64,
                superseded_by: None,
                created_at: chrono::Utc::now(),
                compaction_inserted: false,
                message: m.clone(),
            })
            .collect()
    }

    /// The same, with the trailing `compacted` rows flagged as compaction's
    /// own — copies of what precedes them.
    fn with_compaction_tail(rows: &[ChatMessage], compacted: usize) -> Vec<StoredMessage> {
        let split = rows.len() - compacted;
        numbered(rows, 0)
            .into_iter()
            .enumerate()
            .map(|(i, mut row)| {
                row.compaction_inserted = i >= split;
                row
            })
            .collect()
    }

    #[async_trait]
    impl SessionTranscript for PerSessionTranscript {
        async fn transcript_from(
            &self,
            sid: &SessionId,
            from_ordinal: i64,
        ) -> anyhow::Result<Vec<StoredMessage>> {
            Ok(self
                .0
                .get(sid.as_str())
                .map(|rows| numbered(rows, from_ordinal))
                .unwrap_or_default())
        }
    }

    #[async_trait]
    impl SessionTranscript for FakeTranscript {
        async fn transcript_from(
            &self,
            _sid: &SessionId,
            from_ordinal: i64,
        ) -> anyhow::Result<Vec<StoredMessage>> {
            Ok(numbered(&self.0, from_ordinal))
        }
    }

    #[async_trait]
    impl SessionTranscript for FailingTranscript {
        async fn transcript_from(
            &self,
            _sid: &SessionId,
            _from_ordinal: i64,
        ) -> anyhow::Result<Vec<StoredMessage>> {
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
        let numbered: Vec<_> = numbered(&messages, 0)
            .into_iter()
            .map(|r| (r.ordinal, r.message))
            .collect();
        let out = render_transcript(&numbered, usize::MAX);
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
        let numbered: Vec<_> = numbered(&messages, 0)
            .into_iter()
            .map(|r| (r.ordinal, r.message))
            .collect();
        let out = render_transcript(&numbered, usize::MAX);
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
    async fn another_session_serves_its_own_content_not_the_callers() {
        // The dream fire's own transcript is empty; the whole point is to
        // read a different conversation. Serving `access.session_id` for the
        // requested path would return nothing at all, silently.
        let caller = SessionId::from("sess-dream");
        let requested = SessionId::from("sess-user");
        let reader = SessionTranscriptReader::new(
            Arc::new(PerSessionTranscript::of([
                (caller.as_str(), Vec::new()),
                (
                    requested.as_str(),
                    vec![ChatMessage::user(vec![ContentBlock::Text(
                        "what the user actually said".into(),
                    )])],
                ),
            ])),
            paths(),
        );
        let u = user();
        let access = VirtualReadAccess {
            session_id: &caller,
            user: &u,
        };

        let Some(Ok(text)) = reader
            .resolve(
                &own_path(&requested),
                &access,
                &VirtualReadWindow::default(),
            )
            .await
        else {
            panic!("the named conversation must be served");
        };
        assert!(text.contains("what the user actually said"), "{text}");
    }

    /// The whole point of the `@<ordinal>` form: a recurring pass over a
    /// conversation it has read before pays for the new messages only.
    /// Without it the default page (`offset` 1, `limit` 800) is the *oldest*
    /// 800 lines — the part already consolidated — and the new activity that
    /// got the conversation listed sits past the end of it.
    #[tokio::test]
    async fn a_transcript_read_can_start_where_the_last_pass_stopped() {
        let sid = SessionId::from("sess-long");
        let say = |s: &str| ChatMessage::user(vec![ContentBlock::Text(s.into())]);
        let reader = SessionTranscriptReader::new(
            Arc::new(FakeTranscript(vec![
                say("ancient history"),
                say("also old"),
                say("brand new"),
            ])),
            paths(),
        );
        let u = user();
        let access = VirtualReadAccess {
            session_id: &sid,
            user: &u,
        };

        let from_two = paths().session_log_file_from(sid.as_str(), 2);
        let Some(Ok(text)) = reader
            .resolve(&from_two, &access, &VirtualReadWindow::default())
            .await
        else {
            panic!("an ordinal-anchored path must be served");
        };
        assert!(text.contains("brand new"), "{text}");
        assert!(!text.contains("ancient history"), "{text}");
        // Numbered by stored ordinal, not by position in the slice: a slice
        // that restarts at `[0]` reads as a whole conversation and misplaces
        // every reference into it.
        assert!(text.contains("[2] user"), "{text}");
        assert!(!text.contains("[0] user"), "{text}");

        // Dropping the suffix still reads the whole thing — the pass needs
        // that when the new messages only make sense in context.
        let Some(Ok(whole)) = reader
            .resolve(&own_path(&sid), &access, &VirtualReadWindow::default())
            .await
        else {
            panic!("the plain path must still serve everything");
        };
        assert!(whole.contains("ancient history"), "{whole}");
    }

    #[tokio::test]
    async fn refuses_an_ordinal_suffix_its_own_composer_would_not_write() {
        // The round-trip is the only validation: `@007` and `@-3` parse, but
        // neither is what the composer emits for that number, and `@later`
        // does not parse at all. Serving the caller's own log for any of them
        // — the pre-`named_session` behaviour — would answer a question
        // nobody asked.
        let sid = SessionId::from("sess-a");
        let reader = SessionTranscriptReader::new(
            Arc::new(FakeTranscript(vec![ChatMessage::user(vec![
                ContentBlock::Text("not yours to serve by accident".into()),
            ])])),
            paths(),
        );
        let u = user();
        let access = VirtualReadAccess {
            session_id: &sid,
            user: &u,
        };
        for stem in ["sess-a@007", "sess-a@-3", "sess-a@later", "sess-a@"] {
            let path = paths()
                .sessions_log_dir()
                .join(format!("{stem}.{SESSION_LOG_EXTENSION}"));
            let Some(Err(reason)) = reader
                .resolve(&path, &access, &VirtualReadWindow::default())
                .await
            else {
                panic!("{stem} must be refused, not served");
            };
            assert!(reason.contains("no such session transcript"), "{reason}");
        }
    }

    #[tokio::test]
    async fn refuses_a_path_no_session_id_could_have_produced() {
        // The only thing standing between a request and a transcript is that
        // the path round-trip through the id composer. A stem the composer
        // would have sanitised names no session, so serving anything for it
        // — least of all the caller's own log — would be a lie.
        let caller = SessionId::from("sess-a");
        let reader = SessionTranscriptReader::new(
            Arc::new(PerSessionTranscript::of([
                (caller.as_str(), Vec::new()),
                (
                    "sess-b",
                    vec![ChatMessage::user(vec![ContentBlock::Text(
                        "not yours to serve by accident".into(),
                    )])],
                ),
            ])),
            paths(),
        );
        let u = user();
        let access = VirtualReadAccess {
            session_id: &caller,
            user: &u,
        };
        let unnameable = paths()
            .sessions_log_dir()
            .join(format!(".hidden.{SESSION_LOG_EXTENSION}"));
        let Some(Err(reason)) = reader
            .resolve(&unnameable, &access, &VirtualReadWindow::default())
            .await
        else {
            panic!("an unnameable transcript path must be refused");
        };
        assert!(reason.contains("no such session transcript"), "{reason}");
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
        use baybo_session::test_support::{MemorySessionFolderStore, MemorySessionStore};
        use baybo_session::{SessionFolderStore, SessionStore};

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
        // Compaction's own rows are NOT rendered. This read exists to serve
        // what the summary replaced, and every row compaction wrote is a
        // copy of material still present as the original — so showing them
        // would repeat the exchange and, because the summary head rides as
        // `Role::User` with `MessageSource::Agent`, present it as something
        // the human said.
        assert!(
            !text.contains("summary"),
            "compaction's own rows must not be rendered: {text}"
        );
    }

    /// The filter is positional, not "everything after the originals": a
    /// compaction re-injects the recent turns verbatim, so an unfiltered
    /// render shows them twice.
    #[tokio::test]
    async fn compaction_copies_are_dropped_so_the_exchange_renders_once() {
        let sid = SessionId::from("sess-compacted");
        let say = |s: &str| ChatMessage::user(vec![ContentBlock::Text(s.into())]);
        let rows = with_compaction_tail(
            &[
                say("the real exchange"),
                say("summary of the above"),
                say("the real exchange"),
            ],
            2,
        );
        struct Rows(Vec<StoredMessage>);
        #[async_trait]
        impl SessionTranscript for Rows {
            async fn transcript_from(
                &self,
                _sid: &SessionId,
                from_ordinal: i64,
            ) -> anyhow::Result<Vec<StoredMessage>> {
                Ok(self
                    .0
                    .iter()
                    .filter(|r| r.ordinal >= from_ordinal)
                    .cloned()
                    .collect())
            }
        }
        let reader = SessionTranscriptReader::new(Arc::new(Rows(rows)), paths());
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
        assert_eq!(
            text.matches("the real exchange").count(),
            1,
            "the re-injected copy must not double the exchange: {text}"
        );
        assert!(!text.contains("summary of the above"), "{text}");
    }
}
