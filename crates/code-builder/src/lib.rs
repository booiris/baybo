use std::sync::Arc;

use aura_llm::LlmCompletion;
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
pub fn agent_tool(
    llm: Arc<dyn LlmCompletion>,
    sandbox_runner: Arc<dyn SandboxRunner>,
    leak_detector: Arc<LeakDetector>,
    secret_vault: Arc<SecretVault>,
) -> (Arc<dyn Tool>, ToolManifest) {
    let tool = Arc::new(CodeBuilderTool::new(
        llm,
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
