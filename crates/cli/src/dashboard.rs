//! Dashboard data provider for the TUI channel.
//!
//! The `channels` crate defines [`DashboardProvider`] but has no access to
//! any business managers. This module holds the concrete implementation that
//! pulls from the shared [`CommandContext`] graph and flattens each view into
//! a [`DashboardSnapshot`] (title, columns, rows, footer).
//!
//! Snapshots are built on demand — the provider holds no cache; freshness
//! comes from the manager layer.

use std::sync::Arc;

use async_trait::async_trait;
use aura_channels::{DashboardProvider, DashboardSnapshot, ViewKind};

use crate::context::CommandContext;

/// [`DashboardProvider`] backed by a shared [`CommandContext`].
pub struct CliDashboardProvider {
    ctx: Arc<CommandContext>,
}

impl CliDashboardProvider {
    pub fn new(ctx: Arc<CommandContext>) -> Self {
        Self { ctx }
    }
}

#[async_trait]
impl DashboardProvider for CliDashboardProvider {
    async fn snapshot(&self, kind: ViewKind) -> DashboardSnapshot {
        match kind {
            ViewKind::Skills => skills_snapshot(&self.ctx),
            ViewKind::Jobs => jobs_snapshot(&self.ctx).await,
            ViewKind::Sessions => sessions_snapshot(&self.ctx).await,
            ViewKind::Memory => memory_snapshot(&self.ctx).await,
        }
    }
}

fn skills_snapshot(ctx: &CommandContext) -> DashboardSnapshot {
    // Refreshing the Skills view also hot-reloads from disk so operators
    // can edit `<workspace>/skills/<name>/SKILL.md` and see the change
    // without restarting Aura. Other dashboards don't pair with a
    // filesystem source, so they skip this step.
    let total = ctx.skills.reload();
    let mut names = ctx.skills.list();
    names.sort();
    let rows: Vec<Vec<String>> = names
        .iter()
        .map(|n| {
            let desc = ctx
                .skills
                .get(n)
                .map(|s| first_line(&s.description))
                .unwrap_or_default();
            vec![n.clone(), desc]
        })
        .collect();
    DashboardSnapshot {
        title: "Skills".into(),
        columns: vec!["NAME".into(), "DESCRIPTION".into()],
        rows,
        footer: Some(format!("reloaded from disk · {total} skill(s)")),
    }
}

async fn jobs_snapshot(ctx: &CommandContext) -> DashboardSnapshot {
    let rows = match ctx.job.as_deref() {
        Some(mgr) => match mgr.list(None).await {
            Ok(jobs) => jobs
                .iter()
                .map(|j| {
                    vec![
                        j.id.to_string(),
                        j.session_id.to_string(),
                        j.status.kind().to_string(),
                        j.created_at.to_rfc3339(),
                    ]
                })
                .collect(),
            Err(_) => Vec::new(),
        },
        None => Vec::new(),
    };
    DashboardSnapshot {
        title: "Jobs".into(),
        columns: vec![
            "ID".into(),
            "SESSION".into(),
            "STATUS".into(),
            "CREATED".into(),
        ],
        rows,
        footer: None,
    }
}

async fn sessions_snapshot(ctx: &CommandContext) -> DashboardSnapshot {
    let rows = match ctx.session.as_deref() {
        Some(mgr) => match mgr.list().await {
            Ok(sessions) => sessions
                .iter()
                .map(|s| {
                    vec![
                        s.id.to_string(),
                        s.channel.to_string(),
                        s.messages.len().to_string(),
                        s.last_active.to_rfc3339(),
                    ]
                })
                .collect(),
            Err(_) => Vec::new(),
        },
        None => Vec::new(),
    };
    DashboardSnapshot {
        title: "Sessions".into(),
        columns: vec![
            "ID".into(),
            "CHANNEL".into(),
            "MESSAGES".into(),
            "LAST_ACTIVE".into(),
        ],
        rows,
        footer: None,
    }
}

async fn memory_snapshot(ctx: &CommandContext) -> DashboardSnapshot {
    let rows = match ctx.memory.as_deref() {
        Some(mgr) => match mgr.list(None).await {
            Ok(mut entries) => {
                entries.sort_by(|a, b| {
                    b.importance
                        .partial_cmp(&a.importance)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| b.last_accessed.cmp(&a.last_accessed))
                });
                entries
                    .iter()
                    .map(|e| {
                        vec![
                            e.id.clone(),
                            e.user_id.clone(),
                            category_label(&e.category).into(),
                            format!("{:.2}", e.importance),
                            truncate(&e.content, 80),
                        ]
                    })
                    .collect()
            }
            Err(_) => Vec::new(),
        },
        None => Vec::new(),
    };
    DashboardSnapshot {
        title: "Memory".into(),
        columns: vec![
            "ID".into(),
            "USER".into(),
            "CATEGORY".into(),
            "IMPORTANCE".into(),
            "CONTENT".into(),
        ],
        rows,
        footer: None,
    }
}

fn category_label(c: &aura_model::MemoryCategory) -> &'static str {
    match c {
        aura_model::MemoryCategory::UserPreference => "preference",
        aura_model::MemoryCategory::KeyFact => "fact",
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
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
