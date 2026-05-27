//! Embedding counterpart to the chat billing chokepoint
//! (`guard.rs` / `billed.rs`).
//!
//! [`EmbeddingClient`] is the raw provider contract — the embedding
//! analogue of [`crate::LlmCompletion`]. [`BillableEmbedding`] seals it
//! behind the *same* [`CostHooks`] the chat path uses, and
//! [`BoundBilledEmbedding`] is the only way to reach a provider with
//! recording attached: every `embed` runs gate → call → record, so
//! embedding spend flows through the identical micro-USD chokepoint and a
//! successful return guarantees the call was billed (or explicitly waived
//! via the no-op recorder).
//!
//! No concrete provider lives here yet — only the trait and the billed
//! wrapper. A real embedding backend is wired in separately.

use std::sync::Arc;

use async_trait::async_trait;
use aura_model::MicroUsd;

use crate::TokenUsage;
use crate::billed::{Attribution, CostHooks, LlmCostRecorder};
use crate::guard::LlmCallGuard;

/// Raw embedding provider — the embedding analogue of
/// [`crate::LlmCompletion`]. Batch-oriented: [`Self::embed`] takes many
/// inputs and returns one vector per input, in order. Reports token usage
/// alongside the vectors so the billed wrapper can record spend, exactly
/// as `LlmResponse` carries `usage`. Implemented by concrete providers
/// (deferred) and test stubs.
#[async_trait]
pub trait EmbeddingClient: Send + Sync {
    /// Embed a batch of texts. Returns one vector per input, in the same
    /// order, plus the token usage to bill.
    async fn embed(&self, inputs: &[String]) -> crate::Result<EmbeddingResponse>;

    /// Dimensionality of the vectors this model produces. Lets a caller
    /// size its vector store before issuing a call.
    fn dimensions(&self) -> usize;

    /// Model identifier, e.g. `"text-embedding-3-small"`. The cost
    /// recorder keys its pricing lookup off this, mirroring how chat bills
    /// against `ModelInfo::id`.
    fn model_id(&self) -> &str;
}

/// Vectors + usage returned by a raw [`EmbeddingClient::embed`]. One
/// vector per input, in request order.
#[derive(Debug, Clone)]
pub struct EmbeddingResponse {
    pub embeddings: Vec<Vec<f32>>,
    pub usage: TokenUsage,
}

/// Result of a billed embed: the vectors paired with the billed cost in
/// micro-USD. `cost_micros == 0` is normal for models the pricing table
/// hasn't learned, and for the no-op recorder.
#[derive(Debug, Clone)]
pub struct BilledEmbeddingResponse {
    pub embeddings: Vec<Vec<f32>>,
    pub cost_micros: MicroUsd,
}

/// Sealed embedding handle — the embedding analogue of
/// [`crate::BillableLlm`]. Holds the raw provider plus the same
/// [`CostHooks`] the chat path uses; attribution-free and shared across
/// all sessions. [`BillableEmbedding::bind`] pins an [`Attribution`] to
/// produce a [`BoundBilledEmbedding`], which is the path that actually
/// reaches a provider with recording attached. Like `BillableLlm`, the
/// raw `embed` is `pub(crate)` so the only billed path out of this crate
/// runs the recorder.
pub struct BillableEmbedding {
    inner: Arc<dyn EmbeddingClient>,
    guard: LlmCallGuard,
    recorder: LlmCostRecorder,
}

impl BillableEmbedding {
    /// Wrap `inner` with the shared [`CostHooks`] and return the sealed
    /// handle. Returns `Arc<Self>` so every consumer gets cheap clones,
    /// mirroring [`crate::BillableLlm::new`].
    pub fn new(inner: Arc<dyn EmbeddingClient>, billing: CostHooks) -> Arc<Self> {
        Arc::new(Self {
            inner,
            guard: billing.guard,
            recorder: billing.record,
        })
    }

    /// Test-only construction with pass-through billing (admit every call,
    /// record nothing). Gated so a release build can never reach it
    /// accidentally — the embedding twin of
    /// [`crate::BillableLlm::passthrough`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn passthrough(inner: Arc<dyn EmbeddingClient>) -> Arc<Self> {
        Self::new(inner, CostHooks::passthrough())
    }

    /// Pin an [`Attribution`] to this client, yielding the
    /// [`BoundBilledEmbedding`] that performs gate → call → record. Cheap
    /// (an `Arc` clone); bind once per [`crate::billed::Attribution`]
    /// context.
    pub fn bind(self: &Arc<Self>, attribution: Attribution) -> BoundBilledEmbedding {
        BoundBilledEmbedding {
            embedding: Arc::clone(self),
            attribution,
        }
    }

    pub fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    pub fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    pub(crate) fn record(&self, attribution: &Attribution, usage: &TokenUsage) -> MicroUsd {
        (self.recorder)(attribution, self.inner.model_id(), usage)
    }

    pub(crate) async fn embed(&self, inputs: &[String]) -> crate::Result<EmbeddingResponse> {
        (self.guard)()?;
        self.inner.embed(inputs).await
    }
}

/// A [`BillableEmbedding`] bound to a fixed [`Attribution`]. The sole way
/// to reach an embedding provider with recording attached.
pub struct BoundBilledEmbedding {
    embedding: Arc<BillableEmbedding>,
    attribution: Attribution,
}

impl BoundBilledEmbedding {
    pub fn dimensions(&self) -> usize {
        self.embedding.dimensions()
    }

    pub fn model_id(&self) -> &str {
        self.embedding.model_id()
    }

    pub fn attribution(&self) -> &Attribution {
        &self.attribution
    }

    /// Gate → call → record. A successful return means the recorder ran (a
    /// `cost_records` row was written, or the no-op recorder waived it). A
    /// provider error short-circuits before recording — a call that
    /// produced no usage has nothing to bill.
    pub async fn embed(&self, inputs: &[String]) -> crate::Result<BilledEmbeddingResponse> {
        let response = self.embedding.embed(inputs).await?;
        let cost_micros = self.embedding.record(&self.attribution, &response.usage);
        Ok(BilledEmbeddingResponse {
            embeddings: response.embeddings,
            cost_micros,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;

    /// Records every recorder invocation so a test can assert the bound
    /// attribution + model id + usage that landed in the ledger.
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

    struct StubEmbedding {
        dims: usize,
        response: Mutex<Option<EmbeddingResponse>>,
    }

    #[async_trait]
    impl EmbeddingClient for StubEmbedding {
        async fn embed(&self, _inputs: &[String]) -> crate::Result<EmbeddingResponse> {
            Ok(self
                .response
                .lock()
                .take()
                .expect("stub response not set for embed call"))
        }
        fn dimensions(&self) -> usize {
            self.dims
        }
        fn model_id(&self) -> &str {
            "stub-embed"
        }
    }

    fn usage(input: usize) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: 0,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
        }
    }

    #[tokio::test]
    async fn embed_records_usage_against_bound_attribution() {
        let probe = Arc::new(RecorderProbe::default());
        let stub = Arc::new(StubEmbedding {
            dims: 3,
            response: Mutex::new(Some(EmbeddingResponse {
                embeddings: vec![vec![0.1, 0.2, 0.3]],
                usage: usage(17),
            })),
        });
        let billable = BillableEmbedding::new(stub, billing_with_probe(probe.clone()));
        assert_eq!(billable.dimensions(), 3);

        let bound = billable.bind(Attribution::system("memory-unit-test"));
        let out = bound.embed(&["hello".to_string()]).await.expect("embed ok");
        assert_eq!(out.embeddings, vec![vec![0.1, 0.2, 0.3]]);

        let calls = probe.calls.lock();
        assert_eq!(calls.len(), 1, "exactly one record per embed");
        assert_eq!(calls[0].0, "system:memory-unit-test");
        assert_eq!(calls[0].1, "stub-embed");
        assert_eq!(calls[0].2.input_tokens, 17);
    }

    #[tokio::test]
    async fn guard_rejection_short_circuits_before_provider() {
        let stub = Arc::new(StubEmbedding {
            dims: 3,
            // No response queued: if the provider were reached, `embed`
            // would panic — proving the guard fired first.
            response: Mutex::new(None),
        });
        let billing = CostHooks {
            guard: Arc::new(|| Err(crate::LlmError::GuardRejected("over budget".into()))),
            record: Arc::new(|_, _, _| MicroUsd::ZERO),
        };
        let billable = BillableEmbedding::new(stub, billing);
        let bound = billable.bind(Attribution::system("memory-unit-test"));

        let err = bound
            .embed(&["hello".to_string()])
            .await
            .expect_err("guard must reject");
        assert!(matches!(err, crate::LlmError::GuardRejected(ref m) if m == "over budget"));
    }
}
