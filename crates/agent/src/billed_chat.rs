//! Shared "chat → sanitize → record cost" core. A successful return
//! guarantees a `cost_records` row was written. Span lifecycle is
//! the caller's responsibility — compression wraps the call in
//! `with_llm_span`; the tool path attributes cost to the running tool
//! span via [`BilledChatFactory::bind`].
//!
//! Placeholder policy mirrors the agent loop's tool-dispatch model:
//! compression's response stays placeholdered (just like the agent's
//! brain operates against the sanitized session). The tool-facing
//! adapter [`BilledChatRunner`] reveals placeholders in the LLM's
//! **response** before handing it to the tool — the same step
//! [`ToolExecutor`] takes on the main LLM's `tool_use` args before
//! invoking the tool. The tool gets a plaintext view, then anything
//! it surfaces back through `ToolOutput` is re-tokenised at the
//! sanitize-tool-output boundary.

use std::sync::Arc;

use async_trait::async_trait;
use aura_llm::{BilledChat, BilledChatResponse, ChatRequest, GuardedLlm, ModelInfo};
use aura_model::{JobId, MicroUsd, SessionId, SpanId};
use tracing::warn;

use crate::cost::CostManager;
use crate::security::SecurityGateway;

#[derive(Debug, Clone)]
pub(crate) struct BilledAttribution {
    pub user_id: String,
    pub session_id: SessionId,
    pub job_id: JobId,
    pub span_id: SpanId,
}

#[derive(Debug, Clone)]
pub(crate) struct BilledChatRun {
    pub response: aura_llm::LlmResponse,
    pub cost_micros: MicroUsd,
}

pub(crate) async fn chat_billed_core(
    llm: &GuardedLlm,
    security: &SecurityGateway,
    cost_manager: &Arc<CostManager>,
    model_info: &ModelInfo,
    attribution: &BilledAttribution,
    request: &ChatRequest,
) -> Result<BilledChatRun, String> {
    match llm.chat(request).await {
        Ok(mut response) => {
            if let Err(e) = security.sanitize_llm_response(&mut response).await {
                warn!(error = %e, "billed_chat: sanitize_llm_response failed");
            }
            let cost_micros = cost_manager.record_call(
                &attribution.user_id,
                attribution.session_id.clone(),
                attribution.job_id,
                attribution.span_id,
                &model_info.id,
                response.usage.input_tokens,
                response.usage.output_tokens,
                response.usage.cached_input_tokens,
                response.usage.cache_creation_input_tokens,
            );
            Ok(BilledChatRun {
                response,
                cost_micros,
            })
        }
        Err(e) => {
            let raw = e.to_string();
            let sanitized = security.sanitize_error(&raw).await.unwrap_or(raw);
            Err(sanitized)
        }
    }
}

pub struct BilledChatFactory {
    llm: Arc<GuardedLlm>,
    cost_manager: Arc<CostManager>,
    security_gateway: Arc<SecurityGateway>,
}

impl BilledChatFactory {
    pub fn new(
        llm: Arc<GuardedLlm>,
        cost_manager: Arc<CostManager>,
        security_gateway: Arc<SecurityGateway>,
    ) -> Arc<Self> {
        Arc::new(Self {
            llm,
            cost_manager,
            security_gateway,
        })
    }

    pub fn bind(
        self: &Arc<Self>,
        user_id: String,
        session_id: SessionId,
        job_id: JobId,
        span_id: SpanId,
    ) -> Arc<dyn BilledChat> {
        Arc::new(BilledChatRunner {
            factory: Arc::clone(self),
            attribution: BilledAttribution {
                user_id,
                session_id,
                job_id,
                span_id,
            },
        })
    }
}

struct BilledChatRunner {
    factory: Arc<BilledChatFactory>,
    attribution: BilledAttribution,
}

#[async_trait]
impl BilledChat for BilledChatRunner {
    fn model_info(&self) -> &ModelInfo {
        self.factory.llm.model_info()
    }

    async fn chat(&self, request: &ChatRequest) -> Result<BilledChatResponse, String> {
        let model_info = self.factory.llm.model_info();
        let mut run = chat_billed_core(
            &self.factory.llm,
            &self.factory.security_gateway,
            &self.factory.cost_manager,
            model_info,
            &self.attribution,
            request,
        )
        .await?;
        self.factory
            .security_gateway
            .reveal_llm_response(&mut run.response)
            .await
            .map_err(|e| format!("reveal_llm_response failed: {e}"))?;
        Ok(BilledChatResponse {
            response: run.response,
            cost_micros: run.cost_micros,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use aura_llm::LlmCompletion;
    use aura_llm::test_support::StubLlm;
    use aura_llm::{GuardedLlm, LlmResponse, TokenUsage};
    use aura_model::{ChatMessage, ContentBlock, JobId, Role, SessionId, SpanId};
    use aura_security::leak_detector::{LeakAction, LeakDetectionRule, LeakDetector};
    use aura_security::{EncryptionKey, SecretVault};
    use aura_storage::test_support::{MemoryCostStore, MemorySecretStore};
    use regex::Regex;

    use super::*;
    use crate::cost::{CostManager, SpendingLimits};
    use crate::security::SecurityGateway;

    fn fixture(stub: Arc<StubLlm>) -> (Arc<dyn BilledChat>, Arc<SecurityGateway>) {
        let mut detector = LeakDetector::new();
        detector.add_rule(LeakDetectionRule {
            name: "test_token".into(),
            pattern: Regex::new(r"SECRET_TOKEN_\w+").unwrap(),
            action: LeakAction::Replace,
        });
        let secret_store = Arc::new(MemorySecretStore::new());
        let vault_key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
        let vault = Arc::new(SecretVault::new(vault_key, secret_store));
        let gateway = Arc::new(SecurityGateway::new(Arc::new(detector), vault));

        let pricing = HashMap::new();
        let cost = CostManager::new(
            Arc::new(MemoryCostStore::new()),
            pricing,
            SpendingLimits::default(),
        );

        let llm = GuardedLlm::passthrough(stub as Arc<dyn LlmCompletion>);
        let factory = BilledChatFactory::new(llm, cost, Arc::clone(&gateway));
        let billed = factory.bind(
            "u".into(),
            SessionId::from("s"),
            JobId::new(),
            SpanId::new(),
        );
        (billed, gateway)
    }

    /// Mint a placeholder for `plaintext` so the SecretVault knows how to
    /// reveal it later. Mirrors what `sanitize_input` does on inbound
    /// user messages, but without dragging in the full Session/Message
    /// machinery — the test only needs the placeholder string and the
    /// vault entry behind it.
    async fn seed_placeholder(gateway: &SecurityGateway, plaintext: &str) -> String {
        let placeholder_text = gateway
            .sanitize_stream_fragment(plaintext)
            .await
            .expect("mint");
        assert_ne!(
            placeholder_text, plaintext,
            "sanitize_stream_fragment must mint a placeholder for {plaintext}"
        );
        placeholder_text
    }

    /// Tool-side LLM responses must arrive at the tool as plaintext.
    /// `ToolExecutor::reveal_in_value` decrypts the main LLM's
    /// `tool_use` args before the tool runs; the [`BilledChatRunner`]
    /// adapter mirrors that for the tool's own side-LLM call by
    /// revealing the **response**. Anything the tool surfaces through
    /// `ToolOutput` will be re-tokenised by `sanitize_tool_output`.
    #[tokio::test]
    async fn billed_chat_reveals_placeholders_in_response() {
        let stub = Arc::new(StubLlm::new());
        // Seed the vault with a placeholder so the stub can return a
        // response carrying it (mimicking the model echoing back a
        // session-level placeholder the tool already sees as
        // plaintext via its revealed args).
        let (billed, gateway) = fixture(Arc::clone(&stub));
        let placeholder = seed_placeholder(&gateway, "SECRET_TOKEN_abc123").await;
        stub.push_response(LlmResponse {
            content: format!("the secret is {placeholder}"),
            content_blocks: vec![ContentBlock::Text(format!("echo {placeholder}"))],
            tool_calls: vec![],
            usage: TokenUsage::default(),
            thinking: Some(format!("reasoning about {placeholder}")),
        });

        let request = ChatRequest {
            messages: vec![ChatMessage {
                role: Role::User,
                content: vec![ContentBlock::Text("summarise this page".to_string())],
                from_user: false,
            }],
            temperature: Some(0.0),
            tools: vec![],
        };

        let billed_response = billed.chat(&request).await.expect("chat ok");
        assert!(
            billed_response.response.content.contains("SECRET_TOKEN_abc123"),
            "tool must see revealed plaintext in content: {}",
            billed_response.response.content
        );
        assert!(
            !billed_response.response.content.contains(&placeholder),
            "placeholder must have been revealed in content: {}",
            billed_response.response.content
        );
        match billed_response.response.content_blocks.first() {
            Some(ContentBlock::Text(t)) => assert!(
                t.contains("SECRET_TOKEN_abc123") && !t.contains(&placeholder),
                "content_blocks placeholder not revealed: {t}"
            ),
            other => panic!("expected Text content_block, got {other:?}"),
        }
        let thinking = billed_response
            .response
            .thinking
            .as_ref()
            .expect("thinking present");
        assert!(
            thinking.contains("SECRET_TOKEN_abc123") && !thinking.contains(&placeholder),
            "thinking placeholder not revealed: {thinking}"
        );
    }
}
