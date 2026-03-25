use std::collections::HashMap;
use std::sync::Arc;

use aura_core::{ContentBlock, Message, OutgoingMessage, Result, Session};

use crate::leak_detector::LeakDetector;
use crate::vault::SecretVault;

/// The security boundary through which all messages pass before entering
/// or leaving the agent. Responsible for sanitizing sensitive data.
pub struct SecurityGateway {
    leak_detector: LeakDetector,
    secret_vault: Arc<SecretVault>,
    policy_decider: Arc<dyn NetworkPolicyDecider>,
}

/// A network request descriptor used for policy decisions.
#[derive(Debug, Clone)]
pub struct NetworkRequest {
    pub host: String,
    pub port: u16,
}

/// The outcome of a network policy decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkPolicyDecision {
    Allow,
    Deny(String),
}

/// Trait for deciding whether a given network request from a tool should be
/// allowed or denied. Implementations live in the `agent` or `sandbox` crate;
/// `security` only defines the interface.
pub trait NetworkPolicyDecider: Send + Sync {
    /// Decide whether `request` issued by a tool with the given name is
    /// permitted.
    fn decide(&self, tool_name: &str, request: &NetworkRequest) -> NetworkPolicyDecision;
}

/// A default policy decider that denies all requests.
pub struct DenyAllPolicy;

impl NetworkPolicyDecider for DenyAllPolicy {
    fn decide(&self, _tool_name: &str, _request: &NetworkRequest) -> NetworkPolicyDecision {
        NetworkPolicyDecision::Deny("deny-by-default policy".into())
    }
}

/// Key used in `Session.state.extra` to store placeholder-to-secret-name
/// mappings for the current session.
const SESSION_SECRETS_KEY: &str = "__security_placeholder_map";

impl SecurityGateway {
    /// Create a new `SecurityGateway`.
    pub fn new(
        leak_detector: LeakDetector,
        secret_vault: Arc<SecretVault>,
        policy_decider: Arc<dyn NetworkPolicyDecider>,
    ) -> Self {
        Self {
            leak_detector,
            secret_vault,
            policy_decider,
        }
    }

    /// Convenience constructor using the deny-all network policy.
    pub fn with_deny_all_policy(
        leak_detector: LeakDetector,
        secret_vault: Arc<SecretVault>,
    ) -> Self {
        Self::new(leak_detector, secret_vault, Arc::new(DenyAllPolicy))
    }

    /// Return a reference to the leak detector.
    pub fn leak_detector(&self) -> &LeakDetector {
        &self.leak_detector
    }

    /// Return a reference to the secret vault.
    pub fn secret_vault(&self) -> &Arc<SecretVault> {
        &self.secret_vault
    }

    /// Return a reference to the network policy decider.
    pub fn policy_decider(&self) -> &Arc<dyn NetworkPolicyDecider> {
        &self.policy_decider
    }

    /// Scan incoming message content for sensitive data, replace matches
    /// with placeholders, and record the mapping in the session state.
    ///
    /// If any rule triggers a `Block` action the message content is cleared
    /// and a security error is returned.
    pub async fn sanitize_input(&self, msg: &mut Message, session: &mut Session) -> Result<()> {
        let (scan_result, new_blocks) = self.leak_detector.scan_content_blocks(&msg.content)?;

        if scan_result.blocked {
            msg.content = vec![ContentBlock::Text(
                "[blocked: sensitive data detected]".into(),
            )];
            return Err(aura_core::AuraError::Security(
                scan_result
                    .block_reason
                    .unwrap_or_else(|| "input blocked by leak detection rule".into()),
            ));
        }

        // Persist placeholder mappings into session state for traceability.
        if !scan_result.replacements.is_empty() {
            let existing = session
                .state
                .extra
                .get(SESSION_SECRETS_KEY)
                .and_then(|v| serde_json::from_value::<HashMap<String, String>>(v.clone()).ok())
                .unwrap_or_default();

            let mut map = existing;
            for replacement in &scan_result.replacements {
                // Store placeholder -> rule_name (NOT the original secret).
                map.insert(
                    replacement.placeholder.clone(),
                    replacement.rule_name.clone(),
                );
            }

            let map_value = serde_json::to_value(&map).map_err(|e| {
                aura_core::AuraError::Serialization(format!(
                    "failed to serialize placeholder map: {e}"
                ))
            })?;
            session
                .state
                .extra
                .insert(SESSION_SECRETS_KEY.to_owned(), map_value);

            // Store detected secrets in the vault for later retrieval.
            for replacement in &scan_result.replacements {
                self.secret_vault
                    .store_secret(&replacement.placeholder, replacement.original.as_bytes())
                    .await?;
            }
        }

        msg.content = new_blocks;
        Ok(())
    }

    /// Re-scan an outgoing message to ensure no plaintext secrets leak out.
    /// Placeholders already present are kept as-is.
    pub async fn sanitize_output(
        &self,
        response: &mut OutgoingMessage,
        _session: &Session,
    ) -> Result<()> {
        let (scan_result, new_blocks) =
            self.leak_detector.scan_content_blocks(&response.content)?;

        if scan_result.blocked {
            response.content = vec![ContentBlock::Text(
                "[response redacted: sensitive data detected]".into(),
            )];
            return Err(aura_core::AuraError::Security(
                scan_result
                    .block_reason
                    .unwrap_or_else(|| "output blocked by leak detection rule".into()),
            ));
        }

        response.content = new_blocks;
        Ok(())
    }

    /// Delegate a network policy decision to the configured decider.
    pub fn check_network_access(
        &self,
        tool_name: &str,
        request: &NetworkRequest,
    ) -> NetworkPolicyDecision {
        self.policy_decider.decide(tool_name, request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SecretStore;
    use crate::crypto::EncryptionKey;
    use crate::leak_detector::{LeakAction, LeakDetectionRule, LeakDetector};
    use aura_core::ChannelType;
    use chrono::Utc;
    use regex::Regex;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct MemorySecretStore {
        data: Mutex<HashMap<String, Vec<u8>>>,
    }

    impl MemorySecretStore {
        fn new() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl SecretStore for MemorySecretStore {
        async fn store(&self, name: &str, encrypted_value: &[u8]) -> Result<()> {
            self.data
                .lock()
                .map_err(|e| aura_core::AuraError::Security(e.to_string()))?
                .insert(name.to_owned(), encrypted_value.to_vec());
            Ok(())
        }
        async fn retrieve(&self, name: &str) -> Result<Option<Vec<u8>>> {
            Ok(self
                .data
                .lock()
                .map_err(|e| aura_core::AuraError::Security(e.to_string()))?
                .get(name)
                .cloned())
        }
        async fn delete(&self, name: &str) -> Result<()> {
            self.data
                .lock()
                .map_err(|e| aura_core::AuraError::Security(e.to_string()))?
                .remove(name);
            Ok(())
        }
        async fn list(&self) -> Result<Vec<String>> {
            Ok(self
                .data
                .lock()
                .map_err(|e| aura_core::AuraError::Security(e.to_string()))?
                .keys()
                .cloned()
                .collect())
        }
    }

    fn make_gateway() -> SecurityGateway {
        let detector = LeakDetector::with_default_rules();
        let key = EncryptionKey::new(b"test-key-32-bytes-for-testing!!!".to_vec()).unwrap();
        let store = Arc::new(MemorySecretStore::new());
        let vault = Arc::new(SecretVault::new(key, store));
        SecurityGateway::with_deny_all_policy(detector, vault)
    }

    fn make_message(text: &str) -> Message {
        Message {
            id: "msg-1".into(),
            session_id: "sess-1".into(),
            channel: aura_core::ChannelType::Cli,
            sender: aura_core::User {
                id: "user-1".into(),
                name: Some("Test".into()),
                channel: ChannelType::Cli,
            },
            content: vec![ContentBlock::Text(text.into())],
            timestamp: Utc::now(),
            reply_to: None,
            metadata: Default::default(),
        }
    }

    fn make_session() -> Session {
        Session {
            id: "sess-1".into(),
            user: aura_core::User {
                id: "user-1".into(),
                name: Some("Test".into()),
                channel: ChannelType::Cli,
            },
            channel: aura_core::ChannelType::Cli,
            messages: vec![],
            created_at: Utc::now(),
            last_active: Utc::now(),
            state: Default::default(),
        }
    }

    #[tokio::test]
    async fn sanitize_input_replaces_aws_key() {
        let gw = make_gateway();
        let mut msg = make_message("Here is my key: AKIAIOSFODNN7EXAMPLE please use it");
        let mut session = make_session();

        gw.sanitize_input(&mut msg, &mut session).await.unwrap();

        if let ContentBlock::Text(ref s) = msg.content[0] {
            assert!(!s.contains("AKIAIOSFODNN7EXAMPLE"));
            assert!(s.contains("{{SECRET_"));
        } else {
            panic!("expected text block");
        }

        // Session should have the placeholder map.
        assert!(session.state.extra.contains_key(SESSION_SECRETS_KEY));
    }

    #[tokio::test]
    async fn sanitize_input_blocks_on_block_rule() {
        let mut detector = LeakDetector::new();
        detector.add_rule(LeakDetectionRule {
            name: "block_test".into(),
            pattern: Regex::new(r"TOP_SECRET_\w+").unwrap(),
            action: LeakAction::Block,
        });

        let key = EncryptionKey::new(b"test-key-32-bytes-for-testing!!!".to_vec()).unwrap();
        let store: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        let vault = Arc::new(SecretVault::new(key, store));
        let gw = SecurityGateway::with_deny_all_policy(detector, vault);

        let mut msg = make_message("Here is TOP_SECRET_DATA for you");
        let mut session = make_session();

        let result = gw.sanitize_input(&mut msg, &mut session).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn sanitize_output_catches_leaked_secrets() {
        let gw = make_gateway();
        let session = make_session();

        let mut response = OutgoingMessage {
            session_id: "sess-1".into(),
            channel: aura_core::ChannelType::Cli,
            content: vec![ContentBlock::Text(
                "Here is the key: AKIAIOSFODNN7EXAMPLE".into(),
            )],
            reply_to: None,
            metadata: Default::default(),
        };

        gw.sanitize_output(&mut response, &session).await.unwrap();

        if let ContentBlock::Text(ref s) = response.content[0] {
            assert!(!s.contains("AKIAIOSFODNN7EXAMPLE"));
            assert!(s.contains("{{SECRET_"));
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn clean_input_passes_through() {
        let gw = make_gateway();
        let mut msg = make_message("Hello, how are you today?");
        let mut session = make_session();

        gw.sanitize_input(&mut msg, &mut session).await.unwrap();

        if let ContentBlock::Text(ref s) = msg.content[0] {
            assert_eq!(s, "Hello, how are you today?");
        }
    }

    #[test]
    fn deny_all_policy_denies() {
        let gw = make_gateway();
        let decision = gw.check_network_access(
            "some_tool",
            &NetworkRequest {
                host: "example.com".into(),
                port: 443,
            },
        );
        assert_eq!(
            decision,
            NetworkPolicyDecision::Deny("deny-by-default policy".into())
        );
    }
}
