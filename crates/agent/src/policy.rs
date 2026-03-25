/// Execution policy governing agent behavior limits.
#[derive(Debug, Clone)]
pub struct ExecutionPolicy {
    /// Maximum LLM iterations per user message before stopping.
    pub max_iterations: usize,
    /// Default timeout for tool execution in milliseconds.
    pub default_tool_timeout_ms: u64,
}

impl Default for ExecutionPolicy {
    fn default() -> Self {
        Self {
            max_iterations: 20,
            default_tool_timeout_ms: 30000,
        }
    }
}
