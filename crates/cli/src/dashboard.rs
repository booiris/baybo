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
use baybo_channels::{DashboardProvider, DashboardSnapshot, ViewKind};

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
            ViewKind::Turns => turns_snapshot(&self.ctx).await,
            ViewKind::Sessions => sessions_snapshot(&self.ctx).await,
        }
    }
}

fn skills_snapshot(ctx: &CommandContext) -> DashboardSnapshot {
    // Refreshing the Skills view also hot-reloads from disk so operators
    // can edit `<workspace>/personas/<id>/skills/<name>/SKILL.md` and see the change
    // without restarting Baybo. Other dashboards don't pair with a
    // filesystem source, so they skip this step.
    let total = ctx.skills.reload();
    let rows: Vec<Vec<String>> = ctx
        .skills
        .summaries_for(crate::context::OPERATOR_SKILL_SCOPE)
        .into_iter()
        .map(|s| vec![s.name, first_line(&s.description)])
        .collect();
    DashboardSnapshot {
        title: "Skills".into(),
        columns: vec!["NAME".into(), "DESCRIPTION".into()],
        rows,
        footer: Some(format!("reloaded from disk · {total} skill(s)")),
    }
}

async fn turns_snapshot(ctx: &CommandContext) -> DashboardSnapshot {
    let rows = match ctx.turn.as_deref() {
        Some(mgr) => match mgr.list(None).await {
            Ok(turns) => turns
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
        title: "Turns".into(),
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
        columns: vec!["ID".into(), "CHANNEL".into(), "LAST_ACTIVE".into()],
        rows,
        footer: None,
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}
