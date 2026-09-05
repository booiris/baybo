//! One resident card service: a bun child speaking NDJSON over stdio,
//! plus the parent-side pumps that police its emits and serve its host
//! RPCs (fetch / exec).
//!
//! The child runs directly on the host (no sandbox — the card author is
//! the operator's own trusted agent, the same one trusted to run `Bash`;
//! see the trust model in `docs/modules/deck.md`), inheriting the host
//! environment so a card reaches host state and CLIs exactly like the
//! channel sidecars. `bun` itself is located by
//! [`baybo_process::HostTool`], not by the inherited `PATH` alone — a
//! daemon under a service manager gets that manager's `PATH`, not the
//! operator's. Effects still funnel through the parent by
//! convention — `ctx.fetch` is a host-mediated stdio round-trip so the
//! secret-placeholder reveal + audit keep working — but that is now a
//! convenience of the SDK, not an enforced boundary. Per-call timeouts
//! fail the *call*, never the process; process death is the supervisor's
//! concern.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use crate::error::{DeckError, Result};

/// Per-op-call wall clock: the call fails, the process survives.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Bun cold start + module import budget before the spawn counts as a crash.
pub const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Byte cap on an accepted emit payload / op result (the snapshot cap).
pub const SNAPSHOT_MAX_BYTES: usize = 5 * 1024 * 1024;
/// Emits arriving faster than the clamp are coalesced; more than this many
/// coalesced inside one window is an emit flood (a quarantine strike). Sized
/// against the 1s emit floor so a legitimately lively (e.g. game) pusher
/// doesn't trip quarantine — only a pathological tight loop does.
pub const EMIT_FLOOD_MAX: u64 = 1000;

const PREAMBLE_JS: &str = include_str!("sdk/preamble.js");
const PREAMBLE_FILE: &str = "preamble.js";

/// Host-side capability RPCs served to the child. Implemented by
/// [`crate::host::DeckHost`] (fetch with SSRF floor + placeholder reveal; host
/// exec; and the blob plane — `docs/modules/deck.md` §Blobs). The blob-writing
/// methods take `uploader_card_id` separately from the process `card_id`: the
/// two are equal for a live service but DIVERGE in the dry-run gate, whose
/// process id is a throwaway `gate-<uuid>` while its blobs must be stamped with
/// the eventual real card id so purge/GC can find them.
#[async_trait]
pub(crate) trait HostServices: Send + Sync + 'static {
    async fn fetch(
        &self,
        card_id: &str,
        req: HostFetchRequest,
    ) -> std::result::Result<HostFetchResponse, String>;
    async fn exec(
        &self,
        card_id: &str,
        cmd: String,
    ) -> std::result::Result<HostExecResponse, String>;
    /// Fetch an external URL, streaming the response straight into the blob
    /// store — the bytes never enter the child. Returns only the capability
    /// ref.
    async fn fetch_blob(
        &self,
        uploader_card_id: &str,
        req: HostFetchRequest,
    ) -> std::result::Result<HostBlobRef, String>;
    /// Stream a file from disk into the store (an `exec`-produced artifact).
    /// `card_id` resolves the relative-path base (the process scratch);
    /// `uploader_card_id` stamps the identity.
    async fn blob_put_file(
        &self,
        card_id: &str,
        uploader_card_id: &str,
        req: HostBlobPutFileRequest,
    ) -> std::result::Result<HostBlobRef, String>;
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct HostFetchRequest {
    pub url: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct HostFetchResponse {
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub body: String,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct HostExecResponse {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// The capability ref a blob-producing RPC returns to the card.
#[derive(Debug, serde::Serialize)]
pub(crate) struct HostBlobRef {
    pub blob_id: String,
    pub content_type: String,
    pub size: u64,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct HostBlobPutFileRequest {
    pub path: String,
    #[serde(default)]
    pub content_type: Option<String>,
}

/// Where accepted emits go (the manager: size check → response-schema
/// check → snapshot row → broadcast). Returning `Err` marks the emit
/// invalid; the caller then reports it through [`EmitSink::reject`].
#[async_trait]
pub(crate) trait EmitSink: Send + Sync + 'static {
    async fn emit(&self, card_id: &str, payload: Value) -> std::result::Result<(), String>;

    /// Surface an emit the gateway refused. Without this a rejected tick is
    /// invisible: the tile keeps painting its last accepted snapshot, which
    /// reads as "nothing changed" rather than "this card is broken", and a
    /// card whose payload drifts off its own declared schema can sit frozen
    /// for days behind a single `warn!`.
    async fn reject(&self, card_id: &str, reason: &str);
}

/// Strike recorder the supervisor consults for quarantine decisions.
#[derive(Default)]
pub(crate) struct StrikeRecorder {
    crashes: Mutex<Vec<Instant>>,
    timeouts: Mutex<Vec<Instant>>,
}

pub const QUARANTINE_WINDOW: Duration = Duration::from_secs(600);
pub const QUARANTINE_CRASHES: usize = 5;
pub const QUARANTINE_TIMEOUTS: usize = 10;

impl StrikeRecorder {
    fn push_and_count(list: &Mutex<Vec<Instant>>) -> usize {
        let mut list = list.lock();
        let now = Instant::now();
        list.push(now);
        list.retain(|t| now.duration_since(*t) <= QUARANTINE_WINDOW);
        list.len()
    }

    /// Record a crash; true if the crash budget is exhausted.
    pub fn record_crash(&self) -> bool {
        Self::push_and_count(&self.crashes) >= QUARANTINE_CRASHES
    }

    /// Record a call timeout / emit flood; true if the budget is exhausted.
    pub fn record_timeout(&self) -> bool {
        Self::push_and_count(&self.timeouts) >= QUARANTINE_TIMEOUTS
    }
}

enum CallReply {
    Ok(Value),
    Failed(String),
}

/// Cheap-clone handle for issuing op calls against the running child.
#[derive(Clone)]
pub(crate) struct ServiceHandle {
    card_id: String,
    writer: mpsc::Sender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<CallReply>>>>,
    next_call_id: Arc<AtomicU64>,
    strikes: Arc<StrikeRecorder>,
}

impl ServiceHandle {
    /// Invoke one op with [`CALL_TIMEOUT`]. A timeout fails the call and
    /// records a strike; the process itself survives.
    pub async fn call(&self, op: &str, params: Value) -> Result<Value> {
        let id = self.next_call_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, tx);
        let line = serde_json::json!({"type": "call", "id": id, "op": op, "params": params});
        if self.writer.send(format!("{line}\n")).await.is_err() {
            self.pending.lock().remove(&id);
            return Err(DeckError::ServiceUnavailable(
                "service process is not running".into(),
            ));
        }
        match tokio::time::timeout(CALL_TIMEOUT, rx).await {
            Ok(Ok(CallReply::Ok(v))) => {
                let size = serde_json::to_vec(&v).map(|b| b.len()).unwrap_or(0);
                if size > SNAPSHOT_MAX_BYTES {
                    return Err(DeckError::OpRejected(format!(
                        "op result exceeds {SNAPSHOT_MAX_BYTES} bytes"
                    )));
                }
                Ok(v)
            }
            Ok(Ok(CallReply::Failed(e))) => Err(DeckError::ServiceUnavailable(format!(
                "op `{op}` failed: {e}"
            ))),
            Ok(Err(_)) => Err(DeckError::ServiceUnavailable(
                "service process exited mid-call".into(),
            )),
            Err(_) => {
                self.pending.lock().remove(&id);
                if self.strikes.record_timeout() {
                    tracing::warn!(card = %self.card_id, "deck: call-timeout budget exhausted");
                }
                Err(DeckError::ServiceUnavailable(format!(
                    "op `{op}` timed out after {CALL_TIMEOUT:?}"
                )))
            }
        }
    }
}

/// A spawned, initialized child. `exited` resolves when the process dies;
/// dropping/using `kill` tears it down.
pub(crate) struct RunningService {
    pub handle: ServiceHandle,
    pub exited: oneshot::Receiver<i32>,
    pub kill: mpsc::Sender<()>,
}

pub(crate) struct SpawnConfig {
    /// Process identity: names the scratch cwd, the tracing `card=` field, and
    /// the `ctx.exec` working dir. For the dry-run gate this is a throwaway
    /// `gate-<uuid>`.
    pub card_id: String,
    /// Blob-uploader identity (`deck:<uploader_card_id>`). Equals `card_id` for
    /// a live service; the gate sets it to the eventual real card id so its
    /// blobs are reclaimable. See [`HostServices`].
    pub uploader_card_id: String,
    pub bundle_dir: PathBuf,
    pub scratch_dir: PathBuf,
    /// Effective emit clamp (manifest floor already applied).
    pub emit_interval: Duration,
    pub process_manager: Arc<baybo_process::ProcessManager>,
}

/// Spawn + init one card service on the host. Waits for the child's
/// `ready` (module imported, `ops` export present) before returning, so a
/// boot failure surfaces here — with the child's stderr folded into the
/// error — instead of as a dead handle.
pub(crate) async fn spawn_service(
    cfg: SpawnConfig,
    host: Arc<dyn HostServices>,
    emit_sink: Arc<dyn EmitSink>,
    strikes: Arc<StrikeRecorder>,
) -> Result<RunningService> {
    std::fs::create_dir_all(&cfg.scratch_dir)?;
    let preamble_path = cfg.scratch_dir.join(PREAMBLE_FILE);
    std::fs::write(&preamble_path, PREAMBLE_JS)?;
    let service_js = cfg.bundle_dir.join(crate::bundle::SERVICE_FILE);

    // Runs directly on the host, inheriting the environment so a card
    // can reach host state and CLIs. `TMPDIR` etc. are inherited.
    let bun = baybo_process::HostTool::bun();
    let mut cmd = Command::new(bun.path());
    cmd.arg(&preamble_path)
        .arg(&service_js)
        .current_dir(&cfg.bundle_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cfg
        .process_manager
        .spawn(&mut cmd, format!("deck-service:{}", cfg.card_id))
        .map_err(|e| DeckError::HostToolMissing(bun.launch_failure(&e)))?;
    let stdin = child
        .take_stdin()
        .ok_or_else(|| DeckError::Internal("child stdin unavailable".into()))?;
    let stdout = child
        .take_stdout()
        .ok_or_else(|| DeckError::Internal("child stdout unavailable".into()))?;
    let stderr = child.take_stderr();

    let (writer_tx, mut writer_rx) = mpsc::channel::<String>(64);
    let mut stdin = stdin;
    tokio::spawn(async move {
        while let Some(line) = writer_rx.recv().await {
            if stdin.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            if stdin.flush().await.is_err() {
                break;
            }
        }
    });

    // Capture a stderr tail for crash diagnostics.
    let stderr_tail: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    if let Some(stderr) = stderr {
        let tail = stderr_tail.clone();
        let card = cfg.card_id.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(card = %card, "deck service stderr: {line}");
                let mut t = tail.lock();
                t.push_str(&line);
                t.push('\n');
                let len = t.len();
                if len > 4096 {
                    let cut = len - 4096;
                    t.drain(..cut);
                }
            }
        });
    }

    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<CallReply>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let (ready_tx, ready_rx) = oneshot::channel::<std::result::Result<(), String>>();
    let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));

    let handle = ServiceHandle {
        card_id: cfg.card_id.clone(),
        writer: writer_tx.clone(),
        pending: pending.clone(),
        next_call_id: Arc::new(AtomicU64::new(1)),
        strikes: strikes.clone(),
    };

    // Reader pump: dispatch child → parent messages.
    {
        let card_id = cfg.card_id.clone();
        let uploader_card_id = cfg.uploader_card_id.clone();
        let pending = pending.clone();
        let writer = writer_tx.clone();
        let ready_tx = ready_tx.clone();
        let emit = EmitPolicy::new(
            cfg.card_id.clone(),
            cfg.emit_interval,
            emit_sink,
            strikes.clone(),
        );
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    tracing::debug!(card = %card_id, "deck service emitted non-JSON line");
                    continue;
                };
                match msg.get("type").and_then(Value::as_str) {
                    Some("ready") => {
                        if let Some(tx) = ready_tx.lock().take() {
                            let _ = tx.send(Ok(()));
                        }
                    }
                    Some("fatal") => {
                        let err = msg
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("fatal")
                            .to_string();
                        if let Some(tx) = ready_tx.lock().take() {
                            let _ = tx.send(Err(err));
                        }
                    }
                    Some("result") => {
                        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                            continue;
                        };
                        let Some(tx) = pending.lock().remove(&id) else {
                            continue;
                        };
                        let ok = msg.get("ok").and_then(Value::as_bool).unwrap_or(false);
                        let reply = if ok {
                            CallReply::Ok(msg.get("value").cloned().unwrap_or(Value::Null))
                        } else {
                            CallReply::Failed(
                                msg.get("error")
                                    .and_then(Value::as_str)
                                    .unwrap_or("op failed")
                                    .to_string(),
                            )
                        };
                        let _ = tx.send(reply);
                    }
                    Some("emit") => {
                        emit.on_emit(msg.get("payload").cloned().unwrap_or(Value::Null))
                            .await;
                    }
                    Some("fetch") => {
                        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                            continue;
                        };
                        let req = serde_json::from_value::<HostFetchRequest>(msg.clone());
                        let host = host.clone();
                        let writer = writer.clone();
                        let card = card_id.clone();
                        tokio::spawn(async move {
                            let outcome = match req {
                                Ok(req) => host
                                    .fetch(&card, req)
                                    .await
                                    .map(|r| serde_json::to_value(r).unwrap_or(Value::Null)),
                                Err(e) => Err(format!("malformed fetch request: {e}")),
                            };
                            let _ = writer.send(host_result_line(id, outcome)).await;
                        });
                    }
                    Some("exec") => {
                        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                            continue;
                        };
                        let cmd = msg
                            .get("cmd")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let host = host.clone();
                        let writer = writer.clone();
                        let card = card_id.clone();
                        tokio::spawn(async move {
                            let outcome = host
                                .exec(&card, cmd)
                                .await
                                .map(|r| serde_json::to_value(r).unwrap_or(Value::Null));
                            let _ = writer.send(host_result_line(id, outcome)).await;
                        });
                    }
                    Some("fetchBlob") => {
                        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                            continue;
                        };
                        let req = serde_json::from_value::<HostFetchRequest>(msg.clone());
                        let host = host.clone();
                        let writer = writer.clone();
                        let uploader = uploader_card_id.clone();
                        tokio::spawn(async move {
                            let outcome = match req {
                                Ok(req) => host
                                    .fetch_blob(&uploader, req)
                                    .await
                                    .map(|r| serde_json::to_value(r).unwrap_or(Value::Null)),
                                Err(e) => Err(format!("malformed fetchBlob request: {e}")),
                            };
                            let _ = writer.send(host_result_line(id, outcome)).await;
                        });
                    }
                    Some("blobPutFile") => {
                        let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                            continue;
                        };
                        let req = serde_json::from_value::<HostBlobPutFileRequest>(msg.clone());
                        let host = host.clone();
                        let writer = writer.clone();
                        let card = card_id.clone();
                        let uploader = uploader_card_id.clone();
                        tokio::spawn(async move {
                            let outcome = match req {
                                Ok(req) => host
                                    .blob_put_file(&card, &uploader, req)
                                    .await
                                    .map(|r| serde_json::to_value(r).unwrap_or(Value::Null)),
                                Err(e) => Err(format!("malformed blobPutFile request: {e}")),
                            };
                            let _ = writer.send(host_result_line(id, outcome)).await;
                        });
                    }
                    Some("log") => {
                        let level = msg.get("level").and_then(Value::as_str).unwrap_or("info");
                        let text = msg.get("msg").and_then(Value::as_str).unwrap_or_default();
                        if level == "error" || level == "warn" {
                            tracing::warn!(card = %card_id, "deck service: {text}");
                        } else {
                            tracing::debug!(card = %card_id, "deck service: {text}");
                        }
                    }
                    _ => {}
                }
            }
            // Stream closed: fail every pending call.
            for (_, tx) in pending.lock().drain() {
                let _ = tx.send(CallReply::Failed("service process exited".into()));
            }
        });
    }

    // Init + wait ready.
    let init = serde_json::json!({"type": "init"});
    let _ = writer_tx.send(format!("{init}\n")).await;

    // Waiter task owns the child: kill signal or natural death.
    let (kill_tx, mut kill_rx) = mpsc::channel::<()>(1);
    let (exit_tx, mut exit_rx) = oneshot::channel::<i32>();
    tokio::spawn(async move {
        let status = tokio::select! {
            status = child.wait() => status,
            _ = kill_rx.recv() => {
                let _ = child.start_kill();
                child.wait().await
            }
        };
        let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
        let _ = exit_tx.send(code);
    });

    match tokio::time::timeout(READY_TIMEOUT, ready_rx).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(err))) => {
            let _ = kill_tx.send(()).await;
            let _ = (&mut exit_rx).await;
            let tail = stderr_tail.lock().clone();
            return Err(DeckError::DryRun(format!(
                "service failed to boot: {err}{}",
                if tail.is_empty() {
                    String::new()
                } else {
                    format!("\nstderr:\n{tail}")
                }
            )));
        }
        Ok(Err(_)) | Err(_) => {
            let _ = kill_tx.send(()).await;
            let _ = (&mut exit_rx).await;
            let tail = stderr_tail.lock().clone();
            return Err(DeckError::DryRun(format!(
                "service did not become ready within {READY_TIMEOUT:?}{}",
                if tail.is_empty() {
                    String::new()
                } else {
                    format!("\nstderr:\n{tail}")
                }
            )));
        }
    }

    Ok(RunningService {
        handle,
        exited: exit_rx,
        kill: kill_tx,
    })
}

fn host_result_line(id: u64, outcome: std::result::Result<Value, String>) -> String {
    let msg = match outcome {
        Ok(value) => {
            serde_json::json!({"type": "host_result", "id": id, "ok": true, "value": value})
        }
        Err(error) => {
            serde_json::json!({"type": "host_result", "id": id, "ok": false, "error": error})
        }
    };
    format!("{msg}\n")
}

/// Emit ingestion policing: the service owns its clock, so the gateway
/// polices acceptance — at most one accepted emit per clamp window,
/// excess coalesced to latest and flushed when the window reopens.
/// Floods count toward quarantine.
struct EmitPolicy {
    card_id: String,
    interval: Duration,
    sink: Arc<dyn EmitSink>,
    strikes: Arc<StrikeRecorder>,
    state: Arc<tokio::sync::Mutex<EmitState>>,
}

#[derive(Default)]
struct EmitState {
    last_accepted: Option<Instant>,
    pending_latest: Option<Value>,
    coalesced_in_window: u64,
    flush_scheduled: bool,
}

impl EmitPolicy {
    fn new(
        card_id: String,
        interval: Duration,
        sink: Arc<dyn EmitSink>,
        strikes: Arc<StrikeRecorder>,
    ) -> Arc<Self> {
        Arc::new(Self {
            card_id,
            interval,
            sink,
            strikes,
            state: Arc::new(tokio::sync::Mutex::new(EmitState::default())),
        })
    }

    async fn accept(&self, payload: Value) {
        if let Err(e) = self.sink.emit(&self.card_id, payload).await {
            tracing::warn!(card = %self.card_id, "deck: emit rejected: {e}");
            self.sink.reject(&self.card_id, &e).await;
        }
    }

    async fn on_emit(self: &Arc<Self>, payload: Value) {
        let mut st = self.state.lock().await;
        let now = Instant::now();
        let open = st
            .last_accepted
            .is_none_or(|t| now.duration_since(t) >= self.interval);
        if open && !st.flush_scheduled {
            st.last_accepted = Some(now);
            st.coalesced_in_window = 0;
            drop(st);
            self.accept(payload).await;
            return;
        }
        st.pending_latest = Some(payload);
        st.coalesced_in_window += 1;
        if st.coalesced_in_window == EMIT_FLOOD_MAX && self.strikes.record_timeout() {
            tracing::warn!(card = %self.card_id, "deck: emit flood; strike budget exhausted");
        }
        if !st.flush_scheduled {
            st.flush_scheduled = true;
            let this = self.clone();
            let wait = st
                .last_accepted
                .map(|t| self.interval.saturating_sub(now.duration_since(t)))
                .unwrap_or(self.interval);
            drop(st);
            tokio::spawn(async move {
                tokio::time::sleep(wait).await;
                let mut st = this.state.lock().await;
                st.flush_scheduled = false;
                if let Some(latest) = st.pending_latest.take() {
                    st.last_accepted = Some(Instant::now());
                    st.coalesced_in_window = 0;
                    drop(st);
                    this.accept(latest).await;
                }
            });
        }
    }
}
