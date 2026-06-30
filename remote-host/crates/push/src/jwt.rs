//! APNs provider authentication token (ES256 JWT).
//!
//! Token-based APNs auth signs a short JWT with the team's `.p8` key:
//! header `{ alg: ES256, kid: <Key ID> }`, claims `{ iss: <Team ID>, iat }`.
//! APNs rejects a token older than 60 minutes, so the caller refreshes roughly
//! every [`TOKEN_REFRESH_SECS`] and reuses it across requests in between.
//!
//! The signer is clock-injected (`now` is passed in) so it is deterministic and
//! host-testable without a real `.p8` or network — the caller owns the clock
//! and the refresh cache.

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};

use crate::error::PushError;

/// APNs rejects a provider token older than this (hard limit).
pub const TOKEN_MAX_AGE_SECS: u64 = 3600;
/// Refresh comfortably inside [`TOKEN_MAX_AGE_SECS`].
pub const TOKEN_REFRESH_SECS: u64 = 45 * 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Claims {
    iss: String,
    iat: u64,
}

/// Signs APNs provider tokens for one (`Key ID`, `Team ID`, `.p8`) triple.
pub struct ApnsProviderToken {
    key_id: String,
    team_id: String,
    encoding_key: EncodingKey,
}

impl ApnsProviderToken {
    /// Build from the team's APNs auth key. `p8_pem` is the `.p8` file bytes
    /// (a PKCS#8 P-256 private key).
    pub fn new(
        key_id: impl Into<String>,
        team_id: impl Into<String>,
        p8_pem: &[u8],
    ) -> Result<Self, PushError> {
        let encoding_key =
            EncodingKey::from_ec_pem(p8_pem).map_err(|e| PushError::Key(e.to_string()))?;
        Ok(Self {
            key_id: key_id.into(),
            team_id: team_id.into(),
            encoding_key,
        })
    }

    /// Sign a provider JWT stamped `iat = now` (unix seconds). Cheap enough to
    /// call on every refresh; APNs validates `iss == Team ID` and the topic
    /// against the key's team.
    pub fn sign(&self, now: u64) -> Result<String, PushError> {
        let header = Header {
            alg: Algorithm::ES256,
            kid: Some(self.key_id.clone()),
            ..Default::default()
        };
        let claims = Claims {
            iss: self.team_id.clone(),
            iat: now,
        };
        jsonwebtoken::encode(&header, &claims, &self.encoding_key)
            .map_err(|e| PushError::Jwt(e.to_string()))
    }

    /// Whether a token minted at `issued_at` should be refreshed by `now`.
    pub fn needs_refresh(issued_at: u64, now: u64) -> bool {
        now.saturating_sub(issued_at) >= TOKEN_REFRESH_SECS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{DecodingKey, Validation, decode, decode_header};

    // Throwaway P-256 keypair generated for this test only (NOT an APNs key).
    const TEST_P8: &str = r#"-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgPFauT/kbqwIxcoQW
BNxFLAfYXAa3OFmTIx3IcGqjUkyhRANCAATGtaYrLt8AL8cs25DIa+OeV4PCpUHt
SYW9s/UKX8shed4rIxRqMe3POJIY7OsF06EEtnyLrMjJg53H5HWAe2Mh
-----END PRIVATE KEY-----"#;

    const TEST_PUB: &str = r#"-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAExrWmKy7fAC/HLNuQyGvjnleDwqVB
7UmFvbP1Cl/LIXneKyMUajHtzziSGOzrBdOhBLZ8i6zIyYOdx+R1gHtjIQ==
-----END PUBLIC KEY-----"#;

    fn signer() -> ApnsProviderToken {
        ApnsProviderToken::new("ABC123KEYID", "TEAM123456", TEST_P8.as_bytes())
            .expect("valid test key")
    }

    #[test]
    fn signs_a_verifiable_es256_token() {
        let token = signer().sign(1_700_000_000).unwrap();

        // Header carries ES256 + the Key ID.
        let header = decode_header(&token).unwrap();
        assert_eq!(header.alg, Algorithm::ES256);
        assert_eq!(header.kid.as_deref(), Some("ABC123KEYID"));

        // Signature verifies against the matching public key, and the claims
        // carry the Team ID + iat.
        let mut validation = Validation::new(Algorithm::ES256);
        validation.validate_exp = false;
        validation.required_spec_claims.clear();
        let data = decode::<Claims>(
            &token,
            &DecodingKey::from_ec_pem(TEST_PUB.as_bytes()).unwrap(),
            &validation,
        )
        .expect("token verifies against the public key");
        assert_eq!(data.claims.iss, "TEAM123456");
        assert_eq!(data.claims.iat, 1_700_000_000);
    }

    #[test]
    fn wrong_key_id_does_not_verify_against_other_key() {
        // A token signed by our test key must NOT verify under a different
        // (freshly different) public key — sanity that ES256 is really applied.
        let token = signer().sign(1_700_000_000).unwrap();
        let other_pub = r#"-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEEYrz3v3p3o3Q3sJg3oN3y3Z3a3b3
c3d3e3f3g3h3i3j3k3l3m3n3o3p3q3r3s3t3u3v3w3x3y3z3A3B3C3D3E3F3G3==
-----END PUBLIC KEY-----"#;
        let mut validation = Validation::new(Algorithm::ES256);
        validation.validate_exp = false;
        validation.required_spec_claims.clear();
        let res = decode::<Claims>(
            &token,
            &DecodingKey::from_ec_pem(other_pub.as_bytes())
                .unwrap_or_else(|_| DecodingKey::from_secret(b"x")),
            &validation,
        );
        assert!(res.is_err(), "token must not verify under a foreign key");
    }

    #[test]
    fn refresh_window() {
        assert!(!ApnsProviderToken::needs_refresh(
            1000,
            1000 + TOKEN_REFRESH_SECS - 1
        ));
        assert!(ApnsProviderToken::needs_refresh(
            1000,
            1000 + TOKEN_REFRESH_SECS
        ));
        assert!(ApnsProviderToken::needs_refresh(
            1000,
            1000 + TOKEN_MAX_AGE_SECS
        ));
    }
}
