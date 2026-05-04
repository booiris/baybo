//! Security-related identifiers used by trace `SpanEvent`s and the
//! sanitization pipeline. The actual cryptographic + sanitization logic
//! lives in `aura-security`; only the **types** that other crates need
//! to reference (without depending on `aura-security`) are defined here.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Opaque identifier for a minted placeholder string.
///
/// `aura-security` mints placeholders of the form
/// `[{REDACTED_SECRET_<hex>}]`; this newtype carries the canonical text
/// form so that `SpanEvent::SanitizeHit` can reference the placeholder
/// without copying the secret bytes themselves.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlaceholderId(String);

impl PlaceholderId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl fmt::Display for PlaceholderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for PlaceholderId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// What category of secret was matched by `SecurityGateway::sanitize_*`.
///
/// Used in `SpanEvent::SanitizeHit` for audit ("what kinds of secrets did
/// this LLM response try to leak?"). Closed enum — extend by adding
/// variants. `Other` is reserved for high-entropy strings without a
/// stronger classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    ApiKey,
    BearerToken,
    AwsAccessKey,
    AwsSecretKey,
    PrivateKey,
    Password,
    HighEntropy,
    Other,
}

impl fmt::Display for SecretKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SecretKind::ApiKey => "api_key",
            SecretKind::BearerToken => "bearer_token",
            SecretKind::AwsAccessKey => "aws_access_key",
            SecretKind::AwsSecretKey => "aws_secret_key",
            SecretKind::PrivateKey => "private_key",
            SecretKind::Password => "password",
            SecretKind::HighEntropy => "high_entropy",
            SecretKind::Other => "other",
        };
        f.write_str(s)
    }
}

/// Which lifecycle hook phase fired — used by `SpanEvent::HookDegraded`
/// to record which side of the step boundary the timeout happened on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookPhase {
    PreStep,
    PostStep,
}

impl fmt::Display for HookPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            HookPhase::PreStep => "pre_step",
            HookPhase::PostStep => "post_step",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_id_round_trips() {
        let p = PlaceholderId::new("[{REDACTED_SECRET_abc123}]");
        let s = serde_json::to_string(&p).unwrap();
        let back: PlaceholderId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn secret_kind_round_trips() {
        for kind in [
            SecretKind::ApiKey,
            SecretKind::BearerToken,
            SecretKind::AwsAccessKey,
            SecretKind::AwsSecretKey,
            SecretKind::PrivateKey,
            SecretKind::Password,
            SecretKind::HighEntropy,
            SecretKind::Other,
        ] {
            let s = serde_json::to_string(&kind).unwrap();
            let back: SecretKind = serde_json::from_str(&s).unwrap();
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn hook_phase_round_trips() {
        for p in [HookPhase::PreStep, HookPhase::PostStep] {
            let s = serde_json::to_string(&p).unwrap();
            let back: HookPhase = serde_json::from_str(&s).unwrap();
            assert_eq!(back, p);
        }
    }
}
