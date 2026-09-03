//! Permission policy: how much a tool must ask before acting.
//!
//! Two halves, and they belong to different sets of tools. The **isolation**
//! half — which route a shell command takes, and whether it is sandboxed — is
//! `Bash`'s alone. The **approval** half applies wherever the gate does:
//! `free` waives it for `Bash`, `Write` and `Edit` alike, because an operator
//! who turned prompting off did not mean "except for file writes".

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
    /// No approval gate at all, and no OS sandbox for Bash.
    #[serde(alias = "Free", alias = "open", alias = "none")]
    Free,
}
