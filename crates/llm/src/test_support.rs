//! Deterministic `LlmCompletion` stub for downstream integration tests.
//!
//! Push canned `LlmResponse` values and stream-event vectors before
//! running the agent loop; each `chat()` / `chat_stream()` call pops the
//! next item. Failing assertions ("queue empty") surface as
//! `LlmError::Internal` so tests stay on the normal error path without
//! masquerading as a transient/retriable failure.

use async_trait::async_trait;
use parking_lot::Mutex;

use crate::{
    ChatRequest, LlmCompletion, LlmError, LlmResponse, LlmStream, ModelInfo, ModelPricing,
    StreamEvent,
};

/// Scriptable LLM stub. Cheap to construct; thread-safe (uses interior
/// mutex). The default model_info is generic; override with
/// `with_model_info` if a test inspects provider/id strings.
pub struct StubLlm {
    model_info: ModelInfo,
    chat_queue: Mutex<std::collections::VecDeque<crate::Result<LlmResponse>>>,
    stream_queue: Mutex<std::collections::VecDeque<Vec<crate::Result<StreamEvent>>>>,
    /// If `Some(n)`, every queued `StreamEvent::Text(s)` is split into
    /// successive `Text` fragments of at most `n` UTF-8 chars before
    /// emission. Used to stress `safe_flush_boundary` by forcing
    /// placeholder delimiters across chunk boundaries.
    text_chunk_size: Mutex<Option<usize>>,
    /// Captured `ChatRequest` snapshots, one per `chat`/`chat_stream`
    /// call. Lets tests assert on what the agent loop ended up
    /// sending to the model — particularly useful when the actor's
    /// mutations on its private `Session` aren't reachable through
    /// the harness's shadow copy.
    captured_requests: Mutex<Vec<ChatRequest>>,
}

impl Default for StubLlm {
    fn default() -> Self {
        Self {
            model_info: ModelInfo {
                id: "stub-model".into(),
                provider: "stub".into(),
                context_window: 8_192,
                supports_tools: true,
                supports_vision: false,
                pricing: ModelPricing {
                    input_per_1m_tokens: aura_model::MicroUsd::ZERO,
                    output_per_1m_tokens: aura_model::MicroUsd::ZERO,
                },
            },
            chat_queue: Mutex::new(std::collections::VecDeque::new()),
            stream_queue: Mutex::new(std::collections::VecDeque::new()),
            text_chunk_size: Mutex::new(None),
            captured_requests: Mutex::new(Vec::new()),
        }
    }
}

impl StubLlm {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_model_info(mut self, info: ModelInfo) -> Self {
        self.model_info = info;
        self
    }

    /// Configure UTF-8-char-bounded chunking for queued `Text` stream
    /// events. `n = 1` produces single-char chunks; `None` disables.
    pub fn with_text_chunk_size(self, n: Option<usize>) -> Self {
        *self.text_chunk_size.lock() = n;
        self
    }

    pub fn push_response(&self, response: LlmResponse) {
        self.chat_queue.lock().push_back(Ok(response));
    }

    pub fn push_response_err(&self, err: LlmError) {
        self.chat_queue.lock().push_back(Err(err));
    }

    /// Queue one streaming response. Each call to `chat_stream()` pops
    /// one whole vector and emits its events in order.
    pub fn push_stream(&self, events: Vec<StreamEvent>) {
        self.stream_queue
            .lock()
            .push_back(events.into_iter().map(Ok).collect());
    }

    pub fn push_stream_results(&self, events: Vec<crate::Result<StreamEvent>>) {
        self.stream_queue.lock().push_back(events);
    }

    /// Snapshot of every `ChatRequest` the agent loop sent so far,
    /// in arrival order. Cloned on read so callers can keep iterating
    /// without holding the mutex.
    pub fn captured_requests(&self) -> Vec<ChatRequest> {
        self.captured_requests.lock().clone()
    }
}

fn split_text_chunks(text: &str, n: usize) -> Vec<String> {
    if n == 0 {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut current = String::new();
    let mut count = 0usize;
    for ch in text.chars() {
        current.push(ch);
        count += 1;
        if count >= n {
            out.push(std::mem::take(&mut current));
            count = 0;
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[async_trait]
impl LlmCompletion for StubLlm {
    async fn chat(&self, request: &ChatRequest) -> crate::Result<LlmResponse> {
        self.captured_requests.lock().push(request.clone());
        self.chat_queue
            .lock()
            .pop_front()
            .ok_or_else(|| LlmError::Internal(anyhow::anyhow!("StubLlm: chat queue empty")))?
    }

    async fn chat_stream(&self, request: &ChatRequest) -> crate::Result<LlmStream> {
        self.captured_requests.lock().push(request.clone());
        let raw =
            self.stream_queue.lock().pop_front().ok_or_else(|| {
                LlmError::Internal(anyhow::anyhow!("StubLlm: stream queue empty"))
            })?;
        let chunk_size = *self.text_chunk_size.lock();
        let expanded: Vec<crate::Result<StreamEvent>> = match chunk_size {
            None => raw,
            Some(n) => raw
                .into_iter()
                .flat_map(|res| match res {
                    Ok(StreamEvent::Text(t)) => split_text_chunks(&t, n)
                        .into_iter()
                        .map(|c| Ok(StreamEvent::Text(c)))
                        .collect::<Vec<_>>(),
                    other => vec![other],
                })
                .collect(),
        };
        Ok(LlmStream::from_events(expanded))
    }

    fn model_info(&self) -> &ModelInfo {
        &self.model_info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::{ChatMessage, ContentBlock, Role};
    use futures::StreamExt;

    fn req() -> ChatRequest {
        ChatRequest {
            messages: vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text("hi".into())],
            }],
            temperature: None,
            tools: vec![],
        }
    }

    #[tokio::test]
    async fn chat_returns_queued_response() {
        let stub = StubLlm::new();
        stub.push_response(LlmResponse {
            content: "hello".into(),
            content_blocks: vec![],
            tool_calls: vec![],
            usage: Default::default(),
            thinking: None,
        });
        let out = stub.chat(&req()).await.unwrap();
        assert_eq!(out.content, "hello");
    }

    #[tokio::test]
    async fn chat_stream_chunking_splits_text() {
        let stub = StubLlm::new().with_text_chunk_size(Some(2));
        stub.push_stream(vec![StreamEvent::Text("abcdef".into())]);
        let mut s = stub.chat_stream(&req()).await.unwrap();
        let mut texts = Vec::new();
        while let Some(ev) = s.next().await {
            if let Ok(StreamEvent::Text(t)) = ev {
                texts.push(t);
            }
        }
        assert_eq!(texts, vec!["ab", "cd", "ef"]);
    }

    #[tokio::test]
    async fn empty_queue_yields_internal_error() {
        let stub = StubLlm::new();
        let err = stub.chat(&req()).await.unwrap_err();
        assert!(matches!(err, LlmError::Internal(_)));
    }
}
