//! Billing primitives: the [`Attribution`] a call is billed to, the cost
//! hooks ([`CostHooks`]) every [`BillableLlm`] runs, and
//! [`BoundBilledLlm`] — the bound handle that performs
//! gate → call → record, so a successful return guarantees the spend was
//! accounted (or explicitly waived via a no-op recorder).
//!
//! `baybo-llm` never names `CostManager`: the recorder is an opaque
//! closure the cost layer injects (mirroring the admission guard), so the
//! `baybo-cost → baybo-llm` dependency stays one-directional. Response
//! *sanitization* is intentionally not part of this layer — the billing
//! chokepoint records raw spend; scrubbing the response for display is a
//! separate, caller-side step (`baybo-agent`, where the gateway lives).

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use baybo_model::{CallReason, MicroUsd, SessionId, SpanId, TurnId};
use futures::stream::{Stream, StreamExt};

use crate::guard::{BillableLlm, LlmCallGuard};
use crate::{ChatRequest, LlmResponse, LlmStream, ModelInfo, StreamEvent, TokenUsage};

/// Reserved `user_id` for spend no end user triggered — background safety
/// checks, maintenance probes, other platform overhead. Pairs with a
/// `session_id` of `system:<component>` (see [`Attribution::system`]) so a
/// single filter surfaces all system-initiated spend in `cost_records`.
pub const SYSTEM_USER_ID: &str = "system";

/// Who an LLM call is billed to. Threaded into [`BillableLlm::bind`] and
/// handed to the cost recorder on every call.
#[derive(Debug, Clone)]
pub struct Attribution {
    pub user_id: String,
    pub session_id: SessionId,
    pub turn_id: TurnId,
    pub span_id: SpanId,
    /// Why this call was made. Recorded onto every `cost_records` row so
    /// spend is groupable by purpose. See [`CallReason`].
    pub reason: CallReason,
}

impl Attribution {
    /// Attribution for platform-internal spend that no end user
    /// triggered. `component` names the subsystem (e.g.
    /// `"skill-assessor"`) and becomes the `session_id` suffix so its
    /// spend stays independently filterable. A fresh turn/span is minted
    /// per call — callers that bind once at startup get one stable pair
    /// for the process lifetime, which is the intended grouping.
    pub fn system(component: &str) -> Self {
        Self {
            user_id: SYSTEM_USER_ID.to_string(),
            session_id: SessionId::from(format!("system:{component}")),
            turn_id: TurnId::new(),
            span_id: SpanId::new(),
            reason: CallReason::System,
        }
    }
}

/// Post-call cost recorder: invoked after a provider response with the
/// call's attribution, model id, and token usage; returns the billed
/// amount. Opaque so `baybo-llm` needn't depend on `baybo-cost` — the cost
/// layer injects a closure capturing its `CostManager`.
pub type LlmCostRecorder = Arc<dyn Fn(&Attribution, &str, &TokenUsage) -> MicroUsd + Send + Sync>;

/// The two cost hooks every [`BillableLlm`] runs: an admission `guard`
/// before the provider call and a `record` after it. Bundled because both
/// derive from the same `CostManager` and must be wired together — a
/// client that gates spend but never records it is exactly the bug this
/// pairing makes unrepresentable.
#[derive(Clone)]
pub struct CostHooks {
    pub guard: LlmCallGuard,
    pub record: LlmCostRecorder,
}

impl CostHooks {
    /// Admit every call, record nothing. The deliberate escape hatch for
    /// argv one-shots and tests with no `CostManager` — the only place an
    /// unbilled provider call is intentional. Grep for it.
    pub fn passthrough() -> Self {
        Self {
            guard: Arc::new(|| Ok(())),
            record: Arc::new(|_, _, _| MicroUsd::ZERO),
        }
    }

    /// Real guard, no-op recorder — for unit tests that exercise the gate
    /// without standing up a `CostManager`. Test-gated on purpose:
    /// "gate but don't record" has no production use, and leaving it
    /// callable in release builds would be a billing bypass — exactly
    /// what routing every call through [`BoundBilledLlm`] prevents.
    #[cfg(test)]
    pub(crate) fn unrecorded(guard: LlmCallGuard) -> Self {
        Self {
            guard,
            ..Self::passthrough()
        }
    }
}

/// Result returned by a billed call: the provider response paired with the
/// billed cost in micro-USD. `cost_micros == 0` is normal for models the
/// pricing table hasn't learned, and for the no-op recorder.
#[derive(Debug, Clone)]
pub struct BilledChatResponse {
    pub response: LlmResponse,
    pub cost_micros: MicroUsd,
}

/// In-flow LLM call with built-in cost accounting. Errors are returned as
/// a sanitized string — implementations scrub leaked secrets before
/// surfacing the provider message. Implemented downstream (in
/// `baybo-agent`, where the security gateway lives); this crate owns the
/// trait so any holder of `Arc<dyn BilledChat>` can make a billed call
/// without agent-layer types.
#[async_trait]
pub trait BilledChat: Send + Sync {
    fn model_info(&self) -> &ModelInfo;
    async fn chat(&self, request: &ChatRequest) -> Result<BilledChatResponse, String>;
}

/// A [`BillableLlm`] bound to a fixed [`Attribution`]. The sole way to
/// reach a provider with recording attached: every `chat` / `chat_stream`
/// runs gate → call → record. Returns the *raw* provider response —
/// response sanitization is a separate, caller-side concern.
pub struct BoundBilledLlm {
    llm: Arc<BillableLlm>,
    attribution: Attribution,
}

impl BoundBilledLlm {
    pub(crate) fn new(llm: Arc<BillableLlm>, attribution: Attribution) -> Self {
        Self { llm, attribution }
    }

    pub fn model_info(&self) -> &ModelInfo {
        self.llm.model_info()
    }

    pub fn attribution(&self) -> &Attribution {
        &self.attribution
    }

    /// Gate → call → record. A successful return means the recorder ran (a
    /// `cost_records` row was written, or the no-op recorder waived it). A
    /// provider error short-circuits before recording — a call that
    /// produced no usage has nothing to bill.
    pub async fn chat(&self, request: &ChatRequest) -> crate::Result<BilledChatResponse> {
        let response = self.llm.chat(request).await?;
        let cost_micros = self.llm.record(
            &self.attribution,
            &self.llm.model_info().id,
            &response.usage,
        );
        Ok(BilledChatResponse {
            response,
            cost_micros,
        })
    }

    /// Streaming variant. The returned stream records cost when it ends or
    /// is dropped, billing the last-observed `Usage` event (zero on an
    /// early drop). This preserves partial-spend-on-error: tokens streamed
    /// before a mid-stream failure are still billed.
    ///
    /// Poll and drop the returned stream within a Tokio runtime context:
    /// recording persists asynchronously (`record_call` spawns a task), so
    /// dropping it on a non-runtime thread would panic in that spawn.
    /// Every current caller drains it inside the agent loop, which holds.
    pub async fn chat_stream(&self, request: &ChatRequest) -> crate::Result<LlmStream> {
        let inner = self.llm.chat_stream(request).await?;
        Ok(LlmStream::from_stream(RecordingStream {
            inner,
            llm: Arc::clone(&self.llm),
            attribution: self.attribution.clone(),
            last_usage: TokenUsage::default(),
            recorded: false,
        }))
    }
}

/// Wraps an [`LlmStream`] so the bound attribution is billed exactly
/// once — on the terminal `None` if the consumer drains it, or on `Drop`
/// if it bails early. Records the last `Usage` event observed.
struct RecordingStream {
    inner: LlmStream,
    llm: Arc<BillableLlm>,
    attribution: Attribution,
    last_usage: TokenUsage,
    recorded: bool,
}

impl RecordingStream {
    fn record_once(&mut self) {
        if self.recorded {
            return;
        }
        self.recorded = true;
        let model_id = self.llm.model_info().id.clone();
        self.llm
            .record(&self.attribution, &model_id, &self.last_usage);
    }
}

impl Stream for RecordingStream {
    type Item = crate::Result<StreamEvent>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(StreamEvent::Usage(usage)))) => {
                this.last_usage = usage;
                Poll::Ready(Some(Ok(StreamEvent::Usage(usage))))
            }
            Poll::Ready(None) => {
                this.record_once();
                Poll::Ready(None)
            }
            other => other,
        }
    }
}

impl Drop for RecordingStream {
    fn drop(&mut self) {
        self.record_once();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmCompletion;
    use crate::test_support::StubLlm;
    use parking_lot::Mutex;

    /// Records every recorder invocation so a test can assert the bound
    /// attribution + usage that landed in the ledger.
    #[derive(Default)]
    struct RecorderProbe {
        calls: Mutex<Vec<(String, String, TokenUsage)>>,
    }

    fn billing_with_probe(probe: Arc<RecorderProbe>) -> CostHooks {
        CostHooks {
            guard: Arc::new(|| Ok(())),
            record: Arc::new(move |attr, model_id, usage| {
                probe.calls.lock().push((
                    attr.session_id.as_str().to_string(),
                    model_id.to_string(),
                    *usage,
                ));
                MicroUsd::ZERO
            }),
        }
    }

    fn usage(input: usize, output: usize) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
        }
    }

    fn empty_request() -> ChatRequest {
        ChatRequest {
            messages: vec![],
            temperature: None,
            tools: vec![],
            reasoning_effort: None,
        }
    }

    #[test]
    fn system_attribution_uses_reserved_identity() {
        let attr = Attribution::system("skill-assessor");
        assert_eq!(attr.user_id, SYSTEM_USER_ID);
        assert_eq!(attr.session_id.as_str(), "system:skill-assessor");
    }

    #[tokio::test]
    async fn chat_records_response_usage() {
        let probe = Arc::new(RecorderProbe::default());
        let stub = Arc::new(StubLlm::new());
        stub.push_response(LlmResponse {
            content: "ok".into(),
            content_blocks: vec![],
            tool_calls: vec![],
            usage: usage(12, 34),
            thinking: None,
        });
        let guarded = BillableLlm::new(
            stub as Arc<dyn LlmCompletion>,
            billing_with_probe(probe.clone()),
        );
        let bound = guarded.bind(Attribution::system("unit-test"));

        bound.chat(&empty_request()).await.expect("chat ok");

        let calls = probe.calls.lock();
        assert_eq!(calls.len(), 1, "exactly one record per chat");
        assert_eq!(calls[0].0, "system:unit-test");
        assert_eq!(calls[0].2.input_tokens, 12);
        assert_eq!(calls[0].2.output_tokens, 34);
    }

    #[tokio::test]
    async fn chat_stream_records_terminal_usage_once() {
        let probe = Arc::new(RecorderProbe::default());
        let stub = Arc::new(StubLlm::new());
        stub.push_stream(vec![
            StreamEvent::Text("hi".into()),
            StreamEvent::Usage(usage(7, 9)),
        ]);
        let guarded = BillableLlm::new(
            stub as Arc<dyn LlmCompletion>,
            billing_with_probe(probe.clone()),
        );
        let bound = guarded.bind(Attribution::system("stream-test"));

        let mut stream = bound
            .chat_stream(&empty_request())
            .await
            .expect("stream ok");
        while stream.next().await.is_some() {}
        drop(stream);

        let calls = probe.calls.lock();
        assert_eq!(calls.len(), 1, "drained stream records exactly once");
        assert_eq!(calls[0].2.input_tokens, 7);
        assert_eq!(calls[0].2.output_tokens, 9);
    }

    #[tokio::test]
    async fn chat_stream_records_last_usage_on_mid_stream_error_drop() {
        // The subtle path: the consumer bails on a mid-stream error and
        // drops the stream WITHOUT reaching the terminal `None`, so cost
        // is recorded by `Drop`, not `poll_next(None)`. The last-seen
        // `Usage` (partial spend before the failure) must still be billed.
        let probe = Arc::new(RecorderProbe::default());
        let stub = Arc::new(StubLlm::new());
        stub.push_stream_results(vec![
            Ok(StreamEvent::Usage(usage(7, 9))),
            Err(crate::LlmError::Transient("mid-stream boom".into())),
        ]);
        let guarded = BillableLlm::new(
            stub as Arc<dyn LlmCompletion>,
            billing_with_probe(probe.clone()),
        );
        let bound = guarded.bind(Attribution::system("stream-err"));

        let mut stream = bound
            .chat_stream(&empty_request())
            .await
            .expect("stream ok");
        let mut saw_err = false;
        while let Some(ev) = stream.next().await {
            if ev.is_err() {
                saw_err = true;
                break; // bail like a real consumer; never reach None
            }
        }
        assert!(saw_err, "expected the injected mid-stream error");
        drop(stream); // Drop, not poll_next(None), must do the recording

        let calls = probe.calls.lock();
        assert_eq!(calls.len(), 1, "error + early drop records exactly once");
        assert_eq!(calls[0].2.input_tokens, 7);
        assert_eq!(calls[0].2.output_tokens, 9);
    }
}
