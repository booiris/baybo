/// Execution policy governing agent behavior limits.
#[derive(Debug, Clone)]
pub struct ExecutionPolicy {
    /// Maximum LLM iterations per user message before stopping.
    pub max_iterations: usize,
}

impl Default for ExecutionPolicy {
    fn default() -> Self {
        Self { max_iterations: 20 }
    }
}
