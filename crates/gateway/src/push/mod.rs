//! A-side push dispatcher.
//!
//! Subscribes to the `JobLifecycle` broadcast bus and, for each
//! **successfully-completed real user turn** (`phase == Completed`,
//! `kind == UserChat` — which excludes Cron / Compact / Spawned /
//! SubagentNotification), encrypts a short preview **per approved device**
//! with that device's push key and POSTs the opaque ciphertext to the remote
//! host (C). A encrypts, C relays blind, the iOS NSE decrypts — so the preview
//! is real on the lock screen while C and Apple see only ciphertext.
//!
//! The reply's persisted ordinal rides the `Completed` event
//! ([`JobPhase::Completed { reply_ordinal }`](baybo_job::JobPhase)); the
//! dispatcher reads exactly that row (no read-after-write poll). A completion
//! with no ordinal — a non-message output, or a reply whose store write failed
//! — has no durable row to preview, so it is **not pushed** at all.
//!
//! Modeled on `spawn_turn_state_projector`: subscribe synchronously, then a
//! `select!` loop over the bus with `Lagged`/`Closed` handling (push is
//! best-effort, so a lag just drops that buzz).

pub(crate) mod web;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use std::collections::HashMap;
use std::collections::HashSet;

use base64::Engine;
use baybo_job::{JobInputKind, JobLifecycle, JobLifecycleEvent, JobPhase};
use baybo_model::{ContentBlock, Role, SessionId};
use baybo_security::SecretVault;
use baybo_session::SessionManager;
use baybo_store::{DeviceRow, DeviceStatus, DeviceStore, SessionStore};
use device_proto::aead;
use device_proto::delegation;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::OnceCell;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

/// Max preview characters (kept well under the 4 KB APNs payload once
/// encrypted + base64'd).
const PREVIEW_MAX_CHARS: usize = 200;
/// How many active rows to pull from the reply's ordinal when building the
/// preview — 1 is enough (the reply sits exactly at `reply_ordinal`); a small
/// margin tolerates an interleaved row without a second round-trip.
const PREVIEW_READ_LIMIT: usize = 4;

/// Remote host (C) that direct-mode device push bindings register +
/// notify through. Hardcoded to the public proxy the app already defaults to for
/// pairing, so direct device sessions get push out of the box; not yet operator-
/// configurable (a `[push]` config block can override this later).
pub(crate) const DEFAULT_PUSH_RELAY_URL: &str = "wss://proxy.baybo.space";

/// Secret-vault name for a device's per-device push key. The single source of
/// truth shared by the write site (the device-pair route) and the read site
/// (this dispatcher) so the two can never drift.
pub(crate) fn device_push_key_secret_name(device_id: &str) -> String {
    format!("device.{device_id}.push_key")
}

/// Secret-vault name for a device's persisted APNs registration material.
pub(crate) fn device_apns_secret_name(device_id: &str) -> String {
    format!("device.{device_id}.apns")
}

/// The per-device APNs registration A persists at pairing (vault, keyed by
/// device_id) so the dispatcher can restore C's APNs binding before its first
/// push when the token was available to A.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DeviceApnsRegistration {
    pub apns_token: String,
    pub apns_env: device_proto::pairing::ApnsEnv,
}

/// Secret-vault name for a device's push delegation — the 64-byte Ed25519
/// signature the device made at pairing authorizing A's gateway push key to
/// manage its APNs binding at C. Absent → push is disabled for the device (the
/// gateway can't prove ownership of the binding to the keyless push routes).
pub(crate) fn device_push_delegation_secret_name(device_id: &str) -> String {
    format!("device.{device_id}.push_delegation")
}

/// `SecretVault` key holding A's gateway Ed25519 push-signing key (32-byte seed).
/// One per gateway, stable across restarts — a paired device delegated to its
/// public half, so rotating it forces re-pairing to re-delegate.
const PUSH_SIGNING_VAULT_KEY: &str = "gateway.push_signing_key";

/// Load A's gateway push-signing key, generating + persisting one on first use.
pub(crate) async fn load_or_create_push_signing_key(
    vault: &SecretVault,
) -> anyhow::Result<delegation::SigningKey> {
    if let Some(secret) = vault
        .get_secret(PUSH_SIGNING_VAULT_KEY)
        .await
        .map_err(|e| anyhow::anyhow!("vault get {PUSH_SIGNING_VAULT_KEY}: {e}"))?
    {
        if let Ok(seed) = <[u8; delegation::SEED_LEN]>::try_from(secret.as_bytes()) {
            return Ok(delegation::SigningKey::from_bytes(&seed));
        }
        tracing::warn!(
            "{PUSH_SIGNING_VAULT_KEY} is malformed; regenerating A's gateway push \
             key (paired devices will need to re-pair to re-delegate)",
        );
    }
    let key = delegation::generate_signing_key();
    vault
        .store_secret(PUSH_SIGNING_VAULT_KEY, key.to_bytes().as_slice())
        .await
        .map_err(|e| anyhow::anyhow!("vault store {PUSH_SIGNING_VAULT_KEY}: {e}"))?;
    Ok(key)
}

/// Vault key holding the high-water mark of the push replay counter. Persisting
/// it keeps the strictly-increasing counter rising across a restart even if the
/// wall clock steps backward: an NTP correction straddling a restart could
/// otherwise mint a value below what C last accepted and wedge every push at a
/// 403 until the clock caught up. Best-effort — a vault hiccup just falls back to
/// wall-clock seeding (the pre-existing behaviour).
const PUSH_COUNTER_VAULT_KEY: &str = "gateway.push_counter_highwater";

/// Mint the next counter from `counter`: never below `now`, never below the prior
/// value plus one — strictly increasing within the process regardless of the
/// wall clock. Pure (no I/O) so the monotonicity invariant is unit-testable.
/// Returns the new value; the atomic is left holding it.
fn mint_counter(counter: &AtomicU64, now: u64) -> u64 {
    let mut last = counter.load(Ordering::Relaxed);
    loop {
        let next = now.max(last.wrapping_add(1));
        match counter.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(observed) => last = observed,
        }
    }
}

/// The APNs `apns-collapse-id` for a (device, session): a short hash, never the
/// raw `device_id:session_id`. This keeps it under APNs' 64-byte collapse-id
/// limit (a 32-byte Ed25519 `device_id` alone would overflow the old form) and
/// stops C from learning the cleartext `session_id`, while preserving per-session
/// coalescing on the lock screen (the hash is stable per device+session).
fn push_collapse_id(device_id: &str, session_id: &SessionId) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(device_id.as_bytes());
    h.update(b":");
    h.update(session_id.to_string().as_bytes());
    hex::encode(&h.finalize()[..16])
}

/// The `/notify` + `/register` request bodies are the shared protocol wire
/// types, so A and C serialize/deserialize the exact same shapes.
use remote_host_protocol::push::{NotifyRequest, RegisterRequest};

/// Map the pairing-side [`device_proto::pairing::ApnsEnv`] onto the C-wire
/// [`remote_host_protocol::push::ApnsEnv`] (same variants, distinct crates).
fn to_wire_env(env: device_proto::pairing::ApnsEnv) -> remote_host_protocol::push::ApnsEnv {
    match env {
        device_proto::pairing::ApnsEnv::Sandbox => remote_host_protocol::push::ApnsEnv::Sandbox,
        device_proto::pairing::ApnsEnv::Production => {
            remote_host_protocol::push::ApnsEnv::Production
        }
    }
}

/// Compose the error string for a rejected push POST (`op` = `notify` /
/// `register`): the status, a spelled-out reason where the bare status is the
/// whole story C gives, and any response-body excerpt.
fn push_status_error(op: &str, status: reqwest::StatusCode, body: &str) -> String {
    let reason = match status {
        reqwest::StatusCode::FORBIDDEN => Some("signature/counter rejected"),
        reqwest::StatusCode::TOO_MANY_REQUESTS => Some("rate limited"),
        s if s.is_server_error() => Some("remote host upstream error"),
        _ => None,
    };
    let mut msg = format!("{op} status {status}");
    if let Some(reason) = reason {
        msg.push_str(" (");
        msg.push_str(reason);
        msg.push(')');
    }
    let snippet = crate::http_body_snippet(body);
    if !snippet.is_empty() {
        msg.push_str(": ");
        msg.push_str(&snippet);
    }
    msg
}

/// Why a `/notify` POST to C failed. Distinguishes the one outcome the
/// dispatcher can self-heal — C no longer knows the device (HTTP 404), e.g. its
/// in-memory token store was reset by a remote-host restart — from every other
/// (transient) failure, which is just retried on a later turn.
#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    /// C has no binding for this `device_id` (HTTP 404). The dispatcher drops its
    /// stale "already registered" cache, re-registers from durable pairing
    /// material, and retries the push once.
    #[error("remote host does not know this device (404)")]
    DeviceUnknown,
    /// Any other failure (transport, signing, or a non-404 status). Not specially
    /// handled — the next completed turn retries.
    #[error("{0}")]
    Transient(String),
}

/// Seam over the POST to C's `/notify`. The real impl uses reqwest; tests use a
/// mock so the whole dispatch path is host-testable.
#[async_trait::async_trait]
pub trait NotifySink: Send + Sync {
    async fn post(&self, notify_url: &str, body: &NotifyRequest) -> Result<(), NotifyError>;
}

/// reqwest-backed sink POSTing to a per-device `<base>/notify`. The base is
/// derived from the device row's relay URL at dispatch time, so the sink holds
/// only the (proxy-aware) client.
pub struct HttpNotifySink {
    client: reqwest::Client,
}

impl HttpNotifySink {
    /// `client` should be the workspace's proxy-aware client
    /// ([`baybo_security::http::client`]) — these POST to the remote host (C),
    /// a non-loopback egress target subject to the operator's egress proxy.
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl NotifySink for HttpNotifySink {
    async fn post(&self, notify_url: &str, body: &NotifyRequest) -> Result<(), NotifyError> {
        let resp = self
            .client
            .post(notify_url)
            .json(body)
            .send()
            .await
            .map_err(|e| NotifyError::Transient(format!("notify post: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else if status == reqwest::StatusCode::NOT_FOUND {
            // C's in-memory token store has no binding for this device — the
            // self-healable case (see [`NotifyError::DeviceUnknown`]).
            Err(NotifyError::DeviceUnknown)
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(NotifyError::Transient(push_status_error(
                "notify", status, &body,
            )))
        }
    }
}

/// Seam over the POST to C's `/register`. The dispatcher calls it before a
/// `/notify`, using APNs material persisted at pairing, so C can recover from a
/// missing or stale in-memory APNs binding. Real impl uses reqwest; tests use a
/// mock.
#[async_trait::async_trait]
pub trait ApnsRegistrar: Send + Sync {
    async fn register(&self, register_url: &str, body: &RegisterRequest) -> Result<(), String>;
}

/// reqwest-backed registrar POSTing to a per-device `<base>/register`. The base +
/// admission key come from the device row at dispatch time, so it holds only the
/// (proxy-aware) client.
pub struct HttpApnsRegistrar {
    client: reqwest::Client,
}

impl HttpApnsRegistrar {
    /// `client` should be the workspace's proxy-aware client
    /// ([`baybo_security::http::client`]) — `/register` POSTs to the remote
    /// host (C), a non-loopback egress target subject to the egress proxy.
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl ApnsRegistrar for HttpApnsRegistrar {
    async fn register(&self, register_url: &str, body: &RegisterRequest) -> Result<(), String> {
        let resp = self
            .client
            .post(register_url)
            .json(body)
            .send()
            .await
            .map_err(|e| format!("register post: {e}"))?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(push_status_error("register", status, &body))
        }
    }
}

/// A single push destination: the binding `device_id` plus the remote host (C)
/// base URL to POST `/register` + `/notify` to. Built from a paired [`DeviceRow`]
/// (relay path) or a [`web::WebPushBinding`] (direct path) — the builders below
/// are identical for both, since the push material is keyed by `device_id` in the
/// vault the same way (see [`web`]). Push is keyless, so no admission key rides
/// here; the binding is authorized entirely by the Ed25519 delegation chain.
struct PushTarget {
    device_id: String,
    relay_url: String,
}

impl From<&DeviceRow> for PushTarget {
    fn from(d: &DeviceRow) -> Self {
        Self {
            device_id: d.device_id.clone(),
            relay_url: d.relay_url.clone(),
        }
    }
}

impl From<web::WebPushBinding> for PushTarget {
    fn from(b: web::WebPushBinding) -> Self {
        Self {
            device_id: b.device_id,
            relay_url: b.relay_url,
        }
    }
}

/// Composes the stores, vault, and sink into the per-turn dispatch.
pub struct PushDispatcher {
    device_store: Arc<dyn DeviceStore>,
    session_store: Arc<dyn SessionStore>,
    session_manager: Arc<SessionManager>,
    secret_vault: Arc<SecretVault>,
    sink: Arc<dyn NotifySink>,
    /// Re-registers an approved device with C from the material A persisted at
    /// pairing, self-healing a missing or stale remote-host APNs binding.
    apns_registrar: Option<Arc<dyn ApnsRegistrar>>,
    /// The APNs token last registered with C per device this run, so we register
    /// at most once per (device, token): a token that **changed** (APNs rotation,
    /// pushed to A via `POST /v1/mobile/apns-token`) differs from the cached one
    /// and re-registers; an unchanged token is skipped.
    registered: Mutex<HashMap<String, String>>,
    /// A's gateway Ed25519 push-signing key, lazily loaded from the vault and
    /// cached for the dispatcher's lifetime.
    push_signing_key: OnceCell<Arc<delegation::SigningKey>>,
    /// Strictly-increasing replay counter for signed pushes; the vault high-water
    /// (seeded once via `push_counter_seeded`) carries the floor across restarts.
    /// See [`Self::next_counter`].
    push_counter: AtomicU64,
    /// One-shot guard so the vault high-water is read into `push_counter` exactly
    /// once, on the first mint.
    push_counter_seeded: OnceCell<()>,
}

impl PushDispatcher {
    pub fn new(
        device_store: Arc<dyn DeviceStore>,
        session_store: Arc<dyn SessionStore>,
        session_manager: Arc<SessionManager>,
        secret_vault: Arc<SecretVault>,
        sink: Arc<dyn NotifySink>,
        apns_registrar: Option<Arc<dyn ApnsRegistrar>>,
    ) -> Self {
        Self {
            device_store,
            session_store,
            session_manager,
            secret_vault,
            sink,
            apns_registrar,
            registered: Mutex::new(HashMap::new()),
            push_signing_key: OnceCell::new(),
            push_counter: AtomicU64::new(0),
            push_counter_seeded: OnceCell::new(),
        }
    }

    /// The next strictly-increasing replay counter for a signed push. On first
    /// use it lifts the floor to the persisted high-water (so a backward
    /// wall-clock step across a restart can't regress below what C last
    /// accepted), then advances and re-persists it. A globally-increasing value
    /// is per-device increasing too, so C rejects a `/register`/`/notify` whose
    /// counter doesn't exceed the device's last accepted.
    async fn next_counter(&self) -> u64 {
        self.push_counter_seeded
            .get_or_init(|| async {
                if let Ok(Some(secret)) = self.secret_vault.get_secret(PUSH_COUNTER_VAULT_KEY).await
                    && let Ok(bytes) = <[u8; 8]>::try_from(secret.as_bytes())
                {
                    // Lift the floor to the stored high-water (a no-op if the
                    // clock is already ahead of it).
                    mint_counter(&self.push_counter, u64::from_be_bytes(bytes));
                }
            })
            .await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let next = mint_counter(&self.push_counter, now);
        // Persist the new high-water best-effort; pushes are human-paced (≤ one
        // per completed turn per device) so a write per mint is negligible.
        if let Err(e) = self
            .secret_vault
            .store_secret(PUSH_COUNTER_VAULT_KEY, next.to_be_bytes().as_slice())
            .await
        {
            tracing::warn!(
                error = %e,
                "push: failed to persist replay-counter high-water; a clock step across restart \
                 could 403-wedge pushes"
            );
        }
        next
    }

    /// A's gateway push-signing key, loaded from the vault and cached on first push.
    async fn signing_key(&self) -> Result<Arc<delegation::SigningKey>, String> {
        self.push_signing_key
            .get_or_try_init(|| async {
                load_or_create_push_signing_key(&self.secret_vault)
                    .await
                    .map(Arc::new)
                    .map_err(|e| e.to_string())
            })
            .await
            .map(Arc::clone)
    }

    /// True iff this terminal event might buzz a phone: a successfully-
    /// completed turn whose reply a user is meant to read.
    ///
    /// Three kinds qualify — a real user turn, and both halves of the cron
    /// story: a **recurring** fire, whose session is a conversation the user
    /// reads the result in, and the **delivery** of a one-shot's result into
    /// the conversation that scheduled it. A recurring fire is only confirmed
    /// once the session is loaded (`dispatch_completed`) — a cron session that
    /// is *not* a conversation is a private workspace nobody would open.
    /// Everything else (compaction, subagents, their notifications) is
    /// machinery, not a message.
    pub fn should_dispatch(ev: &JobLifecycleEvent) -> bool {
        matches!(ev.phase, JobPhase::Completed { .. })
            && matches!(
                ev.kind,
                JobInputKind::UserChat | JobInputKind::Cron | JobInputKind::CronNotification
            )
    }

    /// Dispatch on the completed edge of a real user turn. The reply's ordinal
    /// rides the event, so there's no per-job cursor to track on other edges.
    pub async fn handle_event(&self, ev: &JobLifecycleEvent) {
        if Self::should_dispatch(ev) {
            self.dispatch_completed(ev).await;
        }
    }

    async fn dispatch_completed(&self, ev: &JobLifecycleEvent) {
        // `handle_event` only calls this on a Completed edge; the reply ordinal
        // rides that variant. No ordinal → no durable reply row to preview
        // (a Structured completion, or a failed reply write) → don't push at all.
        let JobPhase::Completed {
            reply_ordinal: Some(reply_ordinal),
        } = &ev.phase
        else {
            tracing::debug!(
                session = %ev.session_id,
                job = %ev.job_id,
                "push: completed turn has no durable reply row; not pushing"
            );
            return;
        };
        let reply_ordinal = *reply_ordinal;
        // Skip a vanished session (nothing to preview), but otherwise fan out to
        // every approved device — one gateway = one app, so there is no per-user
        // scoping to apply.
        let session = match self.session_manager.get(&ev.session_id).await {
            Ok(Some(session)) => session,
            Ok(None) => {
                tracing::debug!(
                    session = %ev.session_id,
                    "push: session not found for completed turn; skipping"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    session = %ev.session_id,
                    error = %e,
                    "push: session lookup failed for completed turn; push skipped"
                );
                return;
            }
        };
        // A one-shot fire's session is a private workspace: its reply is
        // delivered into the conversation that scheduled it, and *that*
        // delivery pushes (as a `CronNotification`). Pushing here as well would
        // buzz the phone twice for one scheduled task, with the deep link
        // pointing at a session the user cannot even open.
        if ev.kind == JobInputKind::Cron && !session.trigger.is_cron_conversation() {
            tracing::debug!(
                session = %ev.session_id,
                "push: one-shot cron fire pushes from its origin conversation; not pushing here"
            );
            return;
        }
        // Fan out to every approved paired device AND every direct-mode device push
        // binding — one gateway = one user, so there is no per-user scoping.
        // A direct binding is cryptographically identical to a paired-device
        // binding, so both ride the same dispatch path (see [`web`]).
        let device_rows = match self.device_store.list(Some(DeviceStatus::Approved)).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "push: listing approved devices failed; device targets skipped this turn"
                );
                Vec::new()
            }
        };
        let mut targets: Vec<PushTarget> = device_rows.iter().map(PushTarget::from).collect();
        let mut seen: HashSet<String> = targets.iter().map(|t| t.device_id.clone()).collect();
        for binding in web::list_bindings(&self.secret_vault).await {
            // The same phone, paired then switched to direct, can hold both a
            // device row and a web binding for one device_id — push once. The
            // device row (enumerated first) wins.
            if seen.insert(binding.device_id.clone()) {
                targets.push(binding.into());
            }
        }
        if targets.is_empty() {
            tracing::debug!(
                session = %ev.session_id,
                "push: no push targets (no approved device, no web binding); skipping"
            );
            return;
        }
        let preview = self.build_preview(&ev.session_id, reply_ordinal).await;
        for t in &targets {
            match self.dispatch_to_target(t, &ev.session_id, &preview).await {
                // A 2xx from C's `/notify` — the encrypted preview is on its way to
                // APNs. Logged so the push path is observable end to end.
                Ok(()) => {
                    tracing::info!(device = %t.device_id, "push: preview posted to remote host")
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        device = %t.device_id,
                        session = %ev.session_id,
                        "push: notify to remote host failed for this device"
                    )
                }
            }
        }
    }

    async fn dispatch_to_target(
        &self,
        target: &PushTarget,
        session_id: &SessionId,
        preview: &str,
    ) -> Result<(), String> {
        // Both relay and web targets carry a remote-host endpoint; push is plain
        // HTTP, so swap the `wss`/`ws` scheme to `https`/`http`. An empty relay URL
        // means a device row paired before this existed (web bindings always carry
        // the built-in default).
        let base = relay_url_to_http_base(&target.relay_url);
        if base.is_empty() {
            return Err("push target has no relay url (re-pair to populate)".into());
        }
        let notify_url = remote_host_protocol::push::notify_url(&base);
        self.ensure_registered(target, &base).await;
        match self
            .post_notify(target, session_id, preview, &notify_url)
            .await
        {
            Ok(()) => Ok(()),
            // C no longer knows this device — its in-memory token store was reset
            // (typically a remote-host restart). Our "already registered" cache is
            // stale: drop it, re-register from durable material, and retry the
            // push once so delivery self-heals on this very turn instead of
            // 404'ing until A restarts or the APNs token rotates.
            Err(NotifyError::DeviceUnknown) => {
                tracing::info!(
                    device = %target.device_id,
                    "push: remote host lost this device's binding (404); re-registering and \
                     retrying once"
                );
                self.registered.lock().remove(target.device_id.as_str());
                self.ensure_registered(target, &base).await;
                match self
                    .post_notify(target, session_id, preview, &notify_url)
                    .await
                {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        tracing::warn!(
                            device = %target.device_id, error = %e,
                            "push: target still unknown to remote host after re-register; \
                             delivery wedged (missing delegation/apns material?)",
                        );
                        Err(e.to_string())
                    }
                }
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Build + sign the `/notify` body and POST it to `notify_url`. Split out of
    /// [`Self::dispatch_to_device`] so the 404 self-heal path can re-send the same
    /// push (with a fresh, strictly-larger counter) after a forced re-register.
    async fn post_notify(
        &self,
        target: &PushTarget,
        session_id: &SessionId,
        preview: &str,
        notify_url: &str,
    ) -> Result<(), NotifyError> {
        let signing_key = self.signing_key().await.map_err(NotifyError::Transient)?;
        let key = self
            .load_push_key(&target.device_id)
            .await
            .map_err(NotifyError::Transient)?;
        let body = build_notify_body(
            &target.device_id,
            session_id,
            &key,
            preview,
            &signing_key,
            self.next_counter().await,
        )
        .map_err(NotifyError::Transient)?;
        self.sink.post(notify_url, &body).await
    }

    /// Best-effort: register an approved device with C from the material A
    /// persisted at pairing, the first time we push to it this run. Without this
    /// a restarted or pruned remote-host token store leaves C unaware of the
    /// device, so `/notify` would be rejected as unknown until the app registers
    /// again. Cached so it costs at most one `/register` per device per run.
    async fn ensure_registered(&self, target: &PushTarget, base_http: &str) {
        let Some(registrar) = &self.apns_registrar else {
            tracing::debug!(
                device = %target.device_id,
                "push: no APNs registrar wired; register skipped"
            );
            return;
        };
        let device_id = target.device_id.as_str();
        let secret = match self
            .secret_vault
            .get_secret(&device_apns_secret_name(device_id))
            .await
        {
            Ok(Some(secret)) => secret,
            Ok(None) => {
                tracing::debug!(
                    device = %device_id,
                    "push: skipping remote-host register (no APNs material persisted)"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    device = %device_id,
                    error = %e,
                    "push: vault read of APNs material failed; register skipped"
                );
                return;
            }
        };
        let reg = match serde_json::from_slice::<DeviceApnsRegistration>(secret.as_bytes()) {
            Ok(reg) => reg,
            Err(e) => {
                tracing::warn!(
                    device = %device_id,
                    error = %e,
                    "push: persisted APNs registration is malformed; register skipped \
                     (re-pair or reconnect the app to rewrite it)"
                );
                return;
            }
        };
        if reg.apns_token.is_empty() {
            tracing::debug!(
                device = %device_id,
                "push: skipping remote-host register (empty APNs token)"
            );
            return;
        }
        // Already registered this exact token this run → nothing to do. A token
        // that changed (read from the vault after the app pushed a fresh one over
        // the content channel) differs here and re-registers below.
        if self.registered.lock().get(device_id) == Some(&reg.apns_token) {
            return;
        }
        let signing_key = match self.signing_key().await {
            Ok(key) => key,
            Err(e) => {
                tracing::warn!(
                    device = %device_id,
                    error = %e,
                    "push: gateway push signing key unavailable; register skipped"
                );
                return;
            }
        };
        // The binding is authenticated to C by the device's pairing-time
        // delegation plus our gateway push signature. No delegation (older
        // pairing, or it never arrived) → we can't prove ownership of the binding,
        // so skip registration rather than send an unverifiable one.
        let delegation_sig = match self
            .secret_vault
            .get_secret(&device_push_delegation_secret_name(device_id))
            .await
        {
            Ok(Some(sig)) => sig,
            Ok(None) => {
                tracing::debug!(device = %device_id, "push: no delegation stored; skipping register");
                return;
            }
            Err(e) => {
                tracing::warn!(
                    device = %device_id,
                    error = %e,
                    "push: vault read of push delegation failed; register skipped"
                );
                return;
            }
        };
        let body = build_register_body(
            device_id,
            &reg.apns_token,
            reg.apns_env,
            &signing_key,
            delegation_sig.as_bytes(),
            self.next_counter().await,
        );
        let register_url = remote_host_protocol::push::register_url(base_http);
        match registrar.register(&register_url, &body).await {
            Ok(()) => {
                tracing::info!(
                    device = %device_id,
                    apns_env = ?reg.apns_env,
                    token_len = reg.apns_token.len(),
                    counter = body.counter,
                    "push: registered device APNs binding with remote host"
                );
                self.registered
                    .lock()
                    .insert(device_id.to_string(), reg.apns_token);
            }
            Err(e) => {
                tracing::warn!(
                    device = %device_id,
                    register_url = %register_url,
                    error = %e,
                    "push: device registration with remote host failed; notifies will 404 \
                     until it succeeds"
                )
            }
        }
    }

    async fn load_push_key(&self, device_id: &str) -> Result<[u8; aead::KEY_LEN], String> {
        let name = device_push_key_secret_name(device_id);
        let secret = self
            .secret_vault
            .get_secret(&name)
            .await
            .map_err(|e| format!("vault: {e}"))?
            .ok_or_else(|| "no push key for device".to_string())?;
        secret
            .as_bytes()
            .try_into()
            .map_err(|_| "push key wrong length".to_string())
    }

    /// Build the preview JSON from the turn's reply row, read directly at the
    /// `reply_ordinal` the `Completed` event carried (the no-ordinal case is
    /// dropped upstream in `dispatch_completed`, so it never reaches here). No
    /// read-after-write poll: the reply is appended (awaited) before the event
    /// publishes, and the store is a single shared connection, so the row is
    /// already visible here. A missing/non-assistant/text-less row (e.g. a
    /// tool-only reply) yields the generic placeholder — **never** a stale
    /// previous reply.
    async fn build_preview(&self, session_id: &SessionId, reply_ordinal: i64) -> String {
        preview_json(
            self.reply_text_at(session_id, reply_ordinal)
                .await
                .as_deref(),
            session_id,
        )
    }

    /// The assistant text of the reply row at `ordinal`. Pulls the short slice
    /// at/after that ordinal and returns the matching row's first text block.
    async fn reply_text_at(&self, session_id: &SessionId, ordinal: i64) -> Option<String> {
        let rows = self
            .session_store
            .load_active_session_messages_since(session_id, ordinal - 1, PREVIEW_READ_LIMIT)
            .await
            .ok()?;
        rows.into_iter()
            .find(|(ord, _, _)| *ord == ordinal)
            .filter(|(_, _, m)| m.role == Role::Assistant)
            .and_then(|(_, _, m)| {
                m.content.iter().find_map(|cb| match cb {
                    ContentBlock::Text(t) => Some(t.clone()),
                    _ => None,
                })
            })
    }
}

/// The preview JSON the NSE rewrites into `title`/`body`. `None` text yields the
/// generic placeholder (used when the reply row can't be read), so a stale
/// previous-turn reply is never encrypted.
///
/// `session_id` rides INSIDE the sealed plaintext — never the outer APNs
/// payload — so the app can deep-link a notification tap to its conversation
/// while C stays blind to session ids (the same invariant that hashes
/// [`push_collapse_id`]). Older NSE builds ignore the extra field
/// (`JSONDecoder` tolerates unknown keys); newer ones treat it as optional.
fn preview_json(text: Option<&str>, session_id: &SessionId) -> String {
    let body = match text {
        Some(t) => t.chars().take(PREVIEW_MAX_CHARS).collect::<String>(),
        None => "New message".to_string(),
    };
    json!({ "title": "Baybo", "body": body, "session_id": session_id.to_string() }).to_string()
}

/// Pure: AEAD-seal `preview` under `key`, base64 the output, and frame the
/// `/notify` body. Extracted so the encrypt path is unit-testable without any
/// stores.
fn build_notify_body(
    device_id: &str,
    session_id: &SessionId,
    key: &[u8; aead::KEY_LEN],
    preview: &str,
    signing_key: &delegation::SigningKey,
    counter: u64,
) -> Result<NotifyRequest, String> {
    let (nonce, ciphertext) =
        aead::seal(key, preview.as_bytes()).map_err(|e| format!("seal: {e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let enc = b64.encode(&ciphertext);
    let n = b64.encode(&nonce);
    // Sign the notify so C can verify it against the gateway key it stored at
    // register and reject a forged push to this device_id (the routes are keyless).
    let sig = delegation::sign_notify(signing_key, device_id, &enc, &n, device_id, counter);
    Ok(NotifyRequest {
        device_id: device_id.to_string(),
        collapse_id: push_collapse_id(device_id, session_id),
        bid: device_id.to_string(),
        enc,
        n,
        sig: b64.encode(sig.to_bytes()),
        counter,
    })
}

/// Pure: build + sign a `/register` body. The gateway proves ownership of the
/// binding to C with the device's pairing-time `delegation_sig` (device→gateway)
/// plus its own signature over the binding, so C accepts it with no caller key.
fn build_register_body(
    device_id: &str,
    apns_token: &str,
    env: device_proto::pairing::ApnsEnv,
    signing_key: &delegation::SigningKey,
    delegation_sig: &[u8],
    counter: u64,
) -> RegisterRequest {
    let env_byte = match env {
        device_proto::pairing::ApnsEnv::Sandbox => delegation::ENV_SANDBOX,
        device_proto::pairing::ApnsEnv::Production => delegation::ENV_PRODUCTION,
    };
    let sig = delegation::sign_register(signing_key, device_id, apns_token, env_byte, counter);
    let b64 = base64::engine::general_purpose::STANDARD;
    RegisterRequest {
        device_id: device_id.to_string(),
        apns_token: apns_token.to_string(),
        env: to_wire_env(env),
        gateway_pubkey: b64.encode(signing_key.verifying_key().to_bytes()),
        delegation: b64.encode(delegation_sig),
        sig: b64.encode(sig.to_bytes()),
        counter,
    }
}

/// Derive the HTTP base for push (`/notify`, `/register`) from a device row's
/// relay base URL: relay legs dial `wss://`/`ws://`, push POSTs plain HTTP to the
/// same host. Returns the input unchanged when it isn't a ws(s) URL (already
/// `http(s)`, or empty — the caller treats empty as "no relay url recorded").
fn relay_url_to_http_base(relay_url: &str) -> String {
    if let Some(rest) = relay_url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = relay_url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        relay_url.to_string()
    }
}

/// Subscribe to the lifecycle bus and dispatch pushes until `shutdown` fires.
/// Subscribe happens before the spawn returns so no post-subscribe event slips
/// through unobserved.
pub fn spawn<F>(
    dispatcher: Arc<PushDispatcher>,
    job_lifecycle: Arc<JobLifecycle>,
    shutdown: F,
) -> JoinHandle<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let mut events = job_lifecycle.subscribe_lifecycle_events();
    tokio::spawn(async move {
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                _ = &mut shutdown => break,
                recv = events.recv() => match recv {
                    Ok(ev) => {
                        dispatcher.handle_event(&ev).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            skipped = n,
                            "push: dispatcher lagged behind the lifecycle bus; pushes dropped"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
        tracing::debug!("push dispatcher: stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::JobId;

    fn event(phase: JobPhase, kind: JobInputKind) -> JobLifecycleEvent {
        JobLifecycleEvent {
            job_id: JobId::new(),
            session_id: SessionId::from("s1"),
            parent_job_id: None,
            phase,
            kind,
        }
    }

    #[test]
    fn dispatches_only_completed_turns_a_user_is_meant_to_read() {
        // The three buzzing cases: a real user turn, a cron fire (confirmed to
        // be a conversation once the session is loaded), and the delivery of a
        // one-shot fire's result into the conversation that scheduled it.
        for kind in [
            JobInputKind::UserChat,
            JobInputKind::Cron,
            JobInputKind::CronNotification,
        ] {
            assert!(PushDispatcher::should_dispatch(&event(
                JobPhase::Completed {
                    reply_ordinal: None
                },
                kind,
            )));
        }
        // Machinery, not messages.
        for kind in [
            JobInputKind::Compact,
            JobInputKind::Spawned,
            JobInputKind::SubagentNotification,
        ] {
            assert!(!PushDispatcher::should_dispatch(&event(
                JobPhase::Completed {
                    reply_ordinal: None
                },
                kind,
            )));
        }
        // Non-terminal edges, and failed / cancelled turns, don't buzz.
        assert!(!PushDispatcher::should_dispatch(&event(
            JobPhase::Started,
            JobInputKind::UserChat,
        )));
        assert!(!PushDispatcher::should_dispatch(&event(
            JobPhase::Failed,
            JobInputKind::UserChat,
        )));
    }

    #[test]
    fn notify_body_is_decryptable_with_the_push_key() {
        let key = [9u8; aead::KEY_LEN];
        let preview = r#"{"title":"Baybo","body":"the agent finished"}"#;
        let session = SessionId::from("sess-7");
        let signing = delegation::generate_signing_key();
        let body = build_notify_body("dev-1", &session, &key, preview, &signing, 11).unwrap();

        assert_eq!(body.device_id, "dev-1");
        assert_eq!(body.bid, "dev-1");
        assert_eq!(body.counter, 11);
        // collapse_id is now a short hash — it neither equals the raw
        // device_id:session_id nor leaks the session id, but is stable per pair.
        assert_eq!(body.collapse_id, push_collapse_id("dev-1", &session));
        assert_ne!(body.collapse_id, "dev-1:sess-7");

        let b64 = base64::engine::general_purpose::STANDARD;
        // The notify is signed by the gateway key over the exact wire fields.
        let sig = delegation::signature_from_bytes(&b64.decode(&body.sig).unwrap()).unwrap();
        assert!(delegation::verify_notify(
            &signing.verifying_key(),
            &body.device_id,
            &body.enc,
            &body.n,
            &body.bid,
            body.counter,
            &sig,
        ));

        // The NSE recovers the preview from enc/n with the push key.
        let nonce = b64.decode(&body.n).unwrap();
        let ct = b64.decode(&body.enc).unwrap();
        let plaintext = aead::open(&key, &nonce, &ct).unwrap();
        assert_eq!(plaintext, preview.as_bytes());
    }

    #[test]
    fn preview_json_truncates_and_falls_back_to_placeholder() {
        let session = SessionId::from("sess-9");
        let long = "x".repeat(500);
        let v: serde_json::Value =
            serde_json::from_str(&preview_json(Some(&long), &session)).unwrap();
        assert_eq!(v["title"], "Baybo");
        assert_eq!(
            v["body"].as_str().unwrap().chars().count(),
            PREVIEW_MAX_CHARS
        );
        // The tap-routing target rides inside the sealed plaintext (the outer
        // payload must never carry it — C stays blind to session ids).
        assert_eq!(v["session_id"], "sess-9");
        // None → generic placeholder (never a stale previous-turn reply).
        let g: serde_json::Value = serde_json::from_str(&preview_json(None, &session)).unwrap();
        assert_eq!(g["body"], "New message");
        assert_eq!(g["session_id"], "sess-9");
    }

    #[test]
    fn notify_body_uses_a_fresh_nonce_each_time() {
        let key = [1u8; aead::KEY_LEN];
        let s = SessionId::from("s");
        let signing = delegation::generate_signing_key();
        let a = build_notify_body("d", &s, &key, "same", &signing, 1).unwrap();
        let b = build_notify_body("d", &s, &key, "same", &signing, 2).unwrap();
        assert_ne!(a.n, b.n, "nonce must be random per message");
        assert_ne!(a.enc, b.enc, "ciphertext differs under a fresh nonce");
    }

    #[test]
    fn register_body_matches_remote_host_wire_shape() {
        let signing = delegation::generate_signing_key();
        let body = build_register_body(
            "dev-1",
            "tok",
            device_proto::pairing::ApnsEnv::Sandbox,
            &signing,
            &[7u8; 64],
            5,
        );
        let v: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert_eq!(v["device_id"], "dev-1");
        assert_eq!(v["apns_token"], "tok");
        // Must serialize the same as the push role's RegisterRequest.env.
        assert_eq!(v["env"], "sandbox");
        assert_eq!(v["counter"], 5);
        // The signing material rides the wire (base64), so C can verify the chain.
        assert!(!v["gateway_pubkey"].as_str().unwrap().is_empty());
        assert!(!v["delegation"].as_str().unwrap().is_empty());
        assert!(!v["sig"].as_str().unwrap().is_empty());

        // The register signature verifies under the gateway key over the binding.
        let b64 = base64::engine::general_purpose::STANDARD;
        let sig = delegation::signature_from_bytes(&b64.decode(&body.sig).unwrap()).unwrap();
        assert!(delegation::verify_register(
            &signing.verifying_key(),
            "dev-1",
            "tok",
            delegation::ENV_SANDBOX,
            5,
            &sig,
        ));
    }

    #[test]
    fn relay_ws_base_maps_to_http_for_push() {
        // Relay legs dial wss/ws; push POSTs plain HTTP to the same host.
        assert_eq!(
            relay_url_to_http_base("wss://proxy.baybo.space"),
            "https://proxy.baybo.space"
        );
        assert_eq!(
            relay_url_to_http_base("ws://127.0.0.1:8080"),
            "http://127.0.0.1:8080"
        );
        // Already-HTTP or empty is passed through unchanged.
        assert_eq!(relay_url_to_http_base("https://x"), "https://x");
        assert_eq!(relay_url_to_http_base(""), "");
        // …and the protocol builder appends the push path onto the derived base.
        assert_eq!(
            remote_host_protocol::push::register_url(&relay_url_to_http_base(
                "wss://proxy.baybo.space"
            )),
            "https://proxy.baybo.space/register"
        );
    }

    // --- 404 self-heal + durable counter (the remote-host-restart fix) ---

    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    /// A `NotifySink` that returns `DeviceUnknown` for its first `fail_first`
    /// calls (C lost the binding), then `Ok`. Counts every call.
    struct ScriptedSink {
        calls: AtomicUsize,
        fail_first: usize,
    }
    #[async_trait::async_trait]
    impl NotifySink for ScriptedSink {
        async fn post(&self, _url: &str, _body: &NotifyRequest) -> Result<(), NotifyError> {
            let n = self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            if n < self.fail_first {
                Err(NotifyError::DeviceUnknown)
            } else {
                Ok(())
            }
        }
    }

    /// An `ApnsRegistrar` that always succeeds and counts `/register` calls.
    #[derive(Default)]
    struct CountingRegistrar {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl ApnsRegistrar for CountingRegistrar {
        async fn register(&self, _url: &str, _body: &RegisterRequest) -> Result<(), String> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(())
        }
    }

    /// A dispatcher backed by a tempdir sqlite store, with the durable pairing
    /// material `ensure_registered` reads already seeded in the vault.
    async fn test_dispatcher(
        sink: Arc<dyn NotifySink>,
        registrar: Arc<dyn ApnsRegistrar>,
    ) -> (Arc<PushDispatcher>, DeviceRow, tempfile::TempDir) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let stores = baybo_storage::Store::open(&tempdir.path().join("push-test.db"))
            .await
            .expect("open store");
        let session_manager = Arc::new(SessionManager::new(
            stores.session.clone(),
            stores.session_summary.clone(),
            stores.session_folder.clone(),
        ));
        let vault = Arc::new(baybo_security::SecretVault::new(
            baybo_security::EncryptionKey::new(b"test-master-key-32-bytes-long!!!".to_vec())
                .expect("test key"),
            stores.secret.clone(),
        ));
        let device_id = "dev-1";
        vault
            .store_secret(
                &device_apns_secret_name(device_id),
                &serde_json::to_vec(&DeviceApnsRegistration {
                    apns_token: "apns-tok".into(),
                    apns_env: device_proto::pairing::ApnsEnv::Sandbox,
                })
                .unwrap(),
            )
            .await
            .unwrap();
        vault
            .store_secret(
                &device_push_key_secret_name(device_id),
                &[7u8; aead::KEY_LEN],
            )
            .await
            .unwrap();
        vault
            .store_secret(&device_push_delegation_secret_name(device_id), &[9u8; 64])
            .await
            .unwrap();
        let dispatcher = Arc::new(PushDispatcher::new(
            stores.device.clone(),
            stores.session.clone(),
            session_manager,
            vault,
            sink,
            Some(registrar),
        ));
        let row = DeviceRow {
            device_id: device_id.into(),
            device_pubkey: vec![1u8; 32],
            auth_token: "auth".into(),
            status: DeviceStatus::Approved,
            rendezvous_id: None,
            created_at: 0,
            approved_at: Some(0),
            last_seen_at: None,
            relay_url: "ws://127.0.0.1:9".into(),
            remote_api_key: "key-A".into(),
        };
        (dispatcher, row, tempdir)
    }

    #[tokio::test]
    async fn notify_404_invalidates_cache_reregisters_and_retries() {
        let sink = Arc::new(ScriptedSink {
            calls: AtomicUsize::new(0),
            fail_first: 1,
        });
        let registrar = Arc::new(CountingRegistrar::default());
        let (dispatcher, row, _td) = test_dispatcher(
            Arc::clone(&sink) as Arc<dyn NotifySink>,
            Arc::clone(&registrar) as Arc<dyn ApnsRegistrar>,
        )
        .await;

        let res = dispatcher
            .dispatch_to_target(&PushTarget::from(&row), &SessionId::from("s1"), "preview")
            .await;
        assert!(res.is_ok(), "push self-heals after a 404: {res:?}");
        // Two notify attempts (the 404 then the retry) and two registers (the
        // initial ensure + the forced re-register after cache invalidation).
        assert_eq!(sink.calls.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(registrar.calls.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn notify_404_persisting_after_reregister_surfaces_an_error() {
        let sink = Arc::new(ScriptedSink {
            calls: AtomicUsize::new(0),
            fail_first: 99,
        });
        let registrar = Arc::new(CountingRegistrar::default());
        let (dispatcher, row, _td) = test_dispatcher(
            Arc::clone(&sink) as Arc<dyn NotifySink>,
            Arc::clone(&registrar) as Arc<dyn ApnsRegistrar>,
        )
        .await;

        let res = dispatcher
            .dispatch_to_target(&PushTarget::from(&row), &SessionId::from("s1"), "preview")
            .await;
        assert!(res.is_err(), "a persistent 404 still surfaces an error");
        // Exactly one retry — never an unbounded loop.
        assert_eq!(sink.calls.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(registrar.calls.load(AtomicOrdering::SeqCst), 2);
    }

    /// A `NotifySink` that records the last body it was posted, so a test can
    /// assert the dispatched ciphertext decrypts under the binding's push key.
    struct CapturingSink(parking_lot::Mutex<Option<NotifyRequest>>);
    #[async_trait::async_trait]
    impl NotifySink for CapturingSink {
        async fn post(&self, _url: &str, body: &NotifyRequest) -> Result<(), NotifyError> {
            *self.0.lock() = Some(body.clone());
            Ok(())
        }
    }

    /// End-to-end for the direct-mode path: a registered direct push binding is a
    /// valid [`PushTarget`] whose dispatched `/notify` decrypts under the
    /// client's push key — proving the direct binding reuses the device notify
    /// pipeline byte-for-byte.
    #[tokio::test]
    async fn web_binding_dispatch_yields_decryptable_notify() {
        let sink = Arc::new(CapturingSink(parking_lot::Mutex::new(None)));
        let registrar = Arc::new(CountingRegistrar::default());
        let (dispatcher, _row, _td) = test_dispatcher(
            Arc::clone(&sink) as Arc<dyn NotifySink>,
            Arc::clone(&registrar) as Arc<dyn ApnsRegistrar>,
        )
        .await;

        // Register a web binding (mirrors what POST /v1/push/register persists):
        // a client Ed25519 identity, a random push key, APNs token, delegation.
        let push_key = [5u8; aead::KEY_LEN];
        let device = delegation::generate_signing_key();
        let device_id = delegation::device_id_for(&device.verifying_key());
        let binding = web::WebPushBinding {
            device_id: device_id.clone(),
            relay_url: "ws://127.0.0.1:9".into(),
            created_at: 0,
        };
        web::store_binding(
            &dispatcher.secret_vault,
            &binding,
            &push_key,
            "apns-tok",
            device_proto::pairing::ApnsEnv::Sandbox,
            &[7u8; 64],
        )
        .await
        .unwrap();

        // The binding is enumerable and dispatches like any device target.
        assert_eq!(web::list_bindings(&dispatcher.secret_vault).await.len(), 1);
        let preview = r#"{"title":"Baybo","body":"hi"}"#;
        dispatcher
            .dispatch_to_target(&PushTarget::from(binding), &SessionId::from("s1"), preview)
            .await
            .unwrap();

        // The captured notify is addressed to the web bid and decrypts under the
        // client's push key — exactly what the NSE does on-device.
        let body = sink.0.lock().clone().expect("a notify was posted");
        assert_eq!(body.device_id, device_id);
        assert_eq!(body.bid, device_id);
        let b64 = base64::engine::general_purpose::STANDARD;
        let nonce = b64.decode(&body.n).unwrap();
        let ct = b64.decode(&body.enc).unwrap();
        assert_eq!(
            aead::open(&push_key, &nonce, &ct).unwrap(),
            preview.as_bytes()
        );
    }

    #[test]
    fn mint_counter_is_strictly_increasing_even_when_the_clock_steps_back() {
        let c = AtomicU64::new(0);
        assert_eq!(mint_counter(&c, 1000), 1000);
        // Clock jumps backward: the prior+1 floor wins, never a regression.
        assert_eq!(mint_counter(&c, 500), 1001);
        // Equal-to-last clock still advances.
        assert_eq!(mint_counter(&c, 1001), 1002);
        // Clock jumps forward past the floor: takes the clock value.
        assert_eq!(mint_counter(&c, 5000), 5000);
    }

    #[tokio::test]
    async fn next_counter_mints_above_the_persisted_high_water() {
        // A prior run advanced + persisted a high counter; then the wall clock
        // stepped back and the process restarted. The new dispatcher must mint
        // ABOVE the stored high-water, never below it (GR-1).
        let sink = Arc::new(ScriptedSink {
            calls: AtomicUsize::new(0),
            fail_first: 0,
        });
        let registrar = Arc::new(CountingRegistrar::default());
        let (dispatcher, _row, _td) = test_dispatcher(
            Arc::clone(&sink) as Arc<dyn NotifySink>,
            Arc::clone(&registrar) as Arc<dyn ApnsRegistrar>,
        )
        .await;
        // Well above any plausible current unix-nanos, so only the high-water (not
        // the wall clock) can satisfy the floor.
        let high = u64::MAX - 10;
        dispatcher
            .secret_vault
            .store_secret(PUSH_COUNTER_VAULT_KEY, high.to_be_bytes().as_slice())
            .await
            .unwrap();
        let next = dispatcher.next_counter().await;
        assert!(next > high, "minted {next} must exceed high-water {high}");
    }
}
