use aura_model::ChatMessage;

/// Trait for counting tokens in text and multimodal content.
///
/// Defined in this crate but implemented externally (e.g. by the `llm` crate)
/// so that `context` remains independent of any specific LLM provider.
/// Implementations must account for structural overhead (roles, separators)
/// and provider-specific image counting rules.
pub trait Tokenizer: Send + Sync {
    /// Count the number of tokens in a text string.
    fn count_text(&self, text: &str) -> usize;

    /// Count the token cost of an image given its dimensions.
    fn count_image(&self, width: u32, height: u32) -> usize;

    /// Count the total tokens in a chat message, including structural overhead
    /// such as role markers and separators.
    fn count_message(&self, msg: &ChatMessage) -> usize;
}
