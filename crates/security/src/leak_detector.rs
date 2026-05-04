use std::path::Path;

use regex::Regex;
use serde::{Deserialize, Serialize};

use aura_model::ContentBlock;

use crate::Result;

/// Action to take when a leak detection rule matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeakAction {
    /// Block the entire message from proceeding.
    Block,
    /// Replace the matched fragment with a placeholder (substitution is
    /// performed by the caller; the detector only reports matches).
    Replace,
}

/// A single leak detection rule comprising a name, regex pattern, and action.
pub struct LeakDetectionRule {
    pub name: String,
    pub pattern: Regex,
    pub action: LeakAction,
}

/// Result of scanning text or a batch of content blocks.
#[derive(Default)]
pub struct LeakScanResult {
    /// Matches emitted by any `LeakAction::Replace` rule. Callers are
    /// expected to mint placeholders and perform substitution themselves.
    pub matches: Vec<LeakMatch>,
    /// Whether any rule with `LeakAction::Block` matched.
    pub blocked: bool,
    /// The block reason if blocked.
    pub block_reason: Option<String>,
}

/// A single detected secret fragment. Substitution is the caller's job.
#[derive(Debug, Clone)]
pub struct LeakMatch {
    pub original: String,
    pub rule_name: String,
}

/// Scans content blocks for sensitive data patterns (API keys, tokens, etc.)
/// and reports matches or block signals according to configured rules.
pub struct LeakDetector {
    rules: Vec<LeakDetectionRule>,
}

impl LeakDetector {
    /// Create an empty `LeakDetector` with no rules.
    pub fn new() -> Self {
        Self { rules: Vec::new() }
    }

    /// Create a `LeakDetector` pre-loaded with common sensitive-data patterns.
    pub fn with_default_rules() -> Self {
        let mut detector = Self::new();
        detector.add_default_rules();
        detector
    }

    /// Add a detection rule.
    pub fn add_rule(&mut self, rule: LeakDetectionRule) {
        self.rules.push(rule);
    }

    /// Scan a single string for leaks. Returns a `LeakScanResult` with any
    /// matches and/or block signals.
    pub fn scan_text(&self, text: &str) -> LeakScanResult {
        let mut matches = Vec::new();
        let mut blocked = false;
        let mut block_reason = None;

        for rule in &self.rules {
            for mat in rule.pattern.find_iter(text) {
                match rule.action {
                    LeakAction::Block => {
                        blocked = true;
                        block_reason = Some(format!(
                            "blocked by rule '{}': sensitive data detected",
                            rule.name
                        ));
                    }
                    LeakAction::Replace => {
                        matches.push(LeakMatch {
                            original: mat.as_str().to_owned(),
                            rule_name: rule.name.clone(),
                        });
                    }
                }
            }
        }

        LeakScanResult {
            matches,
            blocked,
            block_reason,
        }
    }

    /// Scan a file on disk. Returns the scan result for the file's contents.
    pub fn check_file(&self, path: &Path) -> std::io::Result<LeakScanResult> {
        let bytes = std::fs::read(path)?;
        let text = match std::str::from_utf8(&bytes) {
            Ok(s) => s.to_string(),
            Err(_) => String::from_utf8_lossy(&bytes).into_owned(),
        };
        Ok(self.scan_text(&text))
    }

    /// Public read-only view of this detector's rules.
    pub fn rules(&self) -> &[LeakDetectionRule] {
        &self.rules
    }

    /// Scan a slice of `ContentBlock`s. Returns matches aggregated across
    /// text blocks. Non-text blocks are skipped. Callers perform substitution.
    pub fn scan_content_blocks(&self, blocks: &[ContentBlock]) -> Result<LeakScanResult> {
        let mut all = Vec::new();
        let mut blocked = false;
        let mut block_reason = None;

        for block in blocks {
            if let ContentBlock::Text(text) = block {
                let r = self.scan_text(text);
                if r.blocked {
                    blocked = true;
                    block_reason = r.block_reason;
                }
                all.extend(r.matches);
            }
        }

        Ok(LeakScanResult {
            matches: all,
            blocked,
            block_reason,
        })
    }

    /// Load a standard set of rules for common secret patterns.
    ///
    /// `scan_text` runs every rule against the input and aggregates
    /// every match, so order does not affect what gets reported; each
    /// vendor-specific `sk-…` regex is narrow enough that a key for
    /// one vendor (e.g. `sk-ant-api…`) is not also captured by the
    /// patterns for another vendor.
    fn add_default_rules(&mut self) {
        let rules = vec![
            // Cloud providers
            LeakDetectionRule {
                name: "aws_access_key".to_owned(),
                pattern: Regex::new(r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "aws_secret_key".to_owned(),
                pattern: Regex::new(
                    r#"(?i)aws[_-]?secret[_-]?access[_-]?key\s*[=:]\s*['"]?([A-Za-z0-9/+=]{40})['"]?"#,
                )
                .unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "google_api_key".to_owned(),
                pattern: Regex::new(r"\bAIza[0-9A-Za-z\-_]{35}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "google_oauth_token".to_owned(),
                pattern: Regex::new(r"\bya29\.[0-9A-Za-z\-_]{20,}").unwrap(),
                action: LeakAction::Replace,
            },
            // Source-control / CI
            LeakDetectionRule {
                name: "github_token".to_owned(),
                pattern: Regex::new(r"\bghp_[A-Za-z0-9]{36}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "github_oauth_token".to_owned(),
                pattern: Regex::new(r"\bgho_[A-Za-z0-9]{36}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "github_user_server_token".to_owned(),
                pattern: Regex::new(r"\bghu_[A-Za-z0-9]{36}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "github_server_token".to_owned(),
                pattern: Regex::new(r"\bghs_[A-Za-z0-9]{36}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "github_refresh_token".to_owned(),
                pattern: Regex::new(r"\bghr_[A-Za-z0-9]{36}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "github_fine_grained_token".to_owned(),
                pattern: Regex::new(r"\bgithub_pat_[A-Za-z0-9_]{59,100}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "gitlab_pat".to_owned(),
                pattern: Regex::new(r"\bglpat-[A-Za-z0-9_\-]{20}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "npm_token".to_owned(),
                pattern: Regex::new(r"\bnpm_[A-Za-z0-9]{36}\b").unwrap(),
                action: LeakAction::Replace,
            },
            // LLM provider keys
            LeakDetectionRule {
                name: "anthropic_api_key".to_owned(),
                pattern: Regex::new(r"\bsk-ant-(?:api|admin)[0-9]{2}-[A-Za-z0-9\-_]{32,120}\b")
                    .unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "anthropic_oauth_token".to_owned(),
                pattern: Regex::new(r"\bsk-ant-oat[0-9]{2}-[A-Za-z0-9_\-]{50,200}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "openrouter_api_key".to_owned(),
                pattern: Regex::new(r"\bsk-or-v1-[a-fA-F0-9]{64}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "groq_api_key".to_owned(),
                pattern: Regex::new(r"\bgsk_[A-Za-z0-9]{40,80}\b").unwrap(),
                action: LeakAction::Replace,
            },
            // Two branches: legacy 48-char form, or any length that carries the
            // distinctive `T3BlbkFJ` marker that OpenAI embeds in modern keys.
            LeakDetectionRule {
                name: "openai_api_key".to_owned(),
                pattern: Regex::new(
                    r"\bsk-(?:[A-Za-z0-9]{48}|(?:proj-)?[A-Za-z0-9_\-]{1,}T3BlbkFJ[A-Za-z0-9_\-]*)\b",
                )
                .unwrap(),
                action: LeakAction::Replace,
            },
            // Payment / messaging
            LeakDetectionRule {
                name: "stripe_key".to_owned(),
                pattern: Regex::new(r"\b(?:sk|rk|pk)_(?:live|test)_[A-Za-z0-9]{20,}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "slack_token".to_owned(),
                pattern: Regex::new(r"\bxox[abdeoprs]-[A-Za-z0-9\-]{10,100}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "sendgrid_api_key".to_owned(),
                pattern: Regex::new(r"\bSG\.[A-Za-z0-9_\-]{22}\.[A-Za-z0-9_\-]{43}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "twilio_api_key".to_owned(),
                pattern: Regex::new(r"\bSK[a-fA-F0-9]{32}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "telegram_bot_token".to_owned(),
                pattern: Regex::new(r"\b\d{8,12}:[A-Za-z0-9_\-]{32,45}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "nearai_session".to_owned(),
                pattern: Regex::new(r"\bsess_[A-Za-z0-9]{32,100}\b").unwrap(),
                action: LeakAction::Replace,
            },
            // Generic credentials
            LeakDetectionRule {
                name: "jwt".to_owned(),
                pattern: Regex::new(
                    r"eyJ[A-Za-z0-9_\-]{10,}\.eyJ[A-Za-z0-9_\-]{10,}\.[A-Za-z0-9_\-]{10,}",
                )
                .unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "pem_private_key".to_owned(),
                pattern: Regex::new(
                    r"(?s)-----BEGIN (?:RSA |EC |DSA |OPENSSH |PGP |ENCRYPTED )?PRIVATE KEY-----.*?-----END (?:RSA |EC |DSA |OPENSSH |PGP |ENCRYPTED )?PRIVATE KEY-----",
                )
                .unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "generic_api_key".to_owned(),
                pattern: Regex::new(
                    r#"(?i)(?:api[_-]?key|apikey|api[_-]?secret)\s*[=:]\s*(?:"[A-Za-z0-9\-_]{32,200}"|'[A-Za-z0-9\-_]{32,200}'|[A-Za-z0-9\-_]{32,200}\b)"#,
                )
                .unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "generic_token_assignment".to_owned(),
                pattern: Regex::new(
                    r#"(?i)(?:access[_-]?token|auth[_-]?token|secret[_-]?token)\s*[=:]\s*(?:"[A-Za-z0-9\-_\.]{20,200}"|'[A-Za-z0-9\-_\.]{20,200}')"#,
                )
                .unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "bearer_token".to_owned(),
                pattern: Regex::new(r"(?i)bearer\s+[A-Za-z0-9\-_.]{20,500}\b").unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "auth_header".to_owned(),
                pattern: Regex::new(
                    r"(?i)authorization:\s*[A-Za-z]+\s+[A-Za-z0-9_\-\.]{20,500}\b",
                )
                .unwrap(),
                action: LeakAction::Replace,
            },
            LeakDetectionRule {
                name: "password_assignment".to_owned(),
                pattern: Regex::new(
                    r#"(?i)(?:password|passwd|pwd)\s*[=:]\s*(?:"[^\s"]{8,200}"|'[^\s']{8,200}')"#,
                )
                .unwrap(),
                action: LeakAction::Replace,
            },
            // High-entropy 64-char hex
            LeakDetectionRule {
                name: "high_entropy_hex".to_owned(),
                pattern: Regex::new(r"\b[a-fA-F0-9]{64}\b").unwrap(),
                action: LeakAction::Replace,
            },
        ];

        for rule in rules {
            self.add_rule(rule);
        }
    }
}

impl Default for LeakDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_aws_access_key() {
        let detector = LeakDetector::with_default_rules();
        let result = detector.scan_text("My key is AKIAIOSFODNN7EXAMPLE ok");
        assert!(!result.blocked);
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].original, "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(result.matches[0].rule_name, "aws_access_key");
    }

    #[test]
    fn detect_aws_sts_temporary_key() {
        let detector = LeakDetector::with_default_rules();
        let result = detector.scan_text("tmp ASIAIOSFODNN7EXAMPLE end");
        assert_eq!(result.matches.len(), 1);
        assert_eq!(result.matches[0].original, "ASIAIOSFODNN7EXAMPLE");
        assert_eq!(result.matches[0].rule_name, "aws_access_key");
    }

    #[test]
    fn aws_access_key_respects_word_boundary() {
        let detector = LeakDetector::with_default_rules();
        // embedded in a longer identifier — no boundary, must not match
        let r = detector.scan_text("prefixAKIAIOSFODNN7EXAMPLEsuffix");
        assert!(r.matches.is_empty());
    }

    #[test]
    fn detect_github_token() {
        let detector = LeakDetector::with_default_rules();
        let text = "token: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let result = detector.scan_text(text);
        assert_eq!(result.matches.len(), 1);
        assert!(result.matches[0].original.starts_with("ghp_"));
    }

    #[test]
    fn detect_slack_token() {
        let detector = LeakDetector::with_default_rules();
        let result = detector.scan_text("Use xoxb-1234567890-abcdefghij to connect");
        assert_eq!(result.matches.len(), 1);
    }

    #[test]
    fn block_action_sets_blocked() {
        let mut detector = LeakDetector::new();
        detector.add_rule(LeakDetectionRule {
            name: "block_rule".into(),
            pattern: Regex::new(r"FORBIDDEN_\w+").unwrap(),
            action: LeakAction::Block,
        });
        let result = detector.scan_text("here is FORBIDDEN_DATA inside");
        assert!(result.blocked);
        assert!(result.block_reason.is_some());
    }

    #[test]
    fn scan_content_blocks_collects_matches() {
        let detector = LeakDetector::with_default_rules();
        let blocks = vec![
            ContentBlock::Text("Key: AKIAIOSFODNN7EXAMPLE".into()),
            ContentBlock::Text("safe text".into()),
        ];
        let r = detector.scan_content_blocks(&blocks).unwrap();
        assert_eq!(r.matches.len(), 1);
        assert!(!r.blocked);
    }

    #[test]
    fn no_false_positives_on_normal_text() {
        let detector = LeakDetector::with_default_rules();
        let result = detector.scan_text("Hello, how can I help you today?");
        assert!(result.matches.is_empty());
        assert!(!result.blocked);
    }

    #[test]
    fn detect_google_api_key() {
        let detector = LeakDetector::with_default_rules();
        let r = detector.scan_text("key=AIzaSyA-1234567890abcdefghijklmnopqrstu end");
        assert!(r.matches.iter().any(|m| m.rule_name == "google_api_key"));
    }

    #[test]
    fn detect_anthropic_api_key() {
        let detector = LeakDetector::with_default_rules();
        let r = detector
            .scan_text("ANTHROPIC_API_KEY=sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-ABC");
        assert!(r.matches.iter().any(|m| m.rule_name == "anthropic_api_key"));
    }

    #[test]
    fn detect_stripe_key() {
        let detector = LeakDetector::with_default_rules();
        let r = detector.scan_text("STRIPE=sk_live_AbCdEfGhIjKlMnOpQrStUvWx");
        assert!(r.matches.iter().any(|m| m.rule_name == "stripe_key"));
    }

    #[test]
    fn detect_jwt() {
        let detector = LeakDetector::with_default_rules();
        let r = detector.scan_text(
            "Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ0ZXN0IiwibmFtZSI6IkpvaG4ifQ.SflKxwRJSMeKKF2QT4f",
        );
        assert!(r.matches.iter().any(|m| m.rule_name == "jwt"));
    }

    #[test]
    fn detect_pem_private_key() {
        let detector = LeakDetector::with_default_rules();
        let r = detector.scan_text(
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKC...\n-----END RSA PRIVATE KEY-----",
        );
        let m = r
            .matches
            .iter()
            .find(|m| m.rule_name == "pem_private_key")
            .expect("pem block should match");
        assert!(m.original.contains("MIIEpAIBAAKC"));
        assert!(m.original.ends_with("-----END RSA PRIVATE KEY-----"));
    }

    #[test]
    fn pem_header_alone_does_not_match() {
        let detector = LeakDetector::with_default_rules();
        let r = detector.scan_text("-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKC...");
        assert!(!r.matches.iter().any(|m| m.rule_name == "pem_private_key"));
    }

    #[test]
    fn detect_gitlab_pat() {
        let detector = LeakDetector::with_default_rules();
        let r = detector.scan_text("token: glpat-AbCdEfGhIjKlMnOpQrSt");
        assert!(r.matches.iter().any(|m| m.rule_name == "gitlab_pat"));
    }

    #[test]
    fn detect_openrouter_api_key() {
        let detector = LeakDetector::with_default_rules();
        // Real OpenRouter v1 keys have exactly 64 hex chars after `sk-or-v1-`.
        let r = detector.scan_text(
            "OPENROUTER=sk-or-v1-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef end",
        );
        assert!(
            r.matches
                .iter()
                .any(|m| m.rule_name == "openrouter_api_key")
        );
    }

    #[test]
    fn detect_anthropic_oauth_token() {
        let detector = LeakDetector::with_default_rules();
        let r = detector
            .scan_text("OAUTH sk-ant-oat01-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789_-AbCdEfGhIjKlMn");
        assert!(
            r.matches
                .iter()
                .any(|m| m.rule_name == "anthropic_oauth_token")
        );
    }

    #[test]
    fn detect_groq_api_key() {
        let detector = LeakDetector::with_default_rules();
        // Real Groq keys are `gsk_` + ~52 alphanumerics.
        let r =
            detector.scan_text("GROQ=gsk_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789AbCdEfGhIjKlMn end");
        assert!(r.matches.iter().any(|m| m.rule_name == "groq_api_key"));
    }

    #[test]
    fn detect_twilio_api_key() {
        let detector = LeakDetector::with_default_rules();
        let r = detector.scan_text("key: SK0123456789abcdef0123456789abcdef");
        assert!(r.matches.iter().any(|m| m.rule_name == "twilio_api_key"));
    }

    #[test]
    fn detect_telegram_bot_token() {
        let detector = LeakDetector::with_default_rules();
        let r = detector.scan_text("TOKEN=123456789:AAEhBP9aAbCdEfGhIjKlMnOpQrStUvWxYz01234");
        assert!(
            r.matches
                .iter()
                .any(|m| m.rule_name == "telegram_bot_token")
        );
    }

    #[test]
    fn openai_api_key_legacy_48_char_form() {
        let detector = LeakDetector::with_default_rules();
        let r =
            detector.scan_text("OPENAI=sk-AbCdEfGhIjKlMnOpQrStUvWxYzAbCdEfGhIjKlMnOpQrStUv end");
        assert!(r.matches.iter().any(|m| m.rule_name == "openai_api_key"));
    }

    #[test]
    fn openai_api_key_modern_t3blbkfj_form() {
        let detector = LeakDetector::with_default_rules();
        let r = detector.scan_text("sk-proj-AbCd1234EfGh5678T3BlbkFJ_9876-zyxwvutsrqponml end");
        assert!(r.matches.iter().any(|m| m.rule_name == "openai_api_key"));
    }

    #[test]
    fn openai_api_key_rejects_loose_short_form() {
        let detector = LeakDetector::with_default_rules();
        // 26 alphanum after `sk-`, no `T3BlbkFJ` marker and not 48 chars.
        let r = detector.scan_text("sk-foobar12345678901234567890 end");
        assert!(!r.matches.iter().any(|m| m.rule_name == "openai_api_key"));
    }

    #[test]
    fn password_assignment_requires_matched_quotes() {
        let detector = LeakDetector::with_default_rules();
        let ok = detector.scan_text(r#"password="supersecretvalue""#);
        assert!(
            ok.matches
                .iter()
                .any(|m| m.rule_name == "password_assignment")
        );
        let bad = detector.scan_text(r#"password="supersecretvalue'"#);
        assert!(
            !bad.matches
                .iter()
                .any(|m| m.rule_name == "password_assignment")
        );
    }

    #[test]
    fn detect_nearai_session() {
        let detector = LeakDetector::with_default_rules();
        let r = detector.scan_text("cookie sess_AbCdEfGhIjKlMnOpQrStUvWxYz01234567");
        assert!(r.matches.iter().any(|m| m.rule_name == "nearai_session"));
    }

    #[test]
    fn detect_auth_header() {
        let detector = LeakDetector::with_default_rules();
        let r = detector.scan_text("Authorization: Token abcdef0123456789abcdef0123456789");
        assert!(r.matches.iter().any(|m| m.rule_name == "auth_header"));
    }

    #[test]
    fn detect_high_entropy_hex() {
        let detector = LeakDetector::with_default_rules();
        let r = detector.scan_text(
            "hash: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef end",
        );
        assert!(r.matches.iter().any(|m| m.rule_name == "high_entropy_hex"));
    }
}
