//! Direct-mode (web-identity) push bindings.
//!
//! A scan-to-pair device gets its push material — Ed25519 identity, `push_key`,
//! delegation, APNs token, and a remote-host endpoint — bootstrapped by the Noise
//! pairing handshake and recorded on the device row. The **direct** transport has
//! no pairing, so a web client bootstraps the *same* material over the
//! authenticated admin-token REST channel (`POST /v1/push/register`) instead: it
//! mints its own Ed25519 identity (so `device_id == ios-<hex(pub)>` self-certifies
//! to C exactly like a device), a random `push_key`, and a delegation authorizing
//! the gateway push key — then the gateway persists it here.
//!
//! The binding is cryptographically **identical** to a device binding, so the
//! dispatcher's `/register` + `/notify` builders, the remote host (C), and the
//! iOS NSE are all unchanged. The only divergences are (a) the `push_key` rides
//! TLS + the admin bearer token rather than a Noise handshake hash — a weaker
//! (but still endpoint-to-endpoint) trust model, see
//! `docs/modules/mobile/relay-push-security.md`; and (b) the remote-host endpoint
//! comes from gateway config (`[push]`), not a pairing QR.
//!
//! Storage reuses the per-device secret names (`device.{id}.push_key` / `.apns` /
//! `.push_delegation`) so [`super::PushDispatcher`]'s read sites need no change; a
//! small `web_push.{id}` meta record carries the remote-host endpoint the
//! dispatcher targets, and `SecretVault::list_names` enumerates them. Push is
//! keyless, so no admission key is stored — the delegation chain authorizes it.

use baybo_security::SecretVault;
use device_proto::aead;
use device_proto::delegation;
use device_proto::pairing::ApnsEnv;
use serde::{Deserialize, Serialize};

use super::{
    DeviceApnsRegistration, device_apns_secret_name, device_push_delegation_secret_name,
    device_push_key_secret_name,
};

/// Prefix for a web-binding meta record's vault key. Disjoint from the
/// `device.{id}.*` push-material keys so [`list_bindings`] enumerates only the
/// meta records (one per web binding).
const WEB_BINDING_PREFIX: &str = "web_push.";

/// Vault key holding a web binding's meta record.
fn web_binding_secret_name(device_id: &str) -> String {
    format!("{WEB_BINDING_PREFIX}{device_id}")
}

/// A direct-mode push binding's routing meta: which remote host (C) the
/// dispatcher reaches it through. The push material (`push_key`, APNs token +
/// env, delegation) lives under the shared `device.{id}.*` secret names so the
/// dispatcher reads it the same way it reads a paired device's.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct WebPushBinding {
    /// `ios-<hex(ed25519 pub)>` — the web client's self-certifying identity.
    pub device_id: String,
    /// Remote-host base WS URL (from gateway `[push]` config). The dispatcher
    /// maps `wss→https` for the keyless `/register` + `/notify` POSTs.
    pub relay_url: String,
    /// Unix seconds the binding was registered (for observability only).
    pub created_at: i64,
}

/// Persist (or update) a verified web push binding: the routing meta plus the
/// push material under the shared `device.{id}.*` names the dispatcher reads.
/// Idempotent — re-registering the same `device_id` overwrites in place (a fresh
/// app launch mints a new `push_key`, or the APNs token rotated).
pub(crate) async fn store_binding(
    vault: &SecretVault,
    binding: &WebPushBinding,
    push_key: &[u8; aead::KEY_LEN],
    apns_token: &str,
    apns_env: ApnsEnv,
    delegation_sig: &[u8; delegation::SIGNATURE_LEN],
) -> anyhow::Result<()> {
    let id = binding.device_id.as_str();
    vault
        .store_secret(&device_push_key_secret_name(id), push_key)
        .await
        .map_err(|e| anyhow::anyhow!("store push_key: {e}"))?;
    vault
        .store_typed(
            &device_apns_secret_name(id),
            &DeviceApnsRegistration {
                apns_token: apns_token.to_string(),
                apns_env,
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("store apns: {e}"))?;
    vault
        .store_secret(&device_push_delegation_secret_name(id), delegation_sig)
        .await
        .map_err(|e| anyhow::anyhow!("store delegation: {e}"))?;
    // The meta record last: it's what [`list_bindings`] keys on, so writing it
    // only after the push material is durable means an enumerated binding always
    // has its secrets present.
    vault
        .store_typed(&web_binding_secret_name(id), binding)
        .await
        .map_err(|e| anyhow::anyhow!("store web binding: {e}"))?;
    Ok(())
}

/// Every registered web push binding. Enumerated by prefix over the vault key
/// space (no separate index). A meta record that fails to decode is skipped
/// rather than failing the whole dispatch — push is best-effort.
pub(crate) async fn list_bindings(vault: &SecretVault) -> Vec<WebPushBinding> {
    let names = match vault.list_names().await {
        Ok(names) => names,
        Err(e) => {
            tracing::debug!(error = %e, "push: list web bindings failed");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for name in names {
        if !name.starts_with(WEB_BINDING_PREFIX) {
            continue;
        }
        match vault.get_typed::<WebPushBinding>(&name).await {
            Ok(Some(b)) => out.push(b),
            Ok(None) => {}
            Err(e) => tracing::debug!(error = %e, name = %name, "push: skip malformed web binding"),
        }
    }
    out
}

/// Drop a web binding and its push material (the direct "disconnect"/unregister
/// path). Idempotent. Currently unused by a live caller — kept so the binding has
/// a clean teardown and a future `direct_logout` can purge the gateway side too.
#[allow(dead_code)]
pub(crate) async fn delete_binding(vault: &SecretVault, device_id: &str) -> anyhow::Result<()> {
    for name in [
        web_binding_secret_name(device_id),
        device_push_key_secret_name(device_id),
        device_apns_secret_name(device_id),
        device_push_delegation_secret_name(device_id),
    ] {
        vault
            .delete_secret(&name)
            .await
            .map_err(|e| anyhow::anyhow!("delete {name}: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn vault() -> SecretVault {
        SecretVault::new(
            baybo_security::EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec())
                .unwrap(),
            Arc::new(baybo_security::test_support::MemorySecretStore::new()),
        )
    }

    fn binding(id: &str) -> WebPushBinding {
        WebPushBinding {
            device_id: id.to_string(),
            relay_url: "wss://proxy.baybo.space".into(),
            created_at: 1_700_000_000,
        }
    }

    #[tokio::test]
    async fn store_then_list_round_trips_and_writes_push_material() {
        let vault = vault();
        let b = binding("ios-aa");
        store_binding(
            &vault,
            &b,
            &[3u8; aead::KEY_LEN],
            "tok",
            ApnsEnv::Sandbox,
            &[4u8; 64],
        )
        .await
        .unwrap();

        // Enumerated back as the same binding.
        let listed = list_bindings(&vault).await;
        assert_eq!(listed, vec![b.clone()]);

        // Push material is under the shared device.{id}.* names the dispatcher reads.
        assert!(
            vault
                .get_secret(&device_push_key_secret_name("ios-aa"))
                .await
                .unwrap()
                .is_some()
        );
        let apns: DeviceApnsRegistration = vault
            .get_typed(&device_apns_secret_name("ios-aa"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(apns.apns_token, "tok");
    }

    #[tokio::test]
    async fn delete_purges_binding_and_material() {
        let vault = vault();
        store_binding(
            &vault,
            &binding("ios-bb"),
            &[1u8; aead::KEY_LEN],
            "tok",
            ApnsEnv::Production,
            &[2u8; 64],
        )
        .await
        .unwrap();
        delete_binding(&vault, "ios-bb").await.unwrap();
        assert!(list_bindings(&vault).await.is_empty());
        assert!(
            vault
                .get_secret(&device_push_key_secret_name("ios-bb"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn list_skips_non_web_keys() {
        let vault = vault();
        vault
            .store_secret("device.ios-x.push_key", &[0u8; 32])
            .await
            .unwrap();
        vault
            .store_secret("gateway.push_signing_key", &[0u8; 32])
            .await
            .unwrap();
        store_binding(
            &vault,
            &binding("ios-cc"),
            &[0u8; aead::KEY_LEN],
            "t",
            ApnsEnv::Sandbox,
            &[0u8; 64],
        )
        .await
        .unwrap();
        let listed = list_bindings(&vault).await;
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].device_id, "ios-cc");
    }
}
