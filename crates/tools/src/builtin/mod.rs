//! Builtin tools modeled after Claude Code's tools reference.
//!
//! Each tool's name matches the string the LLM uses in function calls and the
//! operator uses in permission rules (see `docs/modules/tools.md`).
//!
//! The Claude Code reference also defines tools that depend on larger
//! subsystems not yet landed in Baybo (agent teams, worktrees, LSP, background
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
//! | `SendFile` | implemented   |
//! | everything else listed in `todo.rs` | stubbed  |

use std::sync::Arc;

use baybo_model::TrustLevel;
use baybo_store::BlobStore;
use baybo_workspace::WorkspacePaths;

use crate::{Tool, ToolCapability, ToolManifest};

pub mod background_jobs;
pub mod bash;
mod bash_judge;
pub mod edit;
pub mod glob_tool;
pub mod grep;
pub mod now;
pub(crate) mod paths;
pub mod read;
mod rg;
pub mod secret;
pub mod send_local_file;
pub mod todo;
pub mod web_fetch;
pub mod write;

#[cfg(debug_assertions)]
pub mod echo;

pub use background_jobs::{JobListTool, JobStopTool};
pub use bash::{BashSandboxMode, BashTool, LiveSandboxMode};
#[cfg(debug_assertions)]
pub use echo::EchoTool;
pub use edit::EditTool;
pub use glob_tool::GlobTool;
pub use grep::GrepTool;
pub use now::NowTool;
pub use read::ReadTool;
pub use web_fetch::WebFetchTool;
pub use write::WriteTool;

/// The builtin tools registered by [`crate::ToolRegistry::with_defaults`].
///
/// Each entry pairs an [`Arc<dyn Tool>`] with the [`ToolManifest`] describing
/// its governance ceiling. `ToolExecutor::validate_trust` compares this
/// manifest against the runtime trust policy before executing.
///
/// `WebFetch`'s prompt-driven extraction now reads its LLM handle from
/// the per-call [`crate::ToolContext::llm`] slot that the agent layer
/// binds at tool-call time, so no LLM client needs to be threaded
/// through this factory.
///
/// Browser tools are not listed here — they arrive dynamically when
/// the embedded browser MCP server connects through the reconciler.
pub fn default_tools(
    blob_store: Arc<dyn BlobStore>,
    workspace_paths: WorkspacePaths,
    proxy: Option<reqwest::Proxy>,
    sandbox_mode: Arc<bash::LiveSandboxMode>,
) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
    #[allow(unused_mut)]
    let mut tools: Vec<(Arc<dyn Tool>, ToolManifest)> = vec![
        trusted(ReadTool, vec![ToolCapability::ReadFile]),
        trusted(
            WriteTool::new(workspace_paths.clone()),
            vec![ToolCapability::ReadFile, ToolCapability::WriteFile],
        ),
        trusted(
            EditTool::new(workspace_paths.clone()),
            vec![ToolCapability::ReadFile, ToolCapability::WriteFile],
        ),
        trusted(
            BashTool::new(workspace_paths).with_mode_handle(sandbox_mode),
            vec![ToolCapability::ExecCommand],
        ),
        trusted(GlobTool, vec![ToolCapability::ReadFile]),
        trusted(GrepTool, vec![ToolCapability::ReadFile]),
        trusted(
            WebFetchTool::new(blob_store.clone(), proxy),
            vec![ToolCapability::Http],
        ),
        send_local_file::tool(blob_store.clone()),
        trusted(NowTool, vec![]),
        trusted(secret::SecretAddTool, vec![]),
        trusted(secret::SecretListTool, vec![]),
        trusted(secret::SecretCheckTool, vec![]),
        trusted(JobListTool, vec![]),
        trusted(JobStopTool, vec![]),
    ];
    #[cfg(debug_assertions)]
    tools.push(trusted(echo::EchoTool, vec![]));
    tools
}

pub(crate) fn trusted<T: Tool + 'static>(
    tool: T,
    capabilities: Vec<ToolCapability>,
) -> (Arc<dyn Tool>, ToolManifest) {
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities,
    };
    (Arc::new(tool), manifest)
}
