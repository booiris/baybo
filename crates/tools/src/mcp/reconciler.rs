use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aura_security::SecretVault;
use aura_storage::BlobStore;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::approval::ResourceAccess;
use crate::mcp::config::{McpFile, McpServerEntry, McpTransportConfig};
use crate::mcp::embedded::EmbeddedMcpServer;
use crate::mcp::transport::{McpServerSession, connect_with_extra_env};
use crate::{Tool, ToolRegistry};

const TICK_INTERVAL: Duration = Duration::from_secs(5);

const EMBEDDED_BACKOFF_MIN: Duration = Duration::from_millis(500);
const EMBEDDED_BACKOFF_MAX: Duration = Duration::from_secs(30);

struct Connected {
    session: McpServerSession,
    identity_hash: u64,
}

/// One target the reconciler wants connected on the next tick. Built
/// from either a `.mcp.json` user entry or an [`EmbeddedMcpServer`]
/// the boot path supplied; held in a single `name → Desired` map so
/// the diff logic doesn't have to special-case the two sources.
#[derive(Clone)]
struct Desired {
    entry: McpServerEntry,
    /// Boot-config env to merge onto the child's env (after the
    /// secret-vault load). Empty for user entries.
    extra_env: HashMap<String, String>,
    /// Hash of (entry, extra_env) — identity changes mean reconnect.
    identity_hash: u64,
    /// True when this came from `EmbeddedMcpServer`. Embedded entries
    /// run with restart-on-disconnect backoff and skip the slow user-
    /// config tick semantics.
    is_embedded: bool,
}

#[derive(Default)]
struct EmbeddedBackoff {
    next_attempt_at: Option<Instant>,
    current_delay: Option<Duration>,
}

pub struct McpReconciler {
    workspace_root: PathBuf,
    registry: Arc<ToolRegistry>,
    vault: Arc<SecretVault>,
    blob_store: Option<Arc<dyn BlobStore>>,
    embedded: Vec<EmbeddedMcpServer>,
    cancel: CancellationToken,
    notify: Arc<Notify>,
    state: Mutex<HashMap<String, Connected>>,
    backoff: Mutex<HashMap<String, EmbeddedBackoff>>,
}

impl McpReconciler {
    pub fn new(
        workspace_root: PathBuf,
        registry: Arc<ToolRegistry>,
        vault: Arc<SecretVault>,
        blob_store: Option<Arc<dyn BlobStore>>,
        embedded: Vec<EmbeddedMcpServer>,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            workspace_root,
            registry,
            vault,
            blob_store,
            embedded,
            cancel,
            notify: Arc::new(Notify::new()),
            state: Mutex::new(HashMap::new()),
            backoff: Mutex::new(HashMap::new()),
        })
    }

    /// Wake the reconciler immediately. CLI commands call this after a
    /// successful `aura mcp add/remove` so the gateway picks up changes
    /// without waiting for the next tick. Best-effort: missing the
    /// notify just means the next periodic tick will absorb the change.
    pub fn poke(&self) {
        self.notify.notify_one();
    }

    /// Run one reconciliation pass. Returns the names of servers
    /// connected after the pass.
    pub async fn tick(self: Arc<Self>) -> Vec<String> {
        // Track whether the user file load succeeded so the disconnect
        // step can preserve existing user-server connections when the
        // file is mid-write / parse-broken / read-failed. Without this,
        // a single transient `.mcp.json` glitch tears down every live
        // user MCP server until the operator notices and fixes it.
        let user_load = McpFile::load(&self.workspace_root).await;
        let user_entries_loaded = user_load.is_ok();
        let user_entries = match user_load {
            Ok(f) => f.servers,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "mcp reconciler: failed to load .mcp.json — preserving previous user entries; \
                     embedded entries continue to reconcile",
                );
                Vec::new()
            }
        };

        // Merge user + embedded into a single desired map. Embedded
        // names override user names on collision (the embedded server
        // ships with the binary; a colliding user entry would have been
        // a misconfiguration that left the wrong code spawned).
        let mut desired: HashMap<String, Desired> = HashMap::new();
        for entry in user_entries {
            let identity_hash = identity_hash(&entry, &HashMap::new());
            desired.insert(
                entry.name.clone(),
                Desired {
                    entry,
                    extra_env: HashMap::new(),
                    identity_hash,
                    is_embedded: false,
                },
            );
        }
        let embedded_names: HashSet<String> =
            self.embedded.iter().map(|e| e.entry.name.clone()).collect();
        for emb in &self.embedded {
            if desired.contains_key(&emb.entry.name) {
                tracing::warn!(
                    server = %emb.entry.name,
                    ".mcp.json entry shadows an embedded server with the same name; dropping the user entry"
                );
            }
            let identity_hash = identity_hash(&emb.entry, &emb.extra_env);
            desired.insert(
                emb.entry.name.clone(),
                Desired {
                    entry: emb.entry.clone(),
                    extra_env: emb.extra_env.clone(),
                    identity_hash,
                    is_embedded: true,
                },
            );
        }

        let current_keys: HashSet<String> = self.state.lock().keys().cloned().collect();
        let desired_keys: HashSet<String> = desired.keys().cloned().collect();

        for name in current_keys.difference(&desired_keys) {
            // Embedded names are always authoritative — if one disappeared
            // from the desired set, drop it. User names are only
            // authoritatively absent when the file actually loaded; a
            // load failure must not propagate as "user removed this".
            if should_disconnect(name, user_entries_loaded, &embedded_names) {
                self.disconnect_server(name).await;
            }
        }

        // Probe embedded entries for liveness so a crashed child
        // surfaces as disconnect-and-reconnect rather than silently
        // staying in the state map. User-configured stdio servers keep
        // today's "no probe" semantics (a manual `aura mcp add` re-edit
        // triggers reconciliation).
        let probe_targets: Vec<String> = {
            let state = self.state.lock();
            self.embedded
                .iter()
                .filter(|e| state.contains_key(&e.entry.name))
                .map(|e| e.entry.name.clone())
                .collect()
        };
        for name in probe_targets {
            let peer = self.state.lock().get(&name).map(|c| c.session.peer());
            if let Some(peer) = peer {
                // Cheapest no-op: re-list tools. Returns Err on a
                // dead transport (broken pipe to a stdio child that
                // exited).
                if let Err(e) = peer.list_all_tools().await {
                    tracing::warn!(
                        server = %name,
                        error = %e,
                        "embedded mcp server probe failed; disconnecting and scheduling reconnect"
                    );
                    self.disconnect_server(&name).await;
                    self.bump_backoff(&name);
                }
            }
        }

        for name in desired_keys {
            let target = desired.get(&name).expect("present").clone();
            let needs_reconnect = self
                .state
                .lock()
                .get(&name)
                .is_none_or(|c| c.identity_hash != target.identity_hash);
            if !needs_reconnect {
                continue;
            }

            // Embedded entries respect their own backoff schedule so a
            // server that immediately crashes after spawn doesn't burn
            // CPU spawning the child every 5s.
            if target.is_embedded {
                let now = Instant::now();
                let ready = self
                    .backoff
                    .lock()
                    .get(&name)
                    .and_then(|b| b.next_attempt_at)
                    .is_none_or(|deadline| now >= deadline);
                if !ready {
                    continue;
                }
            }

            self.disconnect_server(&name).await;
            match self
                .connect_server(
                    &target.entry,
                    &target.extra_env,
                    target.identity_hash,
                    target.is_embedded,
                )
                .await
            {
                Ok(()) => {
                    if target.is_embedded {
                        self.backoff.lock().remove(&name);
                    }
                }
                Err(e) => {
                    tracing::warn!(server = %target.entry.name, error = %e, "mcp reconciler: connect failed");
                    if target.is_embedded {
                        self.bump_backoff(&target.entry.name);
                    }
                }
            }
        }

        self.state.lock().keys().cloned().collect()
    }

    pub fn spawn(self: &Arc<Self>) -> JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            // Initial pass at startup.
            let _ = Arc::clone(&this).tick().await;
            loop {
                tokio::select! {
                    _ = this.cancel.cancelled() => break,
                    _ = tokio::time::sleep(TICK_INTERVAL) => {}
                    _ = this.notify.notified() => {}
                }
                let _ = Arc::clone(&this).tick().await;
            }
            this.shutdown().await;
        })
    }

    pub async fn shutdown(self: Arc<Self>) {
        let (names, sessions) = {
            let mut state = self.state.lock();
            let names: Vec<String> = state.keys().cloned().collect();
            let sessions: Vec<McpServerSession> = names
                .iter()
                .filter_map(|n| state.remove(n).map(|c| c.session))
                .collect();
            (names, sessions)
        };
        for n in &names {
            self.registry.unregister_for_source(n);
        }
        for s in sessions {
            s.shutdown().await;
        }
    }

    async fn disconnect_server(self: &Arc<Self>, name: &str) {
        let session = self.state.lock().remove(name).map(|c| c.session);
        self.registry.unregister_for_source(name);
        if let Some(s) = session {
            s.shutdown().await;
        }
    }

    fn bump_backoff(&self, name: &str) {
        let mut backoff = self.backoff.lock();
        let entry = backoff.entry(name.to_string()).or_default();
        let next = match entry.current_delay {
            None => EMBEDDED_BACKOFF_MIN,
            Some(d) => (d * 2).min(EMBEDDED_BACKOFF_MAX),
        };
        entry.current_delay = Some(next);
        entry.next_attempt_at = Some(Instant::now() + next);
    }

    async fn connect_server(
        self: &Arc<Self>,
        entry: &McpServerEntry,
        extra_env: &HashMap<String, String>,
        identity_hash: u64,
        is_embedded: bool,
    ) -> crate::mcp::McpResult<()> {
        // Defense-in-depth trust gate: never spawn / connect an entry
        // that wouldn't have been accepted by `aura mcp add`. A
        // hand-edited `.mcp.json` could otherwise smuggle in an
        // `installed`-trust stdio command and run it at boot.
        entry.validate()?;
        let session = connect_with_extra_env(entry, &self.vault, extra_env).await?;
        let resources = resource_access_for(entry);
        let trust_level: aura_model::TrustLevel = entry.trust_level.into();

        for descriptor in session.tools().to_vec() {
            let tool = crate::mcp::tool::McpTool::new(
                entry.name.clone(),
                descriptor.clone(),
                resources.clone(),
                // Only honor `_meta.aura` for Aura-owned embedded
                // servers. A user `.mcp.json` server can't suppress its
                // own approval gate via per-tool annotations.
                is_embedded,
                session.peer(),
                self.blob_store.clone(),
            );
            let manifest = crate::mcp::tool::build_manifest(
                &format!("{}/{}", entry.name, descriptor.name),
                tool.description(),
                tool.parameters_schema(),
                trust_level.clone(),
                entry.capabilities.clone(),
            );
            self.registry
                .register_dynamic(&entry.name, Arc::new(tool), manifest);
        }

        self.state.lock().insert(
            entry.name.clone(),
            Connected {
                session,
                identity_hash,
            },
        );
        Ok(())
    }
}

fn identity_hash(entry: &McpServerEntry, extra_env: &HashMap<String, String>) -> u64 {
    let mut hasher = DefaultHasher::new();
    entry.name.hash(&mut hasher);
    match &entry.transport {
        McpTransportConfig::Stdio { command, args } => {
            "stdio".hash(&mut hasher);
            command.hash(&mut hasher);
            args.hash(&mut hasher);
        }
        McpTransportConfig::Http { url } => {
            "http".hash(&mut hasher);
            url.hash(&mut hasher);
        }
    }
    let trust_str = format!("{:?}", entry.trust_level);
    trust_str.hash(&mut hasher);
    let mut caps: Vec<String> = entry
        .capabilities
        .iter()
        .map(|c| format!("{c:?}"))
        .collect();
    caps.sort();
    caps.hash(&mut hasher);
    if let Some(o) = &entry.oauth {
        "oauth".hash(&mut hasher);
        o.client_id.hash(&mut hasher);
        o.callback_port.hash(&mut hasher);
    }
    // extra_env is folded in so a boot-config change (e.g. updated
    // chromium binary path) re-spawns the child even when the
    // command/args stay identical.
    let mut env_pairs: Vec<(&String, &String)> = extra_env.iter().collect();
    env_pairs.sort_by(|a, b| a.0.cmp(b.0));
    for (k, v) in env_pairs {
        "env".hash(&mut hasher);
        k.hash(&mut hasher);
        v.hash(&mut hasher);
    }
    hasher.finish()
}

fn resource_access_for(entry: &McpServerEntry) -> Vec<ResourceAccess> {
    match &entry.transport {
        McpTransportConfig::Stdio { command, .. } => {
            vec![ResourceAccess::ExecCommand {
                command: command.clone(),
            }]
        }
        McpTransportConfig::Http { url } => {
            let host = url::Url::parse(url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_string))
                .unwrap_or_default();
            vec![ResourceAccess::Http { host }]
        }
    }
}

/// Whether a currently-connected server (`name`, present in
/// `state` but absent from the desired set) should be disconnected.
///
/// - **Embedded** names are always authoritative — if `self.embedded`
///   no longer lists this name (would only happen if the boot path
///   stopped requesting it), drop it.
/// - **User** names are only authoritatively absent when the
///   `.mcp.json` load actually succeeded. A load failure must not
///   tear down live user MCP servers — preserve them until the next
///   tick re-loads cleanly.
fn should_disconnect(
    name: &str,
    user_entries_loaded: bool,
    embedded_names: &HashSet<String>,
) -> bool {
    embedded_names.contains(name) || user_entries_loaded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names<I: IntoIterator<Item = &'static str>>(names: I) -> HashSet<String> {
        names.into_iter().map(String::from).collect()
    }

    #[test]
    fn should_disconnect_embedded_name_regardless_of_load_state() {
        let embedded = names(["browser"]);
        assert!(should_disconnect("browser", true, &embedded));
        assert!(should_disconnect("browser", false, &embedded));
    }

    #[test]
    fn should_disconnect_user_name_only_when_load_succeeded() {
        let embedded = names(["browser"]);
        // Load succeeded → user removed the entry → disconnect.
        assert!(should_disconnect("github-mcp", true, &embedded));
        // Load failed → can't tell whether the user removed it →
        // preserve the live connection.
        assert!(!should_disconnect("github-mcp", false, &embedded));
    }
}
