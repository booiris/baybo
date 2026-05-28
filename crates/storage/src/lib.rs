pub mod libsql;
pub mod retry;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

// Trait objects named by the `Store` DI bundle below. The contracts and
// their row/DTO types live in the `aura-store` ports crate; consumers
// import them from `aura_store` directly, not via this adapter.
use aura_store::{
    BlobStore, ChannelBotStore, ChannelPairingStore, ChannelSessionStore, CostStore, CronStore,
    JobStore, SecretStore, SessionStore, SessionSummaryStore, SkillRiskStore, TraceStore,
};

/// Bundles all store implementations into a single container
/// for dependency injection by the assembly layer.
///
/// Each field is an `Arc<dyn XxxStore>` so the whole struct is cheap to
/// clone and hand to multiple consumers (manager graph, gateway server,
/// CLI helpers) without forcing each caller to maintain its own bag of
/// store fields.
#[derive(Clone)]
pub struct Store {
    pub session: std::sync::Arc<dyn SessionStore>,
    pub session_summary: std::sync::Arc<dyn SessionSummaryStore>,
    pub trace: std::sync::Arc<dyn TraceStore>,
    pub secret: std::sync::Arc<dyn SecretStore>,
    pub cost: std::sync::Arc<dyn CostStore>,
    pub job: std::sync::Arc<dyn JobStore>,
    pub cron: std::sync::Arc<dyn CronStore>,
    pub risk: std::sync::Arc<dyn SkillRiskStore>,
    pub channel_session: std::sync::Arc<dyn ChannelSessionStore>,
    pub channel_bot: std::sync::Arc<dyn ChannelBotStore>,
    pub channel_pairing: std::sync::Arc<dyn ChannelPairingStore>,
    pub blob: std::sync::Arc<dyn BlobStore>,
}

impl Store {
    /// Open (or create) a `Store` backed by a libsql database at `path`.
    /// Parent directories are created if missing. Blob payloads land in
    /// `<parent>/blobs/` — kept alongside the libsql db so a single
    /// state directory holds the whole persistent state.
    pub async fn open(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let parent_dir = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => {
                std::fs::create_dir_all(parent).map_err(|e| {
                    anyhow::anyhow!(
                        "failed to create storage directory {}: {e}",
                        parent.display()
                    )
                })?;
                parent.to_path_buf()
            }
            // Bare filename or root path: park blobs next to CWD; rare
            // outside tests that pass `:memory:`-style placeholders.
            _ => std::path::PathBuf::from("."),
        };
        let blob_root = parent_dir.join("blobs");
        let pool = libsql::LibsqlPool::open(path).await?;
        let blob = libsql::LibsqlBlobStore::open(pool.clone(), &blob_root).await?;
        Ok(Self {
            session: std::sync::Arc::new(libsql::LibsqlSessionStore::new(pool.clone())),
            session_summary: std::sync::Arc::new(libsql::LibsqlSessionSummaryStore::new(
                pool.clone(),
            )),
            trace: std::sync::Arc::new(libsql::LibsqlTraceStore::new(pool.clone())),
            secret: std::sync::Arc::new(libsql::LibsqlSecretStore::new(pool.clone())),
            cost: std::sync::Arc::new(libsql::LibsqlCostStore::new(pool.clone())),
            job: std::sync::Arc::new(libsql::LibsqlJobStore::new(pool.clone())),
            cron: std::sync::Arc::new(libsql::LibsqlCronStore::new(pool.clone())),
            risk: std::sync::Arc::new(libsql::LibsqlSkillRiskStore::new(pool.clone())),
            channel_session: std::sync::Arc::new(libsql::LibsqlChannelSessionStore::new(
                pool.clone(),
            )),
            channel_bot: std::sync::Arc::new(libsql::LibsqlChannelBotStore::new(pool.clone())),
            channel_pairing: std::sync::Arc::new(libsql::LibsqlChannelPairingStore::new(pool)),
            blob: std::sync::Arc::new(blob),
        })
    }
}
