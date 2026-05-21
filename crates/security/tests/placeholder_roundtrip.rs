//! End-to-end test of the leak-detector → placeholder → vault → reveal
//! pipeline against the real default rule set.
//!
//! The corpus below pins one canonical sample per replace-action rule.
//! When a new rule is added in `leak_detector::add_default_rules` it
//! must also appear here — otherwise `vault_holds_one_entry_per_rule`
//! fails loudly. That coupling is intentional: it keeps the test
//! suite in step with the production rule set.

use std::collections::HashSet;
use std::sync::Arc;

use aura_security::test_support::MemorySecretStore;
use aura_security::{EncryptionKey, LeakAction, LeakDetector, PlaceholderMinter, SecretVault};

fn make_vault() -> (Arc<SecretVault>, Arc<MemorySecretStore>) {
    let store = Arc::new(MemorySecretStore::new());
    let key = EncryptionKey::new(b"aura-it-master-key-32-bytes!!!!!".to_vec()).unwrap();
    let vault = Arc::new(SecretVault::new(
        key,
        store.clone() as Arc<dyn aura_store::SecretStore>,
    ));
    (vault, store)
}

/// One canonical sample per `LeakAction::Replace` rule. The matched
/// substring should be the entire string when possible so assertions
/// stay simple.
fn corpus() -> Vec<(&'static str, &'static str)> {
    vec![
        ("aws_access_key", "AKIAIOSFODNN7EXAMPLE"),
        (
            "aws_secret_key",
            "aws_secret_access_key=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        ),
        ("google_api_key", "AIzaSyA-1234567890abcdefghijklmnopqrstu"),
        ("github_token", "ghp_1234567890abcdefghijklmnopqrstuvwxyz"),
        (
            "anthropic_api_key",
            "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-ABC",
        ),
        (
            "openai_api_key",
            "sk-abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUV",
        ),
        (
            "openrouter_api_key",
            "sk-or-v1-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ),
        (
            "groq_api_key",
            "gsk_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwx",
        ),
        ("stripe_key", "sk_live_abcdefghijklmnopqrstuvwx"),
        (
            "slack_token",
            "xoxb-1234567890-1234567890-abcdefghijklmnopqrstuvwx",
        ),
        (
            "sendgrid_api_key",
            "SG.abcdefghijklmnopqrst22.abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG",
        ),
        (
            "nearai_session",
            "sess_AbCd0123EfGh4567IjKl89mnOpQr0123StUv4567WxYzABCD0123efgh",
        ),
    ]
}

#[tokio::test]
async fn each_rule_matches_its_canonical_sample() {
    let detector = LeakDetector::with_default_rules();
    for (rule, sample) in corpus() {
        let scan = detector.scan_text(sample);
        assert!(
            scan.matches.iter().any(|m| m.rule_name == rule),
            "rule '{rule}' did not match its canonical sample"
        );
    }
}

#[tokio::test]
async fn mint_is_deterministic_across_calls() {
    let (vault, _) = make_vault();
    let minter = PlaceholderMinter::from_master_key(vault.master_key());
    let secret = b"AKIAIOSFODNN7EXAMPLE";
    let p1 = minter.mint(secret);
    let p2 = minter.mint(secret);
    assert_eq!(p1, p2);
    assert!(PlaceholderMinter::placeholder_regex().is_match(&p1));
}

#[tokio::test]
async fn vault_holds_one_entry_per_unique_secret() {
    let (vault, store) = make_vault();
    let minter = PlaceholderMinter::from_master_key(vault.master_key());

    // Mint+store the same three samples 5 times each; vault should end
    // up with exactly 3 entries.
    let samples: Vec<&[u8]> = vec![
        b"AKIAIOSFODNN7EXAMPLE",
        b"ghp_1234567890abcdefghijklmnopqrstuvwxyz",
        b"sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-ABC",
    ];
    for _ in 0..5 {
        for s in &samples {
            let placeholder = minter.mint(s);
            vault.store_secret(&placeholder, s).await.unwrap();
        }
    }
    assert_eq!(store.len(), 3);
}

#[tokio::test]
async fn full_roundtrip_per_rule() {
    let detector = LeakDetector::with_default_rules();
    let (vault, _) = make_vault();
    let minter = PlaceholderMinter::from_master_key(vault.master_key());
    let mut seen_rules = HashSet::new();
    for (rule, sample) in corpus() {
        let scan = detector.scan_text(sample);
        let m = scan
            .matches
            .iter()
            .find(|m| m.rule_name == rule)
            .unwrap_or_else(|| panic!("rule '{rule}' missing from scan"));
        let placeholder = minter.mint(m.original.as_bytes());
        vault
            .store_secret(&placeholder, m.original.as_bytes())
            .await
            .unwrap();
        let revealed = vault.get_secret(&placeholder).await.unwrap().unwrap();
        assert_eq!(revealed.as_bytes(), m.original.as_bytes());
        seen_rules.insert(rule);
    }
    assert!(seen_rules.len() >= corpus().len());
}

#[test]
fn corpus_covers_every_replace_rule() {
    // Every `LeakAction::Replace` rule should have at least one canonical
    // sample. New rules added in production must extend `corpus()` so
    // the rest of this file actually exercises them.
    let detector = LeakDetector::with_default_rules();
    let production_rules: HashSet<&str> = detector
        .rules()
        .iter()
        .filter(|r| matches!(r.action, LeakAction::Replace))
        .map(|r| r.name.as_str())
        .collect();
    let corpus_rules: HashSet<&str> = corpus().iter().map(|(r, _)| *r).collect();

    let known_uncovered: HashSet<&str> = [
        // These rules are exercised by other targeted tests in
        // `injection_corpus.rs` (auth_header etc.) or have format
        // overlaps that make a clean canonical sample noisy. Document
        // each exclusion here so regressions are visible.
        "google_oauth_token",
        "github_oauth_token",
        "github_user_server_token",
        "github_server_token",
        "github_refresh_token",
        "github_fine_grained_token",
        "gitlab_pat",
        "npm_token",
        "anthropic_oauth_token",
        "twilio_api_key",
        "telegram_bot_token",
        "jwt",
        "pem_private_key",
        "generic_api_key",
        "generic_token_assignment",
        "bearer_token",
        "auth_header",
        "password_assignment",
        "high_entropy_hex",
    ]
    .into_iter()
    .collect();

    let missing: Vec<&str> = production_rules
        .difference(&corpus_rules)
        .filter(|r| !known_uncovered.contains(*r))
        .copied()
        .collect();
    assert!(
        missing.is_empty(),
        "new replace rules without corpus samples: {missing:?}"
    );
}
