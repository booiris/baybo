//! Per-credential OAuth refresh state: the token cache, the single-flight
//! gate, and the proactive background loop.
//!
//! One credential — one `const` vault key holding one OAuth bundle — backs
//! many clients: the entry's default model, every `model_list` model,
//! every hot-reload generation, every admin probe. Modelling the refresh
//! state per client let them race into `refresh_token_reused` and wipe the
//! vault out from under the user, so it is interned per credential instead.
//! A second gate, an advisory file lock, extends that to peer processes
//! (a gateway and a `baybo llm probe` share the vault but not this table).
//! See `docs/modules/llm-openai-subscription.md`, "Refresh policy".

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use tokio::sync::Mutex;

use super::oauth::{RefreshError, refresh_at};
use super::token_bundle::OAuthTokenBundle;
use super::token_store::{CredentialKey, VaultTokenStore};
use crate::LlmError;

/// Just-in-time refresh skew. Safety net for the window where the
/// background loop hasn't ticked yet.
pub(super) const REFRESH_SKEW_SECS: i64 = 60;
/// Background refresh cadence — long enough to avoid spinning the
/// refresh-token rotation counter, short enough to keep the cache warm.
const BACKGROUND_REFRESH_INTERVAL_SECS: u64 = 3600;
/// Wider margin for the background loop so it acts before the JIT path
/// has to.
const BACKGROUND_REFRESH_MARGIN_SECS: i64 = 300;

/// A rotated refresh_token must hit the disk; if it doesn't, a process
/// restart reads the burned old token and forces re-login. Retry hard
/// before falling back to the in-memory dirty state.
const VAULT_SAVE_RETRY_ATTEMPTS: usize = 3;
const VAULT_SAVE_RETRY_BACKOFF_MS: [u64; 3] = [100, 500, 2000];

/// Cache-hit path re-validates the vault every N seconds so a
/// cross-process logout is honoured within that window. Within the
/// window, hot-path requests skip the vault read entirely.
const CACHE_VAULT_REVALIDATE_INTERVAL_SECS: i64 = 60;

/// How long to wait on a peer process's refresh before giving up on
/// coordination and proceeding unlocked. A refresh is one HTTP round trip
/// plus a vault write, so a peer holding it longer than this is wedged —
/// and a rare unsynchronised refresh beats a chat that never answers.
const REFRESH_LOCK_WAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);
const REFRESH_LOCK_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Whether this client is long-lived enough to justify a periodic
/// background-refresh task. `create()` builds long-lived pool clients
/// (`Enabled`); one-shot users — `live_models()`'s throwaway probe, and
/// unit tests — pass `Disabled` so no task outlives the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundRefresh {
    Enabled,
    Disabled,
}

#[derive(Clone, Debug)]
struct CachedBundle {
    /// Wrapped in Arc so the hot-path read returns a refcount bump
    /// instead of cloning ~2 KiB of JWT material per chat request.
    bundle: Arc<OAuthTokenBundle>,
    /// `false` = a prior vault save failed; the next refresh-path entry
    /// retries the save before doing anything else.
    persisted: bool,
    /// Last time we re-read the vault. `0` = never. Folded in here (not
    /// a separate atomic) so cache state and revalidation timestamp
    /// can't disagree.
    last_vault_check: i64,
}

/// Process-wide table of per-credential refresh state.
///
/// This INTERNS state derived from a constructor-injected dependency; it
/// does not introduce ambient state. Test isolation falls out of the key —
/// every test builds its own store, hence its own `StoreIdentity`.
///
/// `parking_lot` (not tokio) because the critical section is a `HashMap`
/// lookup with no `.await`; `cache` / `flight` below stay `tokio::sync`
/// because those ARE held across awaits.
static COORDINATORS: LazyLock<parking_lot::Mutex<HashMap<CredentialKey, Arc<RefreshCoordinator>>>> =
    LazyLock::new(|| parking_lot::Mutex::new(HashMap::new()));

/// Why a caller wants a refresh. Decides what counts as "someone else
/// already fixed this" when the gate re-checks the shared cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RefreshTrigger {
    /// The bundle we hold is at or near expiry. Any cached bundle outside
    /// `margin_secs` of expiry satisfies us, whoever minted it — this
    /// subsumes both "a peer rotated" and "the server returned a new
    /// access_token without rotating the refresh_token".
    NearExpiry { margin_secs: i64 },
    /// The API rejected `access_token` with 401 despite a healthy `exp`.
    /// Only a DIFFERENT access_token satisfies us — an equally-rejected
    /// one does not, however fresh its `exp` claim looks.
    Rejected,
}

impl RefreshTrigger {
    fn satisfied_by(self, observed: &OAuthTokenBundle, superseded: &OAuthTokenBundle) -> bool {
        match self {
            Self::NearExpiry { margin_secs } => !observed.is_near_expiry(margin_secs),
            Self::Rejected => observed.access_token != superseded.access_token,
        }
    }
}

/// Server-clock freshness ordering. `expires_at` is the access token's
/// `exp` claim, so it survives clock skew between processes sharing one
/// vault file; `obtained_at` only breaks same-second ties.
fn is_at_least_as_fresh(candidate: &OAuthTokenBundle, baseline: &OAuthTokenBundle) -> bool {
    (candidate.expires_at, candidate.obtained_at) >= (baseline.expires_at, baseline.obtained_at)
}

/// Install a vault-sourced bundle, never regressing the cache to an older
/// one. The vault snapshot is taken without the flight lock, so a
/// concurrent rotation can land between the read and this install — and
/// because the cache is now shared per credential, one reader's stale
/// snapshot would poison every client on it. Writing back a superseded
/// bundle would then feed an already-consumed refresh_token to the next
/// refresh, whose `refresh_token_reused` clears the vault. A dirty entry
/// is additionally the ONLY copy of a token that failed to persist, so
/// dropping it loses both the token and the flag the "refuse to rotate
/// again" guard depends on. Returns whichever bundle ends up cached.
fn install_from_vault(
    slot: &mut Option<CachedBundle>,
    candidate: Arc<OAuthTokenBundle>,
    now: i64,
) -> Arc<OAuthTokenBundle> {
    if let Some(existing) = slot.as_mut()
        && !is_at_least_as_fresh(&candidate, &existing.bundle)
    {
        existing.last_vault_check = now;
        return Arc::clone(&existing.bundle);
    }
    *slot = Some(CachedBundle {
        bundle: Arc::clone(&candidate),
        persisted: true,
        last_vault_check: now,
    });
    candidate
}

/// Pure refresh-policy function — unit-testable without a tokio runtime.
/// `Refresh(bundle)` carries the bundle so callers don't need a separate
/// `.expect("decision implies Some")` after matching.
#[derive(Debug, PartialEq, Eq)]
enum RefreshDecision {
    Skip,
    Refresh(OAuthTokenBundle),
    NotSignedIn,
}

fn decide_refresh(bundle: Option<OAuthTokenBundle>, margin_secs: i64) -> RefreshDecision {
    match bundle {
        None => RefreshDecision::NotSignedIn,
        Some(b) if b.is_near_expiry(margin_secs) => RefreshDecision::Refresh(b),
        Some(_) => RefreshDecision::Skip,
    }
}

pub(super) struct RefreshCoordinator {
    token_store: VaultTokenStore,
    /// OAuth refresh always targets `oauth::ISSUER`, never a per-entry
    /// `base_url`, so one client per credential is exact. First writer
    /// wins; `proxy` is global and rejected by hot reload.
    http: reqwest::Client,
    /// Origin the refresh POST goes to. `ISSUER` everywhere except tests.
    issuer: String,
    /// Lock file serialising refresh across PROCESSES, `None` when the
    /// credential has no on-disk home (in-memory test stores) and so no
    /// peer process can reach it.
    lock_path: Option<PathBuf>,
    cache: Mutex<Option<CachedBundle>>,
    flight: Mutex<()>,
    /// One-way latch: the background loop is armed at most once per
    /// credential and never disarmed, so there is no arm/exit interleaving
    /// in which a credential ends up with zero live loops.
    loop_armed: AtomicBool,
}

impl RefreshCoordinator {
    fn new(token_store: VaultTokenStore, http: reqwest::Client) -> Self {
        let lock_path = token_store.credential_key().lock_path();
        Self {
            token_store,
            http,
            issuer: super::oauth::ISSUER.to_string(),
            lock_path,
            cache: Mutex::new(None),
            flight: Mutex::new(()),
            loop_armed: AtomicBool::new(false),
        }
    }

    /// The single coordinator for this store's credential. Idempotent: the
    /// Nth caller for the same credential gets the same `Arc` and spawns
    /// nothing.
    ///
    /// Spawns a task on the first `Enabled` call per credential, so that
    /// caller needs a tokio runtime — `create()` already required one.
    pub(super) fn shared(
        token_store: VaultTokenStore,
        http: reqwest::Client,
        background: BackgroundRefresh,
    ) -> Arc<Self> {
        let key = token_store.credential_key();
        let coordinator = {
            let mut map = COORDINATORS.lock();
            Arc::clone(
                map.entry(key)
                    .or_insert_with(|| Arc::new(Self::new(token_store, http))),
            )
        };
        if background == BackgroundRefresh::Enabled {
            coordinator.arm_background_loop();
        }
        coordinator
    }

    /// Returns whether this call spawned the loop.
    fn arm_background_loop(self: &Arc<Self>) -> bool {
        if self.loop_armed.swap(true, Ordering::AcqRel) {
            return false;
        }
        let me = Arc::clone(self);
        tokio::spawn(async move { me.background_refresh_loop().await });
        true
    }

    /// Take the per-credential advisory file lock, blocking until a peer
    /// process releases it. `None` when the credential has no on-disk home,
    /// or when the lock file cannot be opened or locked — a coordination
    /// aid that is unavailable must degrade to today's behaviour (racy but
    /// working), never to a hard failure that blocks the user's chat.
    ///
    /// `flock` is per open file description, so this also serialises against
    /// any other opener in this process; the in-process `flight` mutex has
    /// already funnelled us, so that costs nothing.
    async fn lock_refresh_across_processes(&self) -> Option<std::fs::File> {
        let path = self.lock_path.clone()?;
        let handle = tokio::task::spawn_blocking(move || {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(&path)
                .map_err(|e| (path.clone(), e.to_string()))?;
            // Poll rather than block outright: a blocking `lock()` is
            // uncancellable, so a wedged or SIGSTOPped peer would pin this
            // thread — and the user's chat — forever, and would also hang
            // runtime shutdown.
            let deadline = std::time::Instant::now() + REFRESH_LOCK_WAIT_BUDGET;
            loop {
                match file.try_lock() {
                    Ok(()) => return Ok(file),
                    Err(std::fs::TryLockError::WouldBlock) => {
                        if std::time::Instant::now() >= deadline {
                            return Err((
                                path,
                                format!(
                                    "a peer process held the refresh lock for more than {}s",
                                    REFRESH_LOCK_WAIT_BUDGET.as_secs()
                                ),
                            ));
                        }
                        std::thread::sleep(REFRESH_LOCK_POLL_INTERVAL);
                    }
                    Err(std::fs::TryLockError::Error(e)) => return Err((path, e.to_string())),
                }
            }
        })
        .await;
        match handle {
            Ok(Ok(file)) => Some(file),
            Ok(Err((path, error))) => {
                tracing::warn!(
                    event = "openai_subscription_refresh_lock",
                    outcome = "unavailable",
                    path = %path.display(),
                    %error,
                    "cross-process refresh lock unavailable; proceeding without it"
                );
                None
            }
            Err(error) => {
                tracing::warn!(
                    event = "openai_subscription_refresh_lock",
                    outcome = "join_failed",
                    %error,
                    "cross-process refresh lock task failed; proceeding without it"
                );
                None
            }
        }
    }

    pub(super) async fn ensure_fresh_bundle(&self) -> crate::Result<Arc<OAuthTokenBundle>> {
        let now = chrono::Utc::now().timestamp();
        // Hot path: cache hit, fresh, and we read the vault recently.
        if let Some(cached) = self.cache.lock().await.as_ref()
            && !cached.bundle.is_near_expiry(REFRESH_SKEW_SECS)
            && now - cached.last_vault_check < CACHE_VAULT_REVALIDATE_INTERVAL_SECS
        {
            return Ok(Arc::clone(&cached.bundle));
        }

        // Past the revalidate window OR cache miss OR near-expiry: read
        // the vault. Covers cross-process logout, cross-process refresh,
        // and fresh process startup uniformly.
        let from_vault = self.token_store.load().await?;
        let candidate = match from_vault {
            Some(b) => b,
            None => {
                let had_cache = self.cache.lock().await.take().is_some();
                if had_cache {
                    tracing::info!(
                        event = "openai_subscription_cache_invalidated",
                        outcome = "vault_empty",
                        "vault entry disappeared (cross-process logout?); dropped in-memory bundle"
                    );
                }
                return Err(LlmError::Config(
                    "openai-subscription: not signed in — add an entry via \
                     `baybo llm add` (pick the openai-subscription provider) or \
                     re-authenticate an existing entry via `baybo llm edit`"
                        .into(),
                ));
            }
        };
        if !candidate.is_near_expiry(REFRESH_SKEW_SECS) {
            let mut guard = self.cache.lock().await;
            return Ok(install_from_vault(&mut guard, Arc::new(candidate), now));
        }
        self.single_flight_refresh(
            &candidate,
            RefreshTrigger::NearExpiry {
                margin_secs: REFRESH_SKEW_SECS,
            },
        )
        .await
    }

    pub(super) async fn force_refresh(
        &self,
        current: &OAuthTokenBundle,
    ) -> crate::Result<Arc<OAuthTokenBundle>> {
        self.single_flight_refresh(current, RefreshTrigger::Rejected)
            .await
    }

    /// Single-flight refresh. Every refresh path (JIT, reactive 401,
    /// background) funnels through here.
    ///
    /// 1. Take the flight mutex (serialises refreshes).
    /// 2. Self-heal a previously-dirty vault save. If we can't, refuse to
    ///    rotate again — chained unsaved bundles all evaporate on restart.
    /// 3. Dedup: if the cache already satisfies what `trigger` wanted,
    ///    reuse it instead of burning the refresh token again.
    /// 4. `refresh()` the token, persist with retries; on persist failure
    ///    keep usable in-memory but flag dirty for self-heal next time.
    /// 5. Permanent refresh failures clear the vault entry and surface as
    ///    `LlmError::Config` so the caller's error reads "re-login required".
    async fn single_flight_refresh(
        &self,
        superseded: &OAuthTokenBundle,
        trigger: RefreshTrigger,
    ) -> crate::Result<Arc<OAuthTokenBundle>> {
        let _flight_guard = self.flight.lock().await;

        // Cross-process gate. The in-process flight mutex says nothing about
        // a second baybo — a CLI command and a running gateway share the
        // vault but not this map, and `baybo llm probe` / `live-model` do
        // NOT take the workspace singleton lock. Both would post the same
        // refresh_token; the loser draws `refresh_token_reused` and wipes
        // the vault. Taken before the self-heal because that is a vault
        // WRITE too: a peer that read the vault, then saw our heal land, then
        // cleared on its own `refresh_token_reused` would delete the only
        // durable copy of our rotation. Every vault mutation on this path —
        // heal, save, clear — must sit inside the guard.
        let _process_guard = self.lock_refresh_across_processes().await;
        let now = chrono::Utc::now().timestamp();

        // Self-heal pass.
        {
            let mut guard = self.cache.lock().await;
            if let Some(cached) = guard.as_mut()
                && !cached.persisted
            {
                match save_with_retries(&self.token_store, &cached.bundle).await {
                    Ok(()) => {
                        cached.persisted = true;
                        tracing::info!(
                            event = "openai_subscription_refresh",
                            outcome = "dirty_save_recovered",
                            "previously-failed vault save succeeded on retry"
                        );
                    }
                    Err(e) => {
                        return Err(LlmError::Internal(anyhow::anyhow!(
                            "openai-subscription: vault save still failing for previously \
                             rotated token; refusing to rotate again until disk recovers ({e})"
                        )));
                    }
                }
            }
        }

        // Dedup re-check, and pick the token that hasn't been burned yet:
        // the cache can hold a rotation that did NOT satisfy `trigger`
        // (e.g. a rotated bundle that is itself now near expiry) but whose
        // refresh_token still supersedes the caller's. Sending the older
        // one would draw `refresh_token_reused`, which clears the vault.
        let mut effective_refresh_token = {
            let guard = self.cache.lock().await;
            match guard.as_ref() {
                Some(cached) if trigger.satisfied_by(&cached.bundle, superseded) => {
                    tracing::debug!(
                        event = "openai_subscription_refresh",
                        outcome = "deduped_via_cache",
                        "refresh: another caller already satisfied this trigger; reusing"
                    );
                    return Ok(Arc::clone(&cached.bundle));
                }
                Some(cached) if is_at_least_as_fresh(&cached.bundle, superseded) => {
                    cached.bundle.refresh_token.clone()
                }
                _ => superseded.refresh_token.clone(),
            }
        };

        // No peer can be mid-refresh now, so the vault is authoritative:
        // adopt a rotation that landed while we queued rather than posting a
        // token that peer already burned.
        if let Ok(Some(stored)) = self.token_store.load().await {
            let adopted = {
                let mut guard = self.cache.lock().await;
                install_from_vault(&mut guard, Arc::new(stored), now)
            };
            // Test what we would actually hand back, not what the vault held:
            // `install_from_vault` keeps a fresher cache entry over an older
            // vault one, so the two can differ.
            if trigger.satisfied_by(&adopted, superseded) {
                tracing::debug!(
                    event = "openai_subscription_refresh",
                    outcome = "adopted_peer_rotation",
                    "refresh: a peer process rotated while we queued; adopting its bundle"
                );
                return Ok(adopted);
            }
            // Not good enough to return, but still possibly newer than what
            // we planned to spend — a peer may have rotated to a bundle that
            // is itself near expiry. Post the freshest token we know of.
            if is_at_least_as_fresh(&adopted, superseded) {
                effective_refresh_token = adopted.refresh_token.clone();
            }
        }

        let refreshed = match refresh_at(&self.issuer, &effective_refresh_token, &self.http).await {
            Ok(b) => b,
            Err(RefreshError::Permanent(msg)) => {
                self.token_store.clear().await.ok();
                *self.cache.lock().await = None;
                return Err(LlmError::Config(format!(
                    "openai-subscription: refresh token expired/revoked — re-login required ({msg})"
                )));
            }
            Err(RefreshError::Transient(msg)) => {
                return Err(LlmError::Transient(format!(
                    "openai-subscription: token refresh transient: {msg}"
                )));
            }
        };
        let persisted = match save_with_retries(&self.token_store, &refreshed).await {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(
                    event = "openai_subscription_refresh",
                    outcome = "persistence_degraded",
                    error = %e,
                    "rotated token couldn't be persisted after {VAULT_SAVE_RETRY_ATTEMPTS} retries; \
                     in-memory cache is the only copy. Process restart before recovery forces re-login."
                );
                false
            }
        };
        let bundle = Arc::new(refreshed);
        *self.cache.lock().await = Some(CachedBundle {
            bundle: Arc::clone(&bundle),
            persisted,
            last_vault_check: now,
        });
        Ok(bundle)
    }

    /// One task per credential. Never returns: a logged-out or revoked
    /// credential idles on an empty vault instead of exiting, so a later
    /// `baybo llm edit` re-login resumes proactive refresh without needing
    /// a new client to be constructed first.
    async fn background_refresh_loop(self: Arc<Self>) {
        use std::time::Duration;

        let interval = Duration::from_secs(BACKGROUND_REFRESH_INTERVAL_SECS);
        loop {
            // Snapshot bundle from cache, falling back to vault when the
            // cache is empty (covers the case where CLI login populated
            // vault after we spawned but no chat request has run yet).
            let snapshot = {
                self.cache
                    .lock()
                    .await
                    .as_ref()
                    .map(|c| (*c.bundle).clone())
            };
            let bundle = match snapshot {
                Some(b) => Some(b),
                None => match self.token_store.load().await {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            event = "openai_subscription_background_refresh",
                            outcome = "vault_load_error",
                            error = %e,
                            "background refresh: vault load failed; will retry next interval"
                        );
                        tokio::time::sleep(interval).await;
                        continue;
                    }
                },
            };

            match decide_refresh(bundle, BACKGROUND_REFRESH_MARGIN_SECS) {
                RefreshDecision::Skip => {}
                RefreshDecision::NotSignedIn => {
                    tracing::debug!(
                        event = "openai_subscription_background_refresh",
                        outcome = "idle_not_signed_in",
                        "background refresh: no token in vault; idling until sign-in"
                    );
                }
                RefreshDecision::Refresh(b) => {
                    match self
                        .single_flight_refresh(
                            &b,
                            RefreshTrigger::NearExpiry {
                                margin_secs: BACKGROUND_REFRESH_MARGIN_SECS,
                            },
                        )
                        .await
                    {
                        Ok(_) => tracing::debug!(
                            event = "openai_subscription_background_refresh",
                            outcome = "success",
                            "background refresh: token rotated (or reused after concurrent \
                             refresh)"
                        ),
                        Err(LlmError::Config(msg)) => {
                            // Permanent: single_flight_refresh already cleared
                            // the vault and surfaced as Config. Keep idling —
                            // the next sign-in repopulates the vault and this
                            // same loop picks it up.
                            tracing::warn!(
                                event = "openai_subscription_background_refresh",
                                outcome = "permanent_failure_idle",
                                error = %msg,
                                "background refresh: permanently failed; idling until re-login"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                event = "openai_subscription_background_refresh",
                                outcome = "transient_failure",
                                error = %e,
                                "background refresh: transient failure; will retry next interval"
                            );
                        }
                    }
                }
            }
            tokio::time::sleep(interval).await;
        }
    }
}

/// Retry vault save with bounded exponential backoff. The backoff
/// schedule is small ({100ms, 500ms, 2s}) — vault writes hit local
/// sqlite, so any failure is either "disk is genuinely broken" (retry
/// won't help much) or "transient FS glitch" (retry will succeed
/// quickly). Three attempts is the sweet spot.
async fn save_with_retries(
    token_store: &VaultTokenStore,
    bundle: &OAuthTokenBundle,
) -> crate::Result<()> {
    let mut last_err = None;
    for (attempt, backoff_ms) in VAULT_SAVE_RETRY_BACKOFF_MS
        .iter()
        .copied()
        .enumerate()
        .take(VAULT_SAVE_RETRY_ATTEMPTS)
    {
        match token_store.save(bundle).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                tracing::warn!(
                    event = "openai_subscription_vault_save",
                    outcome = "retry",
                    attempt = attempt + 1,
                    of = VAULT_SAVE_RETRY_ATTEMPTS,
                    error = %e,
                    "vault save failed; will retry in {backoff_ms}ms"
                );
                last_err = Some(e);
                if attempt + 1 < VAULT_SAVE_RETRY_ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                }
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| LlmError::Internal(anyhow::anyhow!("vault save: no attempt was made"))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_security::test_support::MemorySecretStore;
    use baybo_security::{EncryptionKey, SecretVault};

    fn sample_bundle(now: i64, expires_at: i64) -> OAuthTokenBundle {
        OAuthTokenBundle {
            access_token: "a".into(),
            refresh_token: "r".into(),
            id_token: "i".into(),
            account_id: None,
            expires_at,
            obtained_at: now,
        }
    }

    fn bundle_with(
        access: &str,
        refresh_token: &str,
        now: i64,
        expires_at: i64,
    ) -> OAuthTokenBundle {
        OAuthTokenBundle {
            access_token: access.into(),
            refresh_token: refresh_token.into(),
            id_token: "id".into(),
            account_id: None,
            expires_at,
            obtained_at: now,
        }
    }

    fn fresh_vault_store() -> VaultTokenStore {
        let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
        let vault = Arc::new(SecretVault::new(key, Arc::new(MemorySecretStore::new())));
        VaultTokenStore::new(vault)
    }

    fn coordinator_for(token_store: VaultTokenStore) -> RefreshCoordinator {
        RefreshCoordinator::new(token_store, reqwest::Client::new())
    }

    #[test]
    fn decide_refresh_skips_when_bundle_is_fresh() {
        let now = chrono::Utc::now().timestamp();
        let bundle = sample_bundle(now, now + 7200); // 2h out
        assert_eq!(
            decide_refresh(Some(bundle), BACKGROUND_REFRESH_MARGIN_SECS),
            RefreshDecision::Skip
        );
    }

    #[test]
    fn decide_refresh_triggers_inside_margin() {
        let now = chrono::Utc::now().timestamp();
        let bundle = sample_bundle(now, now + 120); // 2 min out, inside 5 min margin
        let expected = bundle.clone();
        assert_eq!(
            decide_refresh(Some(bundle), BACKGROUND_REFRESH_MARGIN_SECS),
            RefreshDecision::Refresh(expected)
        );
    }

    #[test]
    fn decide_refresh_treats_missing_bundle_as_not_signed_in() {
        assert_eq!(
            decide_refresh(None, BACKGROUND_REFRESH_MARGIN_SECS),
            RefreshDecision::NotSignedIn
        );
    }

    /// SecretStore adapter that fails the first N stores, then succeeds.
    /// Drives the "vault is broken right now" branch of the durable-save path.
    struct FailingStoreNTimes {
        inner: MemorySecretStore,
        fails_remaining: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl baybo_store::SecretStore for FailingStoreNTimes {
        async fn store(
            &self,
            name: &str,
            encrypted_value: &[u8],
        ) -> std::result::Result<(), baybo_store::StorageError> {
            if self
                .fails_remaining
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst)
                > 0
            {
                return Err(baybo_store::StorageError::Storage(
                    "simulated vault failure".into(),
                ));
            }
            self.inner.store(name, encrypted_value).await
        }
        async fn retrieve(
            &self,
            name: &str,
        ) -> std::result::Result<Option<Vec<u8>>, baybo_store::StorageError> {
            self.inner.retrieve(name).await
        }
        async fn delete(&self, name: &str) -> std::result::Result<(), baybo_store::StorageError> {
            self.inner.delete(name).await
        }
        async fn list(&self) -> std::result::Result<Vec<String>, baybo_store::StorageError> {
            self.inner.list().await
        }
        async fn rewrite_all(
            &self,
            entries: &[(String, Vec<u8>)],
        ) -> std::result::Result<(), baybo_store::StorageError> {
            self.inner.rewrite_all(entries).await
        }
        fn identity(&self) -> baybo_store::StoreIdentity {
            self.inner.identity()
        }
    }

    fn vault_with_failing_store(
        initial_fails: usize,
    ) -> (VaultTokenStore, std::sync::Arc<FailingStoreNTimes>) {
        let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
        let store = std::sync::Arc::new(FailingStoreNTimes {
            inner: MemorySecretStore::new(),
            fails_remaining: std::sync::atomic::AtomicUsize::new(initial_fails),
        });
        let vault = std::sync::Arc::new(SecretVault::new(key, store.clone()));
        (VaultTokenStore::new(vault), store)
    }

    /// Test helper: build a CachedBundle wrapping the given OAuth bundle.
    fn cached(bundle: OAuthTokenBundle, persisted: bool) -> CachedBundle {
        CachedBundle {
            bundle: Arc::new(bundle),
            persisted,
            last_vault_check: chrono::Utc::now().timestamp(),
        }
    }

    #[tokio::test]
    async fn save_with_retries_recovers_within_budget() {
        let (store, _) = vault_with_failing_store(2); // first 2 fail, 3rd succeeds
        let bundle = sample_bundle(0, 0);
        save_with_retries(&store, &bundle)
            .await
            .expect("3rd attempt should succeed");
        assert_eq!(store.load().await.unwrap(), Some(bundle));
    }

    #[tokio::test]
    async fn save_with_retries_gives_up_after_budget() {
        let (store, _) = vault_with_failing_store(usize::MAX); // never succeeds
        let bundle = sample_bundle(0, 0);
        let err = save_with_retries(&store, &bundle).await.unwrap_err();
        assert!(format!("{err}").contains("simulated vault failure"));
        assert!(store.load().await.unwrap().is_none());
    }

    /// Pre-fill cache with a dirty bundle that already satisfies the
    /// caller's trigger — the gate's dedup re-check then bails out without
    /// touching the network, but only after the self-heal step has run.
    /// That lets us assert `persisted` flips back to true without an HTTP
    /// mock.
    #[tokio::test]
    async fn single_flight_refresh_self_heals_dirty_save() {
        let (store, _) = vault_with_failing_store(VAULT_SAVE_RETRY_ATTEMPTS - 1);
        let now = chrono::Utc::now().timestamp();
        let bundle = bundle_with("stale-at", "rotated-rt", now, now + 7200);
        let coord = coordinator_for(store.clone());
        *coord.cache.lock().await = Some(cached(bundle.clone(), false));
        let superseded = bundle_with("caller-at", "different-rt-from-caller", now, now - 1);
        let result = coord
            .single_flight_refresh(
                &superseded,
                RefreshTrigger::NearExpiry {
                    margin_secs: REFRESH_SKEW_SECS,
                },
            )
            .await
            .expect("self-heal then dedup must succeed");
        assert_eq!(result.refresh_token, "rotated-rt");
        let final_state = coord.cache.lock().await.clone().unwrap();
        assert!(final_state.persisted);
        assert_eq!(store.load().await.unwrap(), Some(bundle));
    }

    /// Cache-hit path revalidates the vault every CACHE_VAULT_REVALIDATE_INTERVAL_SECS.
    /// Past the window, a missing vault entry invalidates the cached bundle
    /// so a cross-process `baybo llm remove` (which clears the vault entry) is honoured.
    #[tokio::test]
    async fn ensure_fresh_bundle_invalidates_cache_when_vault_is_emptied() {
        let token_store = fresh_vault_store();
        let now = chrono::Utc::now().timestamp();
        let bundle = bundle_with("at", "rt", now, now + 7200);
        token_store.save(&bundle).await.unwrap();
        let coord = coordinator_for(token_store.clone());

        // Warm cache, then backdate last_vault_check so the next call
        // crosses the revalidation boundary.
        let first = coord.ensure_fresh_bundle().await.unwrap();
        assert_eq!(*first, bundle);
        token_store.clear().await.unwrap();
        if let Some(c) = coord.cache.lock().await.as_mut() {
            c.last_vault_check = 0;
        }

        let err = coord
            .ensure_fresh_bundle()
            .await
            .expect_err("vault is empty — must surface not-signed-in");
        match err {
            LlmError::Config(msg) => assert!(msg.contains("not signed in")),
            other => panic!("expected Config, got {other:?}"),
        }
        assert!(coord.cache.lock().await.is_none());
    }

    #[tokio::test]
    async fn ensure_fresh_bundle_skips_vault_within_revalidate_interval() {
        let token_store = fresh_vault_store();
        let now = chrono::Utc::now().timestamp();
        let bundle = bundle_with("at", "rt", now, now + 7200);
        token_store.save(&bundle).await.unwrap();
        let coord = coordinator_for(token_store.clone());
        let _ = coord.ensure_fresh_bundle().await.unwrap();
        // Within the window the cache wins; an emptied vault does not
        // immediately invalidate.
        token_store.clear().await.unwrap();
        let second = coord.ensure_fresh_bundle().await.unwrap();
        assert_eq!(*second, bundle);
    }

    /// Two consecutive dirty-save failures must NOT chain rotations —
    /// chained unsaved bundles all evaporate on process restart, much
    /// worse than waiting for the disk to recover.
    #[tokio::test]
    async fn single_flight_refresh_refuses_to_rotate_when_dirty_save_keeps_failing() {
        let (store, _) = vault_with_failing_store(usize::MAX);
        let now = chrono::Utc::now().timestamp();
        let bundle = bundle_with("stale-at", "rotated-rt", now, now + 7200);
        let coord = coordinator_for(store);
        *coord.cache.lock().await = Some(cached(bundle, false));
        let superseded = bundle_with("caller-at", "different-rt-from-caller", now, now - 1);
        let err = coord
            .single_flight_refresh(
                &superseded,
                RefreshTrigger::NearExpiry {
                    margin_secs: REFRESH_SKEW_SECS,
                },
            )
            .await
            .expect_err("should refuse to rotate while disk is broken");
        match err {
            LlmError::Internal(e) => assert!(e.to_string().contains("vault save still failing")),
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    /// Concurrent rotation race: two callers reach the gate with the
    /// same refresh_token, server rotates it for the first arriver, the
    /// second would hit `refresh_token_reused` if it called `refresh()`.
    /// The dedup re-check makes the second caller a no-op instead.
    /// Tested by pre-filling the cache with the "already-rotated" state
    /// — the re-check fires before any network call.
    #[tokio::test]
    async fn single_flight_refresh_dedups_after_concurrent_rotation() {
        let now = chrono::Utc::now().timestamp();
        let coord = coordinator_for(fresh_vault_store());
        let already_rotated = bundle_with("new-at", "new-rt", now, now + 7200);
        *coord.cache.lock().await = Some(cached(already_rotated.clone(), true));

        let superseded = bundle_with("old-at", "old-rt", now, now - 1);
        let result = coord
            .single_flight_refresh(
                &superseded,
                RefreshTrigger::NearExpiry {
                    margin_secs: REFRESH_SKEW_SECS,
                },
            )
            .await
            .expect("dedup path must succeed without a network call");
        assert_eq!(result.refresh_token, "new-rt");
        assert_eq!(result.access_token, "new-at");
        let still_cached = coord.cache.lock().await.clone().unwrap();
        assert_eq!(*still_cached.bundle, already_rotated);
        assert!(still_cached.persisted);
    }

    /// The bug this module exists to prevent: an entry's default model and
    /// each of its `model_list` build separate clients, and before
    /// interning they each got their own cache + flight lock. Two of them
    /// hitting expiry together both called `refresh()` with the same
    /// refresh_token; the loser got `refresh_token_reused` → vault wiped.
    #[tokio::test]
    async fn coordinator_is_shared_across_clients_on_one_vault() {
        let token_store = fresh_vault_store();
        let http = reqwest::Client::new();
        let a = RefreshCoordinator::shared(
            token_store.clone(),
            http.clone(),
            BackgroundRefresh::Disabled,
        );
        let b = RefreshCoordinator::shared(
            token_store.clone(),
            http.clone(),
            BackgroundRefresh::Disabled,
        );
        let c = RefreshCoordinator::shared(token_store, http, BackgroundRefresh::Disabled);
        assert!(Arc::ptr_eq(&a, &b));
        assert!(Arc::ptr_eq(&b, &c));
    }

    #[tokio::test]
    async fn coordinator_is_distinct_across_stores() {
        let http = reqwest::Client::new();
        let a = RefreshCoordinator::shared(
            fresh_vault_store(),
            http.clone(),
            BackgroundRefresh::Disabled,
        );
        let b = RefreshCoordinator::shared(fresh_vault_store(), http, BackgroundRefresh::Disabled);
        assert!(!Arc::ptr_eq(&a, &b));
    }

    /// Two `SecretVault` handles over ONE store are two views of ONE
    /// credential. Keying coordination on the vault handle would hand them
    /// separate caches and separate flight locks — the same race as the
    /// `model_list` fan-out, reached a different way.
    #[tokio::test]
    async fn coordinator_is_shared_across_vaults_on_one_store() {
        let store = Arc::new(MemorySecretStore::new());
        let vault_of = || {
            let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
            VaultTokenStore::new(Arc::new(SecretVault::new(key, store.clone())))
        };
        let http = reqwest::Client::new();
        let a = RefreshCoordinator::shared(vault_of(), http.clone(), BackgroundRefresh::Disabled);
        let b = RefreshCoordinator::shared(vault_of(), http, BackgroundRefresh::Disabled);
        assert!(
            Arc::ptr_eq(&a, &b),
            "two vaults over one store must share one coordinator"
        );
    }

    #[tokio::test]
    async fn background_loop_arms_at_most_once_per_credential() {
        // Pre-seed a far-future bundle so the loop's first tick takes the
        // Skip branch and parks on the interval sleep.
        let token_store = fresh_vault_store();
        let now = chrono::Utc::now().timestamp();
        token_store
            .save(&bundle_with("at", "rt", now, now + 86_400))
            .await
            .unwrap();
        let coord = Arc::new(coordinator_for(token_store));
        assert!(coord.arm_background_loop(), "first arm spawns");
        assert!(!coord.arm_background_loop(), "second arm is a no-op");
    }

    /// One coordinator, two clients: the second caller's refresh is
    /// satisfied by what the first already installed, with no network
    /// call. This is the property the single-cache test could only
    /// simulate — and its absence is why the bug shipped.
    #[tokio::test]
    async fn dedup_spans_two_clients() {
        let token_store = fresh_vault_store();
        let http = reqwest::Client::new();
        let now = chrono::Utc::now().timestamp();
        let first = RefreshCoordinator::shared(
            token_store.clone(),
            http.clone(),
            BackgroundRefresh::Disabled,
        );
        let second = RefreshCoordinator::shared(token_store, http, BackgroundRefresh::Disabled);

        let rotated = bundle_with("rotated-at", "rotated-rt", now, now + 7200);
        *first.cache.lock().await = Some(cached(rotated.clone(), true));

        let stale = bundle_with("stale-at", "stale-rt", now, now - 1);
        let got = second
            .force_refresh(&stale)
            .await
            .expect("peer's rotation must satisfy this caller without a network call");
        assert_eq!(got.access_token, "rotated-at");
    }

    #[test]
    fn near_expiry_trigger_is_satisfied_by_any_fresh_bundle() {
        let now = chrono::Utc::now().timestamp();
        let trigger = RefreshTrigger::NearExpiry { margin_secs: 60 };
        let superseded = bundle_with("old-at", "rt", now, now + 10);
        // The case today's `refresh_token !=` predicate misses: the server
        // returned a new access_token WITHOUT rotating the refresh_token.
        let same_rt_new_at = bundle_with("new-at", "rt", now, now + 7200);
        assert!(trigger.satisfied_by(&same_rt_new_at, &superseded));
        // Still near expiry → nobody has fixed anything yet.
        let still_stale = bundle_with("new-at", "new-rt", now, now + 10);
        assert!(!trigger.satisfied_by(&still_stale, &superseded));
    }

    #[test]
    fn rejected_trigger_requires_a_different_access_token() {
        let now = chrono::Utc::now().timestamp();
        let trigger = RefreshTrigger::Rejected;
        let superseded = bundle_with("rejected-at", "rt", now, now + 7200);
        // Fresh-looking `exp` does not help if it is the SAME token the
        // server just refused.
        let same_at = bundle_with("rejected-at", "new-rt", now, now + 7200);
        assert!(!trigger.satisfied_by(&same_at, &superseded));
        let new_at = bundle_with("new-at", "rt", now, now + 7200);
        assert!(trigger.satisfied_by(&new_at, &superseded));
    }

    /// The vault snapshot is taken without the flight lock, so a rotation
    /// can land between the read and the install. With a per-credential
    /// shared cache, writing the stale snapshot back would hand every
    /// client a superseded refresh_token — and the next refresh would draw
    /// `refresh_token_reused`, clearing the vault.
    #[tokio::test]
    async fn vault_hit_does_not_regress_a_newer_clean_rotation() {
        let token_store = fresh_vault_store();
        let now = chrono::Utc::now().timestamp();
        let stale_in_vault = bundle_with("vault-at", "vault-rt", now, now + 3600);
        token_store.save(&stale_in_vault).await.unwrap();

        let coord = coordinator_for(token_store);
        let rotated = bundle_with("rotated-at", "rotated-rt", now + 1, now + 7200);
        *coord.cache.lock().await = Some(CachedBundle {
            bundle: Arc::new(rotated.clone()),
            persisted: true,
            last_vault_check: 0, // force the vault read
        });

        let got = coord.ensure_fresh_bundle().await.unwrap();
        assert_eq!(*got, rotated, "must keep the newer rotation");
        let state = coord.cache.lock().await.clone().unwrap();
        assert_eq!(*state.bundle, rotated);
        assert!(state.last_vault_check >= now);
    }

    #[test]
    fn is_at_least_as_fresh_orders_by_exp_then_obtained_at() {
        let older = bundle_with("a", "r", 100, 1_000);
        let newer_exp = bundle_with("a", "r", 100, 2_000);
        let same_exp_later_obtained = bundle_with("a", "r", 200, 1_000);
        assert!(is_at_least_as_fresh(&newer_exp, &older));
        assert!(!is_at_least_as_fresh(&older, &newer_exp));
        assert!(is_at_least_as_fresh(&same_exp_later_obtained, &older));
        assert!(is_at_least_as_fresh(&older, &older));
    }

    /// A dirty cache entry is the ONLY copy of a rotated token. Refilling
    /// from an older vault bundle would silently drop it — and with it the
    /// `persisted: false` flag the "refuse to rotate again" guard reads.
    #[tokio::test]
    async fn vault_hit_does_not_clobber_a_dirty_cached_rotation() {
        let token_store = fresh_vault_store();
        let now = chrono::Utc::now().timestamp();
        let stale_in_vault = bundle_with("vault-at", "vault-rt", now, now + 3600);
        token_store.save(&stale_in_vault).await.unwrap();

        let coord = coordinator_for(token_store);
        let dirty_rotation = bundle_with("dirty-at", "dirty-rt", now + 1, now + 7200);
        *coord.cache.lock().await = Some(CachedBundle {
            bundle: Arc::new(dirty_rotation.clone()),
            persisted: false,
            last_vault_check: 0, // force the vault read
        });

        let got = coord.ensure_fresh_bundle().await.unwrap();
        assert_eq!(*got, dirty_rotation, "must keep the newer dirty rotation");
        let state = coord.cache.lock().await.clone().unwrap();
        assert!(!state.persisted, "dirty flag must survive the vault read");
        assert!(
            state.last_vault_check >= now,
            "vault check must be re-stamped off the 0 sentinel"
        );
    }
}

/// Cross-process refresh coordination. These drive the real `refresh_at`
/// network path against a local stub issuer, which is the only way to reach
/// the rotation and vault-clearing branches at all.
#[cfg(test)]
mod cross_process_tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use baybo_security::{EncryptionKey, SecretVault};
    use baybo_storage::sqlite::{SqlitePool, SqliteSecretStore};
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn fake_jwt(payload: &serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
        let payload_b64 = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
        let signature = URL_SAFE_NO_PAD.encode(b"fake");
        format!("{header}.{payload_b64}.{signature}")
    }

    /// Minimal stub of the OAuth token endpoint. Counts requests, and hands
    /// every caller the SAME rotated bundle — so a second real refresh is
    /// visible as a hit count of 2 rather than as a hard failure.
    struct StubIssuer {
        origin: String,
        hits: Arc<AtomicUsize>,
    }

    async fn spawn_stub_issuer(
        rotated_access: &'static str,
        rotated_refresh: &'static str,
    ) -> StubIssuer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                // Drain what the client sent; we do not need to parse it.
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let id_token = fake_jwt(&serde_json::json!({
                    "https://api.openai.com/auth": { "chatgpt_account_id": "acc-1" }
                }));
                let access = fake_jwt(&serde_json::json!({
                    "exp": chrono::Utc::now().timestamp() + 7200,
                    "marker": rotated_access,
                }));
                let body = serde_json::json!({
                    "access_token": access,
                    "refresh_token": rotated_refresh,
                    "id_token": id_token,
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        StubIssuer {
            origin: format!("http://{addr}"),
            hits,
        }
    }

    /// A file-backed vault, so the credential has a real `StoreIdentity::File`
    /// and therefore a real cross-process lock path.
    async fn file_backed_store(dir: &std::path::Path) -> VaultTokenStore {
        let pool = SqlitePool::open(dir.join("baybo.db")).await.unwrap();
        let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
        let vault = Arc::new(SecretVault::new(
            key,
            Arc::new(SqliteSecretStore::new(pool)),
        ));
        VaultTokenStore::new(vault)
    }

    /// The stub issuer lives on loopback; a default `reqwest::Client` would
    /// route it through an ambient `HTTP_PROXY` / `ALL_PROXY` and the test
    /// would fail for reasons that have nothing to do with the lock.
    fn direct_client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("build a proxy-free test client")
    }

    fn stale(now: i64) -> OAuthTokenBundle {
        OAuthTokenBundle {
            access_token: "stale-at".into(),
            refresh_token: "stale-rt".into(),
            id_token: "id".into(),
            account_id: None,
            expires_at: now + 10, // inside every margin ⇒ wants a refresh
            obtained_at: now,
        }
    }

    /// TWO coordinators over ONE on-disk credential — the shape of a gateway
    /// and a `baybo llm probe` running side by side. Without the advisory
    /// file lock both POST the same refresh_token and the loser's
    /// `refresh_token_reused` clears the vault. With it, the second one
    /// finds the peer's rotation already in the vault and adopts it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn peer_processes_do_not_both_burn_the_refresh_token() {
        let dir = tempfile::tempdir().unwrap();
        let store = file_backed_store(dir.path()).await;
        let now = chrono::Utc::now().timestamp();
        store.save(&stale(now)).await.unwrap();

        let issuer = spawn_stub_issuer("rotated", "rotated-rt").await;

        // Deliberately NOT via `shared()`: two processes each hold their own
        // coordinator, and that is exactly what we are testing.
        let mut peer_a = RefreshCoordinator::new(store.clone(), direct_client());
        peer_a.issuer = issuer.origin.clone();
        let mut peer_b = RefreshCoordinator::new(store.clone(), direct_client());
        peer_b.issuer = issuer.origin.clone();
        assert!(
            peer_a.lock_path.is_some(),
            "a file-backed credential must have a cross-process lock path"
        );
        assert_eq!(peer_a.lock_path, peer_b.lock_path, "peers must share it");

        let trigger = RefreshTrigger::NearExpiry {
            margin_secs: REFRESH_SKEW_SECS,
        };
        let (superseded_a, superseded_b) = (stale(now), stale(now));
        let (a, b) = tokio::join!(
            peer_a.single_flight_refresh(&superseded_a, trigger),
            peer_b.single_flight_refresh(&superseded_b, trigger),
        );

        let a = a.expect("peer A must get a usable bundle");
        let b = b.expect("peer B must get a usable bundle");
        assert_eq!(a.refresh_token, "rotated-rt");
        assert_eq!(b.refresh_token, "rotated-rt");
        assert_eq!(
            issuer.hits.load(Ordering::SeqCst),
            1,
            "exactly one peer may spend the refresh token; the other must adopt \
             its rotation from the vault"
        );
        assert_eq!(
            store.load().await.unwrap().map(|b| b.refresh_token),
            Some("rotated-rt".to_string()),
            "the vault must hold the rotation, not be cleared"
        );
    }

    /// The self-heal is a vault WRITE, so it must sit inside the
    /// cross-process guard too. If it ran before the lock, a peer could read
    /// the vault, watch our heal land, then clear on its own
    /// `refresh_token_reused` — destroying the only durable copy of our
    /// rotation and signing both processes out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn self_heal_waits_for_the_cross_process_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = file_backed_store(dir.path()).await;
        let now = chrono::Utc::now().timestamp();

        let coord = Arc::new(RefreshCoordinator::new(store.clone(), direct_client()));
        // A rotation that never reached disk: the cache is its only copy.
        let dirty = OAuthTokenBundle {
            access_token: "dirty-at".into(),
            refresh_token: "dirty-rt".into(),
            id_token: "id".into(),
            account_id: None,
            expires_at: now + 7200,
            obtained_at: now,
        };
        *coord.cache.lock().await = Some(CachedBundle {
            bundle: Arc::new(dirty.clone()),
            persisted: false,
            last_vault_check: 0,
        });

        // Stand in for a peer process mid-refresh.
        let lock_path = coord.lock_path.clone().expect("file-backed ⇒ lock path");
        let peer = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        peer.lock().unwrap();

        let mut running = tokio::spawn({
            let coord = Arc::clone(&coord);
            let superseded = stale(now);
            async move {
                coord
                    .single_flight_refresh(
                        &superseded,
                        RefreshTrigger::NearExpiry {
                            margin_secs: REFRESH_SKEW_SECS,
                        },
                    )
                    .await
            }
        });

        // It must still be blocked on the peer's lock. Were the heal outside
        // the guard it would have completed instantly — the cached bundle
        // satisfies the trigger, so the call returns at the dedup with no
        // network involved.
        let outcome =
            tokio::time::timeout(std::time::Duration::from_millis(250), &mut running).await;
        assert!(
            outcome.is_err(),
            "refresh must block on a peer's lock before touching the vault"
        );
        assert!(
            store.load().await.unwrap().is_none(),
            "the self-heal wrote to the vault while a peer held the refresh lock"
        );

        // Peer finishes; our heal may now land.
        drop(peer);
        let healed = running
            .await
            .unwrap()
            .expect("heal then dedup must succeed");
        assert_eq!(healed.refresh_token, "dirty-rt");
        assert_eq!(
            store.load().await.unwrap(),
            Some(dirty),
            "the rotation must reach the vault once the lock is free"
        );
    }

    /// The lock is an aid, not a dependency: an in-memory credential has no
    /// on-disk home, so there is no peer to coordinate with and the refresh
    /// must still work.
    #[tokio::test]
    async fn refresh_works_without_a_lock_path() {
        let key = EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec()).unwrap();
        let vault = Arc::new(SecretVault::new(
            key,
            Arc::new(baybo_security::test_support::MemorySecretStore::new()),
        ));
        let store = VaultTokenStore::new(vault);
        let now = chrono::Utc::now().timestamp();
        store.save(&stale(now)).await.unwrap();

        let issuer = spawn_stub_issuer("rotated", "rotated-rt").await;
        let mut coord = RefreshCoordinator::new(store.clone(), direct_client());
        coord.issuer = issuer.origin.clone();
        assert!(
            coord.lock_path.is_none(),
            "in-memory store has no lock path"
        );

        let got = coord
            .single_flight_refresh(
                &stale(now),
                RefreshTrigger::NearExpiry {
                    margin_secs: REFRESH_SKEW_SECS,
                },
            )
            .await
            .expect("refresh must not depend on the cross-process lock");
        assert_eq!(got.refresh_token, "rotated-rt");
    }
}
