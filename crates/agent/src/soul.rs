use aura_workspace::WorkspaceManager;
use tracing::debug;

/// The Soul system loads personality and identity from workspace files
/// and produces the system prompt for LLM conversations.
pub struct Soul {
    system_prompt: String,
}

impl Soul {
    /// Build a Soul from workspace identity files.
    pub async fn from_workspace(workspace: &WorkspaceManager) -> anyhow::Result<Self> {
        let identity = workspace.load_identity_files().await?;
        let mut parts = Vec::new();

        if let Some(soul_text) = &identity.soul {
            parts.push(soul_text.clone());
        }
        if let Some(identity_text) = &identity.identity {
            parts.push(identity_text.clone());
        }
        if let Some(agents_text) = &identity.agents {
            parts.push(agents_text.clone());
        }

        let system_prompt = if parts.is_empty() {
            "You are Aura, an intelligent assistant.".to_string()
        } else {
            parts.join("\n\n")
        };

        debug!(
            prompt_len = system_prompt.len(),
            "soul system prompt loaded"
        );
        Ok(Self { system_prompt })
    }

    /// Create a Soul with a custom system prompt.
    pub fn custom(prompt: String) -> Self {
        Self {
            system_prompt: prompt,
        }
    }

    /// Get the system prompt.
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }
}
