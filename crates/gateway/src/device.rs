//! A's long-term static Noise identity for device pairing + E2E sessions.
//!
//! The gateway (A) holds one X25519 static keypair, persisted in the
//! [`SecretVault`] so it is stable across restarts — a paired device caches
//! A's public half and authenticates every later Noise session against it, so
//! the key must not rotate underneath an already-paired device. The public
//! half is advertised inside the SPAKE2 K-channel at pairing; the secret half
//! never leaves the vault.

use aura_device_proto::noise::StaticKeypair;
use aura_security::SecretVault;

/// `SecretVault` key holding A's static Noise keypair as `public ‖ secret`.
const NOISE_STATIC_VAULT_KEY: &str = "device.noise_static";

/// Persisted layout: 32-byte public key followed by 32-byte secret key.
const KEYPAIR_BYTES: usize = 64;

/// Load A's persisted static Noise keypair, generating and storing one on
/// first run. A malformed stored value is logged and regenerated (which would
/// force already-paired devices to re-pair — but a corrupt key can't serve
/// them anyway).
pub async fn load_or_create_static_keypair(vault: &SecretVault) -> anyhow::Result<StaticKeypair> {
    if let Some(secret) = vault
        .get_secret(NOISE_STATIC_VAULT_KEY)
        .await
        .map_err(|e| anyhow::anyhow!("vault get {NOISE_STATIC_VAULT_KEY}: {e}"))?
    {
        let bytes = secret.as_bytes();
        if bytes.len() == KEYPAIR_BYTES {
            let mut public = [0u8; 32];
            let mut sk = [0u8; 32];
            public.copy_from_slice(&bytes[..32]);
            sk.copy_from_slice(&bytes[32..]);
            return Ok(StaticKeypair::from_parts(public, sk));
        }
        tracing::warn!(
            len = bytes.len(),
            "{NOISE_STATIC_VAULT_KEY} is malformed; regenerating A's static \
             Noise key (already-paired devices will need to re-pair)",
        );
    }

    let kp = StaticKeypair::generate().map_err(|e| anyhow::anyhow!("generate static key: {e}"))?;
    let mut bytes = Vec::with_capacity(KEYPAIR_BYTES);
    bytes.extend_from_slice(&kp.public());
    bytes.extend_from_slice(&kp.secret());
    vault
        .store_secret(NOISE_STATIC_VAULT_KEY, &bytes)
        .await
        .map_err(|e| anyhow::anyhow!("vault store {NOISE_STATIC_VAULT_KEY}: {e}"))?;
    Ok(kp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_security::EncryptionKey;
    use aura_security::test_support::MemorySecretStore;
    use std::sync::Arc;

    fn vault() -> SecretVault {
        let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec())
            .expect("test encryption key");
        SecretVault::new(key, Arc::new(MemorySecretStore::new()))
    }

    #[tokio::test]
    async fn generates_then_reloads_the_same_key() {
        let v = vault();
        let first = load_or_create_static_keypair(&v).await.unwrap();
        let second = load_or_create_static_keypair(&v).await.unwrap();
        assert_eq!(
            first.public(),
            second.public(),
            "second call must return the persisted key, not a fresh one",
        );
        assert_eq!(first.secret(), second.secret());
    }

    #[tokio::test]
    async fn distinct_vaults_yield_distinct_keys() {
        let a = load_or_create_static_keypair(&vault()).await.unwrap();
        let b = load_or_create_static_keypair(&vault()).await.unwrap();
        assert_ne!(a.public(), b.public());
    }
}
