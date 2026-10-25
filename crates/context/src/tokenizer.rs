use baybo_llm::AudioDelivery;
use baybo_model::{ChatMessage, ContentBlock};

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

    /// The part of [`Self::count_message`] that is a MEDIA price rather
    /// than a tokenizer estimate — an image's tiles, a PDF's pages, a
    /// voice note's seconds, or the delivery cap standing in for whichever
    /// of those the block does not carry.
    ///
    /// Split out because a media price is charged only on the roles that
    /// deliver media to the provider (`baybo_llm::delivers_media`), while
    /// the text around it is charged on every row.
    ///
    /// `audio` is the bound model's: [`Self::count_message`] folds in the
    /// [`AudioDelivery::Native`] price, so subtracting this at that value
    /// leaves exactly the text.
    ///
    /// Defaults to zero for tokenizers that have no media pricing.
    fn count_message_media(&self, _msg: &ChatMessage, _audio: AudioDelivery) -> usize {
        0
    }
}

/// Token overhead per chat message from role markers and separators,
/// following the `<|im_start|>role\n...<|im_end|>\n` convention used by
/// the OpenAI chat formats. Anthropic's harmony-style envelope is also
/// ~4 tokens per turn, so this constant applies to both providers.
const MESSAGE_OVERHEAD: usize = 4;

/// Overhead for the structural envelope of a tool_use / tool_result block
/// (id, name, JSON framing). The actual text/JSON payload is counted
/// separately and added on top.
const TOOL_USE_OVERHEAD: usize = 20;

/// Bytes of UTF-8 one token covers on average, across the vocabularies the
/// providers bill with: English prose and code run ~4.1–4.4 bytes a token
/// on OpenAI, DeepSeek and Gemini alike.
const BYTES_PER_TOKEN: usize = 4;

/// `PADDING_NUM / PADDING_DEN` (1.15) lifts the byte estimate over its own
/// error. Replayed against 76k production calls, bytes/4 with digits
/// doubled still read a median 4–9% below the provider's count and its
/// delta tail lower still, and an under-count is the direction that
/// overflows the window instead of compacting early.
const PADDING_NUM: usize = 23;
const PADDING_DEN: usize = 20;

/// Vocabulary-free token estimate: UTF-8 bytes / [`BYTES_PER_TOKEN`], each
/// ASCII digit counted twice, padded by `PADDING_NUM / PADDING_DEN` — and
/// one token a byte for the [`is_dense_script`] characters.
///
/// Bytes rather than chars, because a CJK character is three UTF-8 bytes —
/// 0.75 tokens here against the 0.56–0.75 the providers measured — where
/// `chars / 4` would charge it a quarter. Digits are doubled because every
/// vocabulary splits numbers finer than prose: Gemini bills digit-dense
/// logs at ~2.2 bytes a token, DeepSeek ~2.9.
///
/// No vocabulary ships with the binary: the provider's own count anchors
/// the budget after every call (see `ContextManager::record_call_actual`),
/// so this only prices what was appended since and the occasional full
/// sweep.
#[derive(Debug, Clone, Copy, Default)]
pub struct HeuristicTokenizer;

impl Tokenizer for HeuristicTokenizer {
    fn count_text(&self, text: &str) -> usize {
        let (mut bytes, mut dense) = (0, 0);
        for ch in text.chars() {
            if is_dense_script(ch) {
                dense += ch.len_utf8();
            } else if ch.is_ascii_digit() {
                bytes += 2;
            } else {
                bytes += ch.len_utf8();
            }
        }
        (bytes * PADDING_NUM).div_ceil(BYTES_PER_TOKEN * PADDING_DEN) + dense
    }

    fn count_image(&self, width: u32, height: u32) -> usize {
        baybo_llm::image_tokens(Some(width), Some(height))
    }

    fn count_message(&self, msg: &ChatMessage) -> usize {
        let mut tokens = MESSAGE_OVERHEAD + self.count_message_media(msg, AudioDelivery::Native);
        for block in &msg.content {
            tokens += match block {
                ContentBlock::Text(t) => self.count_text(t.as_str()),
                // Priced by `count_message_media`, already folded in above.
                ContentBlock::Image { .. }
                | ContentBlock::Audio { .. }
                | ContentBlock::File { .. } => 0,
                ContentBlock::ToolUse { input, .. } => {
                    let s = serde_json::to_string(input).unwrap_or_default();
                    TOOL_USE_OVERHEAD + self.count_text(&s)
                }
                ContentBlock::ToolResult { content, .. } => {
                    TOOL_USE_OVERHEAD + self.count_text(content)
                }
                ContentBlock::Thinking { content, .. } => {
                    let text: String = content
                        .iter()
                        .filter_map(|c| match c {
                            baybo_model::ThinkingContent::Text { text, .. } => Some(text.as_str()),
                            baybo_model::ThinkingContent::Summary { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    TOOL_USE_OVERHEAD + self.count_text(&text)
                }
            };
        }
        tokens
    }

    fn count_message_media(&self, msg: &ChatMessage, audio: AudioDelivery) -> usize {
        // One source of truth: `baybo-llm` owns both what a block is
        // delivered as and the caps that bound each arm, so the price and
        // the delivery decision cannot drift apart.
        msg.content
            .iter()
            .map(|block| baybo_llm::content_block_tokens(block, audio))
            .sum()
    }
}

/// Characters every vocabulary measured here bills at one token per UTF-8
/// byte, where the byte estimate would charge them under a third of that:
/// all of the supplementary planes (CJK Ext-B onward, emoji, historic
/// scripts) and the BMP blocks a BPE has no merges for — CJK Ext-A, which is
/// ordinary in Chinese names and classical text, Yi, the Kangxi and CJK
/// radicals, Cherokee. Measured at 1.000 tokens a byte on both `cl100k` and
/// `o200k` over a 32 KiB body; a token never covers less than a byte, so one
/// a byte is also the ceiling.
fn is_dense_script(ch: char) -> bool {
    ch.len_utf8() == 4
        || matches!(
            ch as u32,
            0x13A0..=0x13FF | 0x2E80..=0x2FDF | 0x3400..=0x4DBF | 0xA000..=0xA4CF
        )
}

/// What one tool definition costs a request: its name, description and
/// JSON schema, which is the bulk of it. The one home for the rule, shared
/// by the live budget and the trace view's tool segment.
pub(crate) fn tool_definition_tokens(
    tokenizer: &dyn Tokenizer,
    name: &str,
    description: &str,
    parameters_schema: &serde_json::Value,
) -> usize {
    tokenizer.count_text(name)
        + tokenizer.count_text(description)
        + tokenizer.count_text(&parameters_schema.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The widest an inlined text file can cost: the full delivery cap
    /// plus a wrapper whose two attribute slots are at their widest, at
    /// one token per delivered byte. That is a hard ceiling rather than a
    /// dense-text average — no provider's token covers less than a byte —
    /// and it is measured over the string
    /// [`baybo_llm::render_inlined_document`] emits.
    ///
    /// A BOUND, not the charge: what a block is really charged is priced
    /// from its own `filename` / `mime_type` / `size_bytes`, which is why
    /// the tests below compare against the DELIVERED string rather than
    /// against this.
    const INLINED_FILE_TOKEN_BOUND: usize = baybo_llm::MAX_INLINED_DOCUMENT_BYTES;

    /// The stub a block really renders, which is also the floor under
    /// every media arm — each degrades to it on a fetch failure or an
    /// over-cap payload.
    fn stub_tokens(block: &ContentBlock) -> usize {
        baybo_llm::multimodal::content_block_to_text(block).len()
    }

    #[test]
    fn count_text_empty_is_zero() {
        let tok = HeuristicTokenizer;
        assert_eq!(tok.count_text(""), 0);
    }

    #[test]
    fn count_text_is_nonzero_for_real_input() {
        let tok = HeuristicTokenizer;
        let count = tok.count_text("Hello, world! This is a tokenization test.");
        assert!(count > 0);
        assert!(count < 50);
    }

    #[test]
    fn count_message_includes_structural_overhead() {
        let tok = HeuristicTokenizer;
        let msg = ChatMessage::agent_context(vec![ContentBlock::Text("hi".to_string())]);
        let text_only = tok.count_text("hi");
        assert_eq!(tok.count_message(&msg), text_only + MESSAGE_OVERHEAD);
    }

    fn file_block(mime: &str) -> ContentBlock {
        pdf_block(mime, None)
    }

    fn pdf_block(mime: &str, page_count: Option<u32>) -> ContentBlock {
        sized_block(mime, page_count, None)
    }

    fn sized_block(mime: &str, page_count: Option<u32>, size_bytes: Option<u32>) -> ContentBlock {
        ContentBlock::File {
            blob: baybo_model::BlobRef {
                blob_id: "sha256:doc.tok".to_string(),
            },
            filename: "notes.md".to_string(),
            mime_type: mime.to_string(),
            duration_ms: None,
            page_count,
            size_bytes,
        }
    }

    fn audio_block(duration_ms: Option<u32>) -> ContentBlock {
        ContentBlock::Audio {
            blob: baybo_model::BlobRef {
                blob_id: "sha256:voice.tok".to_string(),
            },
            mime_type: "audio/ogg".to_string(),
            filename: None,
            duration_ms,
        }
    }

    /// Pin: the LLM layer inlines a text-like `File` block's decoded
    /// bytes into the prompt. Pricing it at the flat stub estimate hid
    /// them from `needs_compression`, so the provider's
    /// `context_length_exceeded` — not compaction — was what noticed.
    /// A block with no size still pays the whole body cap, because
    /// nothing else bounds what the fetch will return.
    #[test]
    fn an_unsized_inlined_file_block_is_charged_the_delivery_cap() {
        let tok = HeuristicTokenizer;
        for mime in [
            "text/markdown",
            "text/plain",
            "application/json",
            "text/x-yaml",
            "application/toml",
            "text/csv; charset=utf-8",
        ] {
            let block = file_block(mime);
            let msg = ChatMessage::agent_context(vec![block.clone()]);
            assert_eq!(
                tok.count_message(&msg),
                baybo_llm::inlined_document_tokens("notes.md", mime, None) + MESSAGE_OVERHEAD,
                "{mime}"
            );
            assert!(
                tok.count_message(&msg) > baybo_llm::MAX_DOCUMENT_TEXT_BYTES,
                "{mime}"
            );
            assert!(tok.count_message(&msg) <= INLINED_FILE_TOKEN_BOUND + MESSAGE_OVERHEAD);
        }
    }

    /// Pin (measured): a tiny attachment is charged for its own bytes AND
    /// for its own wrapper, not for the 16 KiB body cap and not for a
    /// wrapper sized to two 483-byte slots it does not have.
    ///
    /// `MAX_MESSAGE_BATCH_ATTACHMENTS` is 64, so 64 attachments are ONE
    /// message. Charged flat, a 400-byte `.md` read 1,699 and 64 of them
    /// read 108,484 — past a 128k window's 96,000 compaction trigger on a
    /// single message — and one `maybe_compress` pass took the transcript
    /// from 25 messages to a single summary with every `File` block gone.
    /// Priced from the block: 660 for one, 42,240 for sixty-four.
    #[test]
    fn a_small_inlined_file_is_charged_for_its_own_bytes_and_its_own_wrapper() {
        const BATCH_ATTACHMENTS: usize = 64;
        let tok = HeuristicTokenizer;
        let small = |bytes: u32| {
            ChatMessage::agent_context(vec![sized_block("text/markdown", None, Some(bytes))])
        };
        let one = tok.count_message(&small(400));
        assert_eq!(one, 660);

        // The charge covers what is really delivered, wrapper included.
        let delivered =
            baybo_llm::render_inlined_document("notes.md", "text/markdown", &"x".repeat(400));
        assert!(
            one - MESSAGE_OVERHEAD >= tok.count_text(&delivered),
            "{one} charged against {} delivered tokens",
            tok.count_text(&delivered)
        );

        let mut budget = crate::budget::TokenBudget::new(128_000, 0.75, 0);
        budget.update(BATCH_ATTACHMENTS * one);
        assert!(
            !budget.needs_compression(),
            "a full attachment batch reads as {}",
            BATCH_ATTACHMENTS * one
        );

        // The body cap is still the ceiling: a body at or over it, and a
        // legacy row with no size at all, pay the same.
        let cap = baybo_llm::inlined_document_tokens("notes.md", "text/markdown", None);
        assert_eq!(
            tok.count_message(&small(baybo_llm::MAX_DOCUMENT_TEXT_BYTES as u32)),
            cap + MESSAGE_OVERHEAD
        );
        assert_eq!(tok.count_message(&small(u32::MAX)), cap + MESSAGE_OVERHEAD);
        assert_eq!(
            tok.count_message(&ChatMessage::agent_context(vec![file_block(
                "text/markdown"
            )])),
            cap + MESSAGE_OVERHEAD
        );
    }

    /// The size is the SOURCE byte count and the delivered body escapes
    /// every literal `</attached-file>` into a longer form, so the
    /// estimate has to cover the growth — measured against the string the
    /// model really receives.
    #[test]
    fn the_sized_estimate_covers_an_escape_heavy_body() {
        let tok = HeuristicTokenizer;
        for repeats in [1usize, 64, 1_024, baybo_llm::MAX_DOCUMENT_TEXT_BYTES / 16] {
            let body = "</attached-file>".repeat(repeats);
            let delivered = baybo_llm::render_inlined_document("notes.md", "text/markdown", &body);
            let charged = baybo_llm::inlined_document_tokens(
                "notes.md",
                "text/markdown",
                Some(body.len() as u32),
            );
            assert!(
                charged >= tok.count_text(&delivered),
                "{repeats} tags: charged {charged}, delivered {} tokens",
                tok.count_text(&delivered)
            );
        }
    }

    /// A PDF reaches the provider as a native document billed per page, so
    /// it can never share the stub estimate that undeliverable MIMEs get.
    #[test]
    fn native_document_and_undeliverable_file_blocks_are_priced_apart() {
        let tok = HeuristicTokenizer;
        let pdf = ChatMessage::agent_context(vec![file_block("application/pdf")]);
        assert_eq!(
            tok.count_message(&pdf),
            baybo_llm::pdf_document_tokens(None) + MESSAGE_OVERHEAD
        );
        // 64 is `MAX_MESSAGE_BATCH_ATTACHMENTS`, so this is ONE message.
        // At the flat bound it read 96,320 — over the 96,000 trigger of a
        // 128k window — on a transcript whose media really costs ~4,500.
        const BATCH_ATTACHMENTS: usize = 64;
        const TRIGGER: usize = 96_000;
        const { assert!(BATCH_ATTACHMENTS * baybo_llm::MAX_CONTENT_STUB_TOKENS > TRIGGER) };
        for mime in ["application/zip", "video/mp4", "application/octet-stream"] {
            let block = file_block(mime);
            let msg = ChatMessage::agent_context(vec![block.clone()]);
            // The stub it really renders, not the widest any block could.
            assert_eq!(
                tok.count_message(&msg),
                stub_tokens(&block) + MESSAGE_OVERHEAD,
                "{mime}"
            );
            assert!(
                BATCH_ATTACHMENTS * tok.count_message(&msg) < TRIGGER,
                "{mime} reads {}",
                BATCH_ATTACHMENTS * tok.count_message(&msg)
            );
        }
    }

    /// Pin: a PDF is priced from the page count probed at ingest, not
    /// from a flat cap and not from bytes. A 200-page document that fits
    /// in 85 KB — measured, an ordinary PDF 1.5 object-stream output —
    /// costs 1.56M tokens; the byte cap that stood in for a page budget
    /// charged it 62,400.
    #[test]
    fn a_pdf_is_charged_for_the_pages_it_really_has() {
        let tok = HeuristicTokenizer;
        for pages in [1, 3, baybo_llm::MAX_PDF_PAGES] {
            let msg = ChatMessage::agent_context(vec![pdf_block("application/pdf", Some(pages))]);
            assert_eq!(
                tok.count_message(&msg),
                baybo_llm::pdf_document_tokens(Some(pages)) + MESSAGE_OVERHEAD,
                "{pages} pages"
            );
        }
        // Rows persisted before the probe existed pay the delivery cap,
        // which the delivery path's own probe makes a real ceiling.
        let legacy = ChatMessage::agent_context(vec![pdf_block("application/pdf", None)]);
        let at_cap = ChatMessage::agent_context(vec![pdf_block(
            "application/pdf",
            Some(baybo_llm::MAX_PDF_PAGES),
        )]);
        assert_eq!(tok.count_message(&legacy), tok.count_message(&at_cap));
    }

    /// Pin: audio is billed per second and `duration_ms` is what carries
    /// it. Flat at 100 with no cap, a 30-minute voice note was 57,500
    /// tokens of undercount on a 128k window.
    #[test]
    fn audio_is_charged_for_its_duration() {
        let tok = HeuristicTokenizer;
        for ms in [1_000, 60_000, 600_000] {
            let block = audio_block(Some(ms));
            let msg = ChatMessage::agent_context(vec![block.clone()]);
            assert_eq!(
                tok.count_message(&msg),
                baybo_llm::audio_tokens(Some(ms)).max(stub_tokens(&block)) + MESSAGE_OVERHEAD,
                "{ms} ms"
            );
        }
        // A one-second note is cheaper than the stub it degrades to, so
        // the stub is what it costs.
        let one_second = audio_block(Some(1_000));
        assert!(baybo_llm::audio_tokens(Some(1_000)) < stub_tokens(&one_second));
        let long = ChatMessage::agent_context(vec![audio_block(Some(600_000))]);
        let short = ChatMessage::agent_context(vec![audio_block(Some(1_000))]);
        assert!(tok.count_message(&long) > tok.count_message(&short));
    }

    /// Every media arm degrades to a text stub when the blob fetch fails
    /// or the payload is over cap, so none may price below what its own
    /// fallback costs.
    #[test]
    fn no_media_arm_prices_below_its_own_stub_fallback() {
        let tok = HeuristicTokenizer;
        for block in [
            ContentBlock::Image {
                blob: baybo_model::BlobRef {
                    blob_id: "sha256:pic.tok".to_string(),
                },
                mime_type: "image/png".to_string(),
                filename: None,
                width: None,
                height: None,
            },
            audio_block(Some(1)),
            audio_block(None),
            pdf_block("application/pdf", Some(1)),
            pdf_block("application/pdf", Some(10_000)),
            pdf_block("application/zip", None),
            pdf_block("text/markdown", None),
        ] {
            let msg = ChatMessage::agent_context(vec![block.clone()]);
            assert!(
                tok.count_message_media(&msg, AudioDelivery::Native) >= stub_tokens(&block),
                "{block:?} prices below its stub"
            );
        }
    }

    /// The whole point of the split: a media ceiling is charged, but it
    /// is never reported as part of the text estimate.
    #[test]
    fn media_ceilings_are_reported_apart_from_the_text_estimate() {
        let tok = HeuristicTokenizer;
        let text = ChatMessage::agent_context(vec![ContentBlock::Text("hello there".into())]);
        assert_eq!(tok.count_message_media(&text, AudioDelivery::Native), 0);

        let mixed = ChatMessage::agent_context(vec![
            ContentBlock::Text("hello there".into()),
            pdf_block("application/pdf", Some(2)),
        ]);
        let media = tok.count_message_media(&mixed, AudioDelivery::Native);
        assert_eq!(media, baybo_llm::pdf_document_tokens(Some(2)));
        assert_eq!(tok.count_message(&mixed) - media, tok.count_message(&text));
    }

    /// Alphabets the providers' vocabularies charge at their worst rate —
    /// measured at 1.000 tokens per byte over a 32 KiB body on `cl100k` and
    /// `o200k` — and which [`is_dense_script`] therefore prices at one token
    /// a byte too.
    const WORST_CASE_ALPHABETS: [char; 6] = [
        '\u{3400}',  // CJK Ext-A — ordinary in Chinese names and classical text
        '\u{A000}',  // Yi syllables
        '\u{2F00}',  // Kangxi radicals
        '\u{13000}', // Egyptian hieroglyphs
        '\u{13A0}',  // Cherokee
        '\u{12000}', // Cuneiform
    ];

    fn fill(ch: char, bytes: usize) -> String {
        std::iter::repeat_n(ch, bytes.div_ceil(ch.len_utf8())).collect()
    }

    /// One source of truth: the bound lives in `baybo-llm` (which does the
    /// inlining), and the text estimate of the same delivered string must
    /// never exceed it — otherwise a file pasted as a tool result would cost
    /// the budget more than the same file attached, and the attachment's
    /// charge would be the under-count. The dense scripts are the case that
    /// reaches the bound: one token a byte on both sides.
    ///
    /// Measured against what is DELIVERED, not against a bare body: the
    /// template, both client-controlled attribute slots at their widest,
    /// the escaping the terminator guard applies, and the truncation
    /// marker are all real prompt bytes.
    #[test]
    fn inlined_charge_covers_the_text_estimate_of_the_worst_case_wrapper() {
        let tok = HeuristicTokenizer;
        let widest_slot = fill('\u{13000}', baybo_llm::MAX_DOCUMENT_TEXT_BYTES);
        for ch in WORST_CASE_ALPHABETS {
            for body in [
                fill(ch, baybo_llm::MAX_DOCUMENT_TEXT_BYTES * 2),
                format!(
                    "{}{}",
                    "</ATTACHED-FILE\n>".repeat(baybo_llm::MAX_DOCUMENT_TEXT_BYTES / 8),
                    fill(ch, baybo_llm::MAX_DOCUMENT_TEXT_BYTES)
                ),
            ] {
                let delivered =
                    baybo_llm::render_inlined_document(&widest_slot, &widest_slot, &body);
                let estimated = tok.count_text(&delivered);
                assert!(
                    INLINED_FILE_TOKEN_BOUND >= estimated,
                    "U+{:04X}: charge {INLINED_FILE_TOKEN_BOUND} is under the {estimated} \
                     estimated over {} delivered bytes",
                    ch as u32,
                    delivered.len()
                );
            }
        }
    }

    /// The other half of the bound, swept rather than argued: no text is
    /// estimated above one token per byte, the rate no provider's token can
    /// beat. The dense scripts sit exactly on it; everything else — digits
    /// included — well under.
    #[test]
    fn no_input_is_estimated_above_one_token_per_byte() {
        const SWEEP_BODY_BYTES: usize = 2048;
        const SWEEP_STEP: u32 = 2003;
        let tok = HeuristicTokenizer;
        let mut code_point = ' ' as u32;
        while code_point < char::MAX as u32 {
            if let Some(ch) = char::from_u32(code_point) {
                let sample = fill(ch, SWEEP_BODY_BYTES);
                let counted = tok.count_text(&sample);
                assert!(
                    counted <= sample.len(),
                    "U+{code_point:04X}: {counted} tokens over {} bytes",
                    sample.len()
                );
            }
            code_point += SWEEP_STEP;
        }
    }

    /// Pin (in-tree measurement, `cl100k` and `o200k`): these scripts bill
    /// one token a byte, and the byte estimate alone charged them 0.29 — a
    /// 32 KiB tool result of CJK Ext-A read ~9.4k against ~32.8k billed.
    #[test]
    fn a_dense_script_is_charged_a_token_a_byte() {
        let tok = HeuristicTokenizer;
        for ch in WORST_CASE_ALPHABETS {
            let sample = fill(ch, 3_000);
            assert_eq!(tok.count_text(&sample), sample.len(), "U+{:04X}", ch as u32);
        }
        let common_cjk = "上".repeat(1_000);
        assert!(
            tok.count_text(&common_cjk) < common_cjk.len() / 3,
            "common CJK is not dense"
        );
    }

    /// Every estimate is charged against the window, so a single
    /// attachment must not be able to force a compaction on its own on the
    /// smallest window that can receive it.
    #[test]
    fn one_attachment_stays_under_a_small_models_compaction_trigger() {
        // Any provider can be handed inlined text; 32k is the smallest
        // window worth designing against.
        let mut budget = crate::budget::TokenBudget::new(32_000, 0.75, 0);
        budget.update(INLINED_FILE_TOKEN_BOUND + MESSAGE_OVERHEAD);
        assert!(!budget.needs_compression());
        budget.update(2 * (INLINED_FILE_TOKEN_BOUND + MESSAGE_OVERHEAD));
        assert!(budget.needs_compression());

        // A native PDF only reaches OpenAI / Anthropic / Gemini, and audio
        // only OpenAI / Gemini — OpenAI's 128k default is the tightest of
        // either set. Both are measured at their delivery cap.
        for at_cap in [
            baybo_llm::pdf_document_tokens(Some(baybo_llm::MAX_PDF_PAGES)),
            baybo_llm::audio_tokens(Some(baybo_llm::MAX_AUDIO_SECONDS * 1_000)),
        ] {
            let mut budget = crate::budget::TokenBudget::new(128_000, 0.75, 0);
            budget.update(at_cap + MESSAGE_OVERHEAD);
            assert!(!budget.needs_compression(), "{at_cap}");
            budget.update(2 * (at_cap + MESSAGE_OVERHEAD));
            assert!(budget.needs_compression(), "{at_cap}");
        }
    }

    /// The provider ratios this is sized from (probes against
    /// deepseek-v4-flash and gemini billing, 2026-10): prose and code near
    /// four bytes a token, CJK three bytes a character, digits finer.
    #[test]
    fn the_estimate_follows_bytes_with_digits_doubled_and_padded() {
        let tok = HeuristicTokenizer;
        assert_eq!(tok.count_text("abcd"), 2, "4 bytes × 1.15 / 4, rounded up");
        assert_eq!(tok.count_text(&"a".repeat(4_000)), 1_150);
        assert_eq!(tok.count_text(&"上".repeat(1_000)), 863, "3 bytes each");
        assert_eq!(
            tok.count_text(&"7".repeat(1_000)),
            tok.count_text(&"a".repeat(2_000)),
            "a digit costs two bytes' worth"
        );
    }
}
