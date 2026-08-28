//! Provider-neutral push HTTP wire surface.

use serde::{Deserialize, Serialize};

/// Route paths.
pub const NOTIFY: &str = "/notify";
pub const REGISTER: &str = "/register";

/// Cross-provider bound for one platform-issued token on every ingress.
pub const PUSH_TOKEN_MAX_LEN: usize = 4096;

/// Ed25519 domain-separation contexts prefixing each signed message in the
/// push-binding chain. This crate also owns the canonical byte layout below so
/// the gateway signer and remote-host verifier cannot drift.
pub const DELEGATION_CONTEXT: &[u8] = b"baybo/push/delegation/v1";
pub const REGISTER_CONTEXT: &[u8] = b"baybo/push/register/v1";
pub const NOTIFY_CONTEXT: &[u8] = b"baybo/push/notify/v1";

const PROVIDER_APNS: u8 = 0;
const PROVIDER_FCM: u8 = 1;
const APNS_ENV_SANDBOX: u8 = 0;
const APNS_ENV_PRODUCTION: u8 = 1;

/// A configured delivery provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushProvider {
    Apns,
    Fcm,
}

impl PushProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Apns => "apns",
            Self::Fcm => "fcm",
        }
    }
}

/// Which APNs environment issued a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApnsEnvironment {
    Sandbox,
    Production,
}

/// A provider-tagged device token. The enum keeps provider-specific metadata
/// strongly typed: an APNs target always has an environment, while an FCM
/// target cannot accidentally carry one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum PushTarget {
    Apns {
        token: String,
        environment: ApnsEnvironment,
    },
    Fcm {
        token: String,
    },
}

impl PushTarget {
    pub fn provider(&self) -> PushProvider {
        match self {
            Self::Apns { .. } => PushProvider::Apns,
            Self::Fcm { .. } => PushProvider::Fcm,
        }
    }

    pub fn token(&self) -> &str {
        match self {
            Self::Apns { token, .. } | Self::Fcm { token } => token,
        }
    }
}

/// JSON body of `POST /notify`. The encrypted preview is provider-independent;
/// the remote host selects the provider from the stored [`PushTarget`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotifyRequest {
    pub device_id: String,
    pub collapse_key: String,
    pub bid: String,
    pub enc: String,
    pub n: String,
    /// Base64 Ed25519 signature by the delegated gateway push key.
    pub sig: String,
    /// Strictly-increasing per-device replay counter.
    pub counter: u64,
}

impl NotifyRequest {
    pub fn signing_input(&self) -> NotifySigningInput<'_> {
        NotifySigningInput {
            device_id: &self.device_id,
            collapse_key: &self.collapse_key,
            enc: &self.enc,
            n: &self.n,
            bid: &self.bid,
            counter: self.counter,
        }
    }
}

/// Borrowed fields covered by a `/notify` signature.
#[derive(Debug, Clone, Copy)]
pub struct NotifySigningInput<'a> {
    pub device_id: &'a str,
    pub collapse_key: &'a str,
    pub enc: &'a str,
    pub n: &'a str,
    pub bid: &'a str,
    pub counter: u64,
}

/// JSON body of `POST /register` — bind or rebind a provider-tagged token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub device_id: String,
    pub target: PushTarget,
    /// Base64 gateway Ed25519 push public key authorized by `delegation`.
    pub gateway_pubkey: String,
    /// Base64 device-to-gateway delegation signature.
    pub delegation: String,
    /// Base64 gateway signature over this registration.
    pub sig: String,
    /// Strictly-increasing per-device replay counter.
    pub counter: u64,
}

fn push_field(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(bytes);
}

/// Canonical bytes signed by a device when delegating to a gateway push key.
pub fn delegation_signing_message(gateway_pubkey: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(DELEGATION_CONTEXT.len() + gateway_pubkey.len());
    message.extend_from_slice(DELEGATION_CONTEXT);
    message.extend_from_slice(gateway_pubkey);
    message
}

/// Canonical bytes signed by a gateway for `POST /register`.
pub fn register_signing_message(device_id: &str, target: &PushTarget, counter: u64) -> Vec<u8> {
    let mut message = Vec::new();
    message.extend_from_slice(REGISTER_CONTEXT);
    push_field(&mut message, device_id.as_bytes());
    match target {
        PushTarget::Apns { token, environment } => {
            message.push(PROVIDER_APNS);
            push_field(&mut message, token.as_bytes());
            message.push(match environment {
                ApnsEnvironment::Sandbox => APNS_ENV_SANDBOX,
                ApnsEnvironment::Production => APNS_ENV_PRODUCTION,
            });
        }
        PushTarget::Fcm { token } => {
            message.push(PROVIDER_FCM);
            push_field(&mut message, token.as_bytes());
        }
    }
    message.extend_from_slice(&counter.to_le_bytes());
    message
}

/// Canonical bytes signed by a gateway for `POST /notify`.
pub fn notify_signing_message(input: &NotifySigningInput<'_>) -> Vec<u8> {
    let mut message = Vec::new();
    message.extend_from_slice(NOTIFY_CONTEXT);
    push_field(&mut message, input.device_id.as_bytes());
    push_field(&mut message, input.collapse_key.as_bytes());
    push_field(&mut message, input.enc.as_bytes());
    push_field(&mut message, input.n.as_bytes());
    push_field(&mut message, input.bid.as_bytes());
    message.extend_from_slice(&input.counter.to_le_bytes());
    message
}

/// `{base}/notify`.
pub fn notify_url(base: &str) -> String {
    crate::join(base, NOTIFY)
}

/// `{base}/register`.
pub fn register_url(base: &str) -> String {
    crate::join(base, REGISTER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_round_trip_with_provider_specific_metadata() {
        for target in [
            PushTarget::Apns {
                token: "apns-token".into(),
                environment: ApnsEnvironment::Production,
            },
            PushTarget::Fcm {
                token: "fcm-token".into(),
            },
        ] {
            let json = serde_json::to_vec(&target).unwrap();
            assert_eq!(serde_json::from_slice::<PushTarget>(&json).unwrap(), target);
        }
    }

    #[test]
    fn registration_signature_bytes_bind_the_provider() {
        let apns = PushTarget::Apns {
            token: "same-token".into(),
            environment: ApnsEnvironment::Sandbox,
        };
        let fcm = PushTarget::Fcm {
            token: "same-token".into(),
        };
        assert_ne!(
            register_signing_message("device-a", &apns, 1),
            register_signing_message("device-a", &fcm, 1)
        );
    }
}
