use std::collections::HashMap;
use std::sync::Arc;

use aura_security::{crypto, EncryptionKey, SecurityError};
use aura_storage::SecretStore;

type Result<T> = std::result::Result<T, SecurityError>;

// ---------------------------------------------------------------------------
// SecretValue
// ---------------------------------------------------------------------------

/// A secret value wrapper that prevents plaintext from appearing in `Debug`
/// output, logs, or serialized forms.
#[derive(Clone)]
pub struct SecretValue {
    inner: Vec<u8>,
}

impl SecretValue {
    pub fn new(value: Vec<u8>) -> Self {
        Self { inner: value }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.inner
    }

    pub fn as_str(&self) -> Result<&str> {
        std::str::from_utf8(&self.inner)
            .map_err(|e| SecurityError::Encryption(format!("secret is not valid UTF-8: {e}")))
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl std::fmt::Display for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

// ---------------------------------------------------------------------------
// SecretVault
// ---------------------------------------------------------------------------

/// An in-process wrapper that holds a master encryption key and delegates
/// encrypted persistence to a [`SecretStore`] implementation.
pub struct SecretVault {
    master_key: EncryptionKey,
    store: Arc<dyn SecretStore>,
}

impl SecretVault {
    pub fn new(master_key: EncryptionKey, store: Arc<dyn SecretStore>) -> Self {
        Self { master_key, store }
    }

    pub async fn store_secret(&self, name: &str, value: &[u8]) -> Result<()> {
        let encrypted = crypto::encrypt(value, &self.master_key)?;
        self.store.store(name, &encrypted).await
    }

    pub async fn get_secret(&self, name: &str) -> Result<Option<SecretValue>> {
        let encrypted = self.store.retrieve(name).await?;
        match encrypted {
            Some(data) => {
                let plaintext = crypto::decrypt(&data, &self.master_key)?;
                Ok(Some(SecretValue::new(plaintext)))
            }
            None => Ok(None),
        }
    }

    pub async fn get_secrets_for_tool(
        &self,
        _tool_name: &str,
        declared: &[String],
    ) -> Result<HashMap<String, SecretValue>> {
        let mut result = HashMap::new();
        for name in declared {
            if let Some(value) = self.get_secret(name).await? {
                result.insert(name.clone(), value);
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
impl SecretVault {
    async fn delete_secret(&self, name: &str) -> Result<()> {
        self.store.delete(name).await
    }
}

// ---------------------------------------------------------------------------
// SecurityGateway
// ---------------------------------------------------------------------------

use aura_channels::{Message, OutgoingMessage};
use aura_model::ContentBlock;
use aura_security::LeakDetector;
use aura_session::Session;

const SESSION_SECRETS_KEY: &str = "__security_placeholder_map";

/// The security boundary through which all messages pass before entering
/// or leaving the agent.
pub struct SecurityGateway {
    leak_detector: LeakDetector,
    secret_vault: Arc<SecretVault>,
}

impl SecurityGateway {
    pub fn new(leak_detector: LeakDetector, secret_vault: Arc<SecretVault>) -> Self {
        Self {
            leak_detector,
            secret_vault,
        }
    }

    pub async fn sanitize_input(&self, msg: &mut Message, session: &mut Session) -> Result<()> {
        let (scan_result, new_blocks) = self.leak_detector.scan_content_blocks(&msg.content)?;

        if scan_result.blocked {
            msg.content = vec![ContentBlock::Text(
                "[blocked: sensitive data detected]".into(),
            )];
            return Err(SecurityError::Violation(
                scan_result
                    .block_reason
                    .unwrap_or_else(|| "input blocked by leak detection rule".into()),
            ));
        }

        if !scan_result.replacements.is_empty() {
            let existing = session
                .state
                .extra
                .get(SESSION_SECRETS_KEY)
                .and_then(|v| serde_json::from_value::<HashMap<String, String>>(v.clone()).ok())
                .unwrap_or_default();

            let mut map = existing;
            for replacement in &scan_result.replacements {
                map.insert(
                    replacement.placeholder.clone(),
                    replacement.rule_name.clone(),
                );
            }

            let map_value = serde_json::to_value(&map).map_err(|e| {
                SecurityError::Storage(format!("failed to serialize placeholder map: {e}"))
            })?;
            session
                .state
                .extra
                .insert(SESSION_SECRETS_KEY.to_owned(), map_value);

            for replacement in &scan_result.replacements {
                self.secret_vault
                    .store_secret(&replacement.placeholder, replacement.original.as_bytes())
                    .await?;
            }
        }

        msg.content = new_blocks;
        Ok(())
    }

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
            return Err(SecurityError::Violation(
                scan_result
                    .block_reason
                    .unwrap_or_else(|| "output blocked by leak detection rule".into()),
            ));
        }

        response.content = new_blocks;
        Ok(())
    }
}

#[cfg(test)]
impl SecurityGateway {
    fn with_deny_all_policy(leak_detector: LeakDetector, secret_vault: Arc<SecretVault>) -> Self {
        Self::new(leak_detector, secret_vault)
    }

    fn check_network_access(
        &self,
        _tool_name: &str,
        _request: &NetworkRequest,
    ) -> NetworkPolicyDecision {
        NetworkPolicyDecision::Deny("deny-by-default policy".into())
    }
}

#[cfg(test)]
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct NetworkRequest {
    host: String,
    port: u16,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum NetworkPolicyDecision {
    Deny(String),
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use aura_security::leak_detector::{LeakAction, LeakDetectionRule};
    use aura_session::ChannelType;
    use async_trait::async_trait;
    use chrono::Utc;
    use regex::Regex;
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

    #[async_trait]
    impl SecretStore for MemorySecretStore {
        async fn store(&self, name: &str, encrypted_value: &[u8]) -> aura_storage::secret::Result<()> {
            self.data
                .lock()
                .map_err(|e| SecurityError::Violation(e.to_string()))?
                .insert(name.to_owned(), encrypted_value.to_vec());
            Ok(())
        }
        async fn retrieve(&self, name: &str) -> aura_storage::secret::Result<Option<Vec<u8>>> {
            Ok(self
                .data
                .lock()
                .map_err(|e| SecurityError::Violation(e.to_string()))?
                .get(name)
                .cloned())
        }
        async fn delete(&self, name: &str) -> aura_storage::secret::Result<()> {
            self.data
                .lock()
                .map_err(|e| SecurityError::Violation(e.to_string()))?
                .remove(name);
            Ok(())
        }
        async fn list(&self) -> aura_storage::secret::Result<Vec<String>> {
            Ok(self
                .data
                .lock()
                .map_err(|e| SecurityError::Violation(e.to_string()))?
                .keys()
                .cloned()
                .collect())
        }
    }

    fn make_vault() -> SecretVault {
        let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
        let store = Arc::new(MemorySecretStore::new());
        SecretVault::new(key, store)
    }

    fn make_gateway() -> SecurityGateway {
        let detector = LeakDetector::with_default_rules();
        let vault = Arc::new(make_vault());
        SecurityGateway::with_deny_all_policy(detector, vault)
    }

    fn make_message(text: &str) -> Message {
        Message {
            id: "msg-1".into(),
            session_id: "sess-1".into(),
            channel: ChannelType::Cli,
            sender: aura_session::User {
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
            user: aura_session::User {
                id: "user-1".into(),
                name: Some("Test".into()),
                channel: ChannelType::Cli,
            },
            channel: ChannelType::Cli,
            messages: vec![],
            created_at: Utc::now(),
            last_active: Utc::now(),
            state: Default::default(),
        }
    }

    // --- vault tests ---

    #[tokio::test]
    async fn store_and_retrieve_secret() {
        let vault = make_vault();
        vault
            .store_secret("my_api_key", b"super-secret-value")
            .await
            .unwrap();

        let secret = vault.get_secret("my_api_key").await.unwrap().unwrap();
        assert_eq!(secret.as_bytes(), b"super-secret-value");
    }

    #[tokio::test]
    async fn missing_secret_returns_none() {
        let vault = make_vault();
        let result = vault.get_secret("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn delete_secret() {
        let vault = make_vault();
        vault.store_secret("temp", b"temporary").await.unwrap();
        vault.delete_secret("temp").await.unwrap();
        assert!(vault.get_secret("temp").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_secrets_for_tool_returns_only_declared() {
        let vault = make_vault();
        vault.store_secret("secret_a", b"aaa").await.unwrap();
        vault.store_secret("secret_b", b"bbb").await.unwrap();
        vault.store_secret("secret_c", b"ccc").await.unwrap();

        let declared = vec!["secret_a".to_owned(), "secret_c".to_owned()];
        let result = vault
            .get_secrets_for_tool("some_tool", &declared)
            .await
            .unwrap();

        assert_eq!(result.len(), 2);
        assert!(result.contains_key("secret_a"));
        assert!(result.contains_key("secret_c"));
        assert!(!result.contains_key("secret_b"));
    }

    #[test]
    fn secret_value_debug_is_redacted() {
        let sv = SecretValue::new(b"my-password".to_vec());
        let debug = format!("{sv:?}");
        assert_eq!(debug, "[REDACTED]");
        assert!(!debug.contains("my-password"));
    }

    #[test]
    fn secret_value_display_is_redacted() {
        let sv = SecretValue::new(b"my-password".to_vec());
        let display = format!("{sv}");
        assert_eq!(display, "[REDACTED]");
    }

    // --- gateway tests ---

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

        assert!(session.state.extra.contains_key(SESSION_SECRETS_KEY));
    }

    #[tokio::test]
    async fn sanitize_input_blocks_on_block_rule() {
        let mut detector = aura_security::LeakDetector::new();
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
            channel: ChannelType::Cli,
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
