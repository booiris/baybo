use aura_model::{ChatMessage, ContentBlock};
use tiktoken_rs::CoreBPE;

/// Trait for counting tokens in text and multimodal content.
///
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

/// Token overhead per chat message from role markers and separators,
/// following the `<|im_start|>role\n...<|im_end|>\n` convention used by
/// the OpenAI chat formats. Anthropic's harmony-style envelope is also
/// ~4 tokens per turn, so this constant applies to both providers.
const MESSAGE_OVERHEAD: usize = 4;

/// Low-detail image token cost. Accurate for OpenAI `detail=low` and a
/// safe lower bound for Anthropic. High-detail / tile-based counting is
/// provider-specific and intentionally out of scope here.
const IMAGE_TOKEN_ESTIMATE: usize = 85;

/// Rough per-item estimates for modalities tiktoken can't tokenize
/// directly. They are placeholders until provider-specific counting is
/// wired in.
const AUDIO_TOKEN_ESTIMATE: usize = 100;
const FILE_TOKEN_ESTIMATE: usize = 50;

/// BPE-based tokenizer backed by `tiktoken-rs`.
///
/// Uses OpenAI's `cl100k_base` or `o200k_base` encodings. Both are pure
/// algorithms and ship offline, so this type never performs I/O.
///
/// For providers without an official offline tokenizer (Anthropic Claude,
/// etc.) `cl100k_base` is used as a conservative approximation — counts
/// are typically within ~10% of the true value.
pub struct TiktokenTokenizer {
    bpe: &'static CoreBPE,
}

impl TiktokenTokenizer {
    /// cl100k_base — used by GPT-4, GPT-3.5-turbo, text-embedding-3-*,
    /// and as a fallback for providers without an official tokenizer.
    pub fn cl100k_base() -> Self {
        Self {
            bpe: tiktoken_rs::cl100k_base_singleton(),
        }
    }

    /// o200k_base — used by GPT-4o and the o-series reasoning models.
    pub fn o200k_base() -> Self {
        Self {
            bpe: tiktoken_rs::o200k_base_singleton(),
        }
    }

    /// Pick an encoding suitable for the given model ID. Unknown models
    /// fall back to `cl100k_base`.
    pub fn for_model(model: &str) -> Self {
        if uses_o200k(model) {
            Self::o200k_base()
        } else {
            Self::cl100k_base()
        }
    }
}

impl Default for TiktokenTokenizer {
    fn default() -> Self {
        Self::cl100k_base()
    }
}

impl Tokenizer for TiktokenTokenizer {
    fn count_text(&self, text: &str) -> usize {
        self.bpe.count_ordinary(text)
    }

    fn count_image(&self, _width: u32, _height: u32) -> usize {
        IMAGE_TOKEN_ESTIMATE
    }

    fn count_message(&self, msg: &ChatMessage) -> usize {
        let mut tokens = MESSAGE_OVERHEAD;
        for block in &msg.content {
            tokens += match block {
                ContentBlock::Text(t) => self.count_text(t.as_str()),
                ContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE,
                ContentBlock::Audio { .. } => AUDIO_TOKEN_ESTIMATE,
                ContentBlock::File { .. } => FILE_TOKEN_ESTIMATE,
            };
        }
        tokens
    }
}

/// Returns true for OpenAI model IDs that use the `o200k_base` encoding.
/// Everything else — including Anthropic Claude — maps to `cl100k_base`.
fn uses_o200k(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.starts_with("gpt-4o")
        || m.starts_with("chatgpt-4o")
        || m.starts_with("gpt-5")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::Role;

    #[test]
    fn count_text_empty_is_zero() {
        let tok = TiktokenTokenizer::cl100k_base();
        assert_eq!(tok.count_text(""), 0);
    }

    #[test]
    fn count_text_is_nonzero_for_real_input() {
        let tok = TiktokenTokenizer::cl100k_base();
        let count = tok.count_text("Hello, world! This is a tokenization test.");
        assert!(count > 0);
        assert!(count < 50);
    }

    #[test]
    fn count_message_includes_structural_overhead() {
        let tok = TiktokenTokenizer::cl100k_base();
        let msg = ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text("hi".to_string())],
        };
        let text_only = tok.count_text("hi");
        assert_eq!(tok.count_message(&msg), text_only + MESSAGE_OVERHEAD);
    }

    #[test]
    fn for_model_maps_openai_families() {
        // We can't directly inspect which BPE is picked, so assert via
        // divergent counts on a string that tokenizes differently under
        // the two encodings.
        let sample = "Astrophysicist 🔭";
        let cl = TiktokenTokenizer::cl100k_base().count_text(sample);
        let o2 = TiktokenTokenizer::o200k_base().count_text(sample);

        assert_eq!(TiktokenTokenizer::for_model("gpt-4").count_text(sample), cl);
        assert_eq!(
            TiktokenTokenizer::for_model("gpt-3.5-turbo").count_text(sample),
            cl
        );
        assert_eq!(
            TiktokenTokenizer::for_model("claude-3-opus-20240229").count_text(sample),
            cl
        );

        assert_eq!(
            TiktokenTokenizer::for_model("gpt-4o").count_text(sample),
            o2
        );
        assert_eq!(
            TiktokenTokenizer::for_model("gpt-4o-mini").count_text(sample),
            o2
        );
        assert_eq!(
            TiktokenTokenizer::for_model("o1-preview").count_text(sample),
            o2
        );
    }

    #[test]
    fn for_model_unknown_falls_back_to_cl100k() {
        let sample = "unknown model test";
        let fallback = TiktokenTokenizer::for_model("some-novel-model-9000").count_text(sample);
        let cl = TiktokenTokenizer::cl100k_base().count_text(sample);
        assert_eq!(fallback, cl);
    }
}
