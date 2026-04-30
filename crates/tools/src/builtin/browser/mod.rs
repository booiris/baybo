//! Browser tool family — drives a Playwright sidecar over WebSocket.
//!
//! See `docs/modules/tools.md` and the plan in
//! `~/.claude/plans/browser-tool-tools-src-browser-virtual-aurora.md`
//! for design rationale (sidecar transport, isolation guarantees,
//! per-action approval policy).

use std::sync::Arc;

use aura_model::TrustLevel;
use aura_storage::BlobStore;

use crate::{Tool, ToolCapability, ToolManifest};

pub mod back;
pub mod cdp;
pub mod click;
pub mod client;
pub mod console;
pub mod dialog;
pub mod get_images;
pub mod navigate;
pub mod press;
pub mod schema;
pub mod screenshot;
pub mod scroll;
pub mod snapshot;
pub mod type_text;

#[cfg(test)]
mod tests;

pub use back::BrowserBackTool;
pub use cdp::BrowserCdpTool;
pub use click::BrowserClickTool;
pub use client::{BrowserRpcError, BrowserSidecarClient};
pub use console::BrowserConsoleTool;
pub use dialog::BrowserDialogTool;
pub use get_images::BrowserGetImagesTool;
pub use navigate::BrowserNavigateTool;
pub use press::BrowserPressTool;
pub use screenshot::BrowserScreenshotTool;
pub use scroll::BrowserScrollTool;
pub use snapshot::BrowserSnapshotTool;
pub use type_text::BrowserTypeTool;

/// Register every browser tool against a single shared sidecar
/// client. Caller wires this into [`crate::builtin::default_tools`]
/// when (and only when) a browser sidecar is reachable — when no
/// client is plumbed in, the tools are silently absent and the LLM
/// won't see them.
pub fn tools(
    client: Arc<dyn BrowserSidecarClient>,
    blob_store: Arc<dyn BlobStore>,
) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
    vec![
        wrap(
            BrowserNavigateTool::new(client.clone()),
            vec![ToolCapability::Http],
        ),
        wrap(BrowserSnapshotTool::new(client.clone()), vec![]),
        wrap(BrowserClickTool::new(client.clone()), vec![]),
        wrap(BrowserTypeTool::new(client.clone()), vec![]),
        wrap(BrowserScrollTool::new(client.clone()), vec![]),
        wrap(BrowserBackTool::new(client.clone()), vec![]),
        wrap(BrowserPressTool::new(client.clone()), vec![]),
        wrap(BrowserGetImagesTool::new(client.clone()), vec![]),
        wrap(
            BrowserScreenshotTool::new(client.clone(), blob_store.clone()),
            vec![],
        ),
        wrap(
            BrowserConsoleTool::new(client.clone()),
            vec![ToolCapability::ExecCommand],
        ),
        wrap(BrowserDialogTool::new(client.clone()), vec![]),
        wrap(
            BrowserCdpTool::new(client.clone()),
            vec![ToolCapability::ExecCommand],
        ),
    ]
}

fn wrap<T: Tool + 'static>(
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
