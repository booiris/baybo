//! OAuth token bundle: the on-disk shape of a ChatGPT/Codex sign-in.
//! Stored as a single typed entry in [`super::VAULT_KEY_TOKENS`].

use base64::Engine;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::LlmError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthTokenBundle {
    /// JWT bearer sent as `Authorization: Bearer ...` to the Codex Responses
    /// API. Refreshed via the OAuth `refresh_token` grant when nearly expired
    /// or when the API rejects it with 401.
    pub access_token: String,
    /// Opaque token used to mint a new `access_token`. Server may rotate it
    /// on every refresh — callers MUST overwrite with whatever the refresh
    /// response carries.
    pub refresh_token: String,
    /// JWT carrying the user's identity claims (email, plan_type,
    /// chatgpt_account_id). Re-parsed only for `aura llm status` display;
    /// the flat `account_id` field below is the hot-path copy.
    pub id_token: String,
    /// `chatgpt_account_id` claim from `id_token`, sent as the
    /// `ChatGPT-Account-Id` header. `None` for personal-plan accounts that
    /// don't carry the claim.
    pub account_id: Option<String>,
    /// `exp` claim from `access_token`, Unix seconds. Cached so the refresh
    /// pre-flight doesn't decode the JWT on every request.
    pub expires_at: i64,
    /// Wall-clock time the bundle was minted. Diagnostic only.
    pub obtained_at: i64,
}

impl OAuthTokenBundle {
    /// Build a fresh bundle from a token endpoint response. Parses the JWTs
    /// to populate the cached `account_id` and `expires_at` fields. Failure
    /// here is a "definitely-broken upstream" condition — the refresh / login
    /// path treats it as fatal.
    pub fn from_token_response(
        access_token: String,
        refresh_token: String,
        id_token: String,
    ) -> crate::Result<Self> {
        let claims = parse_jwt_payload(&id_token).map_err(|e| {
            LlmError::Decode(format!(
                "openai-subscription: id_token did not parse as a JWT: {e}"
            ))
        })?;
        let account_id = claims
            .get("https://api.openai.com/auth")
            .and_then(|v| v.get("chatgpt_account_id"))
            .and_then(|v| v.as_str())
            .map(str::to_owned);

        let access_claims = parse_jwt_payload(&access_token).map_err(|e| {
            LlmError::Decode(format!(
                "openai-subscription: access_token did not parse as a JWT: {e}"
            ))
        })?;
        let expires_at = access_claims
            .get("exp")
            .and_then(|v| v.as_i64())
            // Server tokens always carry exp; treat absence as immediately
            // expired so the next call refreshes rather than silently ages.
            .unwrap_or(0);

        Ok(Self {
            access_token,
            refresh_token,
            id_token,
            account_id,
            expires_at,
            obtained_at: Utc::now().timestamp(),
        })
    }

    /// True if `access_token` is past `expires_at - skew_secs`. The skew lets
    /// us refresh proactively rather than racing the server's clock tolerance
    /// on a request that's "still valid for 4 seconds".
    pub fn is_near_expiry(&self, skew_secs: i64) -> bool {
        let now = Utc::now().timestamp();
        now >= self.expires_at.saturating_sub(skew_secs)
    }

    /// Email claim from `id_token`, for `aura llm status` display only.
    pub fn email(&self) -> Option<String> {
        let claims = parse_jwt_payload(&self.id_token).ok()?;
        let direct = claims.get("email").and_then(|v| v.as_str());
        if let Some(email) = direct {
            return Some(email.to_owned());
        }
        claims
            .get("https://api.openai.com/profile")
            .and_then(|v| v.get("email"))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }

    /// `chatgpt_plan_type` claim, e.g. `"plus"`, `"pro"`, `"business"`.
    pub fn plan_type(&self) -> Option<String> {
        let claims = parse_jwt_payload(&self.id_token).ok()?;
        claims
            .get("https://api.openai.com/auth")
            .and_then(|v| v.get("chatgpt_plan_type"))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }
}

/// Decode and parse the payload (middle segment) of a JWT. Signature verification
/// is intentionally skipped: the JWT is something we *minted via OAuth in this
/// process*, not something we received from a third party we don't trust.
/// Tampered tokens get rejected by the upstream API on the next call anyway.
fn parse_jwt_payload(jwt: &str) -> Result<Value, JwtParseError> {
    let mut parts = jwt.split('.');
    let (_header, payload, _signature) = match (parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s)) if !h.is_empty() && !p.is_empty() && !s.is_empty() => (h, p, s),
        _ => return Err(JwtParseError::InvalidFormat),
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(JwtParseError::Base64)?;
    serde_json::from_slice(&bytes).map_err(JwtParseError::Json)
}

#[derive(Debug, thiserror::Error)]
pub enum JwtParseError {
    #[error("not a JWT (expected three dot-separated segments)")]
    InvalidFormat,
    #[error(transparent)]
    Base64(#[from] base64::DecodeError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;

    fn fake_jwt(payload: &serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let payload_b64 = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(b"fake");
        format!("{header}.{payload_b64}.{signature}")
    }

    #[test]
    fn from_token_response_parses_account_id_and_exp() {
        let id_token = fake_jwt(&json!({
            "email": "user@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acc-123",
                "chatgpt_plan_type": "pro"
            }
        }));
        let access_token = fake_jwt(&json!({ "exp": 9_999_999_999i64 }));
        let bundle = OAuthTokenBundle::from_token_response(
            access_token,
            "refresh-tok".to_string(),
            id_token,
        )
        .unwrap();
        assert_eq!(bundle.account_id.as_deref(), Some("acc-123"));
        assert_eq!(bundle.expires_at, 9_999_999_999);
        assert_eq!(bundle.email().as_deref(), Some("user@example.com"));
        assert_eq!(bundle.plan_type().as_deref(), Some("pro"));
    }

    #[test]
    fn from_token_response_handles_missing_account_id_claim() {
        // Personal accounts lack `chatgpt_account_id`. Should yield None,
        // not an error — the API accepts requests without the header.
        let id_token = fake_jwt(&json!({
            "email": "solo@example.com",
            "https://api.openai.com/auth": { "chatgpt_plan_type": "plus" }
        }));
        let access_token = fake_jwt(&json!({ "exp": 9_999_999_999i64 }));
        let bundle =
            OAuthTokenBundle::from_token_response(access_token, "rt".to_string(), id_token)
                .unwrap();
        assert!(bundle.account_id.is_none());
        assert_eq!(bundle.plan_type().as_deref(), Some("plus"));
    }

    #[test]
    fn from_token_response_treats_missing_exp_as_immediate_expiry() {
        let id_token = fake_jwt(&json!({}));
        let access_token = fake_jwt(&json!({})); // no exp
        let bundle =
            OAuthTokenBundle::from_token_response(access_token, "rt".to_string(), id_token)
                .unwrap();
        assert_eq!(bundle.expires_at, 0);
        assert!(bundle.is_near_expiry(60));
    }

    #[test]
    fn is_near_expiry_respects_skew() {
        let now = Utc::now().timestamp();
        let bundle = OAuthTokenBundle {
            access_token: "a".into(),
            refresh_token: "r".into(),
            id_token: "i".into(),
            account_id: None,
            expires_at: now + 120, // 2 minutes from now
            obtained_at: now,
        };
        assert!(!bundle.is_near_expiry(60), "120s out > 60s skew → not near");
        assert!(bundle.is_near_expiry(180), "120s out < 180s skew → near");
    }

    #[test]
    fn malformed_jwt_in_id_token_errors_loudly() {
        let bundle_result = OAuthTokenBundle::from_token_response(
            fake_jwt(&json!({ "exp": 0 })),
            "rt".to_string(),
            "not.a.jwt".to_string(),
        );
        // The middle segment "a" decodes as base64 but yields invalid JSON,
        // so the constructor must surface that as a Provider error.
        assert!(bundle_result.is_err());
    }
}
