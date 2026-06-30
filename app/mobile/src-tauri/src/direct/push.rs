//! Direct-mode push registration over the admin-token REST surface.
//!
//! Provisions the SAME push binding a scan-to-pair device gets, but bootstrapped
//! over TLS + the admin Bearer token instead of a Noise handshake: the app reuses
//! its stable Ed25519 push identity (so `device_id == ios-<hex(pub)>`), generates
//! a fresh preview key it stores for its own NSE, signs a delegation over the
//! gateway push key, and registers all of it. The gateway then registers +
//! notifies through its configured remote host (C) exactly as for a paired
//! device, so C and the NSE are unchanged.
//!
//! Best-effort: a no-op when iOS hasn't issued an APNs token yet, or the gateway
//! has no `[push]` remote host configured. The caller re-runs it on foreground so
//! a token that arrives after login still binds. The preview key rides TLS + the
//! admin token rather than a Noise handshake — a weaker (but still
//! endpoint-to-endpoint) trust model than the relay path; see
//! `docs/modules/mobile/relay-push-security.md`.

use device_proto::aead::KEY_LEN;
use device_proto::delegation;
use serde::{Deserialize, Serialize};

use crate::keychain;

#[derive(Deserialize)]
struct PushParams {
    gateway_push_pubkey: String,
}

#[derive(Serialize)]
struct RegisterReq {
    device_id: String,
    apns_token: String,
    apns_env: String,
    push_key: String,
    delegation: String,
}

#[derive(Deserialize)]
struct RegisterResp {
    device_id: String,
}

/// Register (or refresh) this app's direct-mode push binding with the connected
/// gateway. `Ok(None)` when there is no APNs token yet (the caller retries on the
/// next foreground); `Ok(Some(device_id))` on success.
pub async fn register(apns_token: String, apns_env: &str) -> Result<Option<String>, String> {
    // No token yet (iOS hasn't delivered it) → nothing to register; the caller
    // retries on the next foreground once it lands.
    let apns_token = apns_token.trim().to_string();
    if apns_token.is_empty() {
        return Ok(None);
    }
    let creds = super::credentials()?.ok_or("not connected; sign in first")?;
    let client = reqwest::Client::new();

    // 1. Fetch the gateway push key to sign the delegation over.
    let params: PushParams = client
        .get(format!("{}/v1/push/params", creds.base_url))
        .bearer_auth(&creds.token)
        .send()
        .await
        .map_err(|e| format!("push params: {e}"))?
        .error_for_status()
        .map_err(|e| format!("push params: {e}"))?
        .json()
        .await
        .map_err(|e| format!("decode push params: {e}"))?;
    let gw_pub_bytes = hex::decode(params.gateway_push_pubkey.trim())
        .map_err(|_| "gateway push key is not valid hex".to_string())?;
    let gw_pub = delegation::verifying_key_from_bytes(&gw_pub_bytes)
        .map_err(|_| "gateway push key is not a valid Ed25519 key".to_string())?;

    // 2. The app's stable Ed25519 push identity (shared with the relay path, so a
    //    phone keeps one device_id whether it pairs or connects directly).
    let sign_key = keychain::load_or_create_device_sign_key()?;
    let device_id = delegation::device_id_for(&sign_key.verifying_key());

    // 3. The preview key in the SHARED App-Group keychain so this app's NSE can
    //    decrypt previews addressed to `bid = device_id`. Load-or-create: minted
    //    once and reused, so every re-register sends the SAME key — the NSE never
    //    holds a key that mismatches an in-flight push.
    let push_key: [u8; KEY_LEN] =
        keychain::load_or_create_push_key(&device_id).map_err(|e| format!("push key: {e}"))?;

    // 4. Authorize the gateway push key to manage our binding at C.
    let deleg = delegation::sign_delegation(&sign_key, &gw_pub);

    // 5. Register the binding (admin Bearer over TLS).
    let body = RegisterReq {
        device_id: device_id.clone(),
        apns_token,
        apns_env: apns_env.to_string(),
        push_key: hex::encode(push_key),
        delegation: hex::encode(deleg.to_bytes()),
    };
    let resp: RegisterResp = client
        .post(format!("{}/v1/push/register", creds.base_url))
        .bearer_auth(&creds.token)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("push register: {e}"))?
        .error_for_status()
        .map_err(|e| format!("push register: {e}"))?
        .json()
        .await
        .map_err(|e| format!("decode push register: {e}"))?;
    Ok(Some(resp.device_id))
}
