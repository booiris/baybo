//! Tool-facing billed-chat adapter.
//!
//! [`BoundBilledLlm`](baybo_llm::BoundBilledLlm) is the billing
//! chokepoint — it runs gate → call → record and returns the *raw*
//! provider response. [`BilledChatRunner`] is the agent-side decorator a
//! tool's `ctx.lite_llm` resolves to: on top of a bound call it adds the
//! security steps the tool path needs — sanitize the response, then
//! **reveal** session placeholders so the tool sees plaintext for its
//! own API call (mirroring `ToolExecutor`'s reveal of `tool_use` args).
//! Anything the tool surfaces back through `ToolOutput` is re-tokenised
//! at the sanitize-tool-output boundary.
//!
//! The main reasoning loop and compression don't use this adapter — they
//! bind their own [`BoundBilledLlm`] and sanitize inline, because their
//! brain operates against the *placeholdered* transcript (no reveal).

use std::sync::Arc;

use async_trait::async_trait;
use baybo_llm::{BilledChat, BilledChatResponse, BoundBilledLlm, ChatRequest, ModelInfo};
use tracing::warn;

use crate::security::SecurityGateway;

/// Tool-facing [`BilledChat`]: a bound billed call plus the response
/// sanitize + placeholder-reveal the tool path requires.
pub struct BilledChatRunner {
    bound: BoundBilledLlm,
    security_gateway: Arc<SecurityGateway>,
}

impl BilledChatRunner {
    pub fn new(bound: BoundBilledLlm, security_gateway: Arc<SecurityGateway>) -> Self {
        Self {
            bound,
            security_gateway,
        }
    }
}

#[async_trait]
impl BilledChat for BilledChatRunner {
    fn model_info(&self) -> &ModelInfo {
        self.bound.model_info()
    }

    async fn chat(&self, request: &ChatRequest) -> Result<BilledChatResponse, String> {
        let mut billed = match self.bound.chat(request).await {
            Ok(b) => b,
            Err(e) => {
                let raw = e.to_string();
                return Err(self
                    .security_gateway
                    .sanitize_error(&raw)
                    .await
                    .unwrap_or(raw));
            }
        };
        if let Err(e) = self
            .security_gateway
            .sanitize_llm_response(&mut billed.response)
            .await
        {
            warn!(error = %e, "billed_chat: sanitize_llm_response failed");
        }
        self.security_gateway
            .reveal_llm_response(&mut billed.response)
            .await
            .map_err(|e| format!("reveal_llm_response failed: {e}"))?;
        Ok(billed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use baybo_llm::test_support::StubLlm;
    use baybo_llm::{Attribution, BillableLlm, LlmCompletion, LlmResponse, TokenUsage};
    use baybo_model::{ChatMessage, ContentBlock};
    use baybo_security::leak_detector::{LeakAction, LeakDetectionRule, LeakDetector};
    use baybo_security::test_support::MemorySecretStore;
    use baybo_security::{EncryptionKey, SecretVault};
    use regex::Regex;

    use super::*;
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

        let llm = BillableLlm::passthrough(stub as Arc<dyn LlmCompletion>);
        let bound = llm.bind(Attribution::system("tool-test"));
        let runner =
            Arc::new(BilledChatRunner::new(bound, Arc::clone(&gateway))) as Arc<dyn BilledChat>;
        (runner, gateway)
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
    /// `ToolExecutor::reveal_in_value` decrypts the main LLM's `tool_use`
    /// args before the tool runs; the [`BilledChatRunner`] adapter
    /// mirrors that for the tool's own side-LLM call by revealing the
    /// **response**. Anything the tool surfaces through `ToolOutput` will
    /// be re-tokenised by `sanitize_tool_output`.
    #[tokio::test]
    async fn billed_chat_reveals_placeholders_in_response() {
        let stub = Arc::new(StubLlm::new());
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
            messages: vec![ChatMessage::agent_context(vec![ContentBlock::Text(
                "summarise this page".to_string(),
            )])],
            temperature: Some(0.0),
            tools: vec![],
            reasoning_effort: None,
            ..Default::default()
        };

        let billed_response = billed.chat(&request).await.expect("chat ok");
        assert!(
            billed_response
                .response
                .content
                .contains("SECRET_TOKEN_abc123"),
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
