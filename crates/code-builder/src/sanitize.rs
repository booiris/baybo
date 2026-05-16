use std::collections::HashSet;
use std::sync::Arc;

use aura_security::{LeakDetector, PlaceholderMinter, SecretVault};

use crate::error::CodeBuilderError;

/// Re-sanitize already-revealed text before sending it to the planning LLM.
///
/// Background — adversarial review #1: `ToolExecutor` reveals placeholders
/// in tool arguments before invoking the tool, so by the time CodeBuilder's
/// `execute()` runs, `task` and `inputs` may contain plaintext that the
/// outer agent loop carefully kept as placeholders. Forwarding that
/// plaintext to a nested LLM provider would breach the placeholder
/// boundary. This function rescans the text for leak-detector patterns
/// and re-mints any matches into placeholders, persisting the originals
/// in the shared `SecretVault`.
///
/// Residual risk noted in the security review: only secrets that match a
/// `LeakDetector` rule are re-caught — a user-tagged opaque secret that
/// no rule recognizes still flows through. A v2 should receive params
/// pre-reveal; for v1 this scan covers the well-known credential shapes.
pub(crate) async fn rescan_for_llm(
    text: &str,
    leak_detector: &Arc<LeakDetector>,
    minter: &Arc<PlaceholderMinter>,
    vault: &Arc<SecretVault>,
) -> Result<String, CodeBuilderError> {
    let scan = leak_detector.scan_text(text);
    if scan.matches.is_empty() {
        return Ok(text.to_owned());
    }
    let mut out = text.to_owned();
    let mut seen: HashSet<String> = HashSet::new();
    for m in scan.matches {
        if !seen.insert(m.original.clone()) {
            continue;
        }
        let placeholder = minter.mint(m.original.as_bytes());
        out = out.replace(&m.original, &placeholder);
        vault
            .store_secret(&placeholder, m.original.as_bytes())
            .await
            .map_err(|e| CodeBuilderError::Scratch(format!("vault store during rescan: {e}")))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_security::EncryptionKey;
    use aura_security::test_support::MemorySecretStore;

    fn make_vault() -> Arc<SecretVault> {
        let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
        let store = Arc::new(MemorySecretStore::new());
        Arc::new(SecretVault::new(key, store))
    }

    fn make_minter(vault: &SecretVault) -> Arc<PlaceholderMinter> {
        Arc::new(PlaceholderMinter::from_master_key(vault.master_key()))
    }

    #[tokio::test]
    async fn re_mints_aws_key_pattern() {
        let detector = Arc::new(LeakDetector::with_default_rules());
        let vault = make_vault();
        let minter = make_minter(&vault);

        let revealed = "use AKIAIOSFODNN7EXAMPLE in the script please";
        let out = rescan_for_llm(revealed, &detector, &minter, &vault)
            .await
            .unwrap();

        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"), "got: {out}");
        assert!(
            out.contains("REDACTED_SECRET_"),
            "expected placeholder in {out}"
        );

        let placeholder_re = PlaceholderMinter::placeholder_regex();
        let placeholder = placeholder_re
            .find(&out)
            .expect("placeholder present")
            .as_str()
            .to_owned();
        let stored = vault.get_secret(&placeholder).await.unwrap().unwrap();
        let bytes: &[u8] = stored.as_bytes();
        assert_eq!(bytes, b"AKIAIOSFODNN7EXAMPLE");
    }

    #[tokio::test]
    async fn pass_through_when_no_match() {
        let detector = Arc::new(LeakDetector::with_default_rules());
        let vault = make_vault();
        let minter = make_minter(&vault);
        let out = rescan_for_llm("hello world", &detector, &minter, &vault)
            .await
            .unwrap();
        assert_eq!(out, "hello world");
    }
}
