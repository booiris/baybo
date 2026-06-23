use std::sync::Arc;

use baybo_model::ChannelType;
use baybo_store::{ChannelPairingRow, ChannelPairingStore, PairingStatus};
use chrono::Utc;

use crate::code::{GenerateUniqueError, generate_unique};
use crate::error::PairingError;

/// Retry budget handed to [`generate_unique`] when minting a fresh
/// code. See `docs/modules/pairing.md` — at 100 concurrent pendings
/// the collision probability per attempt is ~5×10⁻⁶, so a budget of
/// 8 is effectively unlimited while still surfacing a hard error if
/// the store is completely wedged.
const CODE_MINT_RETRIES: u32 = 8;

/// How long a freshly-minted pending pairing code stays valid before
/// the service overwrites it with a new one on the next inbound.
/// 15 minutes — long enough for a human operator to notice a Telegram
/// buzz and run `baybo pair approve`, short enough that a curious
/// one-time user's code doesn't linger in libsql for days.
const PENDING_TTL_SECONDS: i64 = 900;

/// Outcome of [`PairingService::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Triple is approved. Admit the inbound message.
    Approved,
    /// Triple needs pairing. `code` is the live pending code — either
    /// freshly minted or carried over from a previous inbound. Refuse
    /// the message and surface the code to the end-user.
    Pending { code: String },
}

/// Business logic over [`ChannelPairingStore`]: gate inbound messages,
/// approve pending rows by code, revoke approved rows. The store
/// stays dumb; this service layer knows about TTLs, code minting,
/// and expiry semantics.
pub struct PairingService {
    store: Arc<dyn ChannelPairingStore>,
}

impl PairingService {
    pub fn new(store: Arc<dyn ChannelPairingStore>) -> Self {
        Self { store }
    }

    /// Check whether `(channel_type, bot_id, user_id)` can send
    /// messages. On a miss or on an expired pending row, mint a fresh
    /// code and return [`CheckOutcome::Pending`].
    pub async fn check(
        &self,
        channel_type: &ChannelType,
        bot_id: &str,
        user_id: &str,
    ) -> Result<CheckOutcome, PairingError> {
        let now = Utc::now().timestamp();
        if let Some(row) = self
            .store
            .get(channel_type, bot_id, user_id)
            .await
            .map_err(PairingError::Storage)?
        {
            match row.status {
                PairingStatus::Approved => return Ok(CheckOutcome::Approved),
                PairingStatus::Pending if !row.is_expired(now) => {
                    return Ok(CheckOutcome::Pending { code: row.code });
                }
                PairingStatus::Pending => {
                    // Expired — fall through to mint a fresh code.
                }
            }
        }
        let code = self.mint_unique_code().await?;
        let expires_at = now.saturating_add(PENDING_TTL_SECONDS);
        let row = self
            .store
            .upsert_pending(channel_type, bot_id, user_id, &code, now, expires_at)
            .await
            .map_err(PairingError::Storage)?;
        Ok(CheckOutcome::Pending { code: row.code })
    }

    /// Approve a pending code. Returns the updated row on success,
    /// `None` if the code is unknown, already approved, expired, or
    /// revoked. The caller treats `None` as a flat "not found" — do
    /// not distinguish cases to the operator's terminal (see
    /// `docs/modules/pairing.md` §Constraints).
    pub async fn approve(&self, code: &str) -> Result<Option<ChannelPairingRow>, PairingError> {
        let now = Utc::now().timestamp();
        self.store
            .approve_by_code(code, now)
            .await
            .map_err(PairingError::Storage)
    }

    /// List rows, optionally filtered by status. Callers stamp the
    /// returned rows with an `EXPIRED` badge against their own `now`
    /// via [`ChannelPairingRow::is_expired`].
    pub async fn list(
        &self,
        status: Option<PairingStatus>,
    ) -> Result<Vec<ChannelPairingRow>, PairingError> {
        self.store.list(status).await.map_err(PairingError::Storage)
    }

    /// Hard-delete a pairing row. Subsequent inbound messages for the
    /// triple mint a fresh pending row with a fresh code.
    pub async fn revoke(
        &self,
        channel_type: &ChannelType,
        bot_id: &str,
        user_id: &str,
    ) -> Result<(), PairingError> {
        self.store
            .delete(channel_type, bot_id, user_id)
            .await
            .map_err(PairingError::Storage)
    }

    async fn mint_unique_code(&self) -> Result<String, PairingError> {
        let store = Arc::clone(&self.store);
        let out = generate_unique(
            move |candidate| {
                let store = Arc::clone(&store);
                let candidate = candidate.to_owned();
                async move {
                    let existing = store.get_by_code(&candidate).await?;
                    Ok::<bool, String>(existing.is_none())
                }
            },
            CODE_MINT_RETRIES,
        )
        .await;
        match out {
            Ok(code) => Ok(code),
            Err(GenerateUniqueError::Code(e)) => Err(PairingError::Code(e)),
            Err(GenerateUniqueError::Check(msg)) => Err(PairingError::Storage(msg)),
        }
    }
}
