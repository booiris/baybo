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
//! Modeled on `spawn_turn_state_projector`: subscribe synchronously, then a
//! `select!` loop over the bus with `Lagged`/`Closed` handling (push is
//! best-effort, so a lag just drops that buzz).

use std::sync::Arc;

use std::collections::HashMap;
use std::time::Duration;

use device_proto::aead;
use aura_job::{JobInputKind, JobLifecycle, JobLifecycleEvent, JobPhase, JobShape};
use aura_model::{ContentBlock, JobId, Role, SessionId};
use aura_security::SecretVault;
use aura_session::SessionManager;
use aura_store::{DeviceStatus, DeviceStore, SessionStore};
use base64::Engine;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::json;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

/// Max preview characters (kept well under the 4 KB APNs payload once
/// encrypted + base64'd).
const PREVIEW_MAX_CHARS: usize = 200;
/// Push-key length in bytes.
const PUSH_KEY_LEN: usize = 32;
/// `kid` epoch — always 0 in phase 1 (the field exists so rotation needs no
/// payload change).
const PHASE1_KID: u32 = 0;
/// Read-after-write: how many times to re-check for a fresh assistant row
/// before falling back to the generic placeholder.
const PREVIEW_READ_RETRIES: u32 = 5;
/// Backoff between read-after-write re-checks.
const PREVIEW_READ_BACKOFF: Duration = Duration::from_millis(100);

/// The blind `/notify` body A POSTs to C. `enc`/`n` are base64 of the
/// per-device AEAD output; C copies them verbatim into the APNs payload.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NotifyBody {
    pub instance_key: String,
    pub device_id: String,
    pub collapse_id: String,
    pub kid: u32,
    pub bid: String,
    pub enc: String,
    pub n: String,
}

/// Seam over the POST to C's `/notify`. The real impl uses reqwest; tests use a
/// mock so the whole dispatch path is host-testable.
#[async_trait::async_trait]
pub trait NotifySink: Send + Sync {
    async fn post(&self, body: &NotifyBody) -> Result<(), String>;
}

/// reqwest-backed sink POSTing to `<gateway_url>/notify`.
pub struct HttpNotifySink {
    client: reqwest::Client,
    notify_url: String,
}

impl HttpNotifySink {
    pub fn new(gateway_url: &str) -> Self {
        Self {
            client: reqwest::Client::new(),
            notify_url: format!("{}/notify", gateway_url.trim_end_matches('/')),
        }
    }
}

#[async_trait::async_trait]
impl NotifySink for HttpNotifySink {
    async fn post(&self, body: &NotifyBody) -> Result<(), String> {
        let resp = self
            .client
            .post(&self.notify_url)
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

/// Body A POSTs to C's `/register` to bind a device's APNs token. Matches the
/// remote-host push role's `RegisterRequest` JSON shape (agreed by contract; C
/// is a separate workspace). `env` serializes to `"sandbox"`/`"production"`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct RegisterBody {
    instance_key: String,
    device_id: String,
    apns_token: String,
    env: device_proto::pairing::ApnsEnv,
}

/// Seam over the POST to C's `/register`. The device-pair route calls it
/// (best-effort, gateway-mediated) after a successful handshake so the app
/// never holds a C credential. Real impl uses reqwest; tests use a mock.
#[async_trait::async_trait]
pub trait ApnsRegistrar: Send + Sync {
    async fn register_device(
        &self,
        device_id: &str,
        apns_token: &str,
        env: device_proto::pairing::ApnsEnv,
    ) -> Result<(), String>;
}

/// reqwest-backed registrar POSTing to `<gateway_url>/register`.
pub struct HttpApnsRegistrar {
    client: reqwest::Client,
    register_url: String,
    instance_key: String,
}

impl HttpApnsRegistrar {
    pub fn new(gateway_url: &str, instance_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            register_url: format!("{}/register", gateway_url.trim_end_matches('/')),
            instance_key: instance_key.into(),
        }
    }
}

#[async_trait::async_trait]
impl ApnsRegistrar for HttpApnsRegistrar {
    async fn register_device(
        &self,
        device_id: &str,
        apns_token: &str,
        env: device_proto::pairing::ApnsEnv,
    ) -> Result<(), String> {
        let body = RegisterBody {
            instance_key: self.instance_key.clone(),
            device_id: device_id.to_string(),
            apns_token: apns_token.to_string(),
            env,
        };
        let resp = self
            .client
            .post(&self.register_url)
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
    instance_key: String,
    /// `job_id → session ordinal at turn start`, captured on the `Started`
    /// edge so the completed-turn preview can wait for a row newer than this
    /// (the read-after-write gate). Dropped on any terminal edge.
    start_cursors: Mutex<HashMap<JobId, i64>>,
}

impl PushDispatcher {
    pub fn new(
        device_store: Arc<dyn DeviceStore>,
        session_store: Arc<dyn SessionStore>,
        session_manager: Arc<SessionManager>,
        secret_vault: Arc<SecretVault>,
        sink: Arc<dyn NotifySink>,
        instance_key: impl Into<String>,
    ) -> Self {
        Self {
            device_store,
            session_store,
            session_manager,
            secret_vault,
            sink,
            instance_key: instance_key.into(),
            start_cursors: Mutex::new(HashMap::new()),
        }
    }

    /// True iff this terminal event should buzz a phone: a successfully-
    /// completed real user turn. The `shape == Turn` gate is what excludes
    /// `/compact`.
    pub fn should_dispatch(ev: &JobLifecycleEvent) -> bool {
        ev.phase == JobPhase::Completed
            && ev.shape == JobShape::Turn
            && ev.kind == JobInputKind::UserChat
    }

    /// Process one lifecycle event; returns how many devices were notified.
    /// Tracks the turn's start cursor (for read-after-write) and dispatches on
    /// the completed edge.
    pub async fn handle_event(&self, ev: &JobLifecycleEvent) -> usize {
        // Only real user turns are relevant at all.
        if ev.shape != JobShape::Turn || ev.kind != JobInputKind::UserChat {
            return 0;
        }
        match ev.phase {
            JobPhase::Started => {
                let cursor = self
                    .session_store
                    .latest_session_ordinal(&ev.session_id)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(0);
                self.start_cursors.lock().insert(ev.job_id, cursor);
                0
            }
            JobPhase::Completed => self.dispatch_completed(ev).await,
            JobPhase::Failed | JobPhase::Cancelled => {
                self.start_cursors.lock().remove(&ev.job_id);
                0
            }
        }
    }

    async fn dispatch_completed(&self, ev: &JobLifecycleEvent) -> usize {
        let start_cursor = self.start_cursors.lock().remove(&ev.job_id);
        let Some(session) = self.session_manager.get(&ev.session_id).await.ok().flatten() else {
            return 0;
        };
        let devices = self
            .device_store
            .list_for_user(&session.user.id, Some(DeviceStatus::Approved))
            .await
            .unwrap_or_default();
        if devices.is_empty() {
            return 0;
        }
        let preview = self.build_preview(&ev.session_id, start_cursor).await;
        let mut sent = 0;
        for d in devices {
            match self.dispatch_to_device(&d.device_id, &ev.session_id, &preview).await {
                Ok(()) => sent += 1,
                Err(e) => tracing::debug!(error = %e, "push: skipped a device"),
            }
        }
        sent
    }

    async fn dispatch_to_device(
        &self,
        device_id: &str,
        session_id: &SessionId,
        preview: &str,
    ) -> Result<(), String> {
        let key = self.load_push_key(device_id).await?;
        let body = build_notify_body(&self.instance_key, device_id, session_id, &key, preview)?;
        self.sink.post(&body).await
    }

    async fn load_push_key(&self, device_id: &str) -> Result<[u8; PUSH_KEY_LEN], String> {
        let name = format!("device.{device_id}.push_key");
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

    /// Build the preview JSON from the session's last assistant message, gated
    /// on **read-after-write**: the `Completed` lifecycle event can fire before
    /// the assistant message row is durable, so a naive read could encrypt the
    /// *previous* turn's reply. We re-check (bounded) until a session row newer
    /// than the turn's start cursor has landed, then take the last assistant
    /// text; on expiry we send the generic placeholder, **never** stale text.
    async fn build_preview(&self, session_id: &SessionId, start_cursor: Option<i64>) -> String {
        for attempt in 0..PREVIEW_READ_RETRIES {
            let latest = self
                .session_store
                .latest_session_ordinal(session_id)
                .await
                .ok()
                .flatten();
            if landed(latest, start_cursor)
                && let Some(text) = self.last_assistant_text(session_id).await
            {
                return preview_json(Some(&text));
            }
            if attempt + 1 < PREVIEW_READ_RETRIES {
                tokio::time::sleep(PREVIEW_READ_BACKOFF).await;
            }
        }
        preview_json(None)
    }

    async fn last_assistant_text(&self, session_id: &SessionId) -> Option<String> {
        let messages = self
            .session_store
            .load_active_session_messages(session_id)
            .await
            .ok()?;
        messages.iter().rev().find(|m| m.role == Role::Assistant).and_then(|m| {
            m.content.iter().find_map(|cb| match cb {
                ContentBlock::Text(t) => Some(t.clone()),
                _ => None,
            })
        })
    }
}

/// Read-after-write decision: has a session row newer than the turn's start
/// cursor landed? With no start cursor (the dispatcher missed the `Started`
/// edge — e.g. it booted mid-turn) we accept best-effort.
fn landed(latest: Option<i64>, start_cursor: Option<i64>) -> bool {
    match (latest, start_cursor) {
        (Some(l), Some(s)) => l > s,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// The preview JSON the NSE rewrites into `title`/`body`. `None` text yields the
/// generic placeholder (used when the read-after-write gate never sees a fresh
/// reply), so a stale previous-turn reply is never encrypted.
fn preview_json(text: Option<&str>) -> String {
    let body = match text {
        Some(t) => t.chars().take(PREVIEW_MAX_CHARS).collect::<String>(),
        None => "New message".to_string(),
    };
    json!({ "title": "Aura", "body": body }).to_string()
}

/// Pure: AEAD-seal `preview` under `key`, base64 the output, and frame the
/// `/notify` body. Extracted so the encrypt path is unit-testable without any
/// stores.
fn build_notify_body(
    instance_key: &str,
    device_id: &str,
    session_id: &SessionId,
    key: &[u8; PUSH_KEY_LEN],
    preview: &str,
) -> Result<NotifyBody, String> {
    let (nonce, ciphertext) = aead::seal(key, preview.as_bytes()).map_err(|e| format!("seal: {e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD;
    Ok(NotifyBody {
        instance_key: instance_key.to_string(),
        device_id: device_id.to_string(),
        collapse_id: format!("{device_id}:{session_id}"),
        kid: PHASE1_KID,
        bid: device_id.to_string(),
        enc: b64.encode(&ciphertext),
        n: b64.encode(&nonce),
    })
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
                        let _ = dispatcher.handle_event(&ev).await;
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
    use aura_model::JobId;

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
            JobPhase::Completed,
            JobShape::Turn,
            JobInputKind::UserChat,
        )));
        // `/compact` — UserChat input but Maintenance shape — must NOT buzz.
        assert!(!PushDispatcher::should_dispatch(&event(
            JobPhase::Completed,
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
                JobPhase::Completed,
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
        let key = [9u8; PUSH_KEY_LEN];
        let preview = r#"{"title":"Aura","body":"the agent finished"}"#;
        let body =
            build_notify_body("inst-A", "dev-1", &SessionId::from("sess-7"), &key, preview).unwrap();

        assert_eq!(body.instance_key, "inst-A");
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
    fn read_after_write_gate_waits_for_a_newer_row() {
        assert!(landed(Some(5), Some(4)), "a newer row landed → go");
        assert!(!landed(Some(4), Some(4)), "no new row since turn start → wait");
        assert!(!landed(Some(3), Some(4)), "stale latest → wait");
        assert!(landed(Some(9), None), "no start cursor → best-effort accept");
        assert!(!landed(None, Some(4)), "empty session → wait");
    }

    #[test]
    fn preview_json_truncates_and_falls_back_to_placeholder() {
        let long = "x".repeat(500);
        let v: serde_json::Value = serde_json::from_str(&preview_json(Some(&long))).unwrap();
        assert_eq!(v["title"], "Aura");
        assert_eq!(v["body"].as_str().unwrap().chars().count(), PREVIEW_MAX_CHARS);
        // None → generic placeholder (never a stale previous-turn reply).
        let g: serde_json::Value = serde_json::from_str(&preview_json(None)).unwrap();
        assert_eq!(g["body"], "New message");
    }

    #[test]
    fn notify_body_uses_a_fresh_nonce_each_time() {
        let key = [1u8; PUSH_KEY_LEN];
        let s = SessionId::from("s");
        let a = build_notify_body("i", "d", &s, &key, "same").unwrap();
        let b = build_notify_body("i", "d", &s, &key, "same").unwrap();
        assert_ne!(a.n, b.n, "nonce must be random per message");
        assert_ne!(a.enc, b.enc, "ciphertext differs under a fresh nonce");
    }

    #[test]
    fn register_body_matches_remote_host_wire_shape() {
        let body = RegisterBody {
            instance_key: "inst-A".into(),
            device_id: "dev-1".into(),
            apns_token: "tok".into(),
            env: device_proto::pairing::ApnsEnv::Sandbox,
        };
        let v: serde_json::Value = serde_json::to_value(&body).unwrap();
        assert_eq!(v["instance_key"], "inst-A");
        assert_eq!(v["device_id"], "dev-1");
        assert_eq!(v["apns_token"], "tok");
        // Must serialize the same as the push role's RegisterRequest.env.
        assert_eq!(v["env"], "sandbox");
    }

    #[test]
    fn register_url_is_derived_from_gateway_url() {
        let r = HttpApnsRegistrar::new("https://remote.example/", "inst-A");
        assert_eq!(r.register_url, "https://remote.example/register");
    }
}
