//! Permission policy for shell-out tools (today: `Bash`).
//!
//! `permission` controls the combined approval/isolation shape for shell
//! commands.

use serde::{Deserialize, Serialize};

/// How Bash handles approval and sandbox escape.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PermissionPolicy {
    /// Judge destructive commands before sandboxed execution, and judge sandbox
    /// failures before any automatic unsandboxed retry.
    #[default]
    Auto,
    #[serde(alias = "Manual")]
    /// Ask a human for every Bash command, then ask again before any
    /// unsandboxed retry after sandbox failure.
    Manual,
    /// Run directly without sandboxing or Bash approval.
    #[serde(alias = "Free", alias = "open", alias = "none")]
    Free,
}
