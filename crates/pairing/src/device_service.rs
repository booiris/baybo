//! Business logic for iOS-companion **device pairing**.
//!
//! Orthogonal to [`crate::PairingService`] (which gates channel *senders*);
//! this binds a *device* to a *gateway* and bootstraps an E2E key. The service
//! owns the lifecycle around the two device stores — minting a one-time slot,
//! finalizing a completed SPAKE2 handshake into a durable (pending) device row,
//! and approve / revoke / list. The SPAKE2 + Noise + AEAD math lives in
//! `aura-device-proto`; the WS transport lives in the gateway. This layer knows
//! about TTLs, code minting, and the slot → device-row transition.

use std::sync::Arc;

use aura_store::device::{DeviceRow, DeviceStatus, DeviceStore};
use aura_store::device_pairing::{DevicePairingSlot, DevicePairingStore};
use chrono::Utc;
use rand::RngExt;

use crate::code::{GenerateUniqueError, generate_unique};
use crate::error::DevicePairingError;

/// Retry budget for minting a unique pairing code (see [`crate::PairingService`]
/// — at realistic concurrency a budget of 8 is effectively unlimited).
const CODE_MINT_RETRIES: u32 = 8;

/// How long a minted pairing code stays valid before the janitor reaps it.
/// 15 minutes — long enough to walk to the phone and scan, short enough that a
/// stale code doesn't linger.
const SLOT_TTL_SECONDS: i64 = 900;

/// Length in bytes of a device `auth_token` before hex-encoding (256-bit).
const AUTH_TOKEN_BYTES: usize = 32;

/// Result of a completed pairing handshake.
pub struct DevicePairingService {
    slots: Arc<dyn DevicePairingStore>,
    devices: Arc<dyn DeviceStore>,
}

impl DevicePairingService {
    pub fn new(slots: Arc<dyn DevicePairingStore>, devices: Arc<dyn DeviceStore>) -> Self {
        Self { slots, devices }
    }

    /// Mint a one-time pairing slot for `user_id`. Returns the short code to
    /// render in the QR (`aura device pair`). The slot authorizes the SPAKE2
    /// handshake; it carries no key material.
    pub async fn mint(&self, user_id: &str, label: &str) -> Result<String, DevicePairingError> {
        let now = Utc::now().timestamp();
        let code = self.mint_unique_code().await?;
        let slot = DevicePairingSlot {
            code: code.clone(),
            user_id: user_id.to_string(),
            label: label.to_string(),
            created_at: now,
            expires_at: now.saturating_add(SLOT_TTL_SECONDS),
        };
        self.slots.create_slot(&slot).await?;
        Ok(code)
    }

    /// Look up a **live** (non-expired) slot by code. The gateway WS calls this
    /// before running SPAKE2 — `None` means an unknown or expired code, which
    /// the handshake must refuse.
    pub async fn claim_slot(
        &self,
        code: &str,
    ) -> Result<Option<DevicePairingSlot>, DevicePairingError> {
        let now = Utc::now().timestamp();
        Ok(self
            .slots
            .get_slot(code)
            .await?
            .filter(|s| !s.is_expired(now)))
    }

    /// Finalize a completed handshake: write a **pending** device row (with a
    /// freshly minted, still-inert `auth_token`, retaining the slot's code as
    /// the operator approval handle) and consume the slot. The returned row's
    /// token does not authenticate anything until [`approve`](Self::approve).
    pub async fn complete(
        &self,
        slot: &DevicePairingSlot,
        device_id: &str,
        device_pubkey: Vec<u8>,
    ) -> Result<DeviceRow, DevicePairingError> {
        let now = Utc::now().timestamp();
        let row = DeviceRow {
            user_id: slot.user_id.clone(),
            device_id: device_id.to_string(),
            label: slot.label.clone(),
            device_pubkey,
            auth_token: mint_auth_token(),
            status: DeviceStatus::Pending,
            pairing_code: Some(slot.code.clone()),
            created_at: now,
            approved_at: None,
            last_seen_at: None,
        };
        self.devices.create(&row).await?;
        // Slot is single-use; drop it so the code can't be replayed.
        self.slots.delete_slot(&slot.code).await?;
        Ok(row)
    }

    /// Approve a pending device by its retained pairing code
    /// (`aura device approve <code>`). Returns the activated row, or `None`
    /// for an unknown / non-pending code.
    pub async fn approve(&self, code: &str) -> Result<Option<DeviceRow>, DevicePairingError> {
        let now = Utc::now().timestamp();
        Ok(self.devices.approve_by_code(code, now).await?)
    }

    /// Revoke a device (keeps the row + token slot; the token stops
    /// authenticating). Returns whether a row changed.
    pub async fn revoke(&self, user_id: &str, device_id: &str) -> Result<bool, DevicePairingError> {
        Ok(self.devices.revoke(user_id, device_id).await?)
    }

    /// List device rows, optionally filtered by status (`aura device list`).
    pub async fn list(
        &self,
        status: Option<DeviceStatus>,
    ) -> Result<Vec<DeviceRow>, DevicePairingError> {
        Ok(self.devices.list(status).await?)
    }

    /// Reap expired pairing slots (janitor sweep).
    pub async fn purge_expired_slots(&self) -> Result<u64, DevicePairingError> {
        let now = Utc::now().timestamp();
        Ok(self.slots.purge_expired(now).await?)
    }

    async fn mint_unique_code(&self) -> Result<String, DevicePairingError> {
        let slots = Arc::clone(&self.slots);
        let out = generate_unique(
            move |candidate| {
                let slots = Arc::clone(&slots);
                let candidate = candidate.to_owned();
                async move { Ok(slots.get_slot(&candidate).await?.is_none()) }
            },
            CODE_MINT_RETRIES,
        )
        .await;
        match out {
            Ok(code) => Ok(code),
            Err(GenerateUniqueError::Code(e)) => Err(DevicePairingError::Code(e)),
            Err(GenerateUniqueError::Check(e)) => Err(DevicePairingError::Storage(e)),
        }
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
    use aura_storage::test_support::{MemoryDevicePairingStore, MemoryDeviceStore};

    fn service() -> DevicePairingService {
        DevicePairingService::new(
            Arc::new(MemoryDevicePairingStore::new()),
            Arc::new(MemoryDeviceStore::new()),
        )
    }

    #[tokio::test]
    async fn mint_then_claim_then_complete_then_approve() {
        let svc = service();
        let code = svc.mint("user-1", "Booiris iPhone").await.unwrap();
        assert_eq!(code.len(), crate::CODE_LEN);

        let slot = svc.claim_slot(&code).await.unwrap().unwrap();
        assert_eq!(slot.user_id, "user-1");

        let row = svc
            .complete(&slot, "dev-abc", vec![3u8; 32])
            .await
            .unwrap();
        assert_eq!(row.status, DeviceStatus::Pending);
        assert_eq!(row.device_pubkey, vec![3u8; 32]);
        assert_eq!(row.auth_token.len(), AUTH_TOKEN_BYTES * 2, "hex of 32 bytes");
        assert_eq!(row.pairing_code.as_deref(), Some(code.as_str()));

        // Slot is consumed — can't be claimed again.
        assert!(svc.claim_slot(&code).await.unwrap().is_none());

        // Approve flips the row by its retained code.
        let approved = svc.approve(&code).await.unwrap().unwrap();
        assert_eq!(approved.status, DeviceStatus::Approved);

        let listed = svc.list(Some(DeviceStatus::Approved)).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].device_id, "dev-abc");
    }

    #[tokio::test]
    async fn minted_codes_are_unique() {
        let svc = service();
        let a = svc.mint("u", "A").await.unwrap();
        let b = svc.mint("u", "B").await.unwrap();
        assert_ne!(a, b);
        // Two live slots coexist.
        assert!(svc.claim_slot(&a).await.unwrap().is_some());
        assert!(svc.claim_slot(&b).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn revoke_after_approve() {
        let svc = service();
        let code = svc.mint("u", "phone").await.unwrap();
        let slot = svc.claim_slot(&code).await.unwrap().unwrap();
        let row = svc.complete(&slot, "d1", vec![1u8; 32]).await.unwrap();
        svc.approve(&code).await.unwrap();
        assert!(svc.revoke(&row.user_id, "d1").await.unwrap());
        // Revoked devices don't show under the Approved filter.
        assert!(svc.list(Some(DeviceStatus::Approved)).await.unwrap().is_empty());
    }
}
