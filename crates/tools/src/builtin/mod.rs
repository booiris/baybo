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
//! | Tool         | Status        |
//! |--------------|---------------|
//! | `Read`       | implemented   |
//! | `Write`      | implemented   |
//! | `Edit`       | implemented   |
//! | `Bash`       | implemented   |
//! | `Glob`       | implemented   |
//! | `Grep`       | implemented   |
//! | `WebFetch`   | implemented   |
//! | `AttachFile` | implemented   |
//! | `PutBlob`    | implemented   |
//! | `GetBlob`    | implemented   |
//! | everything else listed in `todo.rs` | stubbed  |

use std::sync::Arc;

use baybo_model::TrustLevel;
use baybo_store::BlobStore;
use baybo_workspace::WorkspacePaths;

use crate::{Tool, ToolCapability, ToolManifest};

pub mod attach_file;
pub mod background_jobs;
pub mod bash;
mod blob_upload;
pub mod edit;
mod get_blob;
pub mod glob_tool;
pub mod grep;
pub(crate) mod managed_repo;
pub mod memory_delete;
pub mod now;
pub(crate) mod paths;
pub mod permission;
mod put_blob;
pub mod read;
mod rg;
pub mod secret;
pub mod todo;
pub mod tool_search;
pub mod web_fetch;
pub mod write;

#[cfg(debug_assertions)]
pub mod echo;

pub use background_jobs::{JobListTool, JobStopTool};
pub use bash::BashTool;
#[cfg(debug_assertions)]
pub use echo::EchoTool;
pub use edit::EditTool;
pub use glob_tool::GlobTool;
pub use grep::GrepTool;
pub use memory_delete::MemoryDeleteTool;
pub use now::NowTool;
pub use permission::{LivePermissionMode, PermissionMode};
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
/// Everything the built-in tool set needs, named at the call site.
///
/// A struct rather than five positional parameters because the last one is a
/// bare `bool`: `with_defaults(store, paths, None, permission, true)` says
/// nothing about what `true` enables, and the two before it are an `Option`
/// and an `Arc` that read the same way in any order.
pub struct DefaultToolsConfig {
    pub blob_store: Arc<dyn BlobStore>,
    pub process_manager: Arc<baybo_process::ProcessManager>,
    pub workspace_paths: WorkspacePaths,
    pub proxy: Option<reqwest::Proxy>,
    pub permission: Arc<permission::LivePermissionMode>,
    /// `memory.builtin.enabled`. With built-in memory off nothing ever
    /// mentions a memory directory to the model, so offering it a verb for
    /// tidying one would describe a place that does not exist in its prompt.
    pub builtin_memory: bool,
}

pub fn default_tools(config: DefaultToolsConfig) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
    let DefaultToolsConfig {
        blob_store,
        process_manager,
        workspace_paths,
        proxy,
        permission,
        builtin_memory,
    } = config;
    let workspace_paths_for_memory = workspace_paths.clone();
    #[allow(unused_mut)]
    let mut tools: Vec<(Arc<dyn Tool>, ToolManifest)> = vec![
        trusted(ReadTool, vec![ToolCapability::ReadFile]),
        trusted(
            WriteTool::new(workspace_paths.clone(), Arc::clone(&permission)),
            vec![ToolCapability::ReadFile, ToolCapability::WriteFile],
        ),
        trusted(
            EditTool::new(workspace_paths.clone(), Arc::clone(&permission)),
            vec![ToolCapability::ReadFile, ToolCapability::WriteFile],
        ),
        trusted(
            BashTool::new(workspace_paths.clone(), Arc::clone(&process_manager))
                .with_permission_handle(Arc::clone(&permission)),
            vec![ToolCapability::ExecCommand],
        ),
        trusted(
            GlobTool::new(Arc::clone(&process_manager)),
            vec![ToolCapability::ReadFile],
        ),
        trusted(
            GrepTool::new(process_manager),
            vec![ToolCapability::ReadFile],
        ),
        trusted(
            WebFetchTool::new(blob_store.clone(), proxy),
            vec![ToolCapability::Http],
        ),
        attach_file::tool(blob_store.clone()),
        put_blob::tool(blob_store.clone()),
        get_blob::tool(blob_store.clone()),
        trusted(NowTool, vec![]),
        trusted(secret::SecretAddTool, vec![]),
        trusted(secret::SecretListTool, vec![]),
        trusted(secret::SecretCheckTool, vec![]),
        trusted(JobListTool::new(workspace_paths), vec![]),
        trusted(JobStopTool, vec![]),
    ];
    if builtin_memory {
        tools.push(trusted(
            MemoryDeleteTool::new(workspace_paths_for_memory),
            vec![ToolCapability::WriteFile],
        ));
    }
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
        channels: Vec::new(),
    };
    (Arc::new(tool), manifest)
}

/// Fixtures for the one workspace shape the membership tests in this module
/// tree turn on: a root reached through a symlink.
///
/// Shared because three test modules need it and each would otherwise grow
/// its own subtly different copy — and the subtlety is the whole test. The
/// directories have to really exist (`absolutise` only resolves what it can
/// `canonicalize`, so a fixture naming a directory that was never created
/// takes the fallback branch and both sides keep the spelling they came
/// with), and the tempdir has to be canonicalised first (`tempfile` can hand
/// back a path that is itself reached through a link — `/var` on macOS —
/// which would make the fixture's own symlink indistinguishable from the
/// platform's).
#[cfg(test)]
pub(crate) mod symlinked_root {
    use baybo_workspace::WorkspacePaths;
    use std::path::{Path, PathBuf};

    /// A tempdir guard plus its resolved path. Keep the guard alive: dropping
    /// it deletes the tree.
    pub(crate) fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().canonicalize().expect("canonical tempdir");
        (tmp, real)
    }

    /// A workspace laid out at `<real>/store`, addressed as
    /// `<real>/home-dot-baybo` — the shape of the release-default
    /// `$HOME/.baybo` on any host where that is a link. Returns the linked
    /// spelling, which is what a run is handed and what the model types.
    pub(crate) fn workspace(real: &Path) -> WorkspacePaths {
        let store = real.join("store");
        let laid_out = WorkspacePaths::new(store.clone());
        std::fs::create_dir_all(laid_out.work_dir()).expect("work dir");
        std::fs::create_dir_all(laid_out.persona_memory_dir("baybo")).expect("memory dir");
        let linked = real.join("home-dot-baybo");
        std::os::unix::fs::symlink(&store, &linked).expect("symlink");
        WorkspacePaths::new(linked)
    }
}
