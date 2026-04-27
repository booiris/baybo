use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aura_security::SecretVault;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::approval::ResourceAccess;
use crate::mcp::config::{McpFile, McpServerEntry, McpTransportConfig};
use crate::mcp::transport::{McpServerSession, connect};
use crate::{Tool, ToolRegistry};

const TICK_INTERVAL: Duration = Duration::from_secs(5);

struct Connected {
    session: McpServerSession,
    identity_hash: u64,
}

pub struct McpReconciler {
    workspace_root: PathBuf,
    registry: Arc<ToolRegistry>,
    vault: Arc<SecretVault>,
    cancel: CancellationToken,
    notify: Arc<Notify>,
    state: Mutex<HashMap<String, Connected>>,
}

impl McpReconciler {
    pub fn new(
        workspace_root: PathBuf,
        registry: Arc<ToolRegistry>,
        vault: Arc<SecretVault>,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            workspace_root,
            registry,
            vault,
            cancel,
            notify: Arc::new(Notify::new()),
            state: Mutex::new(HashMap::new()),
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
        let file = match McpFile::load(&self.workspace_root).await {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "mcp reconciler: failed to load .mcp.json — skipping tick");
                return self.state.lock().keys().cloned().collect::<Vec<_>>();
            }
        };

        let desired: HashMap<String, (McpServerEntry, u64)> = file
            .servers
            .into_iter()
            .map(|e| {
                let h = identity_hash(&e);
                (e.name.clone(), (e, h))
            })
            .collect();

        let current_keys: HashSet<String> = self.state.lock().keys().cloned().collect();
        let desired_keys: HashSet<String> = desired.keys().cloned().collect();

        for name in current_keys.difference(&desired_keys) {
            self.disconnect_server(name).await;
        }

        for name in desired_keys {
            let (entry, hash) = desired.get(&name).expect("present").clone();
            let needs_reconnect = self
                .state
                .lock()
                .get(&name)
                .is_none_or(|c| c.identity_hash != hash);
            if needs_reconnect {
                self.disconnect_server(&name).await;
                if let Err(e) = self.connect_server(&entry, hash).await {
                    tracing::warn!(server = %entry.name, error = %e, "mcp reconciler: connect failed");
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

    async fn connect_server(
        self: &Arc<Self>,
        entry: &McpServerEntry,
        identity_hash: u64,
    ) -> crate::mcp::McpResult<()> {
        // Defense-in-depth trust gate: never spawn / connect an entry
        // that wouldn't have been accepted by `aura mcp add`. A
        // hand-edited `.mcp.json` could otherwise smuggle in an
        // `installed`-trust stdio command and run it at boot.
        entry.validate()?;
        let session = connect(entry, &self.vault).await?;
        let resources = resource_access_for(entry);
        let trust_level: aura_model::TrustLevel = entry.trust_level.into();

        for descriptor in session.tools().to_vec() {
            let tool = crate::mcp::tool::McpTool::new(
                entry.name.clone(),
                descriptor.clone(),
                resources.clone(),
                session.peer(),
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

fn identity_hash(entry: &McpServerEntry) -> u64 {
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
