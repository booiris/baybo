//! Conversation-title LLM call.

use std::sync::Arc;

use baybo_context::prompts::title::{build_title_prompt, sanitize_title};
use baybo_llm::{Attribution, BillableLlm, ChatRequest, ModelInfo};
use baybo_model::{ChatMessage, ContentBlock, MessageSource, SessionId, TurnId};
use baybo_trace::{
    LifecycleOutcome, LlmCallBegin, LlmCallInputs, LlmCallResult, SpanRecorder, StepHandle,
    StepKind,
};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::security::SecurityGateway;

/// Cap on the opening message handed to title generation. Naming a
/// conversation never needs more than its first couple of paragraphs, and
/// the message is unbounded user input — a pasted log as the first turn
/// would otherwise be sent verbatim, which a small lite model may not even
/// have the window for.
const TITLE_QUESTION_MAX_CHARS: usize = 2_000;

/// Cap on the seed's images, for the same reason as
/// [`TITLE_QUESTION_MAX_CHARS`]: the first couple name the conversation,
/// while one inbound row can carry dozens, each billed up to
/// [`baybo_llm::IMAGE_TOKEN_CEILING`].
const TITLE_MAX_IMAGES: usize = 2;

/// Notified after a title is persisted so a display surface can push it live.
pub trait SessionTitleSink: Send + Sync {
    fn title_updated(&self, session_id: &SessionId, title: &str);
}

/// What the title pass names a conversation from: the user's first question,
/// plus — when the lite model can see them — the images that came with it.
#[derive(Debug, PartialEq)]
pub(crate) struct TitleSeed {
    pub(crate) text: String,
    pub(crate) images: Vec<ContentBlock>,
}

impl TitleSeed {
    /// Read the seed off the transcript. Only genuine
    /// `MessageSource::User` rows count — agent-injected rows (a tool's
    /// screenshot follow-up included) and interjections never do.
    ///
    /// `text` is the first user row that carries text, truncated to
    /// [`TITLE_QUESTION_MAX_CHARS`]; `None` until one exists. An image never
    /// titles on its own: the question that follows it says what the user
    /// wants, in the user's language, and a title is written only once.
    ///
    /// `images` are the images of the user rows up to and including that
    /// one that `delivers` — the lite client's
    /// [`BillableLlm::delivers_image_block`] — would hand the model as a
    /// picture, capped at [`TITLE_MAX_IMAGES`]. So a text-only model gets
    /// none, a picture sent uncaptioned just before the question (WeChat has
    /// no captions) still rides along with it, and — short of the blob fetch
    /// failing — nothing reaches the title model as an `[image: …]` stub.
    /// Images sent after the question are not part of the seed.
    pub(crate) fn from_transcript(
        messages: &[ChatMessage],
        delivers: impl Fn(&ContentBlock) -> bool,
    ) -> Option<Self> {
        let mut images = Vec::new();
        for message in messages
            .iter()
            .filter(|m| matches!(m.source(), MessageSource::User))
        {
            let room = TITLE_MAX_IMAGES.saturating_sub(images.len());
            images.extend(
                message
                    .content
                    .iter()
                    .filter(|b| delivers(b))
                    .take(room)
                    .cloned(),
            );
            let text = message
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let text = text.trim();
            if !text.is_empty() {
                let text = match text.char_indices().nth(TITLE_QUESTION_MAX_CHARS) {
                    Some((cut, _)) => text[..cut].to_string(),
                    None => text.to_string(),
                };
                return Some(Self { text, images });
            }
        }
        None
    }
}

pub(crate) struct TitleRunner {
    pub(crate) llm_client: Arc<BillableLlm>,
    pub(crate) recorder: Arc<SpanRecorder>,
    pub(crate) security_gateway: Arc<SecurityGateway>,
    pub(crate) turn_id: TurnId,
    pub(crate) user_id: String,
    pub(crate) session_id: SessionId,
    pub(crate) model_info: ModelInfo,
    pub(crate) cancel_token: CancellationToken,
}

impl TitleRunner {
    pub(crate) async fn run(self, seed: TitleSeed) -> anyhow::Result<Option<String>> {
        let TitleSeed { text, images } = seed;
        let this = &self;
        crate::runtime::scope::with_step(
            self.recorder.as_ref(),
            self.turn_id,
            StepKind::TitleGeneration,
            self.cancel_context(),
            |step| async move {
                let with_images = !images.is_empty();
                let mut title = this.call(&step, &text, images).await;
                // An API that refuses an image fails the whole request, and
                // the per-actor guard means no later turn retries — so a
                // failure with images falls back to what a text-only model
                // gets.
                if let Err(e) = &title
                    && with_images
                    && !this.cancel_token.is_cancelled()
                {
                    warn!(error = %e, "title with images failed; retrying on the text alone");
                    title = this.call(&step, &text, Vec::new()).await;
                }
                Ok((LifecycleOutcome::Ok, title?))
            },
        )
        .await
    }

    fn cancel_context(&self) -> crate::runtime::scope::CancelContext<'_> {
        Some((
            &self.cancel_token,
            baybo_turn::CancelReason::ParentCancelled,
        ))
    }

    async fn call(
        &self,
        step: &StepHandle,
        text: &str,
        images: Vec<ContentBlock>,
    ) -> anyhow::Result<Option<String>> {
        let mut content = vec![ContentBlock::Text(build_title_prompt(text))];
        content.extend(images);
        let messages = vec![ChatMessage::user(content)];
        let request = ChatRequest {
            messages: messages.clone(),
            temperature: None,
            tools: Vec::new(),
            // Naming a conversation is a one-shot classification, so it
            // runs on the session entry's lite model and never carries the
            // session's thinking-level pin.
            reasoning_effort: None,
            ..Default::default()
        };

        let begin = LlmCallBegin {
            model_id: self.model_info.id.clone(),
            provider: self.model_info.provider.clone(),
            reasoning_effort: self
                .llm_client
                .effective_effort(request.reasoning_effort.as_deref()),
            input_messages: LlmCallInputs::Inline(messages),
            temperature: request.temperature,
            tools: None,
        };

        crate::runtime::scope::with_llm_span(
            self.recorder.as_ref(),
            step,
            self.turn_id,
            begin,
            self.cancel_context(),
            |span| async move {
                let bound = self.llm_client.bind(Attribution {
                    user_id: self.user_id.clone(),
                    session_id: self.session_id.clone(),
                    turn_id: self.turn_id,
                    span_id: span.span_id,
                    reason: baybo_llm::CallReason::Title,
                });
                match bound.chat(&request).await {
                    Ok(billed) => {
                        let mut response = billed.response;
                        if let Err(e) = self
                            .security_gateway
                            .sanitize_llm_response(&mut response)
                            .await
                        {
                            warn!(error = %e, "title: sanitize_llm_response failed");
                        }
                        let call_result = LlmCallResult {
                            output_content: response.content.clone(),
                            thinking: response.thinking.clone(),
                            tool_calls: vec![],
                            input_tokens: response.usage.input_tokens,
                            output_tokens: response.usage.output_tokens,
                            cached_input_tokens: response.usage.cached_input_tokens,
                            cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
                        };
                        (call_result, Ok(sanitize_title(&response.content)))
                    }
                    Err(e) => {
                        let raw = e.to_string();
                        let msg = self
                            .security_gateway
                            .sanitize_error(&raw)
                            .await
                            .unwrap_or(raw);
                        (LlmCallResult::default(), Err(anyhow::anyhow!(msg)))
                    }
                }
            },
        )
        .await
    }
}

#[cfg(test)]
mod seed_tests {
    use baybo_model::{BlobRef, ChatMessage, ContentBlock, SHA256_PREFIX};

    use super::{TITLE_MAX_IMAGES, TITLE_QUESTION_MAX_CHARS, TitleSeed};

    fn image(tag: char, mime: &str) -> ContentBlock {
        ContentBlock::Image {
            blob: BlobRef {
                blob_id: format!("{SHA256_PREFIX}{}.tok", tag.to_string().repeat(64)),
            },
            mime_type: mime.into(),
            filename: None,
            width: Some(800),
            height: Some(600),
        }
    }

    fn png(tag: char) -> ContentBlock {
        image(tag, "image/png")
    }

    fn text(t: &str) -> ContentBlock {
        ContentBlock::Text(t.into())
    }

    /// A text-only lite model: no image would reach it as a picture.
    fn blind(_: &ContentBlock) -> bool {
        false
    }

    /// A lite model that would get every image but a HEIC as a picture.
    /// Which images those are is the LLM crate's answer
    /// (`delivers_image_block`); the seed only has to honour it.
    fn sees_all_but_heic(block: &ContentBlock) -> bool {
        matches!(block, ContentBlock::Image { mime_type, .. } if mime_type != "image/heic")
    }

    fn seed(text: &str, images: Vec<ContentBlock>) -> Option<TitleSeed> {
        Some(TitleSeed {
            text: text.into(),
            images,
        })
    }

    /// The opening message is unbounded user input; a pasted log must not ride
    /// into the title prompt verbatim.
    #[test]
    fn a_long_opening_message_is_truncated() {
        let long = "x".repeat(TITLE_QUESTION_MAX_CHARS * 3);
        let msgs = vec![ChatMessage::user(vec![ContentBlock::Text(long)])];
        let o = TitleSeed::from_transcript(&msgs, blind).expect("text-bearing row");
        assert_eq!(o.text.chars().count(), TITLE_QUESTION_MAX_CHARS);
    }

    /// Truncation slices on a char boundary, not a byte one.
    #[test]
    fn truncation_is_char_boundary_safe() {
        let long = "\u{03b1}\u{03b2}\u{03b3}".repeat(TITLE_QUESTION_MAX_CHARS);
        let msgs = vec![ChatMessage::user(vec![ContentBlock::Text(long)])];
        let o = TitleSeed::from_transcript(&msgs, blind).expect("text-bearing row");
        assert_eq!(o.text.chars().count(), TITLE_QUESTION_MAX_CHARS);
    }

    #[test]
    fn picks_first_genuine_user_row_over_injected_and_assistant() {
        let msgs = vec![
            ChatMessage::system(vec![text("system prompt")]),
            ChatMessage::user(vec![text("How do I reset my password?")]),
            ChatMessage::assistant(vec![text("Sure…")]),
            ChatMessage::user(vec![text("second question")]),
        ];
        for delivers in [blind as fn(&ContentBlock) -> bool, sees_all_but_heic] {
            assert_eq!(
                TitleSeed::from_transcript(&msgs, delivers),
                seed("How do I reset my password?", vec![]),
            );
        }
    }

    #[test]
    fn skips_agent_injected_role_user_rows() {
        let msgs = vec![
            ChatMessage::agent_context(vec![text("injected context")]),
            ChatMessage::user(vec![text("the real question")]),
        ];
        assert_eq!(
            TitleSeed::from_transcript(&msgs, blind),
            seed("the real question", vec![]),
        );
    }

    #[test]
    fn none_when_there_is_nothing_to_title_from() {
        let no_user = vec![ChatMessage::system(vec![text("s")])];
        let blank = vec![ChatMessage::user(vec![text("  \n")])];
        for msgs in [&no_user, &blank] {
            assert_eq!(TitleSeed::from_transcript(msgs, sees_all_but_heic), None);
        }
    }

    /// The question that follows the picture says what the user wants, in
    /// their language — and a title is written only once.
    #[test]
    fn an_image_never_titles_on_its_own() {
        let msgs = vec![ChatMessage::user(vec![png('a')])];
        for delivers in [blind as fn(&ContentBlock) -> bool, sees_all_but_heic] {
            assert_eq!(TitleSeed::from_transcript(&msgs, delivers), None);
        }
    }

    #[test]
    fn without_vision_images_are_left_out() {
        let msgs = vec![
            ChatMessage::user(vec![png('a')]),
            ChatMessage::user(vec![text("How do I reset my password?"), png('b')]),
        ];
        assert_eq!(
            TitleSeed::from_transcript(&msgs, blind),
            seed("How do I reset my password?", vec![]),
        );
    }

    #[test]
    fn images_sent_with_or_just_before_the_question_ride_along() {
        let same_row = vec![ChatMessage::user(vec![
            text("why does this fail?"),
            png('a'),
        ])];
        assert_eq!(
            TitleSeed::from_transcript(&same_row, sees_all_but_heic),
            seed("why does this fail?", vec![png('a')]),
        );

        // WeChat has no captions: the picture is answered as a turn of its
        // own, and the question is the next one.
        let split = vec![
            ChatMessage::user(vec![png('a')]),
            ChatMessage::assistant(vec![text("What would you like to know?")]),
            ChatMessage::user(vec![text("why does this fail?")]),
        ];
        assert_eq!(
            TitleSeed::from_transcript(&split, sees_all_but_heic),
            seed("why does this fail?", vec![png('a')]),
        );
    }

    #[test]
    fn images_sent_after_the_question_are_left_out() {
        let msgs = vec![
            ChatMessage::user(vec![text("How do I reset my password?")]),
            ChatMessage::assistant(vec![text("Like this…")]),
            ChatMessage::user(vec![png('a')]),
        ];
        assert_eq!(
            TitleSeed::from_transcript(&msgs, sees_all_but_heic),
            seed("How do I reset my password?", vec![]),
        );
    }

    #[test]
    fn images_are_capped_in_arrival_order_across_rows() {
        let msgs = vec![
            ChatMessage::user(vec![png('a')]),
            ChatMessage::user(vec![png('b'), png('c'), png('d')]),
            ChatMessage::user(vec![text("which of these is best?"), png('e')]),
        ];
        let o = TitleSeed::from_transcript(&msgs, sees_all_but_heic).expect("seed");
        assert_eq!(o.images.len(), TITLE_MAX_IMAGES);
        assert_eq!(
            o.images,
            [png('a'), png('b'), png('c'), png('d'), png('e')][..TITLE_MAX_IMAGES]
        );
    }

    /// An image the lite model would only read as an `[image: …]` stub — a
    /// HEIC to Anthropic, anything past the byte cap — is left out, while the
    /// ones beside it still ride along.
    #[test]
    fn an_image_the_model_would_not_see_is_left_out() {
        let msgs = vec![ChatMessage::user(vec![
            text("what breed is this?"),
            image('a', "image/heic"),
            image('b', "image/jpeg"),
        ])];
        assert_eq!(
            TitleSeed::from_transcript(&msgs, sees_all_but_heic),
            seed("what breed is this?", vec![image('b', "image/jpeg")]),
        );
    }

    /// A tool's screenshot comes back as an agent-context row carrying Image
    /// blocks; it is the agent's picture, not the user's.
    #[test]
    fn a_tool_screenshot_row_is_not_the_users_image() {
        let msgs = vec![
            ChatMessage::agent_context(vec![text("[image attachment(s)]"), png('a')]),
            ChatMessage::user_interjection(vec![png('b')]),
            ChatMessage::user(vec![text("summarise the page")]),
        ];
        assert_eq!(
            TitleSeed::from_transcript(&msgs, sees_all_but_heic),
            seed("summarise the page", vec![]),
        );
    }
}

#[cfg(test)]
mod runner_tests {
    use std::sync::Arc;

    use baybo_llm::test_support::StubLlm;
    use baybo_llm::{BillableLlm, LlmCompletion, LlmError, LlmResponse};
    use baybo_model::{BlobRef, ContentBlock, SHA256_PREFIX, SessionId, TurnId};
    use baybo_security::leak_detector::LeakDetector;
    use baybo_security::test_support::MemorySecretStore;
    use baybo_security::{EncryptionKey, SecretVault};
    use baybo_trace::test_support::MemoryTraceStore;
    use baybo_trace::{SpanRecorder, TraceEventStream, TraceStore};
    use tokio_util::sync::CancellationToken;

    use super::{TitleRunner, TitleSeed};
    use crate::security::SecurityGateway;

    fn runner(stub: &Arc<StubLlm>) -> TitleRunner {
        let vault = Arc::new(SecretVault::new(
            EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap(),
            Arc::new(MemorySecretStore::new()),
        ));
        TitleRunner {
            llm_client: BillableLlm::passthrough(Arc::clone(stub) as Arc<dyn LlmCompletion>),
            recorder: Arc::new(SpanRecorder::new(
                SessionId::from("s"),
                "u".into(),
                Arc::new(MemoryTraceStore::new()) as Arc<dyn TraceStore>,
                TraceEventStream::new(),
            )),
            security_gateway: Arc::new(SecurityGateway::new(
                Arc::new(LeakDetector::with_default_rules()),
                vault,
            )),
            turn_id: TurnId::new(),
            user_id: "u".into(),
            session_id: SessionId::from("s"),
            model_info: stub.model_info().clone(),
            cancel_token: CancellationToken::new(),
        }
    }

    fn reply(content: &str) -> LlmResponse {
        LlmResponse {
            content: content.into(),
            content_blocks: vec![],
            tool_calls: vec![],
            usage: Default::default(),
            thinking: None,
        }
    }

    fn png() -> ContentBlock {
        ContentBlock::Image {
            blob: BlobRef {
                blob_id: format!("{SHA256_PREFIX}{}.tok", "a".repeat(64)),
            },
            mime_type: "image/png".into(),
            filename: None,
            width: Some(800),
            height: Some(600),
        }
    }

    #[tokio::test]
    async fn the_seed_images_ride_the_title_request_after_the_prompt() {
        let stub = Arc::new(StubLlm::new());
        stub.push_response(reply("\"Login error screenshot.\""));
        let title = runner(&stub)
            .run(TitleSeed {
                text: "why does this fail?".into(),
                images: vec![png()],
            })
            .await
            .unwrap();
        assert_eq!(title.as_deref(), Some("Login error screenshot"));

        let requests = stub.captured_requests();
        assert_eq!(requests.len(), 1);
        let [message] = requests[0].messages.as_slice() else {
            panic!("one user message, got {:?}", requests[0].messages);
        };
        let [ContentBlock::Text(prompt), rest @ ..] = message.content.as_slice() else {
            panic!("the prompt leads, got {:?}", message.content);
        };
        assert!(prompt.contains("why does this fail?"));
        assert_eq!(rest, [png()]);
    }

    #[tokio::test]
    async fn a_text_only_seed_sends_the_prompt_alone() {
        let stub = Arc::new(StubLlm::new());
        stub.push_response(reply("Reset password flow"));
        runner(&stub)
            .run(TitleSeed {
                text: "How do I reset my password?".into(),
                images: vec![],
            })
            .await
            .unwrap();

        let requests = stub.captured_requests();
        let [ContentBlock::Text(prompt)] = requests[0].messages[0].content.as_slice() else {
            panic!("text only, got {:?}", requests[0].messages[0].content);
        };
        assert!(!prompt.contains("image"), "{prompt}");
    }

    fn captioned_seed() -> TitleSeed {
        TitleSeed {
            text: "why does this fail?".into(),
            images: vec![png()],
        }
    }

    /// An API that refuses the image (a text-only endpoint whose vision flag
    /// is a guess) fails the whole request; the text alone still names the
    /// conversation, as it did before images were sent.
    #[tokio::test]
    async fn a_failed_call_with_images_retries_on_the_text_alone() {
        let stub = Arc::new(StubLlm::new());
        stub.push_response_err(LlmError::BadRequest("image input not supported".into()));
        stub.push_response(reply("Login failure"));
        let title = runner(&stub).run(captioned_seed()).await.unwrap();
        assert_eq!(title.as_deref(), Some("Login failure"));

        let requests = stub.captured_requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].messages[0].content.contains(&png()));
        let [ContentBlock::Text(prompt)] = requests[1].messages[0].content.as_slice() else {
            panic!(
                "the retry is text only, got {:?}",
                requests[1].messages[0].content
            );
        };
        assert!(prompt.contains("why does this fail?"));
        assert!(!prompt.contains("image"), "{prompt}");
    }

    #[tokio::test]
    async fn a_failed_text_only_call_is_not_retried() {
        let stub = Arc::new(StubLlm::new());
        stub.push_response_err(LlmError::BadRequest("nope".into()));
        let result = runner(&stub)
            .run(TitleSeed {
                text: "How do I reset my password?".into(),
                images: vec![],
            })
            .await;
        assert!(result.is_err());
        assert_eq!(stub.captured_requests().len(), 1);
    }

    #[tokio::test]
    async fn a_stopped_turn_does_not_retry() {
        let stub = Arc::new(StubLlm::new());
        stub.push_response_err(LlmError::BadRequest("nope".into()));
        let runner = runner(&stub);
        runner.cancel_token.cancel();
        assert!(runner.run(captioned_seed()).await.is_err());
        assert_eq!(stub.captured_requests().len(), 1);
    }
}
