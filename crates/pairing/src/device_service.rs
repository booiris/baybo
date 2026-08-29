//! Business logic for iOS-companion **device pairing**.
//!
//! Orthogonal to [`crate::PairingService`] (which gates channel *senders*);
//! this binds a *device* to a *gateway* and bootstraps an E2E key. Pairing is
//! driven entirely by one interactive `baybo device pair` process: it mints a
//! rendezvous id + secret, hosts the relay leg, runs the XXpsk0 handshake,
//! brokers the live mutual confirm, and finalizes into a durable (approved)
//! device row. That run pairs exactly one device, so the in-flight **slot** is a
//! single in-memory value here (no durable table, no cross-process store) — the
//! only persisted artefact is the approved [`DeviceRow`]. The Noise + AEAD math
//! lives in `device-proto`; the WS transport lives in the gateway.

use std::sync::Arc;

use crate::device_slot::DevicePairingSlot;
use baybo_store::device::{DeviceRow, DeviceStatus, DeviceStore, hash_auth_token};
use chrono::Utc;
use device_proto::psk_pair::PairingSecret;
use parking_lot::Mutex;
use rand::RngExt;

use crate::error::DevicePairingError;

/// A freshly staged device: the durable row plus the only copy of its plaintext
/// bearer.
///
/// Separated at the type level so the plaintext cannot be persisted by
/// accident — [`DeviceRow`] carries only the digest.
pub struct StagedDevice {
    pub row: DeviceRow,
    /// Plaintext bearer. Send it to the pairing device and drop it; it is not
    /// recoverable from storage afterwards.
    pub auth_token: String,
}

/// How long a minted pairing slot stays valid. 15 minutes — long enough to walk
/// to the phone and scan, short enough that a stale slot doesn't linger. With no
/// background sweeper the slot simply ages out of [`claim_slot`] and dies with
/// the process.
const SLOT_TTL_SECONDS: i64 = 900;

/// Length in bytes of a device `auth_token` before hex-encoding (256-bit).
const AUTH_TOKEN_BYTES: usize = 32;

/// Owns the live (in-memory) pairing slot and the durable device registry.
pub struct DevicePairingService {
    /// The single in-flight pairing slot. One `baybo device pair` run mints one
    /// rendezvous id + secret and pairs one device, so there is at most one at a
    /// time; a fresh `mint` supersedes any prior slot. Never persisted — and it
    /// is the *only* place the QR secret lives (zeroized on drop).
    slot: Mutex<Option<DevicePairingSlot>>,
    devices: Arc<dyn DeviceStore>,
}

impl DevicePairingService {
    pub fn new(devices: Arc<dyn DeviceStore>) -> Self {
        Self {
            slot: Mutex::new(None),
            devices,
        }
    }

    /// Mint the one-time pairing slot. Returns the public `rendezvous_id` (the
    /// relay routes on it; baked into the QR as `r=`) and the high-entropy
    /// `secret` (the Noise PSK; baked into the QR as `s=` and never transmitted
    /// over the relay). The secret is held only in the in-memory slot from here
    /// on.
    pub async fn mint(&self) -> Result<(String, PairingSecret), DevicePairingError> {
        let now = Utc::now().timestamp();
        let rendezvous_id = uuid::Uuid::new_v4().to_string();
        let secret = PairingSecret::generate();
        *self.slot.lock() = Some(DevicePairingSlot {
            rendezvous_id: rendezvous_id.clone(),
            secret: secret.clone(),
            created_at: now,
            expires_at: now.saturating_add(SLOT_TTL_SECONDS),
            confirm_code: None,
            device_id: None,
            operator_decision: None,
            device_decision: None,
        });
        Ok((rendezvous_id, secret))
    }

    /// Look up the **live** (non-expired) slot if its rendezvous id matches. The
    /// handshake calls this before running XXpsk0 — `None` means an unknown or
    /// expired rendezvous, which the handshake must refuse.
    pub async fn claim_slot(
        &self,
        rendezvous_id: &str,
    ) -> Result<Option<DevicePairingSlot>, DevicePairingError> {
        let now = Utc::now().timestamp();
        Ok(self
            .slot
            .lock()
            .as_ref()
            .filter(|s| s.rendezvous_id == rendezvous_id && !s.is_expired(now))
            .cloned())
    }

    /// Publish the confirm challenge: record the confirmation code + the live
    /// device id so the operator's `baybo device pair` can display them. Called
    /// once the handshake + `DeviceHello` complete. No-op if the slot is gone or
    /// its rendezvous id no longer matches.
    pub async fn publish_confirm(
        &self,
        rendezvous_id: &str,
        confirm_code: &str,
        device_id: &str,
    ) -> Result<(), DevicePairingError> {
        if let Some(slot) = self
            .slot
            .lock()
            .as_mut()
            .filter(|s| s.rendezvous_id == rendezvous_id)
        {
            slot.confirm_code = Some(confirm_code.to_string());
            slot.device_id = Some(device_id.to_string());
        }
        Ok(())
    }

    /// Record the operator's confirm decision (written by `baybo device pair`).
    /// The handshake observes it before sealing the welcome. No-op if the slot is
    /// gone or its rendezvous id no longer matches.
    pub async fn set_operator_decision(
        &self,
        rendezvous_id: &str,
        accepted: bool,
    ) -> Result<(), DevicePairingError> {
        if let Some(slot) = self
            .slot
            .lock()
            .as_mut()
            .filter(|s| s.rendezvous_id == rendezvous_id)
        {
            slot.operator_decision = Some(accepted);
        }
        Ok(())
    }

    /// Record the phone-side outcome (set when the phone declines or drops during
    /// the confirm step). The operator's live `baybo device pair` polls it so it
    /// stops waiting the moment the phone backs out. No-op if the slot is gone or
    /// its rendezvous id no longer matches.
    pub async fn set_device_decision(
        &self,
        rendezvous_id: &str,
        accepted: bool,
    ) -> Result<(), DevicePairingError> {
        if let Some(slot) = self
            .slot
            .lock()
            .as_mut()
            .filter(|s| s.rendezvous_id == rendezvous_id)
        {
            slot.device_decision = Some(accepted);
        }
        Ok(())
    }

    /// The gateway's current bound device, if any — at most one (one gateway =
    /// one app). The operator's `baybo device pair` queries this before minting a
    /// slot so it can warn that the new pairing will replace the existing
    /// binding.
    pub async fn current_device(&self) -> Result<Option<DeviceRow>, DevicePairingError> {
        Ok(self
            .devices
            .list(Some(DeviceStatus::Approved))
            .await?
            .into_iter()
            .next())
    }

    /// Stage a confirmed handshake: write a **non-authenticating** provisioning
    /// device row (with a freshly minted `auth_token`) and consume the slot.
    /// Called only once both the phone user and the operator confirmed. The row
    /// becomes active only after [`approve_staged`](Self::approve_staged), which
    /// the gateway calls after it has sent the welcome frame.
    ///
    /// Enforces the 1:1 binding: any device already bound is atomically revoked
    /// as this one is written, so the gateway is never bound to two devices at
    /// once. The replacement happens **here**, at finalize — not when the
    /// operator first runs `device pair` — so a working binding is kept live
    /// until the new pairing actually completes.
    ///
    /// Returns the plaintext bearer alongside the row because this is the only
    /// moment it exists: the row stores only its digest.
    pub async fn stage(
        &self,
        slot: &DevicePairingSlot,
        device_id: &str,
        device_pubkey: Vec<u8>,
        relay_url: &str,
        push_url: &str,
        remote_api_key: &str,
    ) -> Result<StagedDevice, DevicePairingError> {
        // Consume the single-use slot up front: a second finalize for the same
        // rendezvous (a retrying leg) gets `SlotConsumed` and is refused, so one
        // rendezvous mints at most one device row. Dropping the slot here also
        // zeroizes the QR secret it held.
        {
            let mut guard = self.slot.lock();
            match guard.as_ref() {
                Some(s) if s.rendezvous_id == slot.rendezvous_id => *guard = None,
                _ => return Err(DevicePairingError::SlotConsumed),
            }
        }
        let now = Utc::now().timestamp();
        let auth_token = mint_auth_token();
        let row = DeviceRow {
            device_id: device_id.to_string(),
            device_pubkey,
            auth_token_sha256: hash_auth_token(&auth_token),
            status: DeviceStatus::Provisioning,
            // Only the public rendezvous id lands in the durable row — never the
            // secret.
            rendezvous_id: Some(slot.rendezvous_id.clone()),
            created_at: now,
            approved_at: None,
            last_seen_at: None,
            // Relay routing comes from the QR; push routing is supplied
            // independently by the operator's pairing command.
            relay_url: relay_url.to_string(),
            push_url: push_url.to_string(),
            remote_api_key: remote_api_key.to_string(),
        };
        self.devices.create_provisioning(&row).await?;
        Ok(StagedDevice { row, auth_token })
    }

    /// Promote a staged device row to `Approved` and supersede any prior approved
    /// binding in one transaction.
    pub async fn approve_staged(&self, device_id: &str) -> Result<DeviceRow, DevicePairingError> {
        let approved_at = Utc::now().timestamp();
        let replaced = self
            .devices
            .approve_replacing(device_id, approved_at)
            .await?;
        if !replaced.is_empty() {
            tracing::info!(
                new_device = %device_id,
                replaced = ?replaced,
                "device pairing superseded the prior binding (one gateway = one app)",
            );
        }
        self.devices.get(device_id).await?.ok_or_else(|| {
            baybo_store::StorageError::NotFound(format!("device {device_id}")).into()
        })
    }

    /// Convenience helper for tests and non-transport callers: stage and approve
    /// immediately.
    pub async fn complete(
        &self,
        slot: &DevicePairingSlot,
        device_id: &str,
        device_pubkey: Vec<u8>,
        relay_url: &str,
        push_url: &str,
        remote_api_key: &str,
    ) -> Result<StagedDevice, DevicePairingError> {
        let staged = self
            .stage(
                slot,
                device_id,
                device_pubkey,
                relay_url,
                push_url,
                remote_api_key,
            )
            .await?;
        let row = self.approve_staged(&staged.row.device_id).await?;
        Ok(StagedDevice {
            row,
            auth_token: staged.auth_token,
        })
    }

    /// Revoke a device (keeps the row + token slot; the token stops
    /// authenticating). Returns whether a row changed.
    pub async fn revoke(&self, device_id: &str) -> Result<bool, DevicePairingError> {
        Ok(self.devices.revoke(device_id).await?)
    }

    /// Fetch one device row by its natural key. The operator's live
    /// `device pair` polls this to report whether the pairing finalized.
    pub async fn device(&self, device_id: &str) -> Result<Option<DeviceRow>, DevicePairingError> {
        Ok(self.devices.get(device_id).await?)
    }

    /// List device rows, optionally filtered by status (`baybo device list`).
    pub async fn list(
        &self,
        status: Option<DeviceStatus>,
    ) -> Result<Vec<DeviceRow>, DevicePairingError> {
        Ok(self.devices.list(status).await?)
    }
}

fn mint_auth_token() -> String {
    let mut rng = rand::rng();
    let bytes: [u8; AUTH_TOKEN_BYTES] = std::array::from_fn(|_| rng.random_range(0..=255) as u8);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_storage::test_support::MemoryDeviceStore;

    fn service() -> DevicePairingService {
        DevicePairingService::new(Arc::new(MemoryDeviceStore::new()))
    }

    #[tokio::test]
    async fn mint_claim_confirm_complete_yields_approved() {
        let svc = service();
        let (rid, secret) = svc.mint().await.unwrap();
        assert!(!rid.is_empty(), "rendezvous id minted");

        let slot = svc.claim_slot(&rid).await.unwrap().unwrap();
        // The slot holds the same secret the QR carries (the only copy on the
        // gateway side).
        assert_eq!(slot.secret.as_bytes(), secret.as_bytes());

        // Publish the confirm challenge; both ends would now show it.
        svc.publish_confirm(&rid, "123456", "dev-abc")
            .await
            .unwrap();
        let slot = svc.claim_slot(&rid).await.unwrap().unwrap();
        assert_eq!(slot.confirm_code.as_deref(), Some("123456"));
        assert_eq!(slot.device_id.as_deref(), Some("dev-abc"));

        // Operator approves in the live session.
        svc.set_operator_decision(&rid, true).await.unwrap();
        assert_eq!(
            svc.claim_slot(&rid)
                .await
                .unwrap()
                .unwrap()
                .operator_decision,
            Some(true)
        );

        // Finalize → an active (approved) row, token live from creation.
        let row = svc
            .complete(
                &slot,
                "dev-abc",
                vec![3u8; 32],
                "wss://relay.test",
                "https://push.test",
                "inst-test",
            )
            .await
            .unwrap();
        let staged = row;
        let row = &staged.row;
        assert_eq!(row.status, DeviceStatus::Approved);
        assert!(row.approved_at.is_some());
        assert_eq!(row.device_pubkey, vec![3u8; 32]);
        assert_eq!(row.relay_url, "wss://relay.test");
        assert_eq!(row.push_url, "https://push.test");
        assert_eq!(row.remote_api_key, "inst-test");
        assert_eq!(
            staged.auth_token.len(),
            AUTH_TOKEN_BYTES * 2,
            "the plaintext handed to the device is hex of 32 bytes"
        );
        assert_eq!(
            row.auth_token_sha256,
            hash_auth_token(&staged.auth_token),
            "the durable row stores only the digest"
        );
        assert!(
            !row.auth_token_sha256.contains(&staged.auth_token),
            "the plaintext must not appear anywhere in the stored value"
        );
        // Only the public rendezvous id is retained on the durable row.
        assert_eq!(row.rendezvous_id.as_deref(), Some(rid.as_str()));

        // Slot is consumed — can't be claimed again.
        assert!(svc.claim_slot(&rid).await.unwrap().is_none());

        let listed = svc.list(Some(DeviceStatus::Approved)).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].device_id, "dev-abc");
    }

    #[tokio::test]
    async fn one_rendezvous_mints_at_most_one_device() {
        let svc = service();
        let (rid, _secret) = svc.mint().await.unwrap();
        // Two finalize attempts read the same live slot before either consumed it.
        let slot = svc.claim_slot(&rid).await.unwrap().unwrap();
        // The first finalize consumes the slot; the second is refused, so the
        // rendezvous can't mint a second approvable device.
        svc.complete(
            &slot,
            "dev-1",
            vec![1u8; 32],
            "wss://relay.test",
            "https://push.test",
            "inst-test",
        )
        .await
        .unwrap();
        assert!(matches!(
            svc.complete(
                &slot,
                "dev-2",
                vec![2u8; 32],
                "wss://relay.test",
                "https://push.test",
                "inst-test"
            )
            .await,
            Err(DevicePairingError::SlotConsumed),
        ));
        let all = svc.list(None).await.unwrap();
        assert_eq!(all.len(), 1, "exactly one device row from one rendezvous");
        assert_eq!(all[0].device_id, "dev-1");
    }

    #[tokio::test]
    async fn mint_supersedes_any_prior_slot() {
        let svc = service();
        let (a, _) = svc.mint().await.unwrap();
        let (b, _) = svc.mint().await.unwrap();
        assert_ne!(a, b);
        // One in-flight pairing per service: the latest mint replaces the prior,
        // so only the newest rendezvous is claimable.
        assert!(svc.claim_slot(&a).await.unwrap().is_none());
        assert!(svc.claim_slot(&b).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn revoke_after_pair() {
        let svc = service();
        let (rid, _) = svc.mint().await.unwrap();
        let slot = svc.claim_slot(&rid).await.unwrap().unwrap();
        let row = svc
            .complete(
                &slot,
                "d1",
                vec![1u8; 32],
                "wss://relay.test",
                "https://push.test",
                "inst-test",
            )
            .await
            .unwrap();
        assert_eq!(row.row.status, DeviceStatus::Approved);
        assert!(svc.revoke("d1").await.unwrap());
        // Revoked devices don't show under the Approved filter.
        assert!(
            svc.list(Some(DeviceStatus::Approved))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn second_pairing_replaces_the_first() {
        let svc = service();
        // First device.
        let (r1, _) = svc.mint().await.unwrap();
        let slot1 = svc.claim_slot(&r1).await.unwrap().unwrap();
        svc.complete(
            &slot1,
            "dev-a",
            vec![1u8; 32],
            "wss://relay.test",
            "https://push.test",
            "inst-test",
        )
        .await
        .unwrap();
        assert_eq!(
            svc.current_device().await.unwrap().unwrap().device_id,
            "dev-a"
        );

        // A second pairing supersedes it: one gateway = one app.
        let (r2, _) = svc.mint().await.unwrap();
        let slot2 = svc.claim_slot(&r2).await.unwrap().unwrap();
        svc.complete(
            &slot2,
            "dev-b",
            vec![2u8; 32],
            "wss://relay.test",
            "https://push.test",
            "inst-test",
        )
        .await
        .unwrap();

        let approved = svc.list(Some(DeviceStatus::Approved)).await.unwrap();
        assert_eq!(approved.len(), 1, "exactly one approved device");
        assert_eq!(approved[0].device_id, "dev-b", "the newest wins");
        assert_eq!(
            svc.current_device().await.unwrap().unwrap().device_id,
            "dev-b"
        );
        // The superseded device is retained as revoked (audit), not deleted.
        let all = svc.list(None).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn expired_slot_is_not_claimable() {
        let svc = service();
        let (rid, _) = svc.mint().await.unwrap();
        // Backdate the slot well past its TTL; `claim_slot` filters it out.
        {
            let mut guard = svc.slot.lock();
            guard.as_mut().unwrap().expires_at = Utc::now().timestamp() - 1;
        }
        assert!(svc.claim_slot(&rid).await.unwrap().is_none());
    }
}
