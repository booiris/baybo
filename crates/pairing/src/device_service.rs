//! Business logic for iOS-companion **device pairing**.
//!
//! Orthogonal to [`crate::PairingService`] (which gates channel *senders*);
//! this binds a *device* to a *gateway* and bootstraps an E2E key. The service
//! owns the lifecycle around the two device stores — minting a one-time slot,
//! brokering the live mutual confirm (publish the confirmation code, record the
//! operator's decision), finalizing a confirmed SPAKE2 handshake into a durable
//! (approved) device row, and revoke / list. The SPAKE2 + Noise + AEAD math
//! lives in `device-proto`; the WS transport lives in the gateway. This layer
//! knows about TTLs, code minting, and the slot → device-row transition.

use std::sync::Arc;

use baybo_store::device::{DeviceRow, DeviceStatus, DeviceStore};
use baybo_store::device_pairing::{DevicePairingSlot, DevicePairingStore};
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
    /// render in the QR (`baybo device pair`). The slot authorizes the SPAKE2
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
            confirm_code: None,
            device_id: None,
            operator_decision: None,
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

    /// Publish the confirm challenge: record the confirmation code + the live
    /// device id + the resolved label (the device's reported name, or the
    /// operator's override) so the operator's `baybo device pair` can display
    /// them. Called by the gateway once SPAKE2 + `DeviceHello` complete.
    pub async fn publish_confirm(
        &self,
        code: &str,
        confirm_code: &str,
        device_id: &str,
        label: &str,
    ) -> Result<(), DevicePairingError> {
        Ok(self
            .slots
            .set_confirm(code, confirm_code, device_id, label)
            .await?)
    }

    /// Record the operator's confirm decision (written by `baybo device pair`).
    /// The gateway polls the slot for it before finalizing.
    pub async fn set_operator_decision(
        &self,
        code: &str,
        accepted: bool,
    ) -> Result<(), DevicePairingError> {
        Ok(self.slots.set_operator_decision(code, accepted).await?)
    }

    /// Finalize a confirmed handshake: write an **approved** device row (with a
    /// freshly minted, active `auth_token`) and consume the slot. `label` is the
    /// resolved device name (the device's reported label, or the operator's
    /// override). The gateway calls this only once both the phone user and the
    /// operator confirmed.
    pub async fn complete(
        &self,
        slot: &DevicePairingSlot,
        device_id: &str,
        device_pubkey: Vec<u8>,
        label: &str,
    ) -> Result<DeviceRow, DevicePairingError> {
        // Atomically consume the single-use slot up front: two clients that
        // scanned the same live code race here, and the loser gets `false` and
        // is refused — so one code mints at most one device row (and a consumed
        // code can't be replayed).
        if !self.slots.delete_slot(&slot.code).await? {
            return Err(DevicePairingError::SlotConsumed);
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

    /// List all pairing slots, newest first. The relay-pairing host manager
    /// hosts a leg per live slot; callers filter expiry against their own `now`.
    pub async fn list_slots(&self) -> Result<Vec<DevicePairingSlot>, DevicePairingError> {
        Ok(self.slots.list_slots().await?)
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
    use baybo_storage::test_support::{MemoryDevicePairingStore, MemoryDeviceStore};

    fn service() -> DevicePairingService {
        DevicePairingService::new(
            Arc::new(MemoryDevicePairingStore::new()),
            Arc::new(MemoryDeviceStore::new()),
        )
    }

    #[tokio::test]
    async fn mint_claim_confirm_complete_yields_approved() {
        let svc = service();
        let code = svc.mint("user-1", "Booiris iPhone").await.unwrap();
        assert_eq!(code.len(), crate::CODE_LEN);

        let slot = svc.claim_slot(&code).await.unwrap().unwrap();
        assert_eq!(slot.user_id, "user-1");

        // Gateway publishes the confirm challenge (with the device's reported
        // name); both ends would now show it.
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
            svc.claim_slot(&code).await.unwrap().unwrap().operator_decision,
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
        // Two clients both read the same live slot before either finalized.
        let slot = svc.claim_slot(&code).await.unwrap().unwrap();
        // The first finalize atomically consumes the slot; the second is refused,
        // so the code can't mint a second approvable device.
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
}
