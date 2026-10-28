//! OAuth token bundle: the on-disk shape of a Sign in with ChatGPT connection.
//! Stored as a single typed entry in [`super::VAULT_KEY_TOKENS`].

use base64::Engine;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::LlmError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthTokenBundle {
    pub connection: ChatGptConnection,
    /// Bearer sent as `Authorization: Bearer ...` to the Responses API. May be
    /// opaque. Refreshed via the OAuth `refresh_token` grant when nearly
    /// expired or when the API rejects it with 401.
    pub access_token: String,
    /// Opaque token used to mint a new `access_token`. Server may rotate it
    /// on every refresh — callers MUST overwrite with whatever the refresh
    /// response carries.
    pub refresh_token: String,
    /// Verified ID token, re-parsed only for `baybo llm status` display.
    pub id_token: String,
    /// Unix seconds, computed from the token endpoint's `expires_in`.
    pub expires_at: i64,
    /// Wall-clock time the bundle was minted.
    pub obtained_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatGptConnection {
    pub client_id: String,
    pub subject: String,
    pub scopes: Vec<String>,
}

impl OAuthTokenBundle {
    pub fn require_sharing(&self) -> crate::Result<()> {
        if self.sharing_enabled() {
            Ok(())
        } else {
            Err(LlmError::Auth(format!(
                "ChatGPT identity connected, but plan usage is disabled. Re-authenticate to enable it, or use an API-key provider. Manage usage: {}",
                super::MANAGE_USAGE_URL,
            )))
        }
    }

    pub fn sharing_enabled(&self) -> bool {
        self.connection
            .scopes
            .iter()
            .any(|scope| scope == super::oauth::SHARING_SCOPE)
    }

    /// True if `access_token` is past `expires_at - skew_secs`. The skew lets
    /// us refresh proactively rather than racing the server's clock tolerance
    /// on a request that's "still valid for 4 seconds". An identity-only
    /// connection carries no inference token, so it never needs refreshing.
    pub fn is_near_expiry(&self, skew_secs: i64) -> bool {
        if !self.sharing_enabled() {
            return false;
        }
        let now = Utc::now().timestamp();
        now >= self.expires_at.saturating_sub(skew_secs)
    }

    /// Email claim from `id_token`, for `baybo llm status` display only.
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

/// Decode display metadata only; the ID token was verified before storage.
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
pub(super) fn sharing_connection() -> ChatGptConnection {
    ChatGptConnection {
        client_id: "oaiapp_test".into(),
        subject: "user".into(),
        scopes: vec![super::oauth::SHARING_SCOPE.into()],
    }
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

    fn bundle(id_token: String, expires_at: i64) -> OAuthTokenBundle {
        OAuthTokenBundle {
            connection: sharing_connection(),
            access_token: "a".into(),
            refresh_token: "r".into(),
            id_token,
            expires_at,
            obtained_at: Utc::now().timestamp(),
        }
    }

    #[test]
    fn display_claims_come_from_the_id_token() {
        let id_token = fake_jwt(&json!({
            "email": "user@example.com",
            "https://api.openai.com/auth": { "chatgpt_plan_type": "pro" }
        }));
        let bundle = bundle(id_token, 0);
        assert_eq!(bundle.email().as_deref(), Some("user@example.com"));
        assert_eq!(bundle.plan_type().as_deref(), Some("pro"));
        let opaque = OAuthTokenBundle {
            id_token: "not.a.jwt".into(),
            ..bundle
        };
        assert!(opaque.email().is_none());
        assert!(opaque.plan_type().is_none());
    }

    #[test]
    fn is_near_expiry_respects_skew() {
        let now = Utc::now().timestamp();
        let bundle = bundle("i".into(), now + 120);
        assert!(!bundle.is_near_expiry(60), "120s out > 60s skew → not near");
        assert!(bundle.is_near_expiry(180), "120s out < 180s skew → near");
    }
}
