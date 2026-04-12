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
    /// Replace the matched fragment with a placeholder.
    Replace,
}

/// A single leak detection rule comprising a name, regex pattern, and action.
pub struct LeakDetectionRule {
    pub name: String,
    pub pattern: Regex,
    pub action: LeakAction,
}

/// Result of scanning a single content block.
pub struct LeakScanResult {
    /// Replacement mappings: placeholder -> matched secret text.
    pub replacements: Vec<LeakReplacement>,
    /// Whether any rule with `LeakAction::Block` matched.
    pub blocked: bool,
    /// The block reason if blocked.
    pub block_reason: Option<String>,
}

/// A single replacement entry.
pub struct LeakReplacement {
    pub placeholder: String,
    pub original: String,
    pub rule_name: String,
}

/// Scans content blocks for sensitive data patterns (API keys, tokens, etc.)
/// and replaces or blocks them according to configured rules.
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
    /// replacements and/or block signals.
    pub fn scan_text(&self, text: &str) -> LeakScanResult {
        let mut replacements = Vec::new();
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
                        let suffix = generate_placeholder_suffix();
                        let placeholder = format!("{{{{SECRET_{suffix}}}}}");
                        replacements.push(LeakReplacement {
                            placeholder,
                            original: mat.as_str().to_owned(),
                            rule_name: rule.name.clone(),
                        });
                    }
                }
            }
        }

        LeakScanResult {
            replacements,
            blocked,
            block_reason,
        }
    }

    /// Scan a file on disk. Returns the scan result for the file's contents.
    ///
    /// I/O errors (missing file, permissions) are returned via `std::io::Error`;
    /// non-UTF-8 content is handled with a lossy conversion so binary blobs
    /// can still be surfaced if the operator asks for them.
    pub fn check_file(&self, path: &Path) -> std::io::Result<LeakScanResult> {
        let bytes = std::fs::read(path)?;
        let text = match std::str::from_utf8(&bytes) {
            Ok(s) => s.to_string(),
            Err(_) => String::from_utf8_lossy(&bytes).into_owned(),
        };
        Ok(self.scan_text(&text))
    }

    /// Public read-only view of this detector's rules.
    ///
    /// Exposed for auditing — `SecurityGateway::audit` and `aura security audit`
    /// need to describe which rules are active without touching the regex internals.
    pub fn rules(&self) -> &[LeakDetectionRule] {
        &self.rules
    }

    /// Scan a slice of `ContentBlock`s. Returns the scan result and, if not
    /// blocked, a new set of content blocks with replacements applied.
    pub fn scan_content_blocks(
        &self,
        blocks: &[ContentBlock],
    ) -> Result<(LeakScanResult, Vec<ContentBlock>)> {
        let mut all_replacements = Vec::new();
        let mut blocked = false;
        let mut block_reason = None;
        let mut new_blocks = Vec::with_capacity(blocks.len());

        for block in blocks {
            match block {
                ContentBlock::Text(text) => {
                    let result = self.scan_text(text);
                    if result.blocked {
                        blocked = true;
                        block_reason = result.block_reason;
                    }

                    // Apply replacements to produce sanitized text.
                    let mut sanitized = text.clone();
                    for replacement in &result.replacements {
                        sanitized =
                            sanitized.replace(&replacement.original, &replacement.placeholder);
                    }
                    all_replacements.extend(result.replacements);
                    new_blocks.push(ContentBlock::Text(sanitized));
                }
                // Non-text blocks pass through unchanged.
                other => new_blocks.push(other.clone()),
            }
        }

        let combined = LeakScanResult {
            replacements: all_replacements,
            blocked,
            block_reason,
        };

        Ok((combined, new_blocks))
    }

    /// Load a standard set of rules for common secret patterns.
    fn add_default_rules(&mut self) {
        let patterns: &[(&str, &str)] = &[
            // Generic long hex/base64 API keys (32+ chars)
            (
                "generic_api_key",
                r#"(?i)(?:api[_-]?key|apikey|api[_-]?secret)\s*[=:]\s*['"]?([A-Za-z0-9\-_]{32,})['"]?"#,
            ),
            // AWS access key IDs
            ("aws_access_key", r"AKIA[0-9A-Z]{16}"),
            // AWS secret access keys (40 chars base64-ish)
            (
                "aws_secret_key",
                r#"(?i)aws[_-]?secret[_-]?access[_-]?key\s*[=:]\s*['"]?([A-Za-z0-9/+=]{40})['"]?"#,
            ),
            // GitHub personal access tokens
            ("github_token", r"ghp_[A-Za-z0-9]{36}"),
            // GitHub fine-grained tokens
            ("github_fine_grained_token", r"github_pat_[A-Za-z0-9_]{22,}"),
            // Slack tokens
            ("slack_token", r"xox[bpras]-[A-Za-z0-9\-]{10,}"),
            // Bearer tokens in headers
            ("bearer_token", r"(?i)bearer\s+[A-Za-z0-9\-_.]{20,}"),
            // Generic password assignments
            (
                "password_assignment",
                r#"(?i)(?:password|passwd|pwd)\s*[=:]\s*['"][^\s'"]{8,}['"]"#,
            ),
        ];

        for (name, pattern) in patterns {
            // Invalid regex in default rules is a programming error,
            // so we silently skip if it somehow fails to compile.
            if let Ok(re) = Regex::new(pattern) {
                self.add_rule(LeakDetectionRule {
                    name: (*name).to_owned(),
                    pattern: re,
                    action: LeakAction::Replace,
                });
            }
        }
    }
}

impl Default for LeakDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate a short random suffix for placeholders using UUID v4.
fn generate_placeholder_suffix() -> String {
    let id = uuid::Uuid::new_v4();
    // Take the first 8 hex chars for a compact but low-collision suffix.
    id.simple().to_string()[..8].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_aws_access_key() {
        let detector = LeakDetector::with_default_rules();
        let text = "My key is AKIAIOSFODNN7EXAMPLE ok";
        let result = detector.scan_text(text);
        assert!(!result.blocked);
        assert_eq!(result.replacements.len(), 1);
        assert!(
            result.replacements[0]
                .original
                .contains("AKIAIOSFODNN7EXAMPLE")
        );
        assert!(result.replacements[0].placeholder.starts_with("{{SECRET_"));
    }

    #[test]
    fn detect_github_token() {
        let detector = LeakDetector::with_default_rules();
        let text = "token: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij";
        let result = detector.scan_text(text);
        assert_eq!(result.replacements.len(), 1);
        assert!(result.replacements[0].original.starts_with("ghp_"));
    }

    #[test]
    fn detect_slack_token() {
        let detector = LeakDetector::with_default_rules();
        let text = "Use xoxb-1234567890-abcdefghij to connect";
        let result = detector.scan_text(text);
        assert_eq!(result.replacements.len(), 1);
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
    fn scan_content_blocks_replaces_text() {
        let detector = LeakDetector::with_default_rules();
        let blocks = vec![
            ContentBlock::Text("Key: AKIAIOSFODNN7EXAMPLE".into()),
            ContentBlock::Text("safe text".into()),
        ];
        let (result, new_blocks) = detector.scan_content_blocks(&blocks).unwrap();
        assert_eq!(result.replacements.len(), 1);

        if let ContentBlock::Text(ref s) = new_blocks[0] {
            assert!(!s.contains("AKIAIOSFODNN7EXAMPLE"));
            assert!(s.contains("{{SECRET_"));
        } else {
            panic!("expected text block");
        }

        // Second block unchanged.
        if let ContentBlock::Text(ref s) = new_blocks[1] {
            assert_eq!(s, "safe text");
        }
    }

    #[test]
    fn no_false_positives_on_normal_text() {
        let detector = LeakDetector::with_default_rules();
        let result = detector.scan_text("Hello, how can I help you today?");
        assert!(result.replacements.is_empty());
        assert!(!result.blocked);
    }
}
