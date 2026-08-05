//! Conversions between the rich `Turn` domain type and its
//! persistence row (`baybo_store::TurnRow`).
//!
//! The `TurnStore` trait itself lives in `baybo-store` and trades in rows,
//! so the turn state machine stays in this crate while the store contract
//! sits alongside every other one. Callers convert at the boundary.

use crate::{Result, Turn, TurnError, TurnInputKind};
use baybo_store::TurnRow;

/// Snake-case string for the denormalised `turns.kind` column. The column
/// is display-only (never filtered in SQL; `from_row` rebuilds the whole
/// `Turn` from `data`), so it carries the input-kind projection.
fn turn_input_kind_str(kind: TurnInputKind) -> &'static str {
    match kind {
        TurnInputKind::UserChat => "user_chat",
        TurnInputKind::Cron => "cron",
        TurnInputKind::CronNotification => "cron_notification",
        TurnInputKind::Compact => "compact",
        TurnInputKind::Spawned => "spawned",
        TurnInputKind::SubagentNotification => "subagent_notification",
        TurnInputKind::IssueRun => "issue_run",
    }
}

impl Turn {
    /// Project this turn into a persistence [`TurnRow`]: the queryable
    /// columns plus the full turn serialized into `data`.
    pub fn to_row(&self) -> Result<TurnRow> {
        let data = serde_json::to_string(self)
            .map_err(|e| TurnError::Storage(format!("failed to serialize turn: {e}")))?;
        Ok(TurnRow {
            id: self.id,
            session_id: self.session_id.clone(),
            parent_turn_id: self.parent_turn_id,
            kind: turn_input_kind_str(self.input.input_kind()).to_string(),
            status_kind: self.status.kind().as_snake_case().to_string(),
            created_at: self.created_at,
            started_at: self.started_at,
            ended_at: self.ended_at,
            data,
        })
    }

    /// Reconstruct a turn from its persistence row.
    pub fn from_row(row: TurnRow) -> Result<Turn> {
        serde_json::from_str(&row.data)
            .map_err(|e| TurnError::Storage(format!("failed to deserialize turn: {e}")))
    }
}

impl From<baybo_store::StorageError> for TurnError {
    fn from(e: baybo_store::StorageError) -> Self {
        match e {
            baybo_store::StorageError::NotFound(s) => TurnError::NotFound(s),
            baybo_store::StorageError::Internal(e) => TurnError::Internal(e),
            other => TurnError::Storage(other.to_string()),
        }
    }
}
