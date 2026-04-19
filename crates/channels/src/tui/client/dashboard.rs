//! [`DashboardProvider`] backed by a [`GatewayClient`].
//!
//! Each view kind fans out to a single `/v1/...` list endpoint and
//! flattens the response into a [`DashboardSnapshot`]. The client-side
//! fan-out means no new aggregate endpoint is needed on the gateway.
//! Errors from the gateway surface as an empty-rows snapshot with an
//! error footer so the TUI renders gracefully.

use std::sync::Arc;

use async_trait::async_trait;

use crate::slash::{DashboardProvider, DashboardSnapshot, ViewKind};
use crate::tui::client::dto::{JobSummary, MemorySummary};
use crate::tui::client::http::GatewayClient;

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
            ViewKind::Skills => skills_snapshot(&self.client).await,
            ViewKind::Jobs => jobs_snapshot(&self.client).await,
            ViewKind::Sessions => sessions_snapshot(&self.client).await,
            ViewKind::Memory => memory_snapshot(&self.client).await,
        }
    }
}

async fn skills_snapshot(client: &GatewayClient) -> DashboardSnapshot {
    match client.list_skills().await {
        Ok(mut names) => {
            names.sort();
            let total = names.len();
            let rows: Vec<Vec<String>> = names.into_iter().map(|n| vec![n]).collect();
            DashboardSnapshot {
                title: "Skills".into(),
                columns: vec!["NAME".into()],
                rows,
                footer: Some(format!("{total} skill(s) · from gateway")),
            }
        }
        Err(e) => error_snapshot("Skills", vec!["NAME".into()], &e.to_string()),
    }
}

async fn jobs_snapshot(client: &GatewayClient) -> DashboardSnapshot {
    let columns = vec![
        "ID".into(),
        "SESSION".into(),
        "STATUS".into(),
        "CREATED".into(),
    ];
    match client.list_jobs().await {
        Ok(jobs) => {
            let rows: Vec<Vec<String>> = jobs
                .iter()
                .map(|j: &JobSummary| {
                    vec![
                        j.id.clone(),
                        j.session_id.clone(),
                        j.status.clone(),
                        j.created_at.to_rfc3339(),
                    ]
                })
                .collect();
            DashboardSnapshot {
                title: "Jobs".into(),
                columns,
                rows,
                footer: None,
            }
        }
        Err(e) => error_snapshot("Jobs", columns, &e.to_string()),
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
        Err(e) => error_snapshot("Sessions", columns, &e.to_string()),
    }
}

async fn memory_snapshot(client: &GatewayClient) -> DashboardSnapshot {
    let columns = vec![
        "ID".into(),
        "USER".into(),
        "CATEGORY".into(),
        "IMPORTANCE".into(),
        "CONTENT".into(),
    ];
    match client.list_memory().await {
        Ok(mut entries) => {
            entries.sort_by(|a, b| {
                b.importance
                    .partial_cmp(&a.importance)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let rows: Vec<Vec<String>> = entries
                .iter()
                .map(|e: &MemorySummary| {
                    vec![
                        e.id.clone(),
                        e.user_id.clone(),
                        category_label(&e.category).into(),
                        format!("{:.2}", e.importance),
                        truncate(&e.content, 80),
                    ]
                })
                .collect();
            DashboardSnapshot {
                title: "Memory".into(),
                columns,
                rows,
                footer: None,
            }
        }
        Err(e) => error_snapshot("Memory", columns, &e.to_string()),
    }
}

fn error_snapshot(title: &str, columns: Vec<String>, err: &str) -> DashboardSnapshot {
    DashboardSnapshot {
        title: title.into(),
        columns,
        rows: Vec::new(),
        footer: Some(format!("error: {err}")),
    }
}

fn category_label(s: &str) -> &'static str {
    match s {
        "UserPreference" => "preference",
        "KeyFact" => "fact",
        _ => "other",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.replace('\n', " ")
    } else {
        let mut out: String = s
            .replace('\n', " ")
            .chars()
            .take(max.saturating_sub(1))
            .collect();
        out.push('…');
        out
    }
}
