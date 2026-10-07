//! The netns matrix's gateway setup: writes an approved relay binding into the
//! workspace a config file names — the device row the relay binding resolves
//! from, and the gateway's Noise static and relay node id in its vault — and
//! prints the phone's matching pairing record (the app keychain's JSON).
//!
//! `seed_relay_binding <config.json> <relay_url> <remote_api_key> <record_out>`

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use baybo_config::BayboConfig;
use baybo_gateway::device::load_or_create_static_keypair;
use baybo_gateway::relay::load_or_create_relay_node_id;
use baybo_security::SecretVault;
use baybo_storage::Store;
use baybo_store::device::{DeviceRow, DeviceStatus, hash_auth_token};
use baybo_workspace::paths::WorkspacePaths;
use device_proto::delegation;
use device_proto::noise::StaticKeypair;
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [config, relay_url, remote_api_key, record_out] = args.as_slice() else {
        anyhow::bail!(
            "usage: seed_relay_binding <config.json> <relay_url> <remote_api_key> <record_out>"
        );
    };
    let config = BayboConfig::load_from_file(Path::new(config)).await?;
    let stores =
        Store::open(WorkspacePaths::new(PathBuf::from(&config.workspace.path)).storage_db())
            .await
            .context("open the workspace's store")?;
    let key_file = config
        .security
        .encryption_key_file
        .as_ref()
        .context("security.encryption_key_file is not set")?;
    let key = baybo_security::key_file::resolve_pending(Path::new(key_file), &stores.secret)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let vault = Arc::new(SecretVault::new(key, stores.secret.clone()));

    let gateway = load_or_create_static_keypair(&vault).await?;
    let relay_node_id = load_or_create_relay_node_id(&vault).await?;
    let device = StaticKeypair::generate()?;
    let device_id = delegation::device_id_for(&delegation::generate_signing_key().verifying_key());
    let auth_token = hex::encode(rand::random::<[u8; 32]>());
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as i64;
    stores
        .device
        .create(&DeviceRow {
            device_id: device_id.clone(),
            device_pubkey: device.public().to_vec(),
            auth_token_sha256: hash_auth_token(&auth_token),
            status: DeviceStatus::Approved,
            rendezvous_id: None,
            created_at: now,
            approved_at: Some(now),
            last_seen_at: None,
            relay_url: relay_url.clone(),
            push_url: String::new(),
            remote_api_key: remote_api_key.clone(),
        })
        .await
        .context("seed the approved device")?;

    let record = json!({
        "device_id": device_id,
        "auth_token": auth_token,
        "gateway_static_pubkey": gateway.public(),
        "relay_node_id": relay_node_id,
        "relay_url": relay_url,
        "remote_api_key": remote_api_key,
        "noise_secret": device.secret(),
        "noise_public": device.public(),
    });
    std::fs::write(record_out, serde_json::to_vec(&record)?)?;
    println!("{relay_node_id}");
    Ok(())
}
