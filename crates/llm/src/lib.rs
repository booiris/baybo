pub mod billed;
pub mod credentials;
pub mod effort;
mod error;
pub mod guard;
pub mod json_extract;
pub mod media_probe;
pub mod multimodal;
pub mod openrouter;
pub mod providers;
pub mod registry;
mod tool_name;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use futures::stream::{Stream, StreamExt};
use rig::OneOrMany;
use rig::completion::message::{
    Audio, AudioMediaType, Document, DocumentMediaType, DocumentSourceKind, Image, ImageDetail,
    ImageMediaType,
};
use rig::completion::{
    self, AssistantContent, CompletionError, CompletionModel, CompletionRequest, GetTokenUsage,
    ToolDefinition,
};
use rig::message::{Message, Text, UserContent};
use rig::providers::{
    anthropic, cohere, deepseek, gemini, groq, huggingface, hyperbolic, llamafile, minimax,
    mistral, moonshot, ollama, openai, perplexity, together, xai, xiaomimimo, zai,
};
use rig::streaming::{self, StreamedAssistantContent};
use serde::{Deserialize, Serialize};
use tracing::debug;

pub use crate::billed::{
    Attribution, BilledChat, BilledChatResponse, BoundBilledLlm, CostHooks, LlmCostRecorder,
    SYSTEM_USER_ID,
};
pub use crate::error::LlmError;
pub(crate) use crate::error::{
    proxied_client, reqwest_to_error, rig_completion_to_error, status_to_error,
};
pub use crate::guard::{BillableLlm, LlmCallGuard};
pub use crate::json_extract::extract_json_object;
pub use crate::providers::{FactoryDefaults, factory_defaults_for};
pub use crate::registry::{
    LiveModelInfo, LlmPricingOverride, LlmProviderConfig, LlmProviderRegistry,
};
/// Re-exported next to [`Attribution`] (which carries it) so call sites
/// binding an attribution don't need a separate `baybo_model` import.
pub use baybo_model::CallReason;

/// Process-wide handle to the default provider registry. Lazily
/// constructed on first lookup, shared by the metadata-helper fns
/// below. The default registry is pure (registers stack-built unit
/// factories, no I/O) so first-touch latency is negligible.
fn default_registry() -> &'static LlmProviderRegistry {
    static REGISTRY: OnceLock<LlmProviderRegistry> = OnceLock::new();
    REGISTRY.get_or_init(LlmProviderRegistry::with_default_providers)
}

/// Default chat-completion base URL each built-in factory advertises.
/// Resolves through the default registry so a new provider only has to
/// declare its URL via [`crate::registry::LlmProviderFactory::default_base_url`]
/// (or the `base_url = …` macro kwarg) — no separate match arm here.
/// `None` for providers that don't advertise a canonical default; the
/// runtime falls through to whatever the underlying rig client bakes
/// in, and the setup wizard shows an empty default prompt.
pub fn default_base_url_for_provider(provider: &str) -> Option<&'static str> {
    default_registry().factory_for(provider)?.default_base_url()
}

/// Conventional env var each built-in factory advertises for its API
/// key. Resolves through the default registry; the last-resort fallback
/// in [`crate::credentials::resolve_api_key`] consults this when neither
/// an explicit `api_key_env` nor the per-entry vault key is set.
/// `None` for keyless / OAuth providers and for providers without a
/// well-known env-var convention.
pub fn default_api_key_env_for_provider(provider: &str) -> Option<&'static str> {
    default_registry()
        .factory_for(provider)?
        .default_api_key_env()
}

/// Strip `; charset=…` and lowercase, then map common MIME strings to
/// rig's `ImageMediaType` enum. `None` means the model can't natively
/// consume this image kind — the caller falls back to a text stub.
fn parse_image_media_type(mime: &str) -> Option<ImageMediaType> {
    let bare = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    match bare.as_str() {
        "image/jpeg" | "image/jpg" => Some(ImageMediaType::JPEG),
        "image/png" => Some(ImageMediaType::PNG),
        "image/gif" => Some(ImageMediaType::GIF),
        "image/webp" => Some(ImageMediaType::WEBP),
        "image/heic" => Some(ImageMediaType::HEIC),
        "image/heif" => Some(ImageMediaType::HEIF),
        "image/svg+xml" => Some(ImageMediaType::SVG),
        _ => None,
    }
}

fn parse_audio_media_type(mime: &str) -> Option<AudioMediaType> {
    let bare = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    match bare.as_str() {
        "audio/wav" | "audio/x-wav" => Some(AudioMediaType::WAV),
        "audio/mpeg" => Some(AudioMediaType::MP3),
        "audio/aac" => Some(AudioMediaType::AAC),
        "audio/ogg" | "audio/opus" => Some(AudioMediaType::OGG),
        "audio/flac" => Some(AudioMediaType::FLAC),
        "audio/mp4" | "audio/m4a" => Some(AudioMediaType::M4A),
        _ => None,
    }
}

/// Substrings every audio-capable OpenAI model carries in its id
/// (`gpt-4o-audio-preview`, `gpt-4o-mini-realtime-preview`, `gpt-audio`,
/// `gpt-realtime`). No non-audio OpenAI model is named for either.
const OPENAI_AUDIO_MODEL_MARKERS: [&str; 2] = ["audio", "realtime"];
pub(crate) const DOCUMENT_FILENAME_PARAM: &str = "filename";

/// OpenAI takes `input_audio` on its audio / realtime model families
/// only: `gpt-4o`, `gpt-4.1` and `o3` answer the WHOLE request with a
/// 400, and because the block stays in history every later turn 400s too
/// — the poisoning just moves from rig's converter to the provider.
/// [`ModelInfo`] carries no per-model audio flag, so the naming
/// convention is the only signal available; anything else — including an
/// OpenAI-compatible `base_url` serving some other vendor's model — is
/// treated as audio-incapable and gets the text stub.
fn openai_model_accepts_audio(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    OPENAI_AUDIO_MODEL_MARKERS
        .iter()
        .any(|marker| id.contains(marker))
}

/// How a `ContentBlock::File` can actually reach the model. rig's
/// provider converters only take a PDF as a real document, and only on
/// some providers: Anthropic returns `Err` for every other
/// `DocumentMediaType` — and that error is collected over the WHOLE chat
/// history, so one `.md` upload fails every later turn until compaction
/// evicts the row — while the DeepSeek / Ollama / HuggingFace converters
/// splice the base64 payload undecoded into the joined user text, feeding
/// the model literal base64. Text-like documents are therefore decoded
/// here and sent as plain text, which every provider handles; the PDF leg
/// additionally needs [`AnyCompletionModel::accepts_pdf_document`].
enum DocumentDelivery {
    Pdf,
    Text,
}

/// Strip `; charset=…` and lowercase, then classify. `None` means the
/// bytes can't be delivered at all — the caller falls back to a text
/// stub. Only MIMEs that are unambiguously UTF-8 text earn
/// [`DocumentDelivery::Text`]; `application/octet-stream` deliberately
/// does not, since it is routinely binary.
fn document_delivery(mime: &str) -> Option<DocumentDelivery> {
    let bare = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    match bare.as_str() {
        "application/pdf" => Some(DocumentDelivery::Pdf),
        "text/plain"
        | "text/html"
        | "text/css"
        | "text/markdown"
        | "text/csv"
        | "application/xml"
        | "text/xml"
        | "application/javascript"
        | "text/javascript"
        | "text/x-python"
        | "application/x-python"
        | "application/json"
        | "text/json"
        | "application/yaml"
        | "application/x-yaml"
        | "text/yaml"
        | "text/x-yaml"
        | "application/toml"
        | "text/x-toml" => Some(DocumentDelivery::Text),
        _ => None,
    }
}

/// Whether a `ContentBlock::File` of this MIME is decoded and inlined as
/// prompt text by [`LlmClient::user_content_for_block`]. Exported for
/// `baybo-context`'s tokenizer: an inlined file spends a real slice of
/// the context window, and pricing it like an undelivered stub is what
/// let a transcript sail past the budget without ever tripping
/// compaction.
pub fn inlines_document_as_text(mime: &str) -> bool {
    matches!(document_delivery(mime), Some(DocumentDelivery::Text))
}

/// Whether a `ContentBlock::File` of this MIME can reach the model as a
/// native document. Exported for the same reason as
/// [`inlines_document_as_text`]: a PDF is billed per page, so pricing it
/// like an undelivered stub hides an order of magnitude more from the
/// budget than an inlined `.md` does. Whether the *model* takes documents
/// is a separate gate ([`AnyCompletionModel::accepts_pdf_document`]) the
/// tokenizer cannot see; charging every PDF as if it were delivered
/// over-counts on a model that stubs it, which is the safe direction and
/// self-corrects on the next successful call's `record_call_actual`.
pub fn delivers_pdf_document(mime: &str) -> bool {
    matches!(document_delivery(mime), Some(DocumentDelivery::Pdf))
}

/// Whether a row of this role carries its media blocks to the provider.
///
/// `build_completion_request` runs [`LlmClient::user_content_for_block`] —
/// the only path that materialises bytes — on a **user** row alone. An
/// assistant row keeps text, tool calls and thinking; a system row is
/// flattened to text; a tool row keeps only its result. Media anywhere
/// else is dropped without even a stub, so it costs the provider nothing.
///
/// Exported because the price and the delivery decision must not drift
/// apart, and `baybo-context`'s budget cannot re-derive this:
/// [`content_block_tokens`] takes a bare `&ContentBlock` and never sees a
/// role. The agent loop folds `AttachFile` media onto the turn's final
/// **assistant** row so the file persists across a reload, which is
/// exactly the case that would otherwise be charged
/// [`IMAGE_TOKEN_CEILING`] against a provider charge of zero.
/// `delivers_media_matches_the_conversion` pins the two together.
pub fn delivers_media(role: baybo_model::Role) -> bool {
    matches!(role, baybo_model::Role::User)
}

/// Wrapper the model is asked to read as a delimiter around an inlined
/// attachment. Every slot is client-controlled — the filename and MIME
/// come off the wire and the body is the file itself — so all three are
/// sanitized before substitution and filled in ONE pass by
/// [`render_slots`]. Sequential `String::replace` calls would let the
/// filename, substituted first, supply the `{{content}}` placeholder and
/// have the whole body expanded into the name attribute.
const DOCUMENT_TEXT_TEMPLATE: &str = r#"<attached-file name="{{filename}}" type="{{mime_type}}">
{{content}}
</attached-file>"#;

const DOCUMENT_CLOSE_TAG: &str = "</attached-file>";
const DOCUMENT_CLOSE_TAG_ESCAPED: &str = "&lt;/attached-file&gt;";

/// Marker appended when the body is cut. Rendered through
/// [`render_slots`] so the literal has one home and its worst-case length
/// is derivable from it.
const DOCUMENT_TRUNCATION_MARKER: &str = "\n... [truncated {{elided}} bytes, total {{total}}] ...";

/// Widest decimal rendering of a `usize`, for the two byte counts
/// [`DOCUMENT_TRUNCATION_MARKER`] substitutes.
const MAX_USIZE_DIGITS: usize = usize::MAX.ilog10() as usize + 1;

const DOCUMENT_TRUNCATION_MARKER_MAX_BYTES: usize =
    DOCUMENT_TRUNCATION_MARKER.len() + 2 * MAX_USIZE_DIGITS;

/// Cap on the text [`document_body`] emits for one document attachment —
/// enforced over the bytes actually delivered, not over the decoded input,
/// so escaping a terminator can never push the body past it.
///
/// These bytes are real prompt text and `baybo-context`'s tokenizer
/// charges every inlined `File` block one token per delivered byte (the
/// block carries no size, so the cap is the only bound available without
/// I/O). Honestly priced, a full cap plus its wrapper costs
/// [`MAX_INLINED_DOCUMENT_BYTES`] ≈ 17.5k tokens, which stays under the
/// 0.75 compaction trigger of even a 32k-window model (24,000): one
/// attachment can never force a compaction on its own, two can — which is
/// right, because two genuinely would cost that much. The 32 KiB this
/// replaces would have been ~33.9k tokens, more than such a model's whole
/// window for a single attachment. 16 KiB is still ~400 lines / ~2.5k
/// words, which covers the realistic "here is my config / log / doc".
pub const MAX_DOCUMENT_TEXT_BYTES: usize = 16 * 1024;

/// Gemini crops any image with a side over 384 px into tiles of at most
/// 768x768 and bills 258 tokens per tile, with no downscaling first — so
/// its cost grows with the pixel count and it, not Anthropic, sets the
/// cap below.
const IMAGE_TILE_PX: u32 = 768;
const IMAGE_TOKENS_PER_TILE: usize = 258;

/// OpenAI's `detail: high` tiling: the image is scaled so its SHORT side
/// is 768 px, then cut into 512-px tiles billed at
/// [`OPENAI_TOKENS_PER_TILE`] over a flat [`OPENAI_IMAGE_BASE_TOKENS`].
/// The short side is always exactly 768 after that scaling, so the tile
/// grid is two wide and `ceil(long' / 512)` tall — the "at most eight
/// tiles" this replaces holds only below an 8:3 aspect ratio, and a
/// 1170x23400 iOS scrolling screenshot becomes 768x15360 = 60 tiles.
const OPENAI_TILE_PX: u32 = 512;
const OPENAI_SHORT_EDGE_PX: u32 = 768;
const OPENAI_TOKENS_PER_TILE: usize = 170;
const OPENAI_IMAGE_BASE_TOKENS: usize = 85;

/// Anthropic downscales to a long edge of 1,568 px and reads the result
/// as 28-px patches, so its cost is bounded at 56 x 56 = 3,136 tokens
/// whatever arrives — the only one of the three that bounds itself.
const ANTHROPIC_MAX_EDGE_PX: u64 = 1_568;
const ANTHROPIC_PATCH_PX: u64 = 28;

/// Square [`IMAGE_TOKEN_CEILING`] is derived from. 4,096 covers a 12 MP
/// phone photo (3024x4032 → 4x6 tiles) and a 300 dpi A4 scan (2480x3508 →
/// 4x5), which is what actually arrives. It is not itself a guard — the
/// guard is on the resulting PRICE, which is the only quantity an edge
/// does not bound.
const MAX_IMAGE_EDGE_PX: u32 = 4_096;

/// Cap on the raw bytes of an image handed to a provider, the same guard
/// the audio and PDF arms carry. 5 MiB is Anthropic's documented
/// per-image API limit: above it the request is rejected outright, and
/// because `build_completion_request` re-walks the whole history every
/// turn that rejection would repeat until compaction evicted the row.
/// Over-cap degrades to the text stub instead.
///
/// **It bounds the payload, not the price.** Compressed bytes say nothing
/// about pixels and no provider downscales before billing, so this cap
/// admits a 12000x9000 flat render — under a megabyte of PNG, 192 Gemini
/// tiles, 49,536 tokens. [`IMAGE_TOKEN_CEILING`] is the guard that bounds
/// what is priced.
pub const MAX_IMAGE_DOCUMENT_BYTES: usize = 5 * 1024 * 1024;

/// Tokens of image one attachment may cost the budget, enforced at
/// delivery from the PROBED dimensions
/// ([`media_probe::image_dimensions`]) exactly as [`MAX_PDF_PAGES`] is
/// enforced from the probed page count. An image priced above it — or one
/// whose dimensions cannot be read at all — degrades to the text stub.
///
/// It is also what [`image_tokens`] charges a block carrying no
/// dimensions, and the delivery guard is what makes that a true ceiling
/// rather than a hope. Before the guard the same number was derived from
/// a 4,096-px edge that nothing enforced, so it was not a ceiling in
/// either direction it was claimed to be: a routine 6000x4000 design
/// export costs 12,384, a 48 MP iPhone HEIF 22,704, and a 1170x23400
/// scrolling screenshot 15,996.
///
/// **Why an edge cap alone would not do it**: Gemini's cost is bounded by
/// the pixel grid, but OpenAI's is driven by ASPECT — a 1x4096 sliver is
/// 12,288 tiles — so the only quantity that bounds the price is the
/// price.
pub const IMAGE_TOKEN_CEILING: usize = {
    let per_edge = MAX_IMAGE_EDGE_PX.div_ceil(IMAGE_TILE_PX) as usize;
    per_edge * per_edge * IMAGE_TOKENS_PER_TILE
};

/// What `baybo-context`'s tokenizer charges one `ContentBlock::Image`,
/// priced from the tiling rules of the providers images are delivered to
/// rather than from a flat ceiling.
///
/// The maximum over the three, because any of them may be the model on
/// the other end of a given turn and the block is priced once, before the
/// tokenizer knows which: Gemini tiles the raw pixel grid at
/// [`IMAGE_TILE_PX`] and does not downscale, OpenAI scales the short side
/// to [`OPENAI_SHORT_EDGE_PX`] and tiles at [`OPENAI_TILE_PX`], and
/// Anthropic downscales to [`ANTHROPIC_MAX_EDGE_PX`] and reads
/// [`ANTHROPIC_PATCH_PX`] patches.
///
/// Capped at [`IMAGE_TOKEN_CEILING`] rather than zeroed: since delivery
/// became per-provider ([`AnyCompletionModel::delivers_image`]), a price
/// above the ceiling is no longer proof of a stub — Anthropic bills a
/// 24 MP photo 2,352 and takes it, while Gemini's grid puts the same
/// photo at 12,384. Charging the ceiling over-counts a stubbed image and
/// under-counts none that is delivered. Absent dimensions charge the
/// ceiling too; a zero-pixel header charges nothing and lets
/// [`content_block_tokens`]'s stub floor price it.
pub fn image_tokens(width: Option<u32>, height: Option<u32>) -> usize {
    let (Some(width), Some(height)) = (width, height) else {
        return IMAGE_TOKEN_CEILING;
    };
    if width == 0 || height == 0 {
        return 0;
    }
    provider_image_tokens(width, height).min(IMAGE_TOKEN_CEILING as u64) as usize
}

/// The dearest of the three providers' tilings for one image, in `u64`
/// because the grids are evaluated on the raw dimensions and a 1 x
/// `u32::MAX` sliver is 8.4 billion OpenAI tiles.
///
/// The estimate, not the gate: [`image_tokens`] is computed with no model
/// in sight, so it pays the worst of the three. What one provider really
/// bills is [`AnyCompletionModel::delivers_image`]'s business.
fn provider_image_tokens(width: u32, height: u32) -> u64 {
    gemini_image_tokens(width, height)
        .max(openai_image_tokens(width, height))
        .max(anthropic_image_tokens(width, height))
}

fn gemini_image_tokens(width: u32, height: u32) -> u64 {
    u64::from(width.div_ceil(IMAGE_TILE_PX))
        * u64::from(height.div_ceil(IMAGE_TILE_PX))
        * IMAGE_TOKENS_PER_TILE as u64
}

/// Undefined on a zero edge, which divides by the short side — every
/// caller gates on [`image_tokens`]' or [`AnyCompletionModel::delivers_image`]'s
/// zero-pixel refusal first.
fn openai_image_tokens(width: u32, height: u32) -> u64 {
    let (short, long) = (u64::from(width.min(height)), u64::from(width.max(height)));
    // Scaling the short side to 768 leaves the long side at
    // `long * 768 / short`, so its tile count is
    // `ceil(long * 768 / (short * 512))` with no float step.
    let long_tiles =
        (long * u64::from(OPENAI_SHORT_EDGE_PX)).div_ceil(short * u64::from(OPENAI_TILE_PX));
    let short_tiles = u64::from(OPENAI_SHORT_EDGE_PX.div_ceil(OPENAI_TILE_PX));
    OPENAI_IMAGE_BASE_TOKENS as u64 + long_tiles * short_tiles * OPENAI_TOKENS_PER_TILE as u64
}

fn anthropic_image_tokens(width: u32, height: u32) -> u64 {
    let (short, long) = (u64::from(width.min(height)), u64::from(width.max(height)));
    let (aw, ah) = if long > ANTHROPIC_MAX_EDGE_PX {
        (
            (short * ANTHROPIC_MAX_EDGE_PX).div_ceil(long),
            ANTHROPIC_MAX_EDGE_PX,
        )
    } else {
        (short, long)
    };
    aw.div_ceil(ANTHROPIC_PATCH_PX) * ah.div_ceil(ANTHROPIC_PATCH_PX)
}

/// What one page of a native PDF costs. Anthropic bills a page as up to
/// 3,000 text tokens **plus** the page rendered as an image, and an image
/// costs `ceil(w / 28) * ceil(h / 28)` visual tokens capped at 4,784 on
/// the high-resolution tier — so 7,800 is the documented ceiling rounded
/// up.
const PDF_TOKENS_PER_PAGE: usize = 7_800;

/// Pages of native PDF one attachment may cost the budget, enforced from
/// the PROBED page count ([`media_probe::pdf_page_count`]). 128k is the
/// smallest context window among the providers
/// [`AnyCompletionModel::accepts_pdf_document`] admits (OpenAI — see
/// `providers::factory_defaults_for`), whose 0.75 compaction trigger is
/// 96,000 tokens: 12 × 7,800 = 93,600 fits under it and 13 × 7,800 =
/// 101,400 does not, so one PDF can never force a compaction on its own.
///
/// A byte cap cannot stand in for this, which is what the version before
/// it assumed. Measured page densities, three producers, page counts from
/// Apple's `CGPDFDocument` and reproduced exactly by our own probe:
///
/// | producer                       | bytes/page | pages under a 64 KiB cap |
/// |--------------------------------|-----------:|-------------------------:|
/// | `cupsfilter`, 375p prose       |      2,158 |                       30 |
/// | classic xref, 50p dense        |      4,007 |                       16 |
/// | 1.5 object streams, 64p dense  |      3,849 |                       17 |
/// | 1.5 object streams, 200p sparse|        425 |                      154 |
/// | 1.5 object streams, 5000p blank|         10 |                    6,658 |
///
/// The floor is ~10 bytes per page, not the 8 KiB the old cap assumed, so
/// 64 KiB admitted 6,658 pages — 51.9M tokens charged as 62,400. Density
/// spans three orders of magnitude and a blank page still pays the
/// per-page image render, so nothing derived from byte count bounds it.
pub const MAX_PDF_PAGES: u32 = 12;

/// Page counts the delivery path will hand over as a native document.
/// Inclusive from ONE: a PDF that loads but whose page tree cannot be
/// walked — a `/Pages` node whose `/Kids` cycle back to it — probes as
/// zero pages, which is a failed parse wearing a valid-looking price.
/// `pdf_document_tokens` already charges it the stub floor; without the
/// lower bound the gate delivered it anyway.
const DELIVERABLE_PDF_PAGES: std::ops::RangeInclusive<u32> = 1..=MAX_PDF_PAGES;

/// Cap on the raw bytes of a PDF handed to a provider as a native
/// document. With [`MAX_PDF_PAGES`] enforced from the probed page count
/// this is no longer a page budget in disguise — it bounds what gets
/// parsed and base64'd into one request. 8 MiB covers twelve pages of
/// anything a scanner produces (~680 KiB/page) and inflates to ~10.7 MiB
/// of base64; over-cap degrades to the text stub, which is also what
/// keeps a `Document` from 413ing forever, since
/// `build_completion_request` re-walks the whole history every turn.
///
/// Exported so an ingest probe stops at the same number: a payload above
/// it is ALWAYS stubbed, so a page count recovered from it would be a
/// price charged for a block that costs the stub.
pub const MAX_PDF_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;

/// What `baybo-context`'s tokenizer charges one `ContentBlock::File` the
/// LLM layer may hand over as a native document.
///
/// `page_count` is the probed, server-derived count on the block.
/// [`None`] — a row persisted before the field existed, or a file that
/// did not parse at ingest — charges the full [`MAX_PDF_PAGES`] cap. That
/// is a true ceiling rather than a guess: delivery re-probes the fetched
/// bytes and stubs anything above the cap, so no PDF can reach a provider
/// costing more. It over-charges a one-page legacy row until the next
/// successful call anchors the budget on the provider's own count.
///
/// Zero means "delivery stubs this", not "free" — the stub floor is
/// applied once, per block and from the block's own strings, by
/// [`content_block_tokens`].
pub fn pdf_document_tokens(page_count: Option<u32>) -> usize {
    match page_count {
        // Over the cap the delivery path stubs it, so the stub is what it
        // really costs.
        Some(pages) if pages > MAX_PDF_PAGES => 0,
        Some(pages) => pages as usize * PDF_TOKENS_PER_PAGE,
        None => MAX_PDF_PAGES as usize * PDF_TOKENS_PER_PAGE,
    }
}

/// What a provider charges for one second of input audio. Gemini
/// documents 32 tokens per second and is the only provider family here
/// that publishes a rate; OpenAI's audio models bill audio input by the
/// same order. Priced per second because that is how it is billed — the
/// flat per-item number this replaces was under-counting a ten-minute
/// voice note by 19,100 tokens.
const AUDIO_TOKENS_PER_SECOND: usize = 32;

/// Seconds of audio one attachment may cost the budget. The tightest
/// window among the providers [`AnyCompletionModel::accepts_audio_content`]
/// admits is OpenAI's 128k, whose 0.75 trigger is 96,000 tokens; 1,800 s
/// × 32 = 57,600, so one voice note can never force a compaction on its
/// own and two can. Over-cap degrades to the text stub.
pub const MAX_AUDIO_SECONDS: u32 = 1_800;

/// Cap on the raw bytes of an audio payload handed to a provider. Bounds
/// the probe and the base64'd request, not the token cost — that is
/// [`MAX_AUDIO_SECONDS`]'s job. 16 MiB is 30 minutes at 74 kbps, well
/// above any voice note (Telegram ships 32 kbps Opus: 7.2 MiB for the
/// full half hour) while refusing the 158 MB a half hour of 44.1 kHz
/// stereo WAV would be.
/// Exported for the same reason as [`MAX_PDF_DOCUMENT_BYTES`]: an ingest
/// probe that reads past a delivery cap buys a price for a block that is
/// always stubbed.
pub const MAX_AUDIO_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;

/// What `baybo-context`'s tokenizer charges one `ContentBlock::Audio`.
///
/// `duration_ms` is server-probed ([`media_probe::audio_duration_ms`]) at
/// every ingest point. [`None`] charges the full [`MAX_AUDIO_SECONDS`]
/// cap, which is a true ceiling for the same reason the PDF one is:
/// delivery probes the fetched bytes itself and stubs anything longer, or
/// anything it cannot measure at all.
pub fn audio_tokens(duration_ms: Option<u32>) -> usize {
    let seconds = match duration_ms {
        Some(ms) => ms.div_ceil(MS_PER_SECOND).min(MAX_AUDIO_SECONDS),
        None => MAX_AUDIO_SECONDS,
    };
    seconds as usize * AUDIO_TOKENS_PER_SECOND
}

const MS_PER_SECOND: u32 = 1_000;

/// Bytes `user_content_for_block` delivers around the body of an inlined
/// text document when both attribute slots are at their WIDEST: the
/// template, two full [`multimodal::MAX_SLOT_BYTES`] slots, and the
/// truncation marker. Charged on top of every inlined file no matter how
/// small the file is — the wrapper is real prompt text, and a bound over
/// the body alone would not cover it.
///
/// A bound, not a price. [`inlined_document_tokens`] charges the block's
/// own strings, sanitized the way delivery sanitizes them, because the
/// block carries them and no I/O is needed to read them; this flat number
/// survives only as the worst case the bound has to cover. Charged flat it
/// was 1,145 phantom bytes on every attachment — 64 of them on one
/// message (the per-message attachment cap) read as 108,484 tokens and
/// compacted a 128k window.
const INLINED_DOCUMENT_WRAPPER_BYTES: usize = DOCUMENT_TEXT_TEMPLATE.len()
    + 2 * multimodal::MAX_SLOT_BYTES
    + DOCUMENT_TRUNCATION_MARKER_MAX_BYTES;

/// Upper bound on the bytes `user_content_for_block` delivers for one
/// inlined text document: the wrapper plus a full body.
///
/// `baybo-context`'s tokenizer charges one token per byte of this. That is
/// a ceiling, not an average: `cl100k`'s base vocabulary is the 256 single
/// bytes and every merge replaces two tokens with one, so a token always
/// covers at least one input byte. Counting the template's `{{…}}`
/// placeholders alongside the values that replace them is slack, not
/// error.
pub const MAX_INLINED_DOCUMENT_BYTES: usize =
    INLINED_DOCUMENT_WRAPPER_BYTES + MAX_DOCUMENT_TEXT_BYTES;

/// What `baybo-context`'s tokenizer charges one `ContentBlock::File` the
/// LLM layer inlines as prompt text.
///
/// `size_bytes` is the blob's server-derived byte length. Charging every
/// such block the full [`MAX_INLINED_DOCUMENT_BYTES`] instead — which is
/// what the block carrying no size forced — billed a 400-byte `.md`
/// 17,529 tokens, so six of them read as 105,178 and tripped compaction
/// on a 128k window all by themselves.
///
/// The body is bounded rather than taken at face value: [`document_body`]
/// rewrites a literal [`DOCUMENT_CLOSE_TAG`] into its escaped form, the
/// only step that can make the delivered body longer than the source, and
/// it is capped at [`MAX_DOCUMENT_TEXT_BYTES`] of OUTPUT. [`None`] — a row
/// persisted before the field existed, or a blob that could not be stat'd
/// — keeps the cap as its conservative fallback.
///
/// The WRAPPER is charged from `filename` and `mime_type`, sanitized by
/// the same [`multimodal::sanitize_slot`] delivery substitutes, because
/// the block carries both strings and what they render to is exactly what
/// the model receives — an exact charge with no I/O. The flat
/// [`INLINED_DOCUMENT_WRAPPER_BYTES`] survives only as the bound.
/// Likewise the truncation marker, which delivery emits only when the
/// body really was cut.
pub fn inlined_document_tokens(filename: &str, mime_type: &str, size_bytes: Option<u32>) -> usize {
    let grown = match size_bytes {
        Some(bytes) => (bytes as usize)
            .saturating_mul(DOCUMENT_CLOSE_TAG_ESCAPED.len())
            .div_ceil(DOCUMENT_CLOSE_TAG.len()),
        None => usize::MAX,
    };
    let body = grown.min(MAX_DOCUMENT_TEXT_BYTES);
    let marker = if grown > MAX_DOCUMENT_TEXT_BYTES {
        DOCUMENT_TRUNCATION_MARKER_MAX_BYTES
    } else {
        0
    };
    DOCUMENT_TEXT_TEMPLATE.len()
        + multimodal::sanitize_slot(filename).len()
        + multimodal::sanitize_slot(mime_type).len()
        + body
        + marker
}

/// Token cost of the widest stub any media block can render. A BOUND, not
/// a price: [`content_block_tokens`] charges
/// [`multimodal::content_block_to_text`] — the very string delivery emits
/// — because the block carries every slot that goes into it. Charged flat
/// it was 1,505 tokens for a `[file: report.zip (application/zip)
/// blob_id=…]` that renders 56, and 64 such blocks on one message read as
/// 96,320 against a real 4,500.
pub const MAX_CONTENT_STUB_TOKENS: usize = multimodal::MAX_CONTENT_STUB_BYTES;

/// What `baybo-context`'s tokenizer charges one [`ContentBlock`], and the
/// single place the stub floor is applied.
///
/// Every media arm degrades to a `[image: …]`-style text stub — a fetch
/// failure, an over-cap payload, a provider that will not take the kind —
/// so no arm may price below what its own fallback costs. That floor is
/// the stub the block really renders, not the widest one any block could:
/// the block carries `filename`, `mime_type` and `blob_id`, and
/// [`multimodal::content_block_to_text`] sanitizes them exactly as
/// delivery does, so the exact number is available with no I/O.
///
/// One token per byte throughout, which is a ceiling rather than an
/// average: `cl100k`'s base vocabulary is the 256 single bytes and every
/// merge replaces two tokens with one, so a token always covers at least
/// one input byte.
pub fn content_block_tokens(block: &baybo_model::ContentBlock) -> usize {
    use baybo_model::ContentBlock;

    let delivered = match block {
        ContentBlock::Image { width, height, .. } => image_tokens(*width, *height),
        ContentBlock::Audio { duration_ms, .. } => audio_tokens(*duration_ms),
        ContentBlock::File {
            filename,
            mime_type,
            page_count,
            size_bytes,
            ..
        } => {
            if inlines_document_as_text(mime_type) {
                inlined_document_tokens(filename, mime_type, *size_bytes)
            } else if delivers_pdf_document(mime_type) {
                pdf_document_tokens(*page_count)
            } else {
                0
            }
        }
        _ => return 0,
    };
    delivered.max(multimodal::content_block_to_text(block).len())
}

/// A code point that renders as nothing between the close tag's stem and
/// its `>`. `char::is_whitespace` covers VT, NBSP, U+3000 and the thin
/// spaces — NBSP and U+3000 are pixel-identical to the ASCII space the
/// matcher already defended — but not the zero-width formatting marks
/// (ZWSP, ZWNJ, ZWJ, BOM, soft hyphen, the bidi overrides), which a model
/// reads straight past and a byte comparison keeps verbatim.
fn is_invisible(ch: char) -> bool {
    ch.is_whitespace()
        || ch.is_control()
        || matches!(ch,
            '\u{00AD}' | '\u{061C}' | '\u{180E}' | '\u{FEFF}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FFF9}'..='\u{FFFB}')
}

/// Length of a closing tag starting at `at`, or `None` when there is no
/// match. Tolerant of ASCII case anywhere and of invisible padding before
/// the `>`: a model reads `</ATTACHED-FILE>` and `</attached-file\u{3000}>`
/// as the terminator exactly as readily as the tight form, so both have to
/// be neutralized. The stem and terminator are split out of
/// [`DOCUMENT_CLOSE_TAG`] rather than restated, so the matcher and the
/// template can never disagree.
fn close_tag_len_at(text: &str, at: usize) -> Option<usize> {
    let (terminator, stem) = DOCUMENT_CLOSE_TAG.as_bytes().split_last()?;
    let bytes = text.as_bytes();
    if !bytes
        .get(at..at + stem.len())
        .is_some_and(|w| w.eq_ignore_ascii_case(stem))
    {
        return None;
    }
    // The stem matched byte-wise against ASCII, and a UTF-8 continuation
    // byte can never be ASCII, so `end` is on a char boundary.
    let mut end = at + stem.len();
    while let Some(ch) = text[end..].chars().next().filter(|c| is_invisible(*c)) {
        end += ch.len_utf8();
    }
    (bytes.get(end) == Some(terminator)).then_some(end + 1 - at)
}

/// Render the wrapper's body: rewrite every literal closing tag so the
/// only thing that can close the wrapper is the wrapper, and stop at
/// [`MAX_DOCUMENT_TEXT_BYTES`] of output.
///
/// One pass, because the order matters. Cutting first and escaping second
/// leaves the cap bounding the decoded input rather than the delivered
/// string — a body of nothing but closing tags grows 16 bytes into 22 —
/// and the token estimate is derived from what is delivered.
fn document_body(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len().min(MAX_DOCUMENT_TEXT_BYTES));
    let mut at = 0;
    while at < bytes.len() {
        let (consumed, piece) = match close_tag_len_at(text, at) {
            Some(len) => (len, DOCUMENT_CLOSE_TAG_ESCAPED),
            None => {
                let Some(ch) = text[at..].chars().next() else {
                    break;
                };
                (ch.len_utf8(), &text[at..at + ch.len_utf8()])
            }
        };
        if out.len() + piece.len() > MAX_DOCUMENT_TEXT_BYTES {
            break;
        }
        out.push_str(piece);
        at += consumed;
    }
    if at < bytes.len() {
        out.push_str(&render_slots(
            DOCUMENT_TRUNCATION_MARKER,
            &[
                ("elided", &(bytes.len() - at).to_string()),
                ("total", &bytes.len().to_string()),
            ],
        ));
    }
    out
}

/// Fill every `{{key}}` in `template` in one left-to-right pass, so no
/// substituted value is ever rescanned for another slot's placeholder.
/// An unknown key is left verbatim.
pub(crate) fn render_slots(template: &str, slots: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        let Some(close) = rest[open..].find("}}").map(|at| open + at) else {
            break;
        };
        let key = &rest[open + 2..close];
        out.push_str(&rest[..open]);
        match slots.iter().find(|(slot, _)| *slot == key) {
            Some((_, value)) => out.push_str(value),
            None => out.push_str(&rest[open..close + 2]),
        }
        rest = &rest[close + 2..];
    }
    out.push_str(rest);
    out
}

/// Render the wrapper around one inlined document exactly as
/// `LlmClient::user_content_for_block` does. Shared with
/// `baybo-context`'s tokenizer tests, which have to measure the string the
/// model actually receives rather than a reconstruction of it.
#[cfg(any(test, feature = "test-support"))]
pub fn render_inlined_document(filename: &str, mime_type: &str, text: &str) -> String {
    render_document_wrapper(filename, mime_type, text)
}

fn render_document_wrapper(filename: &str, mime_type: &str, text: &str) -> String {
    render_slots(
        DOCUMENT_TEXT_TEMPLATE,
        &[
            ("filename", &multimodal::sanitize_slot(filename)),
            ("mime_type", &multimodal::sanitize_slot(mime_type)),
            ("content", &document_body(text)),
        ],
    )
}

fn text_stub(block: &baybo_model::ContentBlock) -> UserContent {
    UserContent::Text(Text {
        text: multimodal::content_block_to_text(block),
    })
}

fn b64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Run one of [`media_probe`]'s parsers off the reactor, the way the
/// module's own contract requires and both ingest call sites already do.
///
/// Every one of them is CPU-bound over the WHOLE payload — measured in
/// release, 16 ms for a 501 KiB PDF and ~140 ms for one at
/// [`MAX_PDF_DOCUMENT_BYTES`] — and `build_completion_request` re-walks
/// the whole history every turn, so this is paid per blob per turn.
/// A panic inside the task surfaces as a `JoinError` rather than
/// unwinding through the reactor, and answers [`None`] here: the same
/// text stub an unreadable payload already degrades to.
///
/// The payload is shared rather than moved so it survives that
/// `JoinError` and can still be base64'd by the arms that do deliver.
async fn probe_off_reactor<T, F>(bytes: &Arc<Vec<u8>>, probe: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce(&[u8]) -> Option<T> + Send + 'static,
{
    let bytes = Arc::clone(bytes);
    tokio::task::spawn_blocking(move || probe(&bytes))
        .await
        .ok()
        .flatten()
}

pub type Result<T> = std::result::Result<T, LlmError>;

/// Metadata describing a model's capabilities and pricing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub context_window: usize,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub pricing: ModelPricing,
}

/// Per-token pricing information for a model.
///
/// All rates are micro-USD per **1M tokens** (so $3.00 / MTok →
/// `MicroUsd::from_micros(3_000_000)`). Use [`MicroUsd::cost_for_tokens`]
/// to apply these to a token count — that path keeps everything in
/// integer math, no float drift.
///
/// `cached_input_per_1m_tokens` and `cache_write_per_1m_tokens` are
/// `None` when the provider doesn't bill prompt-cache traffic
/// separately (or the snapshot row pre-dates the field). When set,
/// `compute_cost_usd` charges the cached portion of `input_tokens`
/// at the cached rate instead of the full input rate, and bills
/// `cache_creation_input_tokens` at the write rate. Typical multipliers
/// vs. the input rate: 0.1× cached read, 1.25× cache write.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input_per_1m_tokens: baybo_model::MicroUsd,
    pub output_per_1m_tokens: baybo_model::MicroUsd,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_1m_tokens: Option<baybo_model::MicroUsd>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_1m_tokens: Option<baybo_model::MicroUsd>,
}

/// Unified response structure returned by `LlmClient::chat()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub content: String,
    pub content_blocks: Vec<baybo_model::ContentBlock>,
    pub tool_calls: Vec<ToolCallInfo>,
    pub usage: TokenUsage,
    pub thinking: Option<String>,
}

/// A single tool call extracted from the LLM response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Provider-specific cryptographic signature (e.g. Gemini's
    /// `thought_signature`). Must be echoed back when the tool call is
    /// included in subsequent requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// Token usage statistics for a single LLM call.
///
/// Cache fields are zero when unreported.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cached_input_tokens: usize,
    pub cache_creation_input_tokens: usize,
}

/// Whether a request may invoke its advertised tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    None,
}

/// A chat request to be sent to an LLM provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatRequest {
    pub messages: Vec<baybo_model::ChatMessage>,
    pub temperature: Option<f32>,
    pub tools: Vec<ToolDefinitionForLlm>,
    /// Per-request reasoning-effort override
    /// (`none`/`minimal`/`low`/`medium`/`high`/`xhigh`). Consumed only by
    /// the `openai-subscription` provider (others ignore it); `None` falls
    /// back to the client's construction-time effort. The agent loop sets
    /// this from the session's `last_model`-adjacent `last_effort` pin so the
    /// chat header's thinking level is PER-SESSION, not a global entry edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub tool_choice: ToolChoice,
    /// Provider cache bucket. Billed calls default it to the session id; probes
    /// use a separate constant bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
}

/// A tool definition in the format expected by the LLM layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinitionForLlm {
    pub name: String,
    pub description: String,
    pub parameters_schema: serde_json::Value,
}

/// Events emitted during LLM streaming.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// A text chunk from the model.
    Text(String),
    /// A complete tool call.
    ToolCall(ToolCallInfo),
    /// A reasoning/thinking text chunk (incremental delta).
    Reasoning(String),
    /// A complete, structured reasoning block. Must be preserved for
    /// providers that require thinking to be echoed back (Anthropic,
    /// Gemini).
    ThinkingBlock(baybo_model::ContentBlock),
    /// Token usage statistics (emitted at stream end).
    Usage(TokenUsage),
}

/// A type-erased streaming response from an LLM provider.
pub struct LlmStream {
    inner: Pin<Box<dyn Stream<Item = crate::Result<StreamEvent>> + Send>>,
}

impl Stream for LlmStream {
    type Item = crate::Result<StreamEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl LlmStream {
    /// Build a stream from a fixed sequence of events. Gated to test
    /// builds only — production must construct streams via the rig
    /// adapter so provider-specific event mapping stays a single path.
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_events(events: Vec<crate::Result<StreamEvent>>) -> Self {
        let stream = futures::stream::iter(events);
        Self {
            inner: Box::pin(stream),
        }
    }

    /// Wrap an arbitrary event stream into an `LlmStream`. The billing
    /// layer uses this to interpose a recording wrapper over a provider
    /// stream without the consumer seeing a different type.
    pub(crate) fn from_stream<S>(stream: S) -> Self
    where
        S: Stream<Item = crate::Result<StreamEvent>> + Send + 'static,
    {
        Self {
            inner: Box::pin(stream),
        }
    }

    /// Test-only escape hatch for driving an arbitrary event stream — e.g. one
    /// that yields a few events then parks, which `from_events` (a fixed,
    /// self-terminating sequence) can't express. Lets integration tests
    /// exercise mid-stream behaviour like cancellation.
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_event_stream<S>(stream: S) -> Self
    where
        S: Stream<Item = crate::Result<StreamEvent>> + Send + 'static,
    {
        Self::from_stream(stream)
    }

    /// Wraps a rig `StreamingCompletionResponse` into our type-erased `LlmStream`,
    /// converting provider-specific events into `StreamEvent`.
    fn from_rig_stream<R>(rig_stream: streaming::StreamingCompletionResponse<R>) -> Self
    where
        R: Clone
            + Unpin
            + Send
            + Sync
            + GetTokenUsage
            + serde::Serialize
            + serde::de::DeserializeOwned
            + 'static,
    {
        let mapped = rig_stream.filter_map(|result| {
            futures::future::ready(match result {
                Err(e) => Some(Err(rig_completion_to_error(e))),
                Ok(event) => convert_stream_event(event),
            })
        });
        Self {
            inner: Box::pin(mapped),
        }
    }

    /// Anthropic-specific variant of [`Self::from_rig_stream`].
    /// Anthropic's API reports `input_tokens / cache_read /
    /// cache_creation` as disjoint buckets summing to the total
    /// prompt; this wrapper folds the cache buckets back into
    /// `input_tokens` so the field uniformly means "total prompt"
    /// across providers (matching OpenAI / Gemini). `cached_input_tokens`
    /// and `cache_creation_input_tokens` stay populated so
    /// `compute_cost_usd` can split them out at billing rates.
    fn from_anthropic_stream(
        rig_stream: streaming::StreamingCompletionResponse<
            anthropic::streaming::StreamingCompletionResponse,
        >,
    ) -> Self {
        let mapped = rig_stream.filter_map(|result| {
            futures::future::ready(match result {
                Err(e) => Some(Err(rig_completion_to_error(e))),
                Ok(event) => match convert_stream_event(event) {
                    Some(Ok(StreamEvent::Usage(mut usage))) => {
                        fold_token_usage_cache_into_total(&mut usage);
                        Some(Ok(StreamEvent::Usage(usage)))
                    }
                    other => other,
                },
            })
        });
        Self {
            inner: Box::pin(mapped),
        }
    }

    /// Gemini-specific variant of [`Self::from_rig_stream`]. rig 0.34's
    /// `GetTokenUsage` impl for Gemini's streaming response leaves
    /// `cached_input_tokens` at 0; this wrapper reads
    /// `usage_metadata.cached_content_token_count` straight off the raw
    /// `Final` payload so prompt-cache hits land in cost_records. Drop
    /// once rig upstream fixes the impl.
    fn from_gemini_stream(
        rig_stream: streaming::StreamingCompletionResponse<
            gemini::streaming::StreamingCompletionResponse,
        >,
    ) -> Self {
        let mapped = rig_stream.filter_map(|result| {
            futures::future::ready(match result {
                Err(e) => Some(Err(rig_completion_to_error(e))),
                Ok(StreamedAssistantContent::Final(r)) => {
                    let mut usage = r.token_usage().unwrap_or_default();
                    usage.cached_input_tokens =
                        r.usage_metadata.cached_content_token_count.unwrap_or(0) as u64;
                    Some(Ok(StreamEvent::Usage(TokenUsage {
                        input_tokens: usage.input_tokens as usize,
                        output_tokens: usage.output_tokens as usize,
                        cached_input_tokens: usage.cached_input_tokens as usize,
                        cache_creation_input_tokens: usage.cache_creation_input_tokens as usize,
                    })))
                }
                Ok(event) => convert_stream_event(event),
            })
        });
        Self {
            inner: Box::pin(mapped),
        }
    }
}

/// Anthropic reports `input_tokens / cache_read / cache_creation` as
/// disjoint buckets summing to the total prompt; OpenAI and Gemini
/// report `input_tokens = total prompt` with cached as a subset.
/// Folds the cache buckets back into `input_tokens` so the field
/// uniformly means "total prompt" downstream — `compute_cost_usd`
/// can run one billing formula across providers.
fn fold_anthropic_cache_into_total(usage: &mut completion::Usage) {
    usage.input_tokens = usage
        .input_tokens
        .saturating_add(usage.cached_input_tokens)
        .saturating_add(usage.cache_creation_input_tokens);
}

fn fold_token_usage_cache_into_total(usage: &mut TokenUsage) {
    usage.input_tokens = usage
        .input_tokens
        .saturating_add(usage.cached_input_tokens)
        .saturating_add(usage.cache_creation_input_tokens);
}

fn convert_stream_event<R: GetTokenUsage>(
    event: StreamedAssistantContent<R>,
) -> Option<crate::Result<StreamEvent>> {
    match event {
        StreamedAssistantContent::Text(t) => Some(Ok(StreamEvent::Text(t.text))),
        StreamedAssistantContent::ToolCall { tool_call, .. } => {
            Some(Ok(StreamEvent::ToolCall(ToolCallInfo {
                id: tool_call.id,
                name: tool_name::unsanitize_tool_name(&tool_call.function.name),
                arguments: tool_call.function.arguments,
                signature: tool_call.signature,
            })))
        }
        StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
            Some(Ok(StreamEvent::Reasoning(reasoning)))
        }
        StreamedAssistantContent::Reasoning(reasoning) => Some(Ok(StreamEvent::ThinkingBlock(
            convert_reasoning_to_block(&reasoning),
        ))),
        StreamedAssistantContent::Final(r) => r.token_usage().map(|usage| {
            Ok(StreamEvent::Usage(TokenUsage {
                input_tokens: usage.input_tokens as usize,
                output_tokens: usage.output_tokens as usize,
                cached_input_tokens: usage.cached_input_tokens as usize,
                cache_creation_input_tokens: usage.cache_creation_input_tokens as usize,
            }))
        }),
        // ToolCallDelta events are skipped — we emit the complete
        // ToolCall once it's fully assembled.
        _ => None,
    }
}

/// Provider-specific fields absent from rig's `CompletionRequest`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ProviderCallExtras<'a> {
    pub effort: Option<&'a str>,
    pub prompt_cache_key: Option<&'a str>,
}

pub(crate) enum AnyCompletionModel {
    OpenAI(openai::completion::CompletionModel),
    Anthropic(anthropic::completion::CompletionModel),
    Gemini(gemini::completion::CompletionModel),
    /// DeepSeek's dedicated provider — round-trips `reasoning_content`
    /// on assistant tool-call turns, which thinking mode requires.
    DeepSeek(deepseek::CompletionModel),
    /// Additional rig-backed providers (see `providers::rig_providers`).
    /// All speak through rig's uniform completion model, so they share the
    /// generic `from_rig_stream` path.
    Xai(xai::completion::CompletionModel),
    Mistral(mistral::completion::CompletionModel),
    Cohere(cohere::CompletionModel),
    Perplexity(perplexity::CompletionModel),
    Moonshot(moonshot::CompletionModel),
    Zai(openai::completion::GenericCompletionModel<zai::ZAiExt>),
    XiaomiMimo(openai::completion::GenericCompletionModel<xiaomimimo::XiaomiMimoExt>),
    /// MiniMax via rig's dedicated provider on its Anthropic-compatible
    /// surface — shares Anthropic's cache-bucket folding and stream path.
    Minimax(anthropic::completion::GenericCompletionModel<minimax::MiniMaxAnthropicExt>),
    /// Inference hosts (see `providers::rig_providers`). Also rig-backed,
    /// so they share the generic `from_rig_stream` path.
    Groq(groq::CompletionModel),
    Together(together::completion::CompletionModel),
    Ollama(ollama::CompletionModel),
    Llamafile(llamafile::CompletionModel),
    Hyperbolic(hyperbolic::CompletionModel),
    HuggingFace(huggingface::completion::CompletionModel),
    /// ChatGPT/Codex OAuth subscription path. Doesn't go through rig — the
    /// Codex Responses API uses a different request shape and needs custom
    /// auth + 401-refresh handling. See `providers::openai_subscription`.
    OpenAiSubscription(crate::providers::openai_subscription::OpenAiSubscriptionCompletionModel),
}

/// Erase a provider `CompletionResponse<R>`'s raw-response payload for the
/// agent loop. For OpenAI-compatible rig providers that need no per-field
/// usage massaging (unlike Anthropic's cache folding / Gemini's cache
/// re-derivation).
fn repack_completion<R>(
    resp: completion::CompletionResponse<R>,
) -> completion::CompletionResponse<()> {
    completion::CompletionResponse {
        choice: resp.choice,
        usage: resp.usage,
        raw_response: (),
        message_id: resp.message_id,
    }
}

impl AnyCompletionModel {
    /// Whether this provider **and model** turn `UserContent::Audio` into
    /// real audio input. Anthropic — and the MiniMax generic riding its
    /// surface — returns `Err` for audio, and rig collects that error over
    /// the whole chat history, so one inbound voice note would fail every
    /// later turn in the session. OpenAI needs the extra `model_id` test
    /// because audio is a per-model capability there, not a per-provider
    /// one (see [`openai_model_accepts_audio`]). Whitelist, not blacklist:
    /// anything not verified to consume audio gets the text stub, which is
    /// lossy but never poisons the transcript.
    fn accepts_audio_content(&self, model_id: &str) -> bool {
        match self {
            Self::OpenAI(_) => openai_model_accepts_audio(model_id),
            // Every Gemini chat model rig can build is natively
            // multimodal over audio; the text-only 1.0 line is retired.
            Self::Gemini(_) => true,
            _ => false,
        }
    }

    /// Whether an image of `width` x `height` prices inside
    /// [`IMAGE_TOKEN_CEILING`] **for the provider on the other end of this
    /// turn** — the delivery gate, read off the model in hand exactly as
    /// the audio and PDF gates are.
    ///
    /// Deliberately NOT [`image_tokens`], and the two must not be
    /// unified: that one is the budget's estimate, computed with no model
    /// in sight, so it pays the cross-provider MAX because over-charging
    /// is the safe direction there. Gating DELIVERY on the same maximum
    /// drops an image on the dearest provider's price while the provider
    /// actually billed charges a fraction of it — a 24 MP iPhone photo
    /// (5712x4284) is 12,384 Gemini tokens, 765 OpenAI ones and 2,352
    /// Anthropic ones, and every one of them was being refused.
    ///
    /// Whitelist, not blacklist, the same rule the other two gates use: a
    /// provider whose tiling nobody has read pays the maximum.
    fn delivers_image(&self, width: u32, height: u32) -> bool {
        // A zero-pixel header is a failed read wearing a price, and
        // `openai_image_tokens` divides by the short edge.
        if width == 0 || height == 0 {
            return false;
        }
        let priced = match self {
            Self::Gemini(_) => gemini_image_tokens(width, height),
            Self::OpenAI(_) | Self::OpenAiSubscription(_) => openai_image_tokens(width, height),
            Self::Anthropic(_) => anthropic_image_tokens(width, height),
            _ => provider_image_tokens(width, height),
        };
        priced <= IMAGE_TOKEN_CEILING as u64
    }

    /// Whether this provider's converter turns a base64
    /// `UserContent::Document` into a real document part. Anthropic and
    /// Gemini do so in their native converters; the subscription adapter
    /// emits OpenAI Responses `input_file`. The regular OpenAI adapter is
    /// deliberately absent because it uses Chat Completions, whose rig
    /// converter flattens a base64 PDF into ordinary text.
    fn accepts_pdf_document(&self) -> bool {
        matches!(
            self,
            Self::Anthropic(_) | Self::Gemini(_) | Self::OpenAiSubscription(_)
        )
    }

    /// The level a provider that resolves its own default would apply to a
    /// request carrying `requested` — `None` for providers that apply only
    /// what they are handed.
    ///
    /// Codex is the one that answers for itself: it bakes the entry's level
    /// in at construction and substitutes a model-family default when none
    /// was configured, so an unconfigured entry there still runs at a real
    /// level and must bill as one.
    fn self_resolved_effort(&self, requested: Option<&str>) -> Option<String> {
        match self {
            Self::OpenAiSubscription(m) => Some(m.effective_effort_label(requested)),
            _ => None,
        }
    }

    async fn completion(
        &self,
        request: CompletionRequest,
        extras: ProviderCallExtras<'_>,
    ) -> std::result::Result<completion::CompletionResponse<()>, CompletionError> {
        match self {
            Self::OpenAI(m) => {
                let resp = m.completion(request).await?;
                Ok(completion::CompletionResponse {
                    choice: resp.choice,
                    usage: resp.usage,
                    raw_response: (),
                    message_id: resp.message_id,
                })
            }
            Self::Anthropic(m) => {
                let resp = m.completion(request).await?;
                let mut usage = resp.usage;
                fold_anthropic_cache_into_total(&mut usage);
                Ok(completion::CompletionResponse {
                    choice: resp.choice,
                    usage,
                    raw_response: (),
                    message_id: resp.message_id,
                })
            }
            Self::Gemini(m) => {
                let resp = m.completion(request).await?;
                // rig 0.34's Gemini path leaves `cached_input_tokens` at 0
                // even when the response carries `cachedContentTokenCount`.
                // Re-derive it from the raw `usage_metadata` so prompt-cache
                // hits land in cost_records correctly. Drop this once rig
                // upstream populates the field itself.
                let mut usage = resp.usage;
                if let Some(meta) = resp.raw_response.usage_metadata.as_ref() {
                    usage.cached_input_tokens = meta.cached_content_token_count.unwrap_or(0) as u64;
                }
                Ok(completion::CompletionResponse {
                    choice: resp.choice,
                    usage,
                    raw_response: (),
                    message_id: resp.message_id,
                })
            }
            Self::DeepSeek(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Xai(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Mistral(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Cohere(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Perplexity(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Moonshot(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Zai(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::XiaomiMimo(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Minimax(m) => {
                let resp = m.completion(request).await?;
                let mut usage = resp.usage;
                fold_anthropic_cache_into_total(&mut usage);
                Ok(completion::CompletionResponse {
                    choice: resp.choice,
                    usage,
                    raw_response: (),
                    message_id: resp.message_id,
                })
            }
            Self::Groq(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Together(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Ollama(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Llamafile(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::Hyperbolic(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::HuggingFace(m) => Ok(repack_completion(m.completion(request).await?)),
            Self::OpenAiSubscription(m) => m.completion(request, extras).await,
        }
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        extras: ProviderCallExtras<'_>,
    ) -> std::result::Result<LlmStream, CompletionError> {
        match self {
            Self::OpenAI(m) => {
                let stream = m.stream(request).await?;
                Ok(LlmStream::from_rig_stream(stream))
            }
            Self::Anthropic(m) => {
                let stream = m.stream(request).await?;
                Ok(LlmStream::from_anthropic_stream(stream))
            }
            Self::Gemini(m) => {
                let stream = m.stream(request).await?;
                Ok(LlmStream::from_gemini_stream(stream))
            }
            Self::DeepSeek(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Xai(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Mistral(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Cohere(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Perplexity(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Moonshot(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Zai(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::XiaomiMimo(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Minimax(m) => Ok(LlmStream::from_anthropic_stream(m.stream(request).await?)),
            Self::Groq(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Together(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Ollama(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Llamafile(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::Hyperbolic(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::HuggingFace(m) => Ok(LlmStream::from_rig_stream(m.stream(request).await?)),
            Self::OpenAiSubscription(m) => m.stream(request, extras).await,
        }
    }
}

/// Provider-agnostic completion contract used by the agent loop and any
/// other consumer that doesn't need the concrete `LlmClient` (e.g. probe).
///
/// Implemented by `LlmClient` for production and by test stubs in
/// `baybo-llm`'s `test-support` feature for deterministic integration tests.
#[async_trait::async_trait]
pub trait LlmCompletion: Send + Sync {
    async fn chat(&self, request: &ChatRequest) -> crate::Result<LlmResponse>;
    async fn chat_stream(&self, request: &ChatRequest) -> crate::Result<LlmStream>;
    fn model_info(&self) -> &ModelInfo;
    /// The reasoning effort this client actually applies to a request whose
    /// [`ChatRequest::reasoning_effort`] override is `requested`: the entry's
    /// configured level fills in when the caller passes `None`. `None` means
    /// no effort is sent to this provider. Required rather than defaulted —
    /// a client that silently answers "not applicable" is how effort stopped
    /// reaching `cost_records` in the first place.
    fn effective_effort(&self, requested: Option<&str>) -> Option<String>;
}

#[async_trait::async_trait]
impl LlmCompletion for LlmClient {
    async fn chat(&self, request: &ChatRequest) -> crate::Result<LlmResponse> {
        LlmClient::chat(self, request).await
    }
    async fn chat_stream(&self, request: &ChatRequest) -> crate::Result<LlmStream> {
        LlmClient::chat_stream(self, request).await
    }
    fn model_info(&self) -> &ModelInfo {
        LlmClient::model_info(self)
    }
    fn effective_effort(&self, requested: Option<&str>) -> Option<String> {
        LlmClient::effective_effort(self, requested)
    }
}

/// Capability the LLM client uses to materialise blob bytes for
/// multimodal user content. Decoupled from `baybo-storage::BlobStore`
/// so the LLM crate stays storage-agnostic — the agent runtime wraps
/// its `BlobStore` in an adapter that implements this trait.
#[async_trait::async_trait]
pub trait BlobFetcher: Send + Sync {
    async fn fetch(&self, blob_id: &str) -> Result<Vec<u8>>;
}

/// The main LLM client type wrapping a rig completion model.
pub struct LlmClient {
    pub(crate) model_info: ModelInfo,
    model: AnyCompletionModel,
    /// Optional blob fetcher. When set AND `model_info.supports_vision`
    /// is true, `ContentBlock::Image` / `Audio` / `File` user-content
    /// blocks are materialised into proper rig `Image` / `Audio` /
    /// `Document` content (base64-encoded bytes). Without it, every
    /// non-text block degrades to a `[image: …]`-style text stub —
    /// the model never sees the actual payload.
    blob_fetcher: Option<std::sync::Arc<dyn BlobFetcher>>,
    /// The entry's configured reasoning effort, parsed once. Fills in when a
    /// request carries no per-session pin, so an operator who set it once
    /// gets it on every call — including the auxiliary ones (compression,
    /// titles, tool side-LLMs) that never see a session's pin.
    ///
    /// Held here rather than inside each provider client because it is
    /// operator config, not provider state: the same rung, whichever dialect
    /// ends up carrying it.
    entry_effort: Option<crate::effort::EffortPick>,
}

impl LlmClient {
    /// Creates a new `LlmClient` from a provider-specific completion model.
    pub(crate) fn new(model_info: ModelInfo, model: AnyCompletionModel) -> Self {
        Self {
            model_info,
            model,
            blob_fetcher: None,
            entry_effort: None,
        }
    }

    /// Carry the entry's configured reasoning effort. Set once, centrally,
    /// for every provider — see [`LlmRegistry::build_client`].
    ///
    /// Fails when the operator picked a rung this provider's dialect cannot
    /// express, so a level that would never reach the wire is caught at
    /// startup with a message naming the alternatives, rather than at the
    /// first call — or worse, rounded to a neighbour nobody asked for.
    pub(crate) fn with_entry_effort(mut self, effort: Option<&str>) -> crate::Result<Self> {
        let pick = effort.map(crate::effort::EffortPick::parse);
        if let Some(pick) = &pick {
            self.effort_wire()
                .wire_level(pick)
                .map_err(|e| LlmError::Config(format!("{}: {e}", self.model_info.provider)))?;
        }
        self.entry_effort = pick;
        Ok(self)
    }

    /// Attach a blob fetcher so the client can materialise multimodal
    /// content. Required for `supports_vision: true` to actually mean
    /// "image bytes flow through" — without it, the build path falls
    /// back to a text stub even on vision-capable models.
    pub fn with_blob_fetcher(mut self, fetcher: std::sync::Arc<dyn BlobFetcher>) -> Self {
        self.blob_fetcher = Some(fetcher);
        self
    }

    /// Sends a chat request to the provider and returns a unified response.
    pub async fn chat(&self, request: &ChatRequest) -> crate::Result<LlmResponse> {
        debug!(
            provider = %self.model_info.provider,
            model = %self.model_info.id,
            "sending chat request"
        );

        let rig_request = self.build_completion_request(request).await;

        // Providers that build their own body take the level directly,
        // already translated into their dialect.
        let native_effort = self.wire_effort(request.reasoning_effort.as_deref());
        let extras = ProviderCallExtras {
            effort: native_effort.as_deref(),
            prompt_cache_key: request.prompt_cache_key.as_deref(),
        };
        let response = self
            .model
            .completion(rig_request, extras)
            .await
            .map_err(rig_completion_to_error)?;

        let llm_response = self.convert_response(response);

        debug!(
            content_len = llm_response.content.len(),
            tool_calls = llm_response.tool_calls.len(),
            input_tokens = llm_response.usage.input_tokens,
            output_tokens = llm_response.usage.output_tokens,
            "received LLM response"
        );

        Ok(llm_response)
    }

    /// Sends a chat request and returns a streaming response.
    pub async fn chat_stream(&self, request: &ChatRequest) -> crate::Result<LlmStream> {
        debug!(
            provider = %self.model_info.provider,
            model = %self.model_info.id,
            "sending streaming chat request"
        );

        let rig_request = self.build_completion_request(request).await;

        let native_effort = self.wire_effort(request.reasoning_effort.as_deref());
        let extras = ProviderCallExtras {
            effort: native_effort.as_deref(),
            prompt_cache_key: request.prompt_cache_key.as_deref(),
        };
        let stream = self
            .model
            .stream(rig_request, extras)
            .await
            .map_err(rig_completion_to_error)?;

        Ok(stream)
    }

    /// Build a rig `CompletionRequest` from our `ChatRequest`.
    ///
    /// Async because vision-capable models need their `Image` /
    /// `Audio` / `File` content blocks materialised from the blob
    /// store — the LLM call is the async boundary closest to the
    /// API hop, and base64-encoding a 100 MiB blob inline would
    /// stall the runtime if we did it synchronously.
    async fn build_completion_request(&self, request: &ChatRequest) -> CompletionRequest {
        let mut system_parts = Vec::new();
        let mut chat_messages: Vec<Message> = Vec::new();

        for msg in &request.messages {
            match msg.role {
                baybo_model::Role::System => {
                    system_parts.push(multimodal::extract_text(&msg.content));
                }
                baybo_model::Role::User => {
                    let mut parts: Vec<UserContent> = Vec::with_capacity(msg.content.len());
                    for block in &msg.content {
                        parts.push(self.user_content_for_block(block).await);
                    }
                    if !parts.is_empty() {
                        let first = parts.remove(0);
                        let mut content = OneOrMany::one(first);
                        for part in parts {
                            content.push(part);
                        }
                        chat_messages.push(Message::User { content });
                    }
                }
                baybo_model::Role::Assistant => {
                    let mut parts: Vec<AssistantContent> = Vec::new();
                    for block in &msg.content {
                        match block {
                            baybo_model::ContentBlock::Text(t) if !t.is_empty() => {
                                parts.push(AssistantContent::Text(Text { text: t.clone() }));
                            }
                            baybo_model::ContentBlock::ToolUse {
                                id,
                                name,
                                input,
                                signature,
                            } => {
                                parts.push(AssistantContent::ToolCall(
                                    completion::message::ToolCall {
                                        id: id.clone(),
                                        call_id: None,
                                        function: completion::message::ToolFunction {
                                            name: tool_name::sanitize_tool_name(name),
                                            arguments: input.clone(),
                                        },
                                        signature: signature.clone(),
                                        additional_params: None,
                                    },
                                ));
                            }
                            baybo_model::ContentBlock::Thinking { id, content } => {
                                parts.push(convert_thinking_to_reasoning(id, content));
                            }
                            _ => {}
                        }
                    }
                    if !parts.is_empty() {
                        let first = parts.remove(0);
                        let mut content = OneOrMany::one(first);
                        for part in parts {
                            content.push(part);
                        }
                        chat_messages.push(Message::Assistant { id: None, content });
                    }
                }
                baybo_model::Role::Tool => {
                    let mut parts: Vec<UserContent> = Vec::new();
                    for block in &msg.content {
                        match block {
                            baybo_model::ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                // `meta` is transcript-only side-band data,
                                // never forwarded to the provider.
                                ..
                            } => {
                                parts.push(UserContent::ToolResult(
                                    completion::message::ToolResult {
                                        id: tool_use_id.clone(),
                                        call_id: None,
                                        content: OneOrMany::one(
                                            completion::message::ToolResultContent::Text(Text {
                                                text: content.clone(),
                                            }),
                                        ),
                                    },
                                ));
                            }
                            baybo_model::ContentBlock::Text(text) => {
                                // Legacy fallback for plain-text tool results.
                                parts.push(UserContent::Text(Text { text: text.clone() }));
                            }
                            _ => {}
                        }
                    }
                    if !parts.is_empty() {
                        let first = parts.remove(0);
                        let mut content = OneOrMany::one(first);
                        for part in parts {
                            content.push(part);
                        }
                        chat_messages.push(Message::User { content });
                    }
                }
            }
        }

        let tools: Vec<ToolDefinition> = request
            .tools
            .iter()
            .map(|t| ToolDefinition {
                name: tool_name::sanitize_tool_name(&t.name),
                description: t.description.clone(),
                parameters: t.parameters_schema.clone(),
            })
            .collect();

        let preamble = if system_parts.is_empty() {
            None
        } else {
            Some(system_parts.join("\n"))
        };

        // Ensure at least one message for OneOrMany.
        if chat_messages.is_empty() {
            chat_messages.push(Message::User {
                content: OneOrMany::one(UserContent::Text(Text {
                    text: String::new(),
                })),
            });
        }

        let first = chat_messages.remove(0);
        let mut chat_history = OneOrMany::one(first);
        for msg in chat_messages {
            chat_history.push(msg);
        }

        // The operator's rung, translated into this provider's dialect and
        // wrapped in the shape it expects. `None` when the provider takes it
        // by another route or isn't wired for it — in which case nothing is
        // sent, and the request looks exactly as it did before effort
        // existed.
        let additional_params = self
            .wire_effort(request.reasoning_effort.as_deref())
            .and_then(|level| self.effort_wire().params(&level));

        // OpenAI-compatible providers reject tool_choice without tools.
        let tool_choice = (!tools.is_empty()).then_some(match request.tool_choice {
            ToolChoice::Auto => rig::message::ToolChoice::Auto,
            ToolChoice::None => rig::message::ToolChoice::None,
        });

        CompletionRequest {
            model: None,
            preamble,
            chat_history,
            documents: Vec::new(),
            tools,
            temperature: request.temperature.map(|t| t as f64),
            max_tokens: Some(4096),
            tool_choice,
            additional_params,
            output_schema: None,
        }
    }

    /// Convert one user-side `ContentBlock` into a rig `UserContent`.
    /// `Image` / `Audio` / PDF `File` blocks become real multimodal
    /// content when (1) the model claims vision support, (2) the
    /// provider — and, for audio, the model — is verified to consume the
    /// kind, and (3) a blob fetcher is wired in. Text-like `File` blocks
    /// are the exception: they're decoded and inlined as text (see
    /// [`DocumentDelivery`]), which needs none of the above. Otherwise —
    /// including when blob fetch fails or the payload is too big to
    /// deliver — the block degrades to a `[image: …]`-style text stub so
    /// the conversation can keep going even if the bytes aren't
    /// deliverable.
    async fn user_content_for_block(&self, block: &baybo_model::ContentBlock) -> UserContent {
        match block {
            baybo_model::ContentBlock::Text(t) => UserContent::Text(Text { text: t.clone() }),
            baybo_model::ContentBlock::Image {
                blob, mime_type, ..
            } if self.model_info.supports_vision => {
                let (Some(fetcher), Some(media_type)) = (
                    self.blob_fetcher.as_ref(),
                    parse_image_media_type(mime_type),
                ) else {
                    return text_stub(block);
                };
                let bytes = match fetcher.fetch(&blob.blob_id).await {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        tracing::warn!(
                            blob_id = %blob.blob_id,
                            error = %e,
                            "blob fetch failed; falling back to text stub for image",
                        );
                        return text_stub(block);
                    }
                };
                if bytes.len() > MAX_IMAGE_DOCUMENT_BYTES {
                    tracing::warn!(
                        blob_id = %blob.blob_id,
                        bytes = bytes.len(),
                        limit = MAX_IMAGE_DOCUMENT_BYTES,
                        "image exceeds the payload cap; falling back to text stub",
                    );
                    return text_stub(block);
                }
                // The price guard, re-derived from the bytes rather than
                // read off the block, exactly as the PDF arm re-walks its
                // pages: this is what makes the budget's
                // `image_tokens(None, None)` fallback a real ceiling. The
                // byte cap above cannot stand in for it — it bounds a
                // COMPRESSED payload while every provider bills pixels,
                // and a 12000x9000 flat render sits well under it at
                // 49,536 tokens.
                let bytes = Arc::new(bytes);
                let dimensions = probe_off_reactor(&bytes, media_probe::image_dimensions)
                    .await
                    .filter(|(w, h)| self.model.delivers_image(*w, *h));
                let Some((width, height)) = dimensions else {
                    tracing::warn!(
                        blob_id = %blob.blob_id,
                        provider = %self.model_info.provider,
                        limit = IMAGE_TOKEN_CEILING,
                        "image dimensions are unreadable or over the token cap for this \
                         provider; falling back to text stub",
                    );
                    return text_stub(block);
                };
                debug!(blob_id = %blob.blob_id, width, height, "delivering image");
                UserContent::Image(Image {
                    data: DocumentSourceKind::Base64(b64_encode(&bytes)),
                    media_type: Some(media_type),
                    // Required by the OpenAI-compat converter when the
                    // source is a Base64 data URL — without it rig errors
                    // with "image URI must have image detail" before the
                    // request leaves the client. `Auto` is OpenAI's default
                    // when the field is absent on text-only id URLs anyway,
                    // so it's the lossless choice.
                    detail: Some(ImageDetail::Auto),
                    additional_params: None,
                })
            }
            baybo_model::ContentBlock::Audio {
                blob,
                mime_type,
                duration_ms,
                ..
            } if self.model_info.supports_vision
                && self.model.accepts_audio_content(&self.model_info.id) =>
            {
                let (Some(fetcher), Some(media_type)) = (
                    self.blob_fetcher.as_ref(),
                    parse_audio_media_type(mime_type),
                ) else {
                    return text_stub(block);
                };
                let bytes = match fetcher.fetch(&blob.blob_id).await {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        tracing::warn!(
                            blob_id = %blob.blob_id,
                            error = %e,
                            "blob fetch failed; falling back to text stub for audio",
                        );
                        return text_stub(block);
                    }
                };
                if bytes.len() > MAX_AUDIO_DOCUMENT_BYTES {
                    tracing::warn!(
                        blob_id = %blob.blob_id,
                        bytes = bytes.len(),
                        limit = MAX_AUDIO_DOCUMENT_BYTES,
                        "audio exceeds the payload cap; falling back to text stub",
                    );
                    return text_stub(block);
                }
                // Re-probe rather than trust the block: `duration_ms` is
                // what the budget was charged, and an inbound block's copy
                // came off the wire. The block's value is the fallback for
                // a container we can't read, and a payload neither source
                // can measure is refused — an unpriceable attachment is
                // exactly what "audio costs 100 tokens, no cap" was.
                let bytes = Arc::new(bytes);
                let seconds = probe_off_reactor(&bytes, media_probe::audio_duration_ms)
                    .await
                    .or(*duration_ms)
                    .map(|ms| ms.div_ceil(MS_PER_SECOND));
                let Some(seconds) = seconds.filter(|s| *s <= MAX_AUDIO_SECONDS) else {
                    tracing::warn!(
                        blob_id = %blob.blob_id,
                        seconds,
                        limit = MAX_AUDIO_SECONDS,
                        "audio duration is unreadable or over the cap; falling back to text stub",
                    );
                    return text_stub(block);
                };
                debug!(blob_id = %blob.blob_id, seconds, "delivering audio");
                UserContent::Audio(Audio {
                    data: DocumentSourceKind::Base64(b64_encode(&bytes)),
                    media_type: Some(media_type),
                    additional_params: None,
                })
            }
            baybo_model::ContentBlock::File {
                blob,
                filename,
                mime_type,
                page_count,
                ..
            } => {
                let (Some(fetcher), Some(delivery)) =
                    (self.blob_fetcher.as_ref(), document_delivery(mime_type))
                else {
                    return text_stub(block);
                };
                if matches!(delivery, DocumentDelivery::Pdf) {
                    // Only the PDF leg needs a multimodal model. Decoding
                    // bytes into a text block needs no capability at all,
                    // and gating it on vision hid `.md` attachments from
                    // `supports_vision: false` models that read them fine.
                    if !(self.model_info.supports_vision && self.model.accepts_pdf_document()) {
                        return text_stub(block);
                    }
                    // A count recorded at ingest may only REFUSE, never
                    // admit — the gate below re-derives it from the bytes
                    // for exactly that reason. But a document already over
                    // the cap is a stub this turn and every turn, and
                    // `build_completion_request` re-walks the whole history
                    // per turn: refusing here skips a fetch and a
                    // whole-payload parse that measured 140 ms at
                    // MAX_PDF_DOCUMENT_BYTES, every single turn.
                    if page_count.is_some_and(|pages| pages > MAX_PDF_PAGES) {
                        tracing::warn!(
                            blob_id = %blob.blob_id,
                            page_count = ?page_count,
                            limit = MAX_PDF_PAGES,
                            "pdf was ingested over the page cap; falling back to text stub",
                        );
                        return text_stub(block);
                    }
                }
                let bytes = match fetcher.fetch(&blob.blob_id).await {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        tracing::warn!(
                            blob_id = %blob.blob_id,
                            error = %e,
                            "blob fetch failed; falling back to text stub for document",
                        );
                        return text_stub(block);
                    }
                };
                match delivery {
                    DocumentDelivery::Pdf => {
                        if bytes.len() > MAX_PDF_DOCUMENT_BYTES {
                            tracing::warn!(
                                blob_id = %blob.blob_id,
                                bytes = bytes.len(),
                                limit = MAX_PDF_DOCUMENT_BYTES,
                                "pdf exceeds the payload cap; falling back to text stub",
                            );
                            return text_stub(block);
                        }
                        // The page count is re-derived from the bytes, not
                        // read off the block: this gate is what makes the
                        // budget's `page_count: None` fallback a real
                        // ceiling rather than a hope.
                        //
                        // The range starts at 1 because zero is the one
                        // answer that means the parse failed while still
                        // pricing as a stub: a document whose page tree
                        // `lopdf` cannot walk loads fine and reports no
                        // pages, and other readers refuse it outright.
                        let bytes = Arc::new(bytes);
                        let pages = probe_off_reactor(&bytes, media_probe::pdf_page_count)
                            .await
                            .filter(|pages| DELIVERABLE_PDF_PAGES.contains(pages));
                        let Some(pages) = pages else {
                            tracing::warn!(
                                blob_id = %blob.blob_id,
                                limit = MAX_PDF_PAGES,
                                "pdf is unreadable or over the page cap; falling back to text stub",
                            );
                            return text_stub(block);
                        };
                        debug!(blob_id = %blob.blob_id, pages, "delivering pdf document");
                        UserContent::Document(Document {
                            data: DocumentSourceKind::Base64(b64_encode(&bytes)),
                            media_type: Some(DocumentMediaType::PDF),
                            additional_params: Some(serde_json::json!({
                                DOCUMENT_FILENAME_PARAM: filename,
                            })),
                        })
                    }
                    DocumentDelivery::Text => match String::from_utf8(bytes) {
                        Ok(text) => UserContent::Text(Text {
                            text: render_document_wrapper(filename, mime_type, &text),
                        }),
                        Err(_) => {
                            tracing::warn!(
                                blob_id = %blob.blob_id,
                                mime_type = %mime_type,
                                "document is not valid UTF-8; falling back to text stub",
                            );
                            text_stub(block)
                        }
                    },
                }
            }
            other => text_stub(other),
        }
    }

    /// Convert a rig `CompletionResponse` into our `LlmResponse`.
    fn convert_response(&self, response: completion::CompletionResponse<()>) -> LlmResponse {
        let mut content = String::new();
        let mut content_blocks = Vec::new();
        let mut tool_calls = Vec::new();
        let mut thinking: Option<String> = None;

        for item in response.choice.into_iter() {
            match item {
                AssistantContent::Text(text) => {
                    if !text.text.is_empty() {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(&text.text);
                        content_blocks.push(baybo_model::ContentBlock::Text(text.text));
                    }
                }
                AssistantContent::ToolCall(tc) => {
                    tool_calls.push(ToolCallInfo {
                        id: tc.id,
                        name: tool_name::unsanitize_tool_name(&tc.function.name),
                        arguments: tc.function.arguments,
                        signature: tc.signature,
                    });
                }
                AssistantContent::Reasoning(ref r) => {
                    let reasoning_text: String = r
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            rig::completion::message::ReasoningContent::Text { text, .. } => {
                                Some(text.as_str())
                            }
                            rig::completion::message::ReasoningContent::Summary(s) => {
                                Some(s.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !reasoning_text.is_empty() {
                        thinking = Some(reasoning_text);
                    }
                    content_blocks.push(convert_reasoning_to_block(r));
                }
                AssistantContent::Image(_) => {}
            }
        }

        let usage = TokenUsage {
            input_tokens: response.usage.input_tokens as usize,
            output_tokens: response.usage.output_tokens as usize,
            cached_input_tokens: response.usage.cached_input_tokens as usize,
            cache_creation_input_tokens: response.usage.cache_creation_input_tokens as usize,
        };

        LlmResponse {
            content,
            content_blocks,
            tool_calls,
            usage,
            thinking,
        }
    }

    /// Returns the model identifier (e.g. `"claude-sonnet-4-6"`).
    pub fn model_id(&self) -> &str {
        &self.model_info.id
    }

    /// Returns the full model metadata.
    pub fn model_info(&self) -> &ModelInfo {
        &self.model_info
    }

    /// What the operator asked for on this call: the per-request pin wins,
    /// the entry's configured level fills in. Canonical rungs report their
    /// ladder name so spend stays comparable across providers that spell the
    /// same depth differently; `None` means nothing reaches this provider.
    /// `pub(crate)`: outside this crate the same value is reachable through
    /// [`LlmCompletion::effective_effort`].
    pub(crate) fn effective_effort(&self, requested: Option<&str>) -> Option<String> {
        if !self.effort_wire().carries_effort() {
            return None;
        }
        let pick = self.pick_for(requested);
        match self
            .model
            .self_resolved_effort(pick.as_ref().map(|p| p.label()))
        {
            // Reported in the provider's own spelling; fold it back onto the
            // ladder so cost rows stay comparable across providers.
            Some(label) => Some(
                crate::effort::ReasoningEffort::parse(&label)
                    .map(|l| l.as_str().to_string())
                    .unwrap_or(label),
            ),
            None => pick.map(|p| p.label().to_string()),
        }
    }

    /// The effort dialect this client's provider speaks.
    pub(crate) fn effort_wire(&self) -> crate::effort::EffortWire {
        crate::providers::effort_wire_for_provider(&self.model_info.provider)
    }

    /// The pick governing this call — the request's pin, else the entry's.
    fn pick_for(&self, requested: Option<&str>) -> Option<crate::effort::EffortPick> {
        requested
            .map(crate::effort::EffortPick::parse)
            .or_else(|| self.entry_effort.clone())
    }

    /// The string this provider should receive for this call, already in its
    /// own dialect. `None` when nothing is sent — reasoning off, no pick, or
    /// a provider baybo has no effort wiring for.
    ///
    /// A pin the dialect cannot express is dropped with a warning rather than
    /// failing the call: the entry was validated at startup, so this can only
    /// be a per-session pick, and losing a turn to a picker that offered a
    /// bad rung is worse than running at the provider's default.
    fn wire_effort(&self, requested: Option<&str>) -> Option<String> {
        let pick = self.pick_for(requested)?;
        match self.effort_wire().wire_level(&pick) {
            Ok(level) => level,
            Err(e) => {
                tracing::warn!(
                    provider = %self.model_info.provider,
                    model = %self.model_info.id,
                    "{e}; sending no effort for this call"
                );
                None
            }
        }
    }

    /// Issue a minimal chat request to verify provider connectivity and auth.
    ///
    /// Used by `baybo llm probe` and `baybo doctor`. The request is deliberately
    /// tiny (one-token prompt, no tools) so it is cheap to run repeatedly.
    pub async fn probe(&self) -> crate::Result<ProbeReport> {
        let req = ChatRequest {
            messages: vec![baybo_model::ChatMessage::agent_context(vec![
                baybo_model::ContentBlock::Text("ping".to_string()),
            ])],
            temperature: Some(0.0),
            tools: vec![],
            reasoning_effort: None,
            ..Default::default()
        };
        let start = std::time::Instant::now();
        let response = self.chat(&req).await?;
        Ok(ProbeReport {
            provider: self.model_info.provider.clone(),
            model: self.model_info.id.clone(),
            latency_ms: start.elapsed().as_millis() as u64,
            tokens: response.usage,
            thinking_chars: response.thinking.as_ref().map(|s| s.chars().count()),
            thinking_preview: response
                .thinking
                .as_ref()
                .map(|s| s.lines().next().unwrap_or("").chars().take(120).collect()),
        })
    }
}

pub(crate) const PROBE_PROMPT_CACHE_KEY: &str = "baybo-llm-probe";

/// Result of a successful `LlmClient::probe()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeReport {
    pub provider: String,
    pub model: String,
    pub latency_ms: u64,
    pub tokens: TokenUsage,
    /// Number of UTF-8 characters of reasoning summary the provider
    /// returned. `None` when the model didn't emit any reasoning —
    /// either because reasoning is disabled or because the provider
    /// doesn't support it. Useful for `baybo llm probe` to confirm
    /// reasoning is actually flowing end-to-end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_chars: Option<usize>,
    /// First line of the reasoning summary, truncated to 120 chars,
    /// for human-readable verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_preview: Option<String>,
}

// ---------------------------------------------------------------------------
// Reasoning / Thinking round-trip helpers
// ---------------------------------------------------------------------------

/// Convert a rig `Reasoning` block into our provider-agnostic
/// `ContentBlock::Thinking` representation.
fn convert_reasoning_to_block(
    reasoning: &completion::message::Reasoning,
) -> baybo_model::ContentBlock {
    use baybo_model::ThinkingContent;
    let content = reasoning
        .content
        .iter()
        .map(|c| match c {
            rig::completion::message::ReasoningContent::Text { text, signature } => {
                ThinkingContent::Text {
                    text: text.clone(),
                    signature: signature.clone(),
                }
            }
            rig::completion::message::ReasoningContent::Summary(s) => {
                ThinkingContent::Summary { text: s.clone() }
            }
            rig::completion::message::ReasoningContent::Encrypted(d)
            | rig::completion::message::ReasoningContent::Redacted { data: d } => {
                ThinkingContent::Redacted { data: d.clone() }
            }
            _ => ThinkingContent::Summary {
                text: String::new(),
            },
        })
        .collect();
    baybo_model::ContentBlock::Thinking {
        id: reasoning.id.clone(),
        content,
    }
}

/// Convert our `ContentBlock::Thinking` back into a rig
/// `AssistantContent::Reasoning` for the completion request.
fn convert_thinking_to_reasoning(
    id: &Option<String>,
    content: &[baybo_model::ThinkingContent],
) -> AssistantContent {
    use rig::completion::message::{Reasoning, ReasoningContent};
    let blocks = content
        .iter()
        .map(|tc| match tc {
            baybo_model::ThinkingContent::Text { text, signature } => ReasoningContent::Text {
                text: text.clone(),
                signature: signature.clone(),
            },
            baybo_model::ThinkingContent::Summary { text } => {
                ReasoningContent::Summary(text.clone())
            }
            baybo_model::ThinkingContent::Redacted { data } => {
                ReasoningContent::Redacted { data: data.clone() }
            }
        })
        .collect();
    let mut reasoning = Reasoning::new("").optional_id(id.clone());
    reasoning.content = blocks;
    AssistantContent::Reasoning(reasoning)
}

#[cfg(test)]
mod multimodal_dispatch_tests {
    //! Coverage for `user_content_for_block` — the place where
    //! `ContentBlock::Image` either becomes a real `UserContent::Image`
    //! (bytes the model can actually see) or degrades to a text stub.
    //! The image-delivery bug we're fixing here was exactly the second
    //! branch firing on a vision-capable model because no `BlobFetcher`
    //! was wired in.

    use std::sync::Arc;

    use baybo_model::{BlobRef, ContentBlock};
    use rig::completion::message::{DocumentSourceKind, ImageMediaType};
    use rig::message::UserContent;

    use super::*;
    use crate::registry::LlmProviderRegistry;

    struct StaticFetcher(Vec<u8>);

    #[async_trait::async_trait]
    impl BlobFetcher for StaticFetcher {
        async fn fetch(&self, _blob_id: &str) -> Result<Vec<u8>> {
            Ok(self.0.clone())
        }
    }

    struct FailingFetcher;

    #[async_trait::async_trait]
    impl BlobFetcher for FailingFetcher {
        async fn fetch(&self, blob_id: &str) -> Result<Vec<u8>> {
            Err(LlmError::Transient(format!("nope: {blob_id}")))
        }
    }

    fn vision_client() -> LlmClient {
        // Factory route: any provider produces an `LlmClient` shape we
        // can mutate. MiniMax's factory default is `supports_vision:
        // false` (the M2 family is text-first), so we use the config
        // override path to force vision on for the test fixture —
        // exactly the same way an operator on MiniMax-VL-01 would.
        let registry = LlmProviderRegistry::with_default_providers();
        registry
            .build_client(&LlmProviderConfig {
                provider: "minimax".into(),
                api_key: Some("test".into()),
                base_url: None,
                model: "MiniMax-VL-01".into(),
                supports_vision: Some(true),
                context_window: None,
                pricing: None,
                reasoning_effort: None,
                vault: None,
                proxy: None,
            })
            .unwrap()
    }

    #[tokio::test]
    async fn image_block_with_fetcher_emits_real_image_content() {
        // Real header bytes, because delivery now reads the pixel
        // dimensions out of them: an image it cannot price is one it
        // cannot bound, so it degrades to the stub.
        let bytes = media_probe::fixture::png(640, 480);
        let client = vision_client().with_blob_fetcher(Arc::new(StaticFetcher(bytes.clone())));
        let block = ContentBlock::Image {
            blob: BlobRef {
                blob_id: "sha256:deadbeef.tok".into(),
            },
            mime_type: "image/png".into(),
            filename: None,
            width: None,
            height: None,
        };
        let out = client.user_content_for_block(&block).await;
        match out {
            UserContent::Image(img) => {
                assert_eq!(img.media_type, Some(ImageMediaType::PNG));
                // Required by rig's OpenAI converter — Base64 source
                // without an image detail errors at message-build time.
                assert_eq!(
                    img.detail,
                    Some(rig::completion::message::ImageDetail::Auto)
                );
                let DocumentSourceKind::Base64(b64) = img.data else {
                    panic!("expected base64 data");
                };
                use base64::Engine;
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&b64)
                    .unwrap();
                assert_eq!(decoded, bytes);
            }
            other => panic!("expected Image variant, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn image_block_without_fetcher_falls_back_to_text_stub() {
        // Same vision-capable model, but no blob fetcher attached →
        // the model would have to invent the bytes, which is exactly
        // the bug; assert we degrade explicitly to a text stub.
        let client = vision_client();
        let block = ContentBlock::Image {
            blob: BlobRef {
                blob_id: "sha256:abc.tok".into(),
            },
            mime_type: "image/jpeg".into(),
            filename: None,
            width: None,
            height: None,
        };
        let out = client.user_content_for_block(&block).await;
        match out {
            UserContent::Text(t) => {
                assert!(t.text.contains("[image:"), "stub form, got {}", t.text);
                assert!(t.text.contains("sha256:abc.tok"));
            }
            other => panic!("expected Text fallback, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetcher_error_falls_back_to_text_stub_not_panic() {
        // A blob the gateway can't read (missing, deleted, etc.)
        // must not blow up the LLM call — drop to the same text stub
        // an unconfigured fetcher would emit, with a tracing warn so
        // operators can find it.
        let client = vision_client().with_blob_fetcher(Arc::new(FailingFetcher));
        let block = ContentBlock::Image {
            blob: BlobRef {
                blob_id: "sha256:gone.tok".into(),
            },
            mime_type: "image/png".into(),
            filename: None,
            width: None,
            height: None,
        };
        let out = client.user_content_for_block(&block).await;
        assert!(matches!(out, UserContent::Text(_)));
    }

    #[tokio::test]
    async fn image_block_on_text_only_model_skips_fetcher_and_stubs() {
        // Even with a fetcher attached, a model that doesn't claim
        // vision must NOT pull bytes — otherwise we'd waste 100 MiB
        // of base64 work just to send something the API will reject.
        let bytes = b"unused".to_vec();
        let registry = LlmProviderRegistry::with_default_providers();
        let mut client = registry
            .build_client(&LlmProviderConfig {
                provider: "openai".into(),
                api_key: Some("test".into()),
                base_url: None,
                model: "gpt-3.5-turbo".into(),
                supports_vision: None,
                context_window: None,
                pricing: None,
                reasoning_effort: None,
                vault: None,
                proxy: None,
            })
            .unwrap();
        // OpenAI factory may set vision=true on some models; force
        // false here so the test pins the dispatch rule, not the
        // model_info defaults.
        client.model_info.supports_vision = false;
        let client = client.with_blob_fetcher(Arc::new(StaticFetcher(bytes)));
        let block = ContentBlock::Image {
            blob: BlobRef {
                blob_id: "sha256:never-fetched.tok".into(),
            },
            mime_type: "image/jpeg".into(),
            filename: None,
            width: None,
            height: None,
        };
        let out = client.user_content_for_block(&block).await;
        assert!(matches!(out, UserContent::Text(_)));
    }

    #[test]
    fn parse_image_media_type_recognises_common_mimes() {
        assert_eq!(
            parse_image_media_type("image/jpeg"),
            Some(ImageMediaType::JPEG)
        );
        assert_eq!(
            parse_image_media_type("IMAGE/PNG"),
            Some(ImageMediaType::PNG)
        );
        assert_eq!(
            parse_image_media_type("image/jpeg; charset=binary"),
            Some(ImageMediaType::JPEG),
        );
        assert_eq!(parse_image_media_type("image/x-fancy"), None);
    }
}

#[cfg(test)]
mod document_dispatch_tests {
    //! Pin: rig's Anthropic converter `Err`s on every non-PDF document
    //! and that error is collected over the whole chat history, so a
    //! single `.md` attachment used to fail every later turn in the
    //! session; the DeepSeek / Ollama / HuggingFace converters instead
    //! splice our base64 payload straight into the joined user text.
    //! Text-like documents must therefore arrive as decoded text on every
    //! provider and without needing vision, a PDF must become a
    //! `Document` only where a converter really builds one and only while
    //! it fits the request ceiling, and the wrapper the model is told to
    //! trust as a delimiter must not be forgeable from a filename or a
    //! file body.

    use std::sync::Arc;

    use baybo_model::{BlobRef, ContentBlock, Role};
    use rig::message::UserContent;

    use super::*;
    use crate::registry::{LlmProviderConfig, LlmProviderRegistry};

    const TEXT_LIKE_MIMES: &[&str] = &[
        "text/plain",
        "text/html",
        "text/css",
        "text/markdown",
        "text/csv",
        "application/xml",
        "text/xml",
        "application/javascript",
        "text/javascript",
        "text/x-python",
        "application/x-python",
        "application/json",
        "text/json",
        "application/yaml",
        "application/x-yaml",
        "text/yaml",
        "text/x-yaml",
        "application/toml",
        "text/x-toml",
    ];

    struct StaticFetcher(Vec<u8>);

    #[async_trait::async_trait]
    impl BlobFetcher for StaticFetcher {
        async fn fetch(&self, _blob_id: &str) -> Result<Vec<u8>> {
            Ok(self.0.clone())
        }
    }

    struct FailingFetcher;

    #[async_trait::async_trait]
    impl BlobFetcher for FailingFetcher {
        async fn fetch(&self, blob_id: &str) -> Result<Vec<u8>> {
            Err(LlmError::Transient(format!("nope: {blob_id}")))
        }
    }

    fn client_for(provider: &str, model: &str, supports_vision: bool) -> LlmClient {
        let registry = LlmProviderRegistry::with_default_providers();
        let mut client = registry
            .build_client(&LlmProviderConfig {
                provider: provider.into(),
                api_key: Some("test".into()),
                base_url: None,
                model: model.into(),
                supports_vision: Some(supports_vision),
                context_window: None,
                pricing: None,
                reasoning_effort: None,
                vault: None,
                proxy: None,
            })
            .unwrap();
        client.model_info.supports_vision = supports_vision;
        client
    }

    fn vision_client(provider: &str, model: &str) -> LlmClient {
        client_for(provider, model, true)
    }

    fn file_block(filename: &str, mime_type: &str) -> ContentBlock {
        ContentBlock::File {
            blob: BlobRef {
                blob_id: "sha256:doc.tok".into(),
            },
            filename: filename.into(),
            mime_type: mime_type.into(),
            duration_ms: None,
            page_count: None,
            size_bytes: None,
        }
    }

    fn with_bytes(bytes: Vec<u8>) -> LlmClient {
        vision_client("minimax", "MiniMax-VL-01").with_blob_fetcher(Arc::new(StaticFetcher(bytes)))
    }

    /// A provider whose rig converter really builds a document part.
    fn pdf_client(bytes: Vec<u8>) -> LlmClient {
        vision_client("anthropic", "claude-sonnet-4-20250514")
            .with_blob_fetcher(Arc::new(StaticFetcher(bytes)))
    }

    async fn rendered_text(client: &LlmClient, block: &ContentBlock) -> String {
        match client.user_content_for_block(block).await {
            UserContent::Text(t) => t.text,
            other => panic!("expected Text variant, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pdf_stays_a_base64_document() {
        let bytes = media_probe::fixture::classic(3);
        let client = pdf_client(bytes.clone());
        let out = client
            .user_content_for_block(&file_block("report.pdf", "application/pdf"))
            .await;
        match out {
            UserContent::Document(doc) => {
                assert_eq!(doc.media_type, Some(DocumentMediaType::PDF));
                assert_eq!(
                    doc.additional_params
                        .as_ref()
                        .and_then(|params| params.get(DOCUMENT_FILENAME_PARAM))
                        .and_then(|value| value.as_str()),
                    Some("report.pdf")
                );
                let DocumentSourceKind::Base64(b64) = doc.data else {
                    panic!("expected base64 data");
                };
                use base64::Engine;
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&b64)
                    .unwrap();
                assert_eq!(decoded, bytes);
            }
            other => panic!("expected Document variant, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pdf_stubs_on_providers_that_splice_base64_into_prompt_text() {
        // rig's DeepSeek and Ollama converters `filter_map` a base64
        // Document into the joined user TEXT — the whole payload would
        // arrive as literal prompt characters.
        for (provider, model) in [("deepseek", "deepseek-chat"), ("ollama", "llama3.2")] {
            let client = vision_client(provider, model)
                .with_blob_fetcher(Arc::new(StaticFetcher(media_probe::fixture::classic(1))));
            let text = rendered_text(&client, &file_block("report.pdf", "application/pdf")).await;
            assert!(text.contains("[file:"), "{provider}: {text}");
        }
    }

    #[tokio::test]
    async fn oversize_pdf_stubs_instead_of_poisoning_every_later_turn() {
        let client = pdf_client(vec![b'%'; MAX_PDF_DOCUMENT_BYTES + 1]);
        let text = rendered_text(&client, &file_block("huge.pdf", "application/pdf")).await;
        assert!(text.contains("[file:"), "{text}");
        assert!(text.contains("huge.pdf"));
    }

    /// The gate the byte cap could never provide: an object-stream PDF
    /// whose 200 pages fit in 85 KB — well under any plausible byte cap —
    /// is refused on its PROBED page count. At `PDF_TOKENS_PER_PAGE`
    /// those pages are 1.56M tokens; the previous cap charged 62,400 and
    /// sent them.
    #[tokio::test]
    async fn a_small_but_many_paged_pdf_is_refused_on_its_page_count() {
        for pages in [MAX_PDF_PAGES + 1, 200, 5_000] {
            let bytes = media_probe::fixture::object_stream(pages as usize);
            assert!(bytes.len() < MAX_PDF_DOCUMENT_BYTES, "{pages}");
            let client = pdf_client(bytes);
            let text = rendered_text(&client, &file_block("many.pdf", "application/pdf")).await;
            assert!(text.contains("[file:"), "{pages}: {text}");
        }
    }

    /// A PDF that cannot be parsed cannot be priced, so it is not sent.
    #[tokio::test]
    async fn an_unparseable_pdf_stubs() {
        let client = pdf_client(b"%PDF-1.7 but not really".to_vec());
        let text = rendered_text(&client, &file_block("broken.pdf", "application/pdf")).await;
        assert!(text.contains("[file:"), "{text}");
    }

    /// A document whose `/Pages` nodes reference each other in a cycle:
    /// it LOADS, so `pdf_page_count` answers rather than failing, but the
    /// page tree yields nothing.
    fn cyclic_page_tree_pdf(declared: u32) -> Vec<u8> {
        let node = format!("<< /Type /Pages /Count {declared} /Kids [{{kid}} 0 R] >>");
        let objs: [(usize, String); 3] = [
            (1, "<< /Type /Catalog /Pages 2 0 R >>".to_string()),
            (2, node.replace("{kid}", "3")),
            (3, node.replace("{kid}", "2")),
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = vec![0usize; objs.len() + 1];
        for (num, body) in &objs {
            offsets[*num] = out.len();
            out.extend_from_slice(format!("{num} 0 obj\n{body}\nendobj\n").as_bytes());
        }
        let xref_at = out.len();
        let top = objs.len() + 1;
        out.extend_from_slice(format!("xref\n0 {top}\n0000000000 65535 f \n").as_bytes());
        for off in offsets.iter().take(top).skip(1) {
            out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer\n<< /Size {top} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n")
                .as_bytes(),
        );
        out
    }

    /// A page tree whose `/Kids` cycle back to their parent: the walk
    /// yields nothing, so the declared `/Count` is the only reading left
    /// and it decides both the price and the gate.
    ///
    /// Zero on both readings is the one answer that means the parse
    /// failed while still looking like a price — it charges the stub
    /// floor, and before the gate's lower bound the same document sailed
    /// through `<= MAX_PDF_PAGES` and was DELIVERED at that price. A
    /// declared count over the cap is refused on the count, which is
    /// exactly the reduction the walk used to hide.
    #[tokio::test]
    async fn a_cyclic_page_tree_is_gated_on_its_declared_count() {
        for declared in [0, MAX_PDF_PAGES + 1, 400] {
            let bytes = cyclic_page_tree_pdf(declared);
            assert_eq!(
                lopdf::Document::load_mem(&bytes)
                    .expect("fixture loads")
                    .get_pages()
                    .len(),
                0,
                "fixture no longer reproduces the zero-page walk"
            );
            assert_eq!(media_probe::pdf_page_count(&bytes), Some(declared));
            let client = pdf_client(bytes);
            let text = rendered_text(&client, &file_block("cycle.pdf", "application/pdf")).await;
            assert!(text.contains("[file:"), "{declared}: {text}");
        }
    }

    /// The half of `media_probe`'s contract that is invisible until it
    /// fires: off the reactor a parser panic arrives as a `JoinError`
    /// instead of unwinding through it, and the arm degrades to the same
    /// text stub an unreadable payload gets. The payload survives that,
    /// so an arm that does deliver still has its bytes.
    #[tokio::test]
    async fn a_panicking_probe_answers_none_instead_of_unwinding_the_reactor() {
        let bytes = Arc::new(vec![0u8; 8]);
        let answered =
            probe_off_reactor(&bytes, |_| -> Option<u32> { panic!("parser blew up") }).await;
        assert_eq!(answered, None);
        assert_eq!(bytes.len(), 8);
    }

    /// A PDF block carrying the page count its ingest probe recorded.
    fn pdf_block(page_count: Option<u32>) -> ContentBlock {
        ContentBlock::File {
            blob: BlobRef {
                blob_id: "sha256:doc.tok".into(),
            },
            filename: "report.pdf".into(),
            mime_type: "application/pdf".into(),
            duration_ms: None,
            page_count,
            size_bytes: None,
        }
    }

    /// The parse the delivery path stops paying. A count recorded at
    /// ingest is enough to REFUSE, and refusing early skips a fetch and a
    /// whole-payload walk that `build_completion_request` re-pays every
    /// turn for a document that can never be delivered.
    ///
    /// The bytes here are a perfectly deliverable 3-page document, so the
    /// probe would admit them: only the recorded count can be what refuses.
    #[tokio::test]
    async fn a_pdf_ingested_over_the_page_cap_is_refused_before_it_is_parsed() {
        for pages in [MAX_PDF_PAGES + 1, 200, 5_000] {
            let client = pdf_client(media_probe::fixture::classic(3));
            let text = rendered_text(&client, &pdf_block(Some(pages))).await;
            assert!(text.contains("[file:"), "{pages}: {text}");
        }
    }

    /// The other direction, and the reason the recorded count may only
    /// refuse: it came off the wire, so a low claim over a long document
    /// must not buy delivery. The gate still re-derives the count from the
    /// bytes.
    #[tokio::test]
    async fn a_low_page_count_claim_does_not_smuggle_a_long_document() {
        for claimed in [None, Some(1), Some(MAX_PDF_PAGES)] {
            let client = pdf_client(media_probe::fixture::object_stream(200));
            let text = rendered_text(&client, &pdf_block(claimed)).await;
            assert!(text.contains("[file:"), "{claimed:?}: {text}");
        }
    }

    #[tokio::test]
    async fn a_pdf_ingested_inside_the_page_cap_is_still_delivered() {
        for pages in [1, MAX_PDF_PAGES] {
            let client = pdf_client(media_probe::fixture::classic(pages as usize));
            let out = client.user_content_for_block(&pdf_block(Some(pages))).await;
            assert!(matches!(out, UserContent::Document(_)), "{pages}: {out:?}");
        }
    }

    #[tokio::test]
    async fn pdf_at_the_page_cap_still_goes_through() {
        for pages in [1, MAX_PDF_PAGES] {
            for bytes in [
                media_probe::fixture::classic(pages as usize),
                media_probe::fixture::object_stream(pages as usize),
            ] {
                let client = pdf_client(bytes);
                let out = client
                    .user_content_for_block(&file_block("big.pdf", "application/pdf"))
                    .await;
                assert!(
                    matches!(out, UserContent::Document(_)),
                    "{pages} pages: got {out:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn pdf_stubs_without_vision() {
        let client = client_for("anthropic", "claude-sonnet-4-20250514", false)
            .with_blob_fetcher(Arc::new(StaticFetcher(media_probe::fixture::classic(1))));
        let text = rendered_text(&client, &file_block("report.pdf", "application/pdf")).await;
        assert!(text.contains("[file:"), "{text}");
    }

    #[tokio::test]
    async fn chat_completions_openai_does_not_flatten_pdf_base64_into_text() {
        let client = vision_client("openai", "gpt-4o")
            .with_blob_fetcher(Arc::new(StaticFetcher(media_probe::fixture::classic(1))));
        let text = rendered_text(&client, &file_block("report.pdf", "application/pdf")).await;
        assert!(text.contains("[file:"), "{text}");
    }

    #[tokio::test]
    async fn text_like_documents_decode_into_text_with_filename() {
        for mime in TEXT_LIKE_MIMES {
            let client = with_bytes(b"# heading\nbody line".to_vec());
            let text = rendered_text(&client, &file_block("notes.md", mime)).await;
            assert!(text.contains("# heading\nbody line"), "{mime}: {text}");
            assert!(text.contains("notes.md"), "{mime}: {text}");
            assert!(text.contains(mime), "{mime}: {text}");
        }
    }

    #[tokio::test]
    async fn text_delivery_does_not_need_vision() {
        // deepseek-chat reads a markdown attachment fine; gating the
        // decode on `supports_vision` told the user it couldn't.
        let client = client_for("deepseek", "deepseek-chat", false)
            .with_blob_fetcher(Arc::new(StaticFetcher(b"plan: ship it".to_vec())));
        let text = rendered_text(&client, &file_block("notes.md", "text/markdown")).await;
        assert!(text.contains("plan: ship it"), "{text}");
        assert!(!text.contains("[file:"), "{text}");
    }

    #[tokio::test]
    async fn charset_suffixed_text_mime_still_decodes() {
        let client = with_bytes(b"a,b\n1,2".to_vec());
        let text = rendered_text(&client, &file_block("t.csv", "text/csv; charset=utf-8")).await;
        assert!(text.contains("a,b\n1,2"), "{text}");
    }

    #[tokio::test]
    async fn unknown_binary_document_falls_back_to_stub() {
        let client = with_bytes(vec![0x50, 0x4B, 0x03, 0x04]);
        let text = rendered_text(&client, &file_block("bundle.zip", "application/zip")).await;
        assert!(text.contains("[file:"), "stub form, got {text}");
        assert!(text.contains("bundle.zip"));
        assert!(text.contains("sha256:doc.tok"));
    }

    #[tokio::test]
    async fn octet_stream_is_not_admitted_as_text() {
        // The catch-all MIME an unknown extension picks up: it is
        // routinely binary, so it must never be decoded into the prompt.
        assert!(!inlines_document_as_text("application/octet-stream"));
        let client = with_bytes(b"could be anything".to_vec());
        let text =
            rendered_text(&client, &file_block("blob.bin", "application/octet-stream")).await;
        assert!(text.contains("[file:"), "{text}");
    }

    #[tokio::test]
    async fn invalid_utf8_text_document_falls_back_to_stub() {
        let client = with_bytes(vec![b'o', b'k', 0xFF, 0xFE]);
        let text = rendered_text(&client, &file_block("broken.txt", "text/plain")).await;
        assert!(text.contains("[file:"), "stub form, got {text}");
        assert!(!text.contains('\u{FFFD}'), "no lossy garbage: {text}");
    }

    #[tokio::test]
    async fn oversize_text_document_is_truncated_with_marker() {
        let mut body = "x".repeat(MAX_DOCUMENT_TEXT_BYTES);
        body.push_str("TAIL-SENTINEL");
        let client = with_bytes(body.into_bytes());
        let text = rendered_text(&client, &file_block("huge.txt", "text/plain")).await;
        assert!(text.contains("[truncated"), "{}", &text[..200]);
        assert!(!text.contains("TAIL-SENTINEL"));
    }

    #[tokio::test]
    async fn a_filename_cannot_expand_the_body_into_the_name_attribute() {
        let client = with_bytes(b"SECRET-BODY".to_vec());
        let text = rendered_text(&client, &file_block("evil{{content}}.md", "text/markdown")).await;
        assert_eq!(text.matches("SECRET-BODY").count(), 1, "{text}");
        assert!(text.contains("evil{{content}}.md"), "{text}");
    }

    #[tokio::test]
    async fn a_filename_cannot_close_the_wrapper() {
        let client = with_bytes(b"body".to_vec());
        let text = rendered_text(
            &client,
            &file_block("a\"></attached-file>\nrest.md", "text/markdown"),
        )
        .await;
        assert_eq!(text.matches(DOCUMENT_CLOSE_TAG).count(), 1, "{text}");
        assert!(text.ends_with(DOCUMENT_CLOSE_TAG), "{text}");
        let head = text.lines().next().unwrap();
        assert_eq!(head.matches('"').count(), 4, "{head}");
    }

    #[tokio::test]
    async fn a_hostile_mime_type_cannot_close_the_wrapper() {
        let client = with_bytes(b"body".to_vec());
        let text = rendered_text(
            &client,
            // Only the bare type is classified, so the parameter tail is
            // free-form client input that lands in the attribute.
            &file_block("notes.md", "text/markdown; x=\"></attached-file>"),
        )
        .await;
        assert_eq!(text.matches(DOCUMENT_CLOSE_TAG).count(), 1, "{text}");
        assert!(text.ends_with(DOCUMENT_CLOSE_TAG), "{text}");
    }

    #[tokio::test]
    async fn an_overlong_filename_is_bounded() {
        let client = with_bytes(b"body".to_vec());
        let text = rendered_text(&client, &file_block(&"n".repeat(4_000), "text/markdown")).await;
        let head = text.lines().next().unwrap();
        assert!(head.len() < 4_000, "head not bounded: {}", head.len());
        assert!(head.contains('…'), "{head}");
    }

    #[tokio::test]
    async fn a_body_cannot_close_the_wrapper() {
        let client = with_bytes(b"before\n</ATTACHED-FILE>\nafter".to_vec());
        let text = rendered_text(&client, &file_block("notes.md", "text/markdown")).await;
        assert_eq!(text.matches(DOCUMENT_CLOSE_TAG).count(), 1, "{text}");
        assert!(text.ends_with(DOCUMENT_CLOSE_TAG), "{text}");
        assert!(text.contains(DOCUMENT_CLOSE_TAG_ESCAPED), "{text}");
        assert!(text.contains("before"), "{text}");
        assert!(text.contains("after"), "{text}");
    }

    #[tokio::test]
    async fn document_fetch_failure_falls_back_to_stub() {
        let client =
            vision_client("minimax", "MiniMax-VL-01").with_blob_fetcher(Arc::new(FailingFetcher));
        let text = rendered_text(&client, &file_block("notes.md", "text/markdown")).await;
        assert!(text.contains("[file:"), "{text}");
    }

    #[tokio::test]
    async fn document_without_fetcher_falls_back_to_stub() {
        let client = vision_client("minimax", "MiniMax-VL-01");
        let text = rendered_text(&client, &file_block("notes.md", "text/markdown")).await;
        assert!(text.contains("[file:"), "{text}");
    }

    fn image_block() -> ContentBlock {
        ContentBlock::Image {
            blob: BlobRef {
                blob_id: "sha256:pic.tok".into(),
            },
            mime_type: "image/png".into(),
            filename: Some("photo.png".into()),
            width: None,
            height: None,
        }
    }

    /// The guard the image arm did not have. Anthropic refuses a payload
    /// this size outright, and because `build_completion_request` re-walks
    /// the whole history every turn that refusal would repeat until
    /// compaction evicted the row — the same shape as the audio bug.
    #[tokio::test]
    async fn an_oversize_image_stubs_instead_of_poisoning_every_later_turn() {
        let client = with_bytes(padded_png(MAX_IMAGE_DOCUMENT_BYTES + 1));
        let text = rendered_text(&client, &image_block()).await;
        assert!(text.contains("[image:"), "{text}");
    }

    #[tokio::test]
    async fn an_image_at_the_payload_cap_still_goes_through() {
        let client = with_bytes(padded_png(MAX_IMAGE_DOCUMENT_BYTES));
        let out = client.user_content_for_block(&image_block()).await;
        assert!(matches!(out, UserContent::Image(_)), "got {out:?}");
    }

    /// A real, priceable image padded to an exact byte length, so the
    /// BYTE guard is what the test above exercises rather than the
    /// dimension probe failing on filler.
    fn padded_png(bytes: usize) -> Vec<u8> {
        let mut out = media_probe::fixture::png(1_024, 1_024);
        out.resize(bytes, 0);
        out
    }

    /// The images that actually arrive, with what each of the three
    /// providers really bills for them: `(width, height, gemini, openai,
    /// anthropic)`.
    ///
    /// Only Gemini's price tracks the pixel count, so the three disagree
    /// by more than an order of magnitude on the same photo — which is why
    /// the delivery gate has to read the model in hand instead of the
    /// dearest of the three.
    const REAL_WORLD_IMAGES: &[(u32, u32, u64, u64, u64)] = &[
        // iPhone 12 MP.
        (4032, 3024, 6_192, 765, 2_352),
        // iPhone 24 MP — the DEFAULT camera output of an iPhone 15/16 Pro,
        // and what this app's own picker hands over.
        (5712, 4284, 12_384, 765, 2_352),
        // iPhone 48 MP.
        (8064, 6048, 22_704, 765, 2_352),
        // Design export.
        (6000, 4000, 12_384, 1_105, 2_128),
        // A4 at 600 dpi, i.e. a scan.
        (4960, 7016, 18_060, 1_105, 2_240),
        // iOS scrolling screenshot: the one shape where OpenAI is the
        // dearest, because its grid is driven by ASPECT.
        (1170, 23400, 15_996, 10_285, 168),
    ];

    /// Each provider's own arithmetic, stated as numbers so a change to
    /// any one tiling has to be re-justified here.
    #[test]
    fn the_three_providers_price_the_same_image_differently() {
        for (w, h, gemini, openai, anthropic) in REAL_WORLD_IMAGES {
            assert_eq!(gemini_image_tokens(*w, *h), *gemini, "{w}x{h} gemini");
            assert_eq!(openai_image_tokens(*w, *h), *openai, "{w}x{h} openai");
            assert_eq!(
                anthropic_image_tokens(*w, *h),
                *anthropic,
                "{w}x{h} anthropic"
            );
            // The budget's estimate is the worst of the three, and stays
            // that way: it is computed with no model in sight.
            assert_eq!(
                provider_image_tokens(*w, *h),
                *gemini.max(openai).max(anthropic),
                "{w}x{h} estimate"
            );
        }
    }

    /// Delivery is gated on what the CURRENT provider charges. Gating on
    /// the cross-provider maximum instead silently dropped every image in
    /// this table — including the 24 MP photo the app's own picker
    /// produces by default, which really costs Claude 2,352 tokens — and
    /// nothing in the pipeline downscales, so the user got no signal and
    /// no picture.
    #[tokio::test]
    async fn an_image_is_delivered_wherever_the_provider_billing_it_can_afford_it() {
        let mut delivered_somewhere = 0;
        let mut stubbed_somewhere = 0;
        for (w, h, gemini, openai, anthropic) in REAL_WORLD_IMAGES {
            let bytes = media_probe::fixture::png(*w, *h);
            assert!(bytes.len() < MAX_IMAGE_DOCUMENT_BYTES, "{w}x{h}");
            for (provider, model, priced) in [
                ("gemini", "gemini-2.5-flash", gemini),
                ("openai", "gpt-4o", openai),
                ("anthropic", "claude-sonnet-4-20250514", anthropic),
            ] {
                let client = vision_client(provider, model)
                    .with_blob_fetcher(Arc::new(StaticFetcher(bytes.clone())));
                let out = client.user_content_for_block(&image_block()).await;
                let deliverable = *priced <= IMAGE_TOKEN_CEILING as u64;
                assert_eq!(
                    matches!(out, UserContent::Image(_)),
                    deliverable,
                    "{w}x{h} on {provider} prices {priced}: got {out:?}"
                );
                if deliverable {
                    delivered_somewhere += 1;
                } else {
                    stubbed_somewhere += 1;
                }
            }
        }
        // Both outcomes have to be in the table, or the assertion above
        // could pass by only ever testing one of them.
        assert!(
            delivered_somewhere > 0 && stubbed_somewhere > 0,
            "{delivered_somewhere} delivered / {stubbed_somewhere} stubbed"
        );
    }

    /// An image whose PIXELS price above the cap is stubbed, because the
    /// payload cap above it bounds compressed bytes and no provider
    /// downscales before billing. Every case here fits in well under the
    /// 5 MiB the byte guard allows. The client is MiniMax, whose tiling
    /// nobody has read: an unverified provider pays the cross-provider
    /// maximum, the same whitelist rule the audio and PDF gates use.
    #[tokio::test]
    async fn an_image_whose_pixels_price_over_the_cap_stubs_instead_of_blowing_the_window() {
        for (w, h, tokens) in [
            (6000u32, 4000u32, 12_384u64),
            (8064, 6048, 22_704),
            (12000, 9000, 49_536),
            (1170, 23400, 15_996),
        ] {
            let bytes = media_probe::fixture::png(w, h);
            assert!(bytes.len() < MAX_IMAGE_DOCUMENT_BYTES, "{w}x{h}");
            assert_eq!(provider_image_tokens(w, h), tokens, "{w}x{h}");
            let text = rendered_text(&with_bytes(bytes), &image_block()).await;
            assert!(text.contains("[image:"), "{w}x{h} was delivered: {text}");
        }
    }

    /// …and one that prices under it still goes through, dimensions and
    /// all. A 12 MP phone photo is 24 tiles = 6,192 tokens.
    #[tokio::test]
    async fn an_image_inside_the_token_cap_is_still_delivered() {
        for (w, h) in [(1u32, 1u32), (768, 768), (3024, 4032), (4096, 4096)] {
            let client = with_bytes(media_probe::fixture::png(w, h));
            let out = client.user_content_for_block(&image_block()).await;
            assert!(matches!(out, UserContent::Image(_)), "{w}x{h}: {out:?}");
        }
    }

    #[tokio::test]
    async fn materialising_an_image_never_mutates_the_persistable_chat_message() {
        let bytes = media_probe::fixture::png(64, 64);
        let encoded = b64_encode(&bytes);
        let client =
            vision_client("openai", "gpt-4o").with_blob_fetcher(Arc::new(StaticFetcher(bytes)));
        let request = ChatRequest {
            messages: vec![baybo_model::ChatMessage::user(vec![image_block()])],
            temperature: None,
            tools: Vec::new(),
            reasoning_effort: None,
            ..Default::default()
        };

        let _ephemeral_wire_request = client.build_completion_request(&request).await;
        let persistable = serde_json::to_string(&request.messages).unwrap();

        assert!(persistable.contains("sha256:pic.tok"));
        assert!(
            !persistable.contains(&encoded),
            "base64 bytes must remain confined to the ephemeral provider request"
        );
    }

    #[tokio::test]
    async fn materialising_a_pdf_never_mutates_the_persistable_chat_message() {
        let bytes = media_probe::fixture::classic(1);
        let encoded = b64_encode(&bytes);
        let client = pdf_client(bytes);
        let request = ChatRequest {
            messages: vec![baybo_model::ChatMessage::user(vec![file_block(
                "report.pdf",
                "application/pdf",
            )])],
            temperature: None,
            tools: Vec::new(),
            reasoning_effort: None,
            ..Default::default()
        };

        let _ephemeral_wire_request = client.build_completion_request(&request).await;
        let persistable = serde_json::to_string(&request.messages).unwrap();

        assert!(persistable.contains("sha256:doc.tok"));
        assert!(
            !persistable.contains(&encoded),
            "base64 bytes must remain confined to the ephemeral provider request"
        );
    }

    #[tokio::test]
    async fn tool_choice_rides_only_when_the_request_carries_tools() {
        let client = with_bytes(media_probe::fixture::png(1, 1));
        let toolless = client
            .build_completion_request(&ChatRequest {
                messages: vec![baybo_model::ChatMessage::user(vec![ContentBlock::Text(
                    "hi".into(),
                )])],
                ..Default::default()
            })
            .await;
        assert!(
            toolless.tool_choice.is_none(),
            "a body with no tools must not name a tool_choice"
        );

        let tool = ToolDefinitionForLlm {
            name: "Bash".into(),
            description: "run a command".into(),
            parameters_schema: serde_json::json!({"type": "object"}),
        };
        let with_tools = client
            .build_completion_request(&ChatRequest {
                messages: vec![baybo_model::ChatMessage::user(vec![ContentBlock::Text(
                    "hi".into(),
                )])],
                tools: vec![tool.clone()],
                ..Default::default()
            })
            .await;
        assert!(matches!(
            with_tools.tool_choice,
            Some(rig::message::ToolChoice::Auto)
        ));

        let summariser = client
            .build_completion_request(&ChatRequest {
                messages: vec![baybo_model::ChatMessage::user(vec![ContentBlock::Text(
                    "hi".into(),
                )])],
                tools: vec![tool],
                tool_choice: ToolChoice::None,
                ..Default::default()
            })
            .await;
        assert!(matches!(
            summariser.tool_choice,
            Some(rig::message::ToolChoice::None)
        ));
    }

    /// [`delivers_media`] must match conversion for every role because context
    /// budgeting relies on the predicate alone.
    #[tokio::test]
    async fn delivers_media_matches_the_conversion_for_every_role() {
        let client = with_bytes(media_probe::fixture::png(640, 480));
        let blocks = vec![ContentBlock::Text("here you go".into()), image_block()];

        for role in [Role::User, Role::Assistant, Role::System, Role::Tool] {
            let msg = match role {
                Role::User => baybo_model::ChatMessage::user(blocks.clone()),
                Role::Assistant => baybo_model::ChatMessage::assistant(blocks.clone()),
                Role::System => baybo_model::ChatMessage::system(blocks.clone()),
                Role::Tool => baybo_model::ChatMessage::tool(blocks.clone()),
            };
            let wire = client
                .build_completion_request(&ChatRequest {
                    messages: vec![msg],
                    temperature: None,
                    tools: Vec::new(),
                    reasoning_effort: None,
                    ..Default::default()
                })
                .await;

            let delivered = wire.chat_history.iter().any(|m| match m {
                Message::User { content } => {
                    content.iter().any(|p| matches!(p, UserContent::Image(_)))
                }
                _ => false,
            });
            assert_eq!(delivered, delivers_media(role), "{role:?}");

            // A dropped block leaves nothing at all — not even the
            // `[image: …]` stub an undeliverable *user* block degrades to.
            // A stub would still cost real tokens and would have to be
            // priced; the budget charges zero, so nothing may remain.
            if !delivered {
                let rendered = format!("{:?} {:?}", wire.preamble, wire.chat_history);
                assert!(!rendered.contains("[image:"), "{role:?}: {rendered}");
            }
        }

        // The stakes: this is what the budget would charge for a block no
        // provider receives, since a dimensionless image prices at the cap.
        assert_eq!(content_block_tokens(&image_block()), IMAGE_TOKEN_CEILING);
    }

    /// A format with no pixel grid to read cannot be priced, so it is not
    /// delivered either — an SVG can declare a 100000x100000 viewBox in a
    /// kilobyte, and the byte cap would wave it straight through.
    #[tokio::test]
    async fn an_image_whose_dimensions_cannot_be_read_stubs() {
        let client = with_bytes(br#"<svg width="100000" height="100000"/>"#.to_vec());
        let text = rendered_text(&client, &image_block()).await;
        assert!(text.contains("[image:"), "{text}");
    }

    /// A zero-pixel header PARSES — `imagesize` answers `Some((0, 0))` —
    /// so it reaches the gate as a price rather than as a failed read, and
    /// OpenAI's grid divides by the short edge.
    #[tokio::test]
    async fn a_zero_pixel_image_stubs_on_every_provider() {
        for (w, h) in [(0u32, 0u32), (1_024, 0), (0, 1_024)] {
            for (provider, model) in [
                ("gemini", "gemini-2.5-flash"),
                ("openai", "gpt-4o"),
                ("anthropic", "claude-sonnet-4-20250514"),
                ("minimax", "MiniMax-VL-01"),
            ] {
                let client = vision_client(provider, model)
                    .with_blob_fetcher(Arc::new(StaticFetcher(media_probe::fixture::png(w, h))));
                let text = rendered_text(&client, &image_block()).await;
                assert!(text.contains("[image:"), "{w}x{h} on {provider}: {text}");
            }
        }
    }

    /// What [`IMAGE_TOKEN_CEILING`] has to cover, stated as the providers'
    /// own arithmetic rather than as a number, and evaluated ABOVE the
    /// 4,096-px edge the constant is derived from — the version of this
    /// test that swept only up to 4096x4096 could not fail, because that
    /// is exactly where the number was chosen.
    #[test]
    fn every_deliverable_image_prices_inside_the_ceiling() {
        assert_eq!(provider_image_tokens(3024, 4032), 6_192);
        assert_eq!(provider_image_tokens(4096, 4096), 9_288);
        // Above the cap the arm answers "stubbed", and the cases the old
        // ceiling silently under-charged are all here.
        for (w, h) in [
            (4096u32, 4864u32),
            (6000, 4000),
            (8064, 6048),
            (12000, 9000),
            (1170, 23400),
            (1, u32::MAX),
            (u32::MAX, u32::MAX),
        ] {
            assert!(
                provider_image_tokens(w, h) > IMAGE_TOKEN_CEILING as u64,
                "{w}x{h} is meant to be over the cap"
            );
            // Not zero: Anthropic and OpenAI still deliver most of these,
            // so charging the stub would under-count a block that ships.
            assert_eq!(
                image_tokens(Some(w), Some(h)),
                IMAGE_TOKEN_CEILING,
                "{w}x{h}"
            );
            assert!(
                anthropic_image_tokens(w, h) <= IMAGE_TOKEN_CEILING as u64,
                "{w}x{h} anthropic must stay under the charge it is billed at"
            );
        }
        // Everything the guard admits prices at or under the ceiling, and
        // the ceiling is what an unmeasured block pays.
        for (w, h) in [
            (1u32, 1u32),
            (384, 384),
            (768, 768),
            (1568, 1568),
            (2048, 2048),
            (3024, 4032),
            (4096, 4096),
            (512, 4096),
        ] {
            let charged = image_tokens(Some(w), Some(h));
            assert!(
                (1..=IMAGE_TOKEN_CEILING).contains(&charged),
                "{w}x{h} charged {charged}"
            );
        }
        assert_eq!(image_tokens(None, None), IMAGE_TOKEN_CEILING);
        assert_eq!(image_tokens(Some(4096), None), IMAGE_TOKEN_CEILING);
        assert_eq!(image_tokens(Some(0), Some(4096)), 0);
    }

    /// Anthropic's own arithmetic, which nothing else here exercises: it
    /// downscales to a 1,568-px long edge and reads 28-px patches, so it
    /// is the binding provider for a mid-size image where Gemini's 768-px
    /// tiles are still coarse.
    #[test]
    fn anthropics_patch_grid_is_priced_where_it_binds() {
        // 1000x1000: Gemini 4 tiles = 1,032, Anthropic 36x36 = 1,296.
        assert_eq!(image_tokens(Some(1_000), Some(1_000)), 1_296);
        // At and above the downscale edge Anthropic saturates at 56x56.
        for edge in [1_568u32, 2_048, 4_096] {
            assert!(image_tokens(Some(edge), Some(edge)) >= 3_136, "{edge}");
        }
    }

    fn audio_block() -> ContentBlock {
        ContentBlock::Audio {
            blob: BlobRef {
                blob_id: "sha256:voice.tok".into(),
            },
            mime_type: "audio/wav".into(),
            filename: None,
            duration_ms: None,
        }
    }

    fn audio_client(model: &str, bytes: Vec<u8>) -> LlmClient {
        vision_client("openai", model).with_blob_fetcher(Arc::new(StaticFetcher(bytes)))
    }

    #[tokio::test]
    async fn audio_stubs_on_a_provider_that_rejects_audio() {
        // An inbound Telegram voice note on a Claude-family model: rig
        // errors out over the whole history rather than just this block.
        let client = with_bytes(media_probe::fixture::wav(3));
        let text = rendered_text(&client, &audio_block()).await;
        assert!(text.contains("[audio:"), "stub form, got {text}");
    }

    #[tokio::test]
    async fn audio_stubs_on_an_openai_model_without_audio_input() {
        // gpt-4o answers the WHOLE request with a 400, so the block would
        // poison every later turn just as rig's converter error did.
        for model in ["gpt-4o", "gpt-4.1", "o3"] {
            let client = audio_client(model, media_probe::fixture::wav(3));
            let text = rendered_text(&client, &audio_block()).await;
            assert!(text.contains("[audio:"), "{model}: {text}");
        }
    }

    #[tokio::test]
    async fn audio_reaches_an_openai_audio_model() {
        for model in [
            "gpt-4o-audio-preview",
            "gpt-4o-mini-realtime-preview",
            "gpt-audio",
        ] {
            let client = audio_client(model, media_probe::fixture::wav(3));
            let out = client.user_content_for_block(&audio_block()).await;
            assert!(matches!(out, UserContent::Audio(_)), "{model}: got {out:?}");
        }
    }

    /// The gate that makes `audio_tokens(None)` a ceiling: over the
    /// duration cap the payload is not sent, so nothing can cost more
    /// than `MAX_AUDIO_SECONDS * AUDIO_TOKENS_PER_SECOND`. Priced flat at
    /// 100 with no cap, the same file was 57,500 tokens of undercount.
    #[tokio::test]
    async fn over_long_audio_stubs_on_its_probed_duration() {
        let client = audio_client(
            "gpt-audio",
            media_probe::fixture::wav(MAX_AUDIO_SECONDS + 1),
        );
        let text = rendered_text(&client, &audio_block()).await;
        assert!(text.contains("[audio:"), "{text}");
    }

    #[tokio::test]
    async fn audio_at_the_duration_cap_still_goes_through() {
        let client = audio_client("gpt-audio", media_probe::fixture::wav(MAX_AUDIO_SECONDS));
        let out = client.user_content_for_block(&audio_block()).await;
        assert!(matches!(out, UserContent::Audio(_)), "got {out:?}");
    }

    /// A block's `duration_ms` came off the wire on the inbound path, so
    /// the delivery gate re-derives it from the bytes. A short claim over
    /// a long payload must not buy delivery.
    #[tokio::test]
    async fn a_short_duration_claim_does_not_smuggle_a_long_payload() {
        let ContentBlock::Audio {
            blob, mime_type, ..
        } = audio_block()
        else {
            unreachable!()
        };
        let lying = ContentBlock::Audio {
            blob,
            mime_type,
            filename: None,
            duration_ms: Some(1_000),
        };
        let client = audio_client(
            "gpt-audio",
            media_probe::fixture::wav(MAX_AUDIO_SECONDS + 1),
        );
        let text = rendered_text(&client, &lying).await;
        assert!(text.contains("[audio:"), "{text}");
    }

    /// Audio nothing can measure cannot be priced, so it is not sent.
    #[tokio::test]
    async fn unmeasurable_audio_stubs() {
        let client = audio_client("gpt-audio", b"RIFF but not a wave".to_vec());
        let text = rendered_text(&client, &audio_block()).await;
        assert!(text.contains("[audio:"), "{text}");
    }

    #[tokio::test]
    async fn oversize_audio_stubs_before_it_is_parsed() {
        let client = audio_client("gpt-audio", vec![0u8; MAX_AUDIO_DOCUMENT_BYTES + 1]);
        let text = rendered_text(&client, &audio_block()).await;
        assert!(text.contains("[audio:"), "{text}");
    }
}

#[cfg(test)]
mod document_template_tests {
    //! Pin: both attribute slots and the body are client-controlled, so
    //! substitution happens in one pass (a value can never supply another
    //! slot's placeholder) and every slot is sanitized before it lands.

    use super::*;

    /// [`close_tag_len_at`] only defends the terminator it is told about,
    /// so the two must not drift apart.
    #[test]
    fn the_neutralized_tag_is_the_templates_terminator() {
        assert!(DOCUMENT_TEXT_TEMPLATE.ends_with(DOCUMENT_CLOSE_TAG));
        assert_eq!(
            DOCUMENT_TEXT_TEMPLATE.matches(DOCUMENT_CLOSE_TAG).count(),
            1
        );
    }

    #[test]
    fn render_slots_fills_in_one_pass() {
        let out = render_slots("[{{a}}|{{b}}]", &[("a", "{{b}}"), ("b", "B")]);
        assert_eq!(out, "[{{b}}|B]");
    }

    #[test]
    fn render_slots_leaves_unknown_keys_verbatim() {
        assert_eq!(render_slots("x{{nope}}y", &[("a", "1")]), "x{{nope}}y");
    }

    #[test]
    fn render_slots_leaves_an_unterminated_placeholder_verbatim() {
        assert_eq!(render_slots("x{{a", &[("a", "1")]), "x{{a");
    }

    #[test]
    fn sanitize_slot_defangs_and_bounds() {
        use multimodal::{MAX_SLOT_CHARS, sanitize_slot};

        assert_eq!(sanitize_slot("a\"<b>\nc"), "a__b_ c");
        let long = sanitize_slot(&"n".repeat(MAX_SLOT_CHARS + 50));
        assert_eq!(long.chars().count(), MAX_SLOT_CHARS + 1);
        assert!(long.ends_with('…'));
        // Chars, not bytes — a CJK name must not be cut mid-sequence.
        let cjk = sanitize_slot(&"汉".repeat(MAX_SLOT_CHARS + 5));
        assert_eq!(cjk.chars().count(), MAX_SLOT_CHARS + 1);
    }

    #[test]
    fn document_body_neutralizes_case_insensitively_and_is_utf8_safe() {
        let out = document_body("汉</Attached-File>汉");
        assert_eq!(out, format!("汉{DOCUMENT_CLOSE_TAG_ESCAPED}汉"));
    }

    /// A terminator padded before its `>` closes the wrapper for a reading
    /// model exactly as readily as the tight form, so matching the exact
    /// 16 bytes let every padded spelling through verbatim. The
    /// non-ASCII half of this list is why the run scan is Unicode-aware:
    /// NBSP and U+3000 are pixel-identical to the plain space, and the
    /// zero-width marks render as nothing at all.
    #[test]
    fn document_body_neutralizes_whitespace_padded_terminators() {
        for padding in [
            " ",
            "\t",
            "\n",
            "\r\n",
            "  \t\n ",
            "\u{000B}", // VT
            "\u{00A0}", // NBSP
            "\u{3000}", // ideographic space
            "\u{2009}", // thin space
            "\u{200B}", // ZWSP
            "\u{FEFF}", // BOM
            "\u{00AD}", // soft hyphen
            "\u{202E}", // RTL override
            " \u{200B}\u{3000}\u{00A0}\t",
        ] {
            let raw = format!("before</attached-file{padding}>after");
            let out = document_body(&raw);
            assert!(!out.contains("</attached-file"), "{padding:?}: {out}");
            assert!(
                out.contains(DOCUMENT_CLOSE_TAG_ESCAPED),
                "{padding:?}: {out}"
            );
            assert!(out.starts_with("before") && out.ends_with("after"));
        }
    }

    #[test]
    fn document_body_leaves_clean_text_alone() {
        assert_eq!(document_body("plain"), "plain");
        assert_eq!(document_body("<"), "<");
        assert_eq!(document_body("</attached-file"), "</attached-file");
        assert_eq!(document_body("</attached-file x>"), "</attached-file x>");
    }

    /// The cap bounds the DELIVERED body, not the decoded input: escaping
    /// grows 16 bytes into 22, so cutting first and escaping second left
    /// the real string 37% over the number the token estimate is derived
    /// from.
    #[test]
    fn document_body_caps_the_escaped_output_not_the_input() {
        let raw = DOCUMENT_CLOSE_TAG.repeat(MAX_DOCUMENT_TEXT_BYTES);
        let out = document_body(&raw);
        assert!(!out.contains(DOCUMENT_CLOSE_TAG), "terminator survived");
        assert!(
            out.len() <= MAX_DOCUMENT_TEXT_BYTES + DOCUMENT_TRUNCATION_MARKER_MAX_BYTES,
            "delivered {} bytes",
            out.len()
        );
        assert!(out.contains("[truncated"));
    }

    /// Every byte `user_content_for_block` can deliver for one inlined
    /// document has to sit inside the bound `baybo-context` prices from —
    /// the wrapper, both client-controlled attribute slots at their widest,
    /// a body that escapes on every byte, and the truncation marker.
    #[test]
    fn the_delivered_wrapper_never_exceeds_its_exported_bound() {
        let widest_slot = "𝕏".repeat(multimodal::MAX_SLOT_CHARS * 2);
        for body in [
            DOCUMENT_CLOSE_TAG.repeat(MAX_DOCUMENT_TEXT_BYTES),
            "𓀀".repeat(MAX_DOCUMENT_TEXT_BYTES),
            "x".repeat(MAX_DOCUMENT_TEXT_BYTES * 4),
        ] {
            let out = render_document_wrapper(&widest_slot, &widest_slot, &body);
            assert!(
                out.len() <= MAX_INLINED_DOCUMENT_BYTES,
                "delivered {} bytes, bound is {MAX_INLINED_DOCUMENT_BYTES}",
                out.len()
            );
        }
    }

    /// The same bound, but priced from the block's OWN strings and size
    /// rather than from the flat worst case — the whole point of
    /// [`inlined_document_tokens`]. It must still cover EVERY delivered
    /// byte, including the escape growth (a body of nothing but
    /// terminators renders 21 bytes for every 16), the two attribute
    /// slots and the truncation marker, none of which a byte count of the
    /// source contains.
    ///
    /// Swept over the slot widths too, because the slots are now priced
    /// rather than bounded: an estimate that read them at anything but
    /// their sanitized length would fail here on one side or the other.
    #[test]
    fn the_sized_estimate_covers_the_delivered_wrapper() {
        let slots = [
            ("notes.md", "text/markdown"),
            ("", ""),
            ("𝕏".repeat(multimodal::MAX_SLOT_CHARS * 2).as_str(), "x"),
            ("a\u{7}<b>\"c\"", "text/plain; charset=\"utf-8\""),
            (
                "𓀀".repeat(multimodal::MAX_SLOT_CHARS - 1).as_str(),
                "𝕏".repeat(multimodal::MAX_SLOT_CHARS + 1).as_str(),
            ),
        ]
        .map(|(f, m)| (f.to_string(), m.to_string()));
        for (filename, mime_type) in &slots {
            for body in [
                String::new(),
                "x".repeat(400),
                "𓀀".repeat(1_000),
                DOCUMENT_CLOSE_TAG.to_string(),
                DOCUMENT_CLOSE_TAG.repeat(700),
                DOCUMENT_CLOSE_TAG.repeat(MAX_DOCUMENT_TEXT_BYTES),
                "x".repeat(MAX_DOCUMENT_TEXT_BYTES * 4),
            ] {
                let out = render_document_wrapper(filename, mime_type, &body);
                let charged = inlined_document_tokens(filename, mime_type, Some(body.len() as u32));
                assert!(
                    charged >= out.len(),
                    "charged {charged} for {} delivered bytes",
                    out.len()
                );
                assert!(charged <= MAX_INLINED_DOCUMENT_BYTES);
            }
        }

        // Pin the phantom charge this removes. The flat bound prices two
        // 483-byte slots and a truncation marker no real attachment
        // carries; `notes.md` + `text/markdown` renders 21 bytes and an
        // uncut body renders no marker at all.
        const BODY_400: usize = 550;
        let real = inlined_document_tokens("notes.md", "text/markdown", Some(400));
        let flat = INLINED_DOCUMENT_WRAPPER_BYTES + BODY_400;
        assert_eq!((real, flat), (656, 1_695));
        // 64 is the per-message attachment cap, so this is ONE message:
        // flat, it read past the 96,000 trigger of a 128k window and
        // compacted the transcript.
        assert!(64 * flat > 96_000 && 64 * real < 96_000, "{real} vs {flat}");

        // The flat bound is still the ceiling, and an unsized row still
        // pays the cap.
        let widest = "𝕏".repeat(multimodal::MAX_SLOT_CHARS * 2);
        assert_eq!(
            inlined_document_tokens(&widest, &widest, None),
            MAX_INLINED_DOCUMENT_BYTES
        );
        assert_eq!(
            inlined_document_tokens(&widest, &widest, Some(u32::MAX)),
            MAX_INLINED_DOCUMENT_BYTES
        );
    }

    #[test]
    fn openai_audio_models_are_recognised_by_name() {
        assert!(openai_model_accepts_audio("gpt-4o-audio-preview"));
        assert!(openai_model_accepts_audio("GPT-Realtime"));
        assert!(!openai_model_accepts_audio("gpt-4o"));
        assert!(!openai_model_accepts_audio("o3"));
    }
}

#[cfg(test)]
mod media_pricing_tests {
    //! Pin: what the budget is charged for a media block, and the caps
    //! that make each charge a ceiling rather than a wish. Both prices
    //! are derived from a MEASURED fact carried on the block — pages for
    //! a PDF, seconds for audio — because the byte counts that stood in
    //! for them do not bound either quantity.

    use super::*;

    /// The smallest context window among providers that accept the
    /// respective kind (OpenAI's 128k default) and the compaction
    /// threshold applied to it.
    const TIGHTEST_WINDOW: usize = 128_000;
    const COMPRESSION_THRESHOLD: f64 = 0.75;

    fn trigger() -> usize {
        (TIGHTEST_WINDOW as f64 * COMPRESSION_THRESHOLD) as usize
    }

    fn image(width: Option<u32>, height: Option<u32>) -> baybo_model::ContentBlock {
        baybo_model::ContentBlock::Image {
            blob: baybo_model::BlobRef {
                blob_id: "sha256:pic.tok".into(),
            },
            mime_type: "image/png".into(),
            filename: None,
            width,
            height,
        }
    }

    fn file(filename: &str, mime_type: &str, size_bytes: Option<u32>) -> baybo_model::ContentBlock {
        baybo_model::ContentBlock::File {
            blob: baybo_model::BlobRef {
                blob_id: "sha256:doc.tok".into(),
            },
            filename: filename.into(),
            mime_type: mime_type.into(),
            duration_ms: None,
            page_count: None,
            size_bytes,
        }
    }

    fn pdf(pages: Option<u32>) -> baybo_model::ContentBlock {
        let baybo_model::ContentBlock::File { blob, filename, .. } =
            file("report.pdf", "application/pdf", None)
        else {
            unreachable!("file() builds a File block")
        };
        baybo_model::ContentBlock::File {
            blob,
            filename,
            mime_type: "application/pdf".into(),
            duration_ms: None,
            page_count: pages,
            size_bytes: None,
        }
    }

    /// The arithmetic behind [`MAX_PDF_PAGES`], asserted rather than
    /// asserted-in-prose: one PDF at the cap must fit under the trigger
    /// and one page more must not.
    #[test]
    fn the_pdf_page_cap_is_the_largest_that_cannot_force_a_compaction() {
        assert!(pdf_document_tokens(Some(MAX_PDF_PAGES)) < trigger());
        assert!((MAX_PDF_PAGES as usize + 1) * PDF_TOKENS_PER_PAGE > trigger());
        assert_eq!(pdf_document_tokens(Some(MAX_PDF_PAGES)), 93_600);
    }

    /// The stub floor is the stub the block really renders, not the
    /// widest one any block could render. Charged flat, an undeliverable
    /// attachment cost 1,505 tokens against ~56 delivered bytes, and 64
    /// of them on ONE message (the per-message attachment cap) read
    /// 96,320 — over the 96,000 trigger of a 128k window, on a transcript
    /// whose media really costs about 4,500.
    #[test]
    fn an_undeliverable_block_is_charged_the_stub_it_really_renders() {
        for (name, mime) in [
            ("bundle.zip", "application/zip"),
            ("clip.mp4", "video/mp4"),
            ("blob.bin", "application/octet-stream"),
        ] {
            let block = file(name, mime, None);
            let charged = content_block_tokens(&block);
            assert_eq!(charged, multimodal::content_block_to_text(&block).len());
            assert!(charged < 100, "{name} charged {charged}");
            assert!(
                64 * charged < trigger() && 64 * MAX_CONTENT_STUB_TOKENS > trigger(),
                "{name}: {charged}"
            );
        }
        // The flat bound is still an upper bound on it, however wide the
        // block's own strings are.
        let huge = "𝕏".repeat(100_000);
        let wide = file(&huge, &huge, None);
        assert!(content_block_tokens(&wide) <= MAX_CONTENT_STUB_TOKENS);
    }

    /// No arm may price below the stub it degrades to on a fetch failure
    /// or an over-cap payload — including the arms whose own answer is
    /// the zero that means "delivery stubs this".
    #[test]
    fn no_arm_prices_below_the_stub_it_renders() {
        for block in [
            image(None, None),
            image(Some(1), Some(1)),
            image(Some(12_000), Some(9_000)),
            image(Some(0), Some(0)),
            file("a.md", "text/markdown", Some(0)),
            file("a.md", "text/markdown", None),
            file("doc.pdf", "application/pdf", None),
            file("doc.zip", "application/zip", None),
            baybo_model::ContentBlock::Audio {
                blob: baybo_model::BlobRef {
                    blob_id: "sha256:voice.tok".into(),
                },
                mime_type: "audio/ogg".into(),
                filename: None,
                duration_ms: Some(1),
            },
        ] {
            let stub = multimodal::content_block_to_text(&block).len();
            assert!(
                content_block_tokens(&block) >= stub,
                "{block:?} prices below its {stub}-byte stub"
            );
        }
        // Text is not media and is priced by the tokenizer, not here.
        assert_eq!(
            content_block_tokens(&baybo_model::ContentBlock::Text("hello".into())),
            0
        );
    }

    #[test]
    fn the_audio_second_cap_is_the_largest_that_cannot_force_a_compaction() {
        assert!(audio_tokens(Some(MAX_AUDIO_SECONDS * MS_PER_SECOND)) < trigger());
        assert_eq!(
            audio_tokens(Some(MAX_AUDIO_SECONDS * MS_PER_SECOND)),
            57_600
        );
        assert!(2 * audio_tokens(Some(MAX_AUDIO_SECONDS * MS_PER_SECOND)) > trigger());
    }

    /// A probed page count prices the real document; an absent one — a
    /// legacy row — pays the cap, because delivery's own probe is what
    /// guarantees nothing bigger can reach a provider.
    #[test]
    fn pdf_is_priced_from_its_probed_page_count() {
        assert_eq!(pdf_document_tokens(Some(1)), PDF_TOKENS_PER_PAGE);
        assert_eq!(pdf_document_tokens(Some(5)), 5 * PDF_TOKENS_PER_PAGE);
        assert_eq!(
            pdf_document_tokens(None),
            MAX_PDF_PAGES as usize * PDF_TOKENS_PER_PAGE
        );
        // Over the cap, delivery stubs it — so the stub is the price.
        assert_eq!(pdf_document_tokens(Some(MAX_PDF_PAGES + 1)), 0);
        assert_eq!(pdf_document_tokens(Some(5_000)), 0);
        let over = pdf(Some(MAX_PDF_PAGES + 1));
        assert_eq!(
            content_block_tokens(&over),
            multimodal::content_block_to_text(&over).len()
        );
        assert_eq!(content_block_tokens(&pdf(Some(3))), 3 * PDF_TOKENS_PER_PAGE);
    }

    #[test]
    fn audio_is_priced_from_its_probed_duration() {
        assert_eq!(audio_tokens(Some(60 * MS_PER_SECOND)), 60 * 32);
        // Part seconds round up: a provider bills the second it started.
        assert_eq!(audio_tokens(Some(60_001)), 61 * 32);
        assert_eq!(
            audio_tokens(None),
            MAX_AUDIO_SECONDS as usize * AUDIO_TOKENS_PER_SECOND
        );
    }
}

#[cfg(test)]
mod cost_normalization_tests {
    //! Pin: the Anthropic adapter folds disjoint cache buckets back
    //! into `input_tokens` so downstream `compute_cost_usd` can use
    //! one billing formula across providers. If this regresses,
    //! Anthropic-cache workflows under-bill the cached/cache-write
    //! portions.

    use super::*;

    #[test]
    fn fold_token_usage_adds_cache_buckets_into_total() {
        let mut u = TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 50,
            cache_creation_input_tokens: 30,
        };
        fold_token_usage_cache_into_total(&mut u);
        assert_eq!(u.input_tokens, 180);
        assert_eq!(u.cached_input_tokens, 50);
        assert_eq!(u.cache_creation_input_tokens, 30);
        assert_eq!(u.output_tokens, 20);
    }

    #[test]
    fn fold_token_usage_is_idempotent_on_zero_cache_buckets() {
        let mut u = TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
        };
        fold_token_usage_cache_into_total(&mut u);
        assert_eq!(u.input_tokens, 100);
    }
}

#[cfg(test)]
mod provider_metadata_helpers_tests {
    //! Pin: the setup wizard's "Base URL" prompt and `resolve_api_key`'s
    //! last-resort env fallback both read from these helpers. If the
    //! registry walk regresses, a new provider added via the macro
    //! would silently lose its env-var fallback / URL hint and the
    //! wizard would show empty prompts for it.

    use super::*;

    #[test]
    fn hand_written_factories_advertise_their_metadata() {
        // Hand-written `priced=`-style provider (anthropic): URL +
        // canonical env both round-trip.
        assert_eq!(
            default_base_url_for_provider("anthropic"),
            Some("https://api.anthropic.com"),
        );
        assert_eq!(
            default_api_key_env_for_provider("anthropic"),
            Some("ANTHROPIC_API_KEY"),
        );
    }

    #[test]
    fn macro_factories_advertise_api_key_env_and_base_url() {
        // Macro-generated rig providers mirror rig's baked-in
        // `ProviderBuilder::BASE_URL` so the setup wizard prefills it
        // instead of showing an empty `Base URL: ` prompt; the env-var
        // convention also round-trips.
        assert_eq!(default_api_key_env_for_provider("xai"), Some("XAI_API_KEY"));
        assert_eq!(
            default_base_url_for_provider("xai"),
            Some("https://api.x.ai"),
        );

        // HuggingFace's convention is `HF_TOKEN`, not `*_API_KEY`.
        assert_eq!(
            default_api_key_env_for_provider("huggingface"),
            Some("HF_TOKEN"),
        );
        assert_eq!(
            default_base_url_for_provider("huggingface"),
            Some("https://router.huggingface.co"),
        );

        // Keyless/optional-key hosts still surface their default endpoint.
        assert_eq!(
            default_base_url_for_provider("ollama"),
            Some("http://localhost:11434"),
        );
        assert_eq!(
            default_base_url_for_provider("llamafile"),
            Some("http://localhost:8080"),
        );

        // Macro-converted gemini supplies both kwargs.
        assert_eq!(
            default_api_key_env_for_provider("gemini"),
            Some("GEMINI_API_KEY"),
        );
        assert_eq!(
            default_base_url_for_provider("gemini"),
            Some("https://generativelanguage.googleapis.com"),
        );
    }

    #[test]
    fn oauth_and_keyless_providers_expose_no_env_var() {
        // Subscription provider is OAuth-only — must NOT advertise an
        // env var (would mislead `resolve_api_key` into reading some
        // unrelated env). Base URL is still present for the wizard.
        assert!(default_api_key_env_for_provider("openai-subscription").is_none());
        assert_eq!(
            default_base_url_for_provider("openai-subscription"),
            Some("https://chatgpt.com/backend-api"),
        );

        // llamafile is keyless, ollama has an optional key — both
        // leave env empty (operators wire `api_key_env` per-entry if
        // their deployment needs one).
        assert!(default_api_key_env_for_provider("llamafile").is_none());
        assert!(default_api_key_env_for_provider("ollama").is_none());
    }

    #[test]
    fn unregistered_provider_yields_none_for_both() {
        // Sanity: lookup for an unknown name short-circuits at
        // `factory_for`, so neither helper accidentally falls back to
        // some unrelated factory's metadata.
        assert!(default_api_key_env_for_provider("not-a-real-provider").is_none());
        assert!(default_base_url_for_provider("not-a-real-provider").is_none());
    }
}

/// The operator's reasoning effort has to survive all the way into the
/// provider's request body — and stay out of it for providers baybo does not
/// send it to. Both halves are asserted here because both have failed
/// silently before: the level was recorded but never sent, and every
/// provider but one dropped it on the floor.
#[cfg(test)]
mod effort_wiring_tests {
    use super::*;
    use baybo_model::{ChatMessage, ContentBlock};
    use serde_json::json;

    fn client_with_effort(provider: &str, model: &str, entry_effort: Option<&str>) -> LlmClient {
        LlmProviderRegistry::with_default_providers()
            .build_client(&LlmProviderConfig {
                provider: provider.into(),
                api_key: Some("test".into()),
                base_url: None,
                model: model.into(),
                supports_vision: None,
                context_window: None,
                pricing: None,
                reasoning_effort: entry_effort.map(str::to_string),
                vault: None,
                proxy: None,
            })
            .expect("client builds without a network round trip")
    }

    fn request(pin: Option<&str>) -> ChatRequest {
        ChatRequest {
            messages: vec![ChatMessage::agent_context(vec![ContentBlock::Text(
                "hi".into(),
            )])],
            temperature: None,
            tools: vec![],
            reasoning_effort: pin.map(str::to_string),
            ..Default::default()
        }
    }

    async fn body_effort(client: &LlmClient, pin: Option<&str>) -> Option<serde_json::Value> {
        client
            .build_completion_request(&request(pin))
            .await
            .additional_params
    }

    #[tokio::test]
    async fn the_entry_default_reaches_each_dialect_in_its_own_shape() {
        let openai_compatible = client_with_effort("deepseek", "deepseek-chat", Some("medium"));
        assert_eq!(
            body_effort(&openai_compatible, None).await,
            Some(json!({"reasoning_effort": "medium"}))
        );

        let anthropic = client_with_effort("anthropic", "claude-sonnet-4-6", Some("high"));
        assert_eq!(
            body_effort(&anthropic, None).await,
            Some(json!({"output_config": {"effort": "high"}}))
        );

        let gemini = client_with_effort("gemini", "gemini-2.5-flash", Some("low"));
        assert_eq!(
            body_effort(&gemini, None).await,
            Some(json!({"generationConfig": {"thinkingConfig": {"thinkingLevel": "low"}}}))
        );
    }

    /// The chat header's per-session pin beats the entry default, and is
    /// what the ledger records — on every provider, not just the one that
    /// used to consume a per-request effort.
    #[tokio::test]
    async fn a_session_pin_overrides_the_entry_default() {
        let client = client_with_effort("deepseek", "deepseek-chat", Some("medium"));
        assert_eq!(
            body_effort(&client, Some("high")).await,
            Some(json!({"reasoning_effort": "high"}))
        );
        assert_eq!(
            client.effective_effort(Some("high")).as_deref(),
            Some("high")
        );
    }

    /// A provider baybo does not send effort to must look exactly as it did
    /// before this existed — nothing in the body, and NULL on the cost row
    /// rather than a level that never reached the wire.
    #[tokio::test]
    async fn an_unwired_provider_neither_sends_nor_records_a_level() {
        let client = client_with_effort("cohere", "command-r", Some("high"));
        assert_eq!(body_effort(&client, None).await, None);
        assert_eq!(body_effort(&client, Some("xhigh")).await, None);
        assert_eq!(client.effective_effort(Some("xhigh")), None);
    }

    /// Nothing configured anywhere: the request is byte-for-byte what it
    /// was before effort was wired, so this cannot regress existing setups.
    #[tokio::test]
    async fn no_configured_effort_sends_nothing() {
        let client = client_with_effort("deepseek", "deepseek-chat", None);
        assert_eq!(body_effort(&client, None).await, None);
        assert_eq!(client.effective_effort(None), None);
    }

    /// A rung the dialect cannot say is refused where the operator can still
    /// act on it — at startup, naming the alternatives — instead of being
    /// rounded to a neighbour at request time.
    #[test]
    fn a_rung_the_dialect_cannot_express_fails_the_entry() {
        let registry = LlmProviderRegistry::with_default_providers();
        let err = registry
            .build_client(&LlmProviderConfig {
                provider: "deepseek".into(),
                api_key: Some("test".into()),
                base_url: None,
                model: "deepseek-chat".into(),
                supports_vision: None,
                context_window: None,
                pricing: None,
                // No `reasoning_effort` value disables reasoning on the
                // OpenAI dialect — that is a different mechanism entirely.
                reasoning_effort: Some("off".into()),
                vault: None,
                proxy: None,
            })
            .err();
        // LlmClient is intentionally not Debug; expand expect_err manually.
        let msg = match err {
            Some(e) => e.to_string(),
            None => panic!("`off` has no OpenAI-dialect spelling, so the entry must fail"),
        };
        assert!(msg.contains("off"), "names the rejected rung: {msg}");
        assert!(
            msg.contains("low, medium, high"),
            "lists the usable rungs: {msg}"
        );
    }

    /// Operators who configured this before the ladder existed have `none`
    /// on disk; it is Codex's spelling of `off` and keeps working, while the
    /// cost row records the ladder's own name for that rung.
    #[test]
    fn the_ladder_absorbs_the_codex_spelling_of_off() {
        assert_eq!(
            crate::effort::ReasoningEffort::parse("none"),
            Some(crate::effort::ReasoningEffort::Off)
        );
        assert_eq!(
            crate::effort::EffortPick::parse("NONE").label(),
            crate::effort::ReasoningEffort::Off.as_str()
        );
    }

    /// A level baybo has not learned still reaches the provider, so a vendor
    /// shipping a new rung doesn't have to wait on a baybo release.
    #[tokio::test]
    async fn an_off_ladder_level_is_forwarded_untouched() {
        let client = client_with_effort("deepseek", "deepseek-chat", Some("ultra"));
        assert_eq!(
            body_effort(&client, None).await,
            Some(json!({"reasoning_effort": "ultra"}))
        );
        assert_eq!(client.effective_effort(None).as_deref(), Some("ultra"));
    }
}
