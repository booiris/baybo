use std::sync::Arc;

use aura_sandbox::SandboxRunner;
use aura_security::{LeakDetector, SecretVault};
use aura_tools::{Tool, ToolManifest};

mod error;
mod parse;
mod plan;
mod prompt;
mod run;
mod sanitize;
mod scratch;
mod tool;

#[cfg(test)]
mod test_support;

pub use tool::CodeBuilderTool;

/// Build the CodeBuilder tool and its manifest, ready to register with a
/// `ToolRegistry`.
///
/// `leak_detector` and `secret_vault` are shared with the agent's
/// `SecurityGateway` so that placeholders minted here (when re-sanitizing
/// already-revealed tool args before the nested planning LLM call) are
/// resolvable later by `reveal_in_text` against the same vault.
///
/// No LLM handle is captured at registration time: planning runs
/// against the per-call `ToolContext.llm` injected by `ToolExecutor`
/// from the surrounding actor's currently-selected model, so a
/// session pinned to a non-default LLM correctly cascades into
/// CodeBuilder's planner instead of falling back to the process
/// default.
pub fn agent_tool(
    sandbox_runner: Arc<dyn SandboxRunner>,
    leak_detector: Arc<LeakDetector>,
    secret_vault: Arc<SecretVault>,
) -> (Arc<dyn Tool>, ToolManifest) {
    let tool = Arc::new(CodeBuilderTool::new(
        sandbox_runner,
        leak_detector,
        secret_vault,
    )) as Arc<dyn Tool>;
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        trust_level: aura_model::TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: vec![
            aura_tools::ToolCapability::ExecCommand,
            aura_tools::ToolCapability::WriteFile,
            aura_tools::ToolCapability::Http,
        ],
    };
    (tool, manifest)
}
