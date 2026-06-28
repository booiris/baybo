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

use std::collections::HashSet;

use base64::Engine;
use baybo_job::{JobInputKind, JobLifecycle, JobLifecycleEvent, JobPhase, JobShape};
use baybo_model::{ContentBlock, Role, SessionId};
use baybo_security::SecretVault;
use baybo_session::SessionManager;
use baybo_store::{DeviceRow, DeviceStatus, DeviceStore, SessionStore};
use device_proto::aead;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

/// Max preview characters (kept well under the 4 KB APNs payload once
/// encrypted + base64'd).
const PREVIEW_MAX_CHARS: usize = 200;
/// `kid` epoch — always 0 today (the field exists so rotation needs no
/// payload change).
const PUSH_KEY_EPOCH: u32 = 0;
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
    async fn register_device(
        &self,
        register_url: &str,
        remote_api_key: &str,
        device_id: &str,
        apns_token: &str,
        env: device_proto::pairing::ApnsEnv,
    ) -> Result<(), String>;
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
    async fn register_device(
        &self,
        register_url: &str,
        remote_api_key: &str,
        device_id: &str,
        apns_token: &str,
        env: device_proto::pairing::ApnsEnv,
    ) -> Result<(), String> {
        let body = RegisterRequest {
            remote_api_key: remote_api_key.to_string(),
            device_id: device_id.to_string(),
            apns_token: apns_token.to_string(),
            env: to_wire_env(env),
        };
        let resp = self
            .client
            .post(register_url)
            .json(&body)
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
    /// device_ids re-registered with C this run (so we retry at most once per
    /// device per dispatcher lifetime).
    registered: Mutex<HashSet<String>>,
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
            registered: Mutex::new(HashSet::new()),
        }
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
        let key = self.load_push_key(&device.device_id).await?;
        let body = build_notify_body(
            &device.remote_api_key,
            &device.device_id,
            session_id,
            &key,
            preview,
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
        if self.registered.lock().contains(device_id) {
            return;
        }
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
        let register_url = remote_host_protocol::push::register_url(base_http);
        match registrar
            .register_device(
                &register_url,
                &device.remote_api_key,
                device_id,
                &reg.apns_token,
                reg.apns_env,
            )
            .await
        {
            Ok(()) => {
                self.registered.lock().insert(device_id.to_string());
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
fn build_notify_body(
    remote_api_key: &str,
    device_id: &str,
    session_id: &SessionId,
    key: &[u8; aead::KEY_LEN],
    preview: &str,
) -> Result<NotifyRequest, String> {
    let (nonce, ciphertext) =
        aead::seal(key, preview.as_bytes()).map_err(|e| format!("seal: {e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD;
    Ok(NotifyRequest {
        remote_api_key: remote_api_key.to_string(),
        device_id: device_id.to_string(),
        collapse_id: format!("{device_id}:{session_id}"),
        kid: PUSH_KEY_EPOCH,
        bid: device_id.to_string(),
        enc: b64.encode(&ciphertext),
        n: b64.encode(&nonce),
    })
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
        let body = build_notify_body("inst-A", "dev-1", &SessionId::from("sess-7"), &key, preview)
            .unwrap();

        assert_eq!(body.remote_api_key, "inst-A");
        assert_eq!(body.device_id, "dev-1");
        assert_eq!(body.bid, "dev-1");
        assert_eq!(body.collapse_id, "dev-1:sess-7");
        assert_eq!(body.kid, 0);

        // The NSE recovers the preview from enc/n with the push key.
        let b64 = base64::engine::general_purpose::STANDARD;
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
        let a = build_notify_body("i", "d", &s, &key, "same").unwrap();
        let b = build_notify_body("i", "d", &s, &key, "same").unwrap();
        assert_ne!(a.n, b.n, "nonce must be random per message");
        assert_ne!(a.enc, b.enc, "ciphertext differs under a fresh nonce");
    }

    #[test]
    fn register_body_matches_remote_host_wire_shape() {
        let body = RegisterRequest {
            remote_api_key: "inst-A".into(),
            device_id: "dev-1".into(),
            apns_token: "tok".into(),
            env: remote_host_protocol::push::ApnsEnv::Sandbox,
        };
        let v: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert_eq!(v["remote_api_key"], "inst-A");
        assert_eq!(v["device_id"], "dev-1");
        assert_eq!(v["apns_token"], "tok");
        // Must serialize the same as the push role's RegisterRequest.env.
        assert_eq!(v["env"], "sandbox");
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
