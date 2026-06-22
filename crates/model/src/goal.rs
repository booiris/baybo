//! The session autonomous objective: the durable value types behind the
//! `create_goal` / `get_goal` / `update_goal` tool family and the `/goal`
//! continuation engine.
//!
//! Pure data. `aura-goal` (the tool boundary + service) and `aura-agent` (the
//! continuation runtime) both reach for these; persistence lives in the
//! dedicated `session_goals` table behind `aura_store::GoalStore`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::GoalId;

/// Tool names for the goal lifecycle family. Defined here (next to the value
/// types) so the agent loop can recognize a goal mutation without depending on
/// the `aura-goal` crate — mirroring [`crate::task::TASK_CREATE_TOOL_NAME`].
pub const CREATE_GOAL_TOOL_NAME: &str = "create_goal";
pub const GET_GOAL_TOOL_NAME: &str = "get_goal";
pub const UPDATE_GOAL_TOOL_NAME: &str = "update_goal";

/// The subset of goal tools whose execution mutates the goal, so the agent loop
/// can refresh surfaces afterward. `get_goal` is read-only and excluded.
pub const GOAL_MUTATING_TOOL_NAMES: &[&str] = &[CREATE_GOAL_TOOL_NAME, UPDATE_GOAL_TOOL_NAME];

/// Lifecycle of a session's current goal, classified by **who** triggers each
/// transition (`Active` self-fires; model-driven `Complete`/`Blocked`;
/// user-driven `Paused`; system spend-stops `BudgetLimited`/`SpendCapped`).
/// Only `Active` self-fires; only `Complete` is terminal; every other state is
/// recoverable via `/goal resume`. See `docs/modules/goal.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Complete,
    Blocked,
    Paused,
    BudgetLimited,
    SpendCapped,
}

impl GoalStatus {
    /// Every variant — the single source of truth for status round-trip tests
    /// and the dashboard pill set.
    pub const ALL: [GoalStatus; 6] = [
        Self::Active,
        Self::Complete,
        Self::Blocked,
        Self::Paused,
        Self::BudgetLimited,
        Self::SpendCapped,
    ];

    /// Canonical wire/storage string — the single source of truth for the
    /// `session_goals.status` column. Kept in lockstep with
    /// [`Self::from_storage_str`] (and the `snake_case` serde rename).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Complete => "complete",
            Self::Blocked => "blocked",
            Self::Paused => "paused",
            Self::BudgetLimited => "budget_limited",
            Self::SpendCapped => "spend_capped",
        }
    }

    /// Parse the stored column back. `None` for an unknown value so a row
    /// written by a future variant is skipped rather than failing a `list_all`.
    pub fn from_storage_str(s: &str) -> Option<Self> {
        match s {
            "active" => Some(Self::Active),
            "complete" => Some(Self::Complete),
            "blocked" => Some(Self::Blocked),
            "paused" => Some(Self::Paused),
            "budget_limited" => Some(Self::BudgetLimited),
            "spend_capped" => Some(Self::SpendCapped),
            _ => None,
        }
    }

    /// `true` only for `Active` — the one state in which the actor self-fires a
    /// continuation turn at the turn boundary.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active)
    }

    /// `Complete` is the only terminal state — every other non-`Active` state is
    /// recoverable with `/goal resume`.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Complete)
    }

    /// A stopped state that `/goal resume` can return to `Active` — i.e. neither
    /// `Active` nor terminal (`Blocked` / `Paused` / `BudgetLimited` /
    /// `SpendCapped`).
    pub fn is_resumable(&self) -> bool {
        !self.is_active() && !self.is_terminal()
    }
}

/// A session's current autonomous objective.
///
/// One **current** goal per session (the store key is `session_id`); the value
/// does not carry the session id. `token_budget` is `Option` — omit it and the
/// goal runs until the model marks it complete/blocked or the global cost gate
/// stops it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub id: GoalId,
    /// The user's objective in natural language. Treated as **untrusted user
    /// data** by the steering prompt, not as higher-priority instructions.
    pub objective: String,
    pub status: GoalStatus,
    /// Optional per-goal token ceiling. `None` ⇒ no per-goal budget (the common
    /// case); the global daily/monthly cost gate is the backstop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    /// Tokens billed to this session while a goal turn was running.
    pub tokens_used: u64,
    /// Wall-clock seconds accumulated while the goal was `Active`.
    pub time_used_seconds: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Goal {
    /// Remaining tokens before the per-goal budget is hit, if a budget is set.
    /// `None` when there is no budget; saturates at zero.
    pub fn remaining_budget(&self) -> Option<u64> {
        self.token_budget
            .map(|b| b.saturating_sub(self.tokens_used))
    }

    /// `false` when no budget is set.
    pub fn is_over_budget(&self) -> bool {
        self.token_budget.is_some_and(|b| self.tokens_used >= b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_as_str_round_trips_through_storage_str() {
        for status in GoalStatus::ALL {
            assert_eq!(GoalStatus::from_storage_str(status.as_str()), Some(status));
        }
    }

    #[test]
    fn status_as_str_matches_serde_snake_case() {
        let json = serde_json::to_string(&GoalStatus::BudgetLimited).unwrap();
        assert_eq!(json, "\"budget_limited\"");
        assert_eq!(json.trim_matches('"'), GoalStatus::BudgetLimited.as_str());
    }

    #[test]
    fn unknown_status_is_skipped_not_panicked() {
        assert_eq!(GoalStatus::from_storage_str("from_the_future"), None);
    }

    #[test]
    fn only_active_self_fires() {
        for s in GoalStatus::ALL {
            assert_eq!(s.is_active(), s == GoalStatus::Active);
        }
    }

    #[test]
    fn only_complete_is_terminal() {
        for s in GoalStatus::ALL {
            assert_eq!(s.is_terminal(), s == GoalStatus::Complete);
        }
    }

    #[test]
    fn resumable_is_every_stopped_non_terminal_state() {
        use GoalStatus::*;
        for s in GoalStatus::ALL {
            let expected = matches!(s, Blocked | Paused | BudgetLimited | SpendCapped);
            assert_eq!(s.is_resumable(), expected);
        }
    }

    #[test]
    fn budget_arithmetic_saturates() {
        let base = Goal {
            id: GoalId::new(),
            objective: "ship it".into(),
            status: GoalStatus::Active,
            token_budget: Some(100),
            tokens_used: 130,
            time_used_seconds: 0,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert_eq!(base.remaining_budget(), Some(0));
        assert!(base.is_over_budget());

        let unbudgeted = Goal {
            token_budget: None,
            ..base
        };
        assert_eq!(unbudgeted.remaining_budget(), None);
        assert!(!unbudgeted.is_over_budget());
    }
}
