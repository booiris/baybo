//! [`DashboardProvider`] backed by a [`GatewayClient`].
//!
//! The TUI talks to the gateway's channel UDS only, which exposes
//! session state but not skills / jobs / memory (those live on the
//! admin TCP surface). The Sessions view fetches live data; the other
//! three variants of [`ViewKind`] return a static "admin-only"
//! snapshot so the TUI renders a clear message rather than silently
//! failing.

use std::sync::Arc;

use async_trait::async_trait;
use aura_channels::{DashboardProvider, DashboardSnapshot, ViewKind};

use aura_channels::client::GatewayClient;

/// [`DashboardProvider`] that pulls live data off an `aura gateway`.
pub struct GatewayDashboardProvider {
    client: Arc<GatewayClient>,
}

impl GatewayDashboardProvider {
    pub fn new(client: Arc<GatewayClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl DashboardProvider for GatewayDashboardProvider {
    async fn snapshot(&self, kind: ViewKind) -> DashboardSnapshot {
        match kind {
            ViewKind::Sessions => sessions_snapshot(&self.client).await,
            ViewKind::Skills => admin_only_snapshot("Skills", vec!["NAME".into()]),
            ViewKind::Jobs => admin_only_snapshot(
                "Jobs",
                vec![
                    "ID".into(),
                    "SESSION".into(),
                    "STATUS".into(),
                    "CREATED".into(),
                ],
            ),
            ViewKind::Memory => admin_only_snapshot(
                "Memory",
                vec![
                    "ID".into(),
                    "USER".into(),
                    "CATEGORY".into(),
                    "IMPORTANCE".into(),
                    "CONTENT".into(),
                ],
            ),
        }
    }
}

async fn sessions_snapshot(client: &GatewayClient) -> DashboardSnapshot {
    let columns = vec![
        "ID".into(),
        "CHANNEL".into(),
        "MESSAGES".into(),
        "LAST_ACTIVE".into(),
    ];
    match client.list_sessions().await {
        Ok(sessions) => {
            let rows: Vec<Vec<String>> = sessions
                .iter()
                .map(|s| {
                    vec![
                        s.id.clone(),
                        s.channel.to_string(),
                        s.messages.len().to_string(),
                        s.last_active.to_rfc3339(),
                    ]
                })
                .collect();
            DashboardSnapshot {
                title: "Sessions".into(),
                columns,
                rows,
                footer: None,
            }
        }
        Err(e) => DashboardSnapshot {
            title: "Sessions".into(),
            columns,
            rows: Vec::new(),
            footer: Some(format!("error: {e}")),
        },
    }
}

fn admin_only_snapshot(title: &str, columns: Vec<String>) -> DashboardSnapshot {
    DashboardSnapshot {
        title: title.into(),
        columns,
        rows: Vec::new(),
        footer: Some("admin surface — use `aura cli` (not reachable over channel UDS)".into()),
    }
}
