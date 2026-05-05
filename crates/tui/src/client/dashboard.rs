//! [`DashboardProvider`] for the WS-backed TUI.
//!
//! The TUI's channel-surface no longer carries session CRUD (it's been
//! folded back into the `aura` CLI), so every view here is rendered as
//! an "admin-only" snapshot with a footer pointing the operator at the
//! CLI. The provider still exists so dashboard keybindings keep working
//! and the user sees a clear message rather than an empty pane.

use async_trait::async_trait;
use aura_channels::{DashboardProvider, DashboardSnapshot, ViewKind};

/// [`DashboardProvider`] that renders the admin-only placeholder for
/// every view. Kept as a type (rather than a bare function) to match
/// the trait-object plumbing the TUI uses for its dashboard surface.
pub struct TuiDashboardProvider;

impl TuiDashboardProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TuiDashboardProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DashboardProvider for TuiDashboardProvider {
    async fn snapshot(&self, kind: ViewKind) -> DashboardSnapshot {
        let (title, columns) = match kind {
            ViewKind::Sessions => (
                "Sessions",
                vec![
                    "ID".into(),
                    "CHANNEL".into(),
                    "MESSAGES".into(),
                    "LAST_ACTIVE".into(),
                ],
            ),
            ViewKind::Skills => ("Skills", vec!["NAME".into()]),
            ViewKind::Jobs => (
                "Jobs",
                vec![
                    "ID".into(),
                    "SESSION".into(),
                    "STATUS".into(),
                    "CREATED".into(),
                ],
            ),
            ViewKind::Memory => (
                "Memory",
                vec![
                    "ID".into(),
                    "USER".into(),
                    "CATEGORY".into(),
                    "IMPORTANCE".into(),
                    "CONTENT".into(),
                ],
            ),
        };
        DashboardSnapshot {
            title: title.into(),
            columns,
            rows: Vec::new(),
            footer: Some("admin surface — use the `aura` CLI (not reachable over channel ws)".into()),
        }
    }
}
