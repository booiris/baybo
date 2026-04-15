//! Builtin tools modeled after Claude Code's tools reference.
//!
//! Each tool's name matches the string the LLM uses in function calls and the
//! operator uses in permission rules (see `docs/modules/tools.md`).
//!
//! The Claude Code reference also defines tools that depend on larger
//! subsystems not yet landed in Aura (agent teams, worktrees, LSP, background
//! monitors, MCP client, etc.). Stubs for those live in [`todo`] and are not
//! registered by [`default_tools`] — a follow-up will wire them in as each
//! backing subsystem arrives.
//!
//! | Tool       | Status        |
//! |------------|---------------|
//! | `Read`     | implemented   |
//! | `Write`    | implemented   |
//! | `Edit`     | implemented   |
//! | `Bash`     | implemented   |
//! | `Glob`     | implemented   |
//! | `Grep`     | implemented   |
//! | `WebFetch` | implemented   |
//! | everything else listed in `todo.rs` | stubbed  |

use std::sync::Arc;

use aura_registry::TrustLevel;

use crate::{Tool, ToolCapability, ToolManifest};

pub mod bash;
pub mod edit;
pub mod glob_tool;
pub mod grep;
pub mod read;
pub mod todo;
pub mod web_fetch;
pub mod write;

#[cfg(debug_assertions)]
pub mod echo;

pub use bash::BashTool;
#[cfg(debug_assertions)]
pub use echo::EchoTool;
pub use edit::EditTool;
pub use glob_tool::GlobTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub use web_fetch::WebFetchTool;
pub use write::WriteTool;

/// The set of builtin tools registered by [`crate::ToolRegistry::with_defaults`].
///
/// Each entry pairs an [`Arc<dyn Tool>`] with the [`ToolManifest`] describing
/// its governance ceiling. `ToolExecutor::validate_trust` compares this
/// manifest against the runtime trust policy before executing.
pub fn default_tools() -> Vec<(Arc<dyn Tool>, ToolManifest)> {
    #[allow(unused_mut)]
    let mut tools: Vec<(Arc<dyn Tool>, ToolManifest)> = vec![
        trusted(ReadTool, vec![ToolCapability::ReadWorkspace]),
        trusted(
            WriteTool,
            vec![
                ToolCapability::ReadWorkspace,
                ToolCapability::WriteWorkspace,
            ],
        ),
        trusted(
            EditTool,
            vec![
                ToolCapability::ReadWorkspace,
                ToolCapability::WriteWorkspace,
            ],
        ),
        trusted(BashTool, vec![ToolCapability::SpawnProcess]),
        trusted(GlobTool, vec![ToolCapability::ReadWorkspace]),
        trusted(GrepTool, vec![ToolCapability::ReadWorkspace]),
        // `Http(vec![])` means "any host" here; the security layer narrows it
        // per invocation once the networking policy wiring lands.
        trusted(WebFetchTool, vec![ToolCapability::Http(vec![])]),
    ];
    #[cfg(debug_assertions)]
    tools.push(trusted(echo::EchoTool, vec![]));
    tools
}

fn trusted<T: Tool + 'static>(
    tool: T,
    capabilities: Vec<ToolCapability>,
) -> (Arc<dyn Tool>, ToolManifest) {
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities,
    };
    (Arc::new(tool), manifest)
}
