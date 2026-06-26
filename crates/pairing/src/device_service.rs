//! Business logic for iOS-companion **device pairing**.
//!
//! Orthogonal to [`crate::PairingService`] (which gates channel *senders*);
//! this binds a *device* to a *gateway* and bootstraps an E2E key. Pairing is
//! driven entirely by one interactive `baybo device pair` process: it mints a
//! code, hosts the relay leg, runs the SPAKE2 handshake, brokers the live mutual
//! confirm, and finalizes into a durable (approved) device row. That run pairs
//! exactly one device, so the in-flight **slot** is a single in-memory value here
//! (no durable table, no cross-process store) — the only persisted artefact is
//! the approved [`DeviceRow`]. The SPAKE2 + Noise + AEAD math lives in
//! `device-proto`; the WS transport lives in the gateway.

use std::sync::Arc;

use baybo_store::device::{DeviceRow, DeviceStatus, DeviceStore};
use baybo_store::device_pairing::DevicePairingSlot;
use chrono::Utc;
use parking_lot::Mutex;
use rand::RngExt;

use crate::code::generate_code;
use crate::error::DevicePairingError;

/// How long a minted pairing code stays valid. 15 minutes — long enough to walk
/// to the phone and scan, short enough that a stale slot doesn't linger. With no
/// background sweeper the slot simply ages out of [`claim_slot`] and dies with
/// the process.
const SLOT_TTL_SECONDS: i64 = 900;

/// Length in bytes of a device `auth_token` before hex-encoding (256-bit).
const AUTH_TOKEN_BYTES: usize = 32;

/// Owns the live (in-memory) pairing slot and the durable device registry.
pub struct DevicePairingService {
    /// The single in-flight pairing slot. One `baybo device pair` run mints one
    /// code and pairs one device, so there is at most one at a time; a fresh
    /// `mint` supersedes any prior slot. Never persisted.
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

    /// Mint the one-time pairing slot for `user_id`. Returns the short code to
    /// render in the QR (`baybo device pair`). The slot authorizes the SPAKE2
    /// handshake; it carries no key material.
    pub async fn mint(&self, user_id: &str, label: &str) -> Result<String, DevicePairingError> {
        let now = Utc::now().timestamp();
        let code = generate_code();
        *self.slot.lock() = Some(DevicePairingSlot {
            code: code.clone(),
            user_id: user_id.to_string(),
            label: label.to_string(),
            created_at: now,
            expires_at: now.saturating_add(SLOT_TTL_SECONDS),
            confirm_code: None,
            device_id: None,
            operator_decision: None,
            device_decision: None,
        });
        Ok(code)
    }

    /// Look up the **live** (non-expired) slot if its code matches. The handshake
    /// calls this before running SPAKE2 — `None` means an unknown or expired code,
    /// which the handshake must refuse.
    pub async fn claim_slot(
        &self,
        code: &str,
    ) -> Result<Option<DevicePairingSlot>, DevicePairingError> {
        let now = Utc::now().timestamp();
        Ok(self
            .slot
            .lock()
            .as_ref()
            .filter(|s| s.code == code && !s.is_expired(now))
            .cloned())
    }

    /// Publish the confirm challenge: record the confirmation code + the live
    /// device id + the resolved label (the device's reported name, or the
    /// operator's override) so the operator's `baybo device pair` can display
    /// them. Called once SPAKE2 + `DeviceHello` complete. No-op if the slot is
    /// gone or its code no longer matches.
    pub async fn publish_confirm(
        &self,
        code: &str,
        confirm_code: &str,
        device_id: &str,
        label: &str,
    ) -> Result<(), DevicePairingError> {
        if let Some(slot) = self.slot.lock().as_mut().filter(|s| s.code == code) {
            slot.confirm_code = Some(confirm_code.to_string());
            slot.device_id = Some(device_id.to_string());
            slot.label = label.to_string();
        }
        Ok(())
    }

    /// Record the operator's confirm decision (written by `baybo device pair`).
    /// The handshake observes it before sealing the welcome. No-op if the slot is
    /// gone or its code no longer matches.
    pub async fn set_operator_decision(
        &self,
        code: &str,
        accepted: bool,
    ) -> Result<(), DevicePairingError> {
        if let Some(slot) = self.slot.lock().as_mut().filter(|s| s.code == code) {
            slot.operator_decision = Some(accepted);
        }
        Ok(())
    }

    /// Record the phone-side outcome (set when the phone declines or drops during
    /// the confirm step). The operator's live `baybo device pair` polls it so it
    /// stops waiting the moment the phone backs out. No-op if the slot is gone or
    /// its code no longer matches.
    pub async fn set_device_decision(
        &self,
        code: &str,
        accepted: bool,
    ) -> Result<(), DevicePairingError> {
        if let Some(slot) = self.slot.lock().as_mut().filter(|s| s.code == code) {
            slot.device_decision = Some(accepted);
        }
        Ok(())
    }

    /// Finalize a confirmed handshake: write an **approved** device row (with a
    /// freshly minted, active `auth_token`) and consume the slot. `label` is the
    /// resolved device name (the device's reported label, or the operator's
    /// override). Called only once both the phone user and the operator confirmed.
    pub async fn complete(
        &self,
        slot: &DevicePairingSlot,
        device_id: &str,
        device_pubkey: Vec<u8>,
        label: &str,
    ) -> Result<DeviceRow, DevicePairingError> {
        // Consume the single-use slot up front: a second finalize for the same
        // code (a retrying leg) gets `SlotConsumed` and is refused, so one code
        // mints at most one device row.
        {
            let mut guard = self.slot.lock();
            match guard.as_ref() {
                Some(s) if s.code == slot.code => *guard = None,
                _ => return Err(DevicePairingError::SlotConsumed),
            }
        }
        let now = Utc::now().timestamp();
        let row = DeviceRow {
            user_id: slot.user_id.clone(),
            device_id: device_id.to_string(),
            label: label.to_string(),
            device_pubkey,
            auth_token: mint_auth_token(),
            status: DeviceStatus::Approved,
            pairing_code: Some(slot.code.clone()),
            created_at: now,
            approved_at: Some(now),
            last_seen_at: None,
        };
        self.devices.create(&row).await?;
        Ok(row)
    }

    /// Revoke a device (keeps the row + token slot; the token stops
    /// authenticating). Returns whether a row changed.
    pub async fn revoke(&self, user_id: &str, device_id: &str) -> Result<bool, DevicePairingError> {
        Ok(self.devices.revoke(user_id, device_id).await?)
    }

    /// Fetch one device row by its natural key. The operator's live
    /// `device pair` polls this to report whether the pairing finalized.
    pub async fn device(
        &self,
        user_id: &str,
        device_id: &str,
    ) -> Result<Option<DeviceRow>, DevicePairingError> {
        Ok(self.devices.get(user_id, device_id).await?)
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
        let code = svc.mint("user-1", "Booiris iPhone").await.unwrap();
        assert_eq!(code.len(), crate::CODE_LEN);

        let slot = svc.claim_slot(&code).await.unwrap().unwrap();
        assert_eq!(slot.user_id, "user-1");

        // Publish the confirm challenge (with the device's reported name); both
        // ends would now show it.
        svc.publish_confirm(&code, "123456", "dev-abc", "Booiris iPhone")
            .await
            .unwrap();
        let slot = svc.claim_slot(&code).await.unwrap().unwrap();
        assert_eq!(slot.confirm_code.as_deref(), Some("123456"));
        assert_eq!(slot.device_id.as_deref(), Some("dev-abc"));
        assert_eq!(slot.label, "Booiris iPhone");

        // Operator approves in the live session.
        svc.set_operator_decision(&code, true).await.unwrap();
        assert_eq!(
            svc.claim_slot(&code)
                .await
                .unwrap()
                .unwrap()
                .operator_decision,
            Some(true)
        );

        // Finalize → an active (approved) row, token live from creation.
        let row = svc
            .complete(&slot, "dev-abc", vec![3u8; 32], "Booiris iPhone")
            .await
            .unwrap();
        assert_eq!(row.status, DeviceStatus::Approved);
        assert_eq!(row.label, "Booiris iPhone");
        assert_eq!(row.approved_at, Some(row.created_at));
        assert_eq!(row.device_pubkey, vec![3u8; 32]);
        assert_eq!(
            row.auth_token.len(),
            AUTH_TOKEN_BYTES * 2,
            "hex of 32 bytes"
        );
        assert_eq!(row.pairing_code.as_deref(), Some(code.as_str()));

        // Slot is consumed — can't be claimed again.
        assert!(svc.claim_slot(&code).await.unwrap().is_none());

        let listed = svc.list(Some(DeviceStatus::Approved)).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].device_id, "dev-abc");
    }

    #[tokio::test]
    async fn one_code_mints_at_most_one_device() {
        let svc = service();
        let code = svc.mint("user-1", "Phone").await.unwrap();
        // Two finalize attempts read the same live slot before either consumed it.
        let slot = svc.claim_slot(&code).await.unwrap().unwrap();
        // The first finalize consumes the slot; the second is refused, so the
        // code can't mint a second approvable device.
        svc.complete(&slot, "dev-1", vec![1u8; 32], "iPhone")
            .await
            .unwrap();
        assert!(matches!(
            svc.complete(&slot, "dev-2", vec![2u8; 32], "iPhone").await,
            Err(DevicePairingError::SlotConsumed),
        ));
        let all = svc.list(None).await.unwrap();
        assert_eq!(all.len(), 1, "exactly one device row from one code");
        assert_eq!(all[0].device_id, "dev-1");
    }

    #[tokio::test]
    async fn mint_supersedes_any_prior_slot() {
        let svc = service();
        let a = svc.mint("u", "A").await.unwrap();
        let b = svc.mint("u", "B").await.unwrap();
        assert_ne!(a, b);
        // One in-flight pairing per service: the latest mint replaces the prior,
        // so only the newest code is claimable.
        assert!(svc.claim_slot(&a).await.unwrap().is_none());
        assert!(svc.claim_slot(&b).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn revoke_after_pair() {
        let svc = service();
        let code = svc.mint("u", "phone").await.unwrap();
        let slot = svc.claim_slot(&code).await.unwrap().unwrap();
        let row = svc
            .complete(&slot, "d1", vec![1u8; 32], "phone")
            .await
            .unwrap();
        assert_eq!(row.status, DeviceStatus::Approved);
        assert!(svc.revoke(&row.user_id, "d1").await.unwrap());
        // Revoked devices don't show under the Approved filter.
        assert!(
            svc.list(Some(DeviceStatus::Approved))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn expired_slot_is_not_claimable() {
        let svc = service();
        let code = svc.mint("u", "phone").await.unwrap();
        // Backdate the slot well past its TTL; `claim_slot` filters it out.
        {
            let mut guard = svc.slot.lock();
            guard.as_mut().unwrap().expires_at = Utc::now().timestamp() - 1;
        }
        assert!(svc.claim_slot(&code).await.unwrap().is_none());
    }
}
