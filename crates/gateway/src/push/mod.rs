//! A-side push dispatcher.
//!
//! Subscribes to the `JobLifecycle` broadcast bus and, for each
//! **successfully-completed real user turn** (`phase == Completed`,
//! `shape == Turn`, `kind == UserChat` — which excludes Cron / System /
//! Spawned / SubagentNotification *and* `/compact`, a `UserChat`-input but
//! `Maintenance`-shape job), encrypts a short preview **per approved device**
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

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use std::collections::HashMap;

use base64::Engine;
use baybo_job::{JobInputKind, JobLifecycle, JobLifecycleEvent, JobPhase, JobShape};
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
/// gateway can't prove ownership to C under a shared `remote_api_key`).
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

/// Strictly-increasing replay counter for signed pushes, seeded from the wall
/// clock so it keeps rising across restarts; the atomic guarantees strict
/// monotonicity within a process even if two pushes read the same instant. A
/// globally-increasing value is per-device increasing too, so C can reject a
/// `/register`/`/notify` whose counter doesn't exceed the device's last accepted.
static PUSH_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_push_counter() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut last = PUSH_COUNTER.load(Ordering::Relaxed);
    loop {
        let next = now.max(last.wrapping_add(1));
        match PUSH_COUNTER.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
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

/// Seam over the POST to C's `/notify`. The real impl uses reqwest; tests use a
/// mock so the whole dispatch path is host-testable.
#[async_trait::async_trait]
pub trait NotifySink: Send + Sync {
    async fn post(&self, notify_url: &str, body: &NotifyRequest) -> Result<(), String>;
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
    async fn post(&self, notify_url: &str, body: &NotifyRequest) -> Result<(), String> {
        let resp = self
            .client
            .post(notify_url)
            .json(body)
            .send()
            .await
            .map_err(|e| format!("notify post: {e}"))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("notify status {}", resp.status()))
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
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("register status {}", resp.status()))
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
    /// pushed to A over the content channel via `Frame::UpdateApnsToken`) differs
    /// from the cached one and re-registers; an unchanged token is skipped.
    registered: Mutex<HashMap<String, String>>,
    /// A's gateway Ed25519 push-signing key, lazily loaded from the vault and
    /// cached for the dispatcher's lifetime.
    push_signing_key: OnceCell<Arc<delegation::SigningKey>>,
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
        }
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

    /// True iff this terminal event should buzz a phone: a successfully-
    /// completed real user turn. The `shape == Turn` gate is what excludes
    /// `/compact`.
    pub fn should_dispatch(ev: &JobLifecycleEvent) -> bool {
        matches!(ev.phase, JobPhase::Completed { .. })
            && ev.shape == JobShape::Turn
            && ev.kind == JobInputKind::UserChat
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
            return;
        };
        let reply_ordinal = *reply_ordinal;
        // Skip a vanished session (nothing to preview), but otherwise fan out to
        // every approved device — one gateway = one app, so there is no per-user
        // scoping to apply.
        if self
            .session_manager
            .get(&ev.session_id)
            .await
            .ok()
            .flatten()
            .is_none()
        {
            return;
        }
        let devices = self
            .device_store
            .list(Some(DeviceStatus::Approved))
            .await
            .unwrap_or_default();
        if devices.is_empty() {
            return;
        }
        let preview = self.build_preview(&ev.session_id, reply_ordinal).await;
        for d in &devices {
            match self.dispatch_to_device(d, &ev.session_id, &preview).await {
                // A 2xx from C's `/notify` — the encrypted preview is on its way to
                // APNs. Logged so the push path is observable end to end.
                Ok(()) => {
                    tracing::debug!(device = %d.device_id, "push: preview posted to remote host")
                }
                Err(e) => {
                    tracing::debug!(error = %e, device = %d.device_id, "push: skipped a device")
                }
            }
        }
    }

    async fn dispatch_to_device(
        &self,
        device: &DeviceRow,
        session_id: &SessionId,
        preview: &str,
    ) -> Result<(), String> {
        // Relay and push share the device's recorded endpoint + admission key;
        // push is plain HTTP, so swap the relay's `wss`/`ws` scheme to `https`/
        // `http`. An empty relay URL means a row paired before this existed.
        let base = relay_url_to_http_base(&device.relay_url);
        if base.is_empty() {
            return Err("device row has no relay url (re-pair to populate)".into());
        }
        self.ensure_registered(device, &base).await;
        let signing_key = self.signing_key().await?;
        let key = self.load_push_key(&device.device_id).await?;
        let body = build_notify_body(
            &device.remote_api_key,
            &device.device_id,
            session_id,
            &key,
            preview,
            &signing_key,
            next_push_counter(),
        )?;
        let notify_url = remote_host_protocol::push::notify_url(&base);
        self.sink.post(&notify_url, &body).await
    }

    /// Best-effort: register an approved device with C from the material A
    /// persisted at pairing, the first time we push to it this run. Without this
    /// a restarted or pruned remote-host token store leaves C unaware of the
    /// device, so `/notify` would be rejected as unknown until the app registers
    /// again. Cached so it costs at most one `/register` per device per run.
    async fn ensure_registered(&self, device: &DeviceRow, base_http: &str) {
        let Some(registrar) = &self.apns_registrar else {
            return;
        };
        let device_id = device.device_id.as_str();
        let Ok(Some(secret)) = self
            .secret_vault
            .get_secret(&device_apns_secret_name(device_id))
            .await
        else {
            return;
        };
        let Ok(reg) = serde_json::from_slice::<DeviceApnsRegistration>(secret.as_bytes()) else {
            return;
        };
        if reg.apns_token.is_empty() {
            return;
        }
        // Already registered this exact token this run → nothing to do. A token
        // that changed (read from the vault after the app pushed a fresh one over
        // the content channel) differs here and re-registers below.
        if self.registered.lock().get(device_id) == Some(&reg.apns_token) {
            return;
        }
        // The binding is authenticated to C by the device's pairing-time
        // delegation plus our gateway push signature. No delegation (older
        // pairing, or it never arrived) → we can't prove ownership under a shared
        // `remote_api_key`, so skip registration rather than send an unverifiable one.
        let Ok(signing_key) = self.signing_key().await else {
            return;
        };
        let Ok(Some(delegation_sig)) = self
            .secret_vault
            .get_secret(&device_push_delegation_secret_name(device_id))
            .await
        else {
            tracing::debug!(device = %device_id, "push: no delegation stored; skipping register");
            return;
        };
        let body = build_register_body(
            &device.remote_api_key,
            device_id,
            &reg.apns_token,
            reg.apns_env,
            &signing_key,
            delegation_sig.as_bytes(),
            next_push_counter(),
        );
        let register_url = remote_host_protocol::push::register_url(base_http);
        match registrar.register(&register_url, &body).await {
            Ok(()) => {
                self.registered
                    .lock()
                    .insert(device_id.to_string(), reg.apns_token);
            }
            Err(e) => {
                tracing::debug!(error = %e, "push: device re-registration with remote host failed; will retry")
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
            .find(|(ord, _)| *ord == ordinal)
            .filter(|(_, m)| m.role == Role::Assistant)
            .and_then(|(_, m)| {
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
fn preview_json(text: Option<&str>) -> String {
    let body = match text {
        Some(t) => t.chars().take(PREVIEW_MAX_CHARS).collect::<String>(),
        None => "New message".to_string(),
    };
    json!({ "title": "Baybo", "body": body }).to_string()
}

/// Pure: AEAD-seal `preview` under `key`, base64 the output, and frame the
/// `/notify` body. Extracted so the encrypt path is unit-testable without any
/// stores.
#[allow(clippy::too_many_arguments)]
fn build_notify_body(
    remote_api_key: &str,
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
    // Sign the notify so C can reject a co-tenant's push to this device_id under a
    // shared `remote_api_key` (verified against the gateway key C stored at register).
    let sig = delegation::sign_notify(signing_key, device_id, &enc, &n, device_id, counter);
    Ok(NotifyRequest {
        remote_api_key: remote_api_key.to_string(),
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
/// plus its own signature over the binding, so C accepts it even under a shared
/// `remote_api_key`.
fn build_register_body(
    remote_api_key: &str,
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
        remote_api_key: remote_api_key.to_string(),
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
                        tracing::debug!(skipped = n, "push dispatcher lagged; buzz(es) dropped");
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

    fn event(phase: JobPhase, shape: JobShape, kind: JobInputKind) -> JobLifecycleEvent {
        JobLifecycleEvent {
            job_id: JobId::new(),
            session_id: SessionId::from("s1"),
            parent_job_id: None,
            phase,
            kind,
            shape,
        }
    }

    #[test]
    fn dispatches_only_completed_user_turns() {
        // The one buzzing case.
        assert!(PushDispatcher::should_dispatch(&event(
            JobPhase::Completed {
                reply_ordinal: None
            },
            JobShape::Turn,
            JobInputKind::UserChat,
        )));
        // `/compact` — UserChat input but Maintenance shape — must NOT buzz.
        assert!(!PushDispatcher::should_dispatch(&event(
            JobPhase::Completed {
                reply_ordinal: None
            },
            JobShape::Maintenance,
            JobInputKind::UserChat,
        )));
        // Non-terminal, and non-user inputs.
        assert!(!PushDispatcher::should_dispatch(&event(
            JobPhase::Started,
            JobShape::Turn,
            JobInputKind::UserChat,
        )));
        for kind in [
            JobInputKind::Cron,
            JobInputKind::System,
            JobInputKind::Spawned,
            JobInputKind::SubagentNotification,
        ] {
            assert!(!PushDispatcher::should_dispatch(&event(
                JobPhase::Completed {
                    reply_ordinal: None
                },
                JobShape::Turn,
                kind,
            )));
        }
        // Failed / cancelled turns don't buzz either.
        assert!(!PushDispatcher::should_dispatch(&event(
            JobPhase::Failed,
            JobShape::Turn,
            JobInputKind::UserChat,
        )));
    }

    #[test]
    fn notify_body_is_decryptable_with_the_push_key() {
        let key = [9u8; aead::KEY_LEN];
        let preview = r#"{"title":"Baybo","body":"the agent finished"}"#;
        let session = SessionId::from("sess-7");
        let signing = delegation::generate_signing_key();
        let body =
            build_notify_body("inst-A", "dev-1", &session, &key, preview, &signing, 11).unwrap();

        assert_eq!(body.remote_api_key, "inst-A");
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
        let long = "x".repeat(500);
        let v: serde_json::Value = serde_json::from_str(&preview_json(Some(&long))).unwrap();
        assert_eq!(v["title"], "Baybo");
        assert_eq!(
            v["body"].as_str().unwrap().chars().count(),
            PREVIEW_MAX_CHARS
        );
        // None → generic placeholder (never a stale previous-turn reply).
        let g: serde_json::Value = serde_json::from_str(&preview_json(None)).unwrap();
        assert_eq!(g["body"], "New message");
    }

    #[test]
    fn notify_body_uses_a_fresh_nonce_each_time() {
        let key = [1u8; aead::KEY_LEN];
        let s = SessionId::from("s");
        let signing = delegation::generate_signing_key();
        let a = build_notify_body("i", "d", &s, &key, "same", &signing, 1).unwrap();
        let b = build_notify_body("i", "d", &s, &key, "same", &signing, 2).unwrap();
        assert_ne!(a.n, b.n, "nonce must be random per message");
        assert_ne!(a.enc, b.enc, "ciphertext differs under a fresh nonce");
    }

    #[test]
    fn register_body_matches_remote_host_wire_shape() {
        let signing = delegation::generate_signing_key();
        let body = build_register_body(
            "inst-A",
            "dev-1",
            "tok",
            device_proto::pairing::ApnsEnv::Sandbox,
            &signing,
            &[7u8; 64],
            5,
        );
        let v: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert_eq!(v["remote_api_key"], "inst-A");
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
}
