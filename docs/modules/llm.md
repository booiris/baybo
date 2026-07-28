# llm - LLM Client Layer

## Overview

The `llm` crate is Baybo's infrastructure layer for LLM calls, wrapping the **rig** framework into a unified `LlmClient` interface.

Core responsibilities:

- Provide a unified invocation interface — upper layers call `LlmClient::chat()` or `chat_stream()` without caring about the backend provider
- Hide provider differences behind registry-style extension via `LlmProviderRegistry`
- Leverage rig's native function calling for structured tool-use responses

**Design constraint**: this crate is pure infrastructure with no business logic. It does not depend on `tools` or `skills`. It does depend on `baybo-security` — the `openai-subscription` provider reads its OAuth bundle from the `SecretVault`.

## Design Decisions

### rig-based completion with enum dispatch

`LlmClient` wraps `AnyCompletionModel`, an enum with one variant per provider — `OpenAI`, `Anthropic`, `Gemini`, `DeepSeek`, `Minimax`, a set of rig-backed hosts added via the `rig_provider_factory!` macro (xAI, Mistral, Cohere, Perplexity, Moonshot, Z.ai, XiaomiMiMo, Groq, Together, Ollama, llamafile, Hyperbolic, HuggingFace), and `OpenAiSubscription` (the ChatGPT/Codex OAuth path, documented in [`llm-openai-subscription.md`](llm-openai-subscription.md)). The MiniMax provider uses rig's dedicated MiniMax client on its Anthropic-compatible surface (default base URL `https://api.minimaxi.com/anthropic`), sharing the Anthropic variant's cache-bucket folding and stream path; DeepSeek uses rig's dedicated `deepseek` provider (default `https://api.deepseek.com`) rather than the generic OpenAI-compatible path, because thinking mode requires `reasoning_content` round-tripped on assistant tool-call turns. This uses compile-time enum dispatch instead of trait objects — rig's `CompletionModel` trait is not object-safe (`Clone` + `impl Future`), and the deprecated `CompletionModelDyn` has been removed. Adding a new provider means adding an enum variant and a match arm.

`OpenAiSubscription` bypasses the rig adapter: it speaks the Codex Responses API directly over HTTP with its own OAuth dance; it plugs into the same enum dispatch as the rig providers — `LlmClient` builds the rig `CompletionRequest` normally, and `OpenAiSubscriptionCompletionModel` converts it into a Codex Responses API request with custom auth and 401-refresh handling.

Subprocess-driven agents (the `claude` binary) are **not** LLM providers and live outside this crate. See [`external-agents.md`](../external-agents.md).

### Streaming

`LlmClient::chat_stream()` returns `LlmStream`, a type-erased `futures::Stream<Item = Result<StreamEvent>>`. `StreamEvent` has five variants: `Text`, `ToolCall`, `Reasoning` (incremental delta), `ThinkingBlock(baybo_model::ContentBlock)` (complete structured reasoning block, preserved for providers that require thinking to be echoed back), and `Usage`. The stream maps rig's `StreamedAssistantContent` to these unified events, hiding provider-specific response types.

### Provider registry pattern

`LlmProviderRegistry` holds factory functions keyed by provider name. Built-in providers (OpenAI, Anthropic, Gemini, MiniMax, DeepSeek, xAI, Mistral, Cohere, Perplexity, Moonshot, Z.ai, XiaomiMiMo, Groq, Together, Ollama, llamafile, Hyperbolic, HuggingFace, OpenAI-subscription) are registered by the crate itself. New providers are added by implementing `LlmProviderFactory` and registering it.

### Multimodal support

When a `BlobFetcher` is attached (`LlmClient::with_blob_fetcher`) and the model reports `supports_vision`, `ContentBlock::Image` / `Audio` / `File` user blocks are materialised into real rig `Image` / `Audio` / `Document` content (base64-encoded blob bytes). Otherwise — no fetcher, text-only model, unsupported MIME type, or blob fetch failure — the block degrades to a descriptive text placeholder via the `multimodal` module (`[image: …]`-style stubs). `extract_text` joins text blocks for system/assistant message conversion.

Every stub slot (`filename`, `mime_type`, `blob_id`) goes through the same 120-character sanitizer the inlined-document wrapper uses, because the stub **is** the wrapper's fallback: a laxer stub would admit exactly the bytes the wrapper refuses.

### Delivery caps, and what the budget is charged

A media block costs the context window, so this crate owns both the gate and the price and `baybo-context` reads the price off it — one number, two enforcement points that cannot drift.

`content_block_tokens(block)` is the single entry point; it dispatches to the arm below and applies the stub floor.

| kind | delivered while | charged |
| --- | --- | --- |
| native PDF | `1 ≤ pages ≤ MAX_PDF_PAGES` (12) **and** `bytes ≤ 8 MiB` | `pdf_document_tokens(page_count)` = `pages × 7,800` |
| audio | `seconds ≤ MAX_AUDIO_SECONDS` (1,800) **and** `bytes ≤ 16 MiB` | `audio_tokens(duration_ms)` = `ceil(s) × 32` |
| image | dimensions readable **and** priced `≤ IMAGE_TOKEN_CEILING` (9,288) **by the provider on the other end** **and** `bytes ≤ 5 MiB` | `image_tokens(width, height)` = the dearest of the three providers' tilings |
| inlined text document | always (no capability needed) | `inlined_document_tokens(filename, mime_type, size_bytes)` = template (85) + the two sanitized slots + escaped body, bounded by `MAX_INLINED_DOCUMENT_BYTES` (17,529) |
| anything undeliverable | — | the stub the block itself renders, bounded by `MAX_CONTENT_STUB_TOKENS` (1,505) |

**Every price is the block's own, never a flat worst case where the block carries the inputs.** The wrapper and the stub are charged from `filename` / `mime_type` / `blob_id` run through the same `sanitize_slot` delivery substitutes — an exact charge with no I/O. `MAX_INLINED_DOCUMENT_BYTES` and `MAX_CONTENT_STUB_TOKENS` survive as bounds only. Charged flat they were ~1,064 phantom tokens on every attachment: `MAX_MESSAGE_BATCH_ATTACHMENTS` is 64, so 64 × 1,695 = 108,480 on ONE message crossed a 128k window's 96,000 trigger, and 64 undeliverable `.zip`/`.mp4` blocks delivering ~70 bytes each were charged 96,320 against a real ~4,500. One such pass was measured taking a 25-message transcript to a single summary with every `File` block gone.

The PDF and audio caps are sized so one attachment cannot force a compaction on its own on the tightest window that accepts it (OpenAI's 128k → a 96,000-token trigger): 12 × 7,800 = 93,600 and 1,800 × 32 = 57,600, while 13 pages would be 101,400.

The page range starts at **one** because zero is a failed parse wearing a valid-looking price: a `/Pages` cycle loads fine and reports no pages, which prices as the stub floor and used to pass the `≤ 12` gate and get delivered.

Pages, seconds, pixels and bytes are **probed or stat'd, not declared** — `media_probe::{pdf_page_count, audio_duration_ms, image_dimensions}` plus `BlobStore::stat`, wired at every ingest point (`AttachFile`, the gateway's inbound conversion); pages, seconds and dimensions are re-derived here before delivery. An ingest probe reads at most the **delivery cap of the arm it is probing**, not the widest of them: above that cap the block is always stubbed, so a fact recovered from it would be a price charged for something that costs the stub.

Every probe — ingest **and** delivery — runs under `spawn_blocking`: each parser is CPU-bound over the whole payload (measured in release, 16 ms for a 501 KiB PDF and ~140 ms for one at the 8 MiB delivery cap) and `build_completion_request` re-walks the whole history every turn, so a delivery probe is paid per blob per turn. A panic inside the blocking task surfaces as a `JoinError` instead of unwinding through the reactor, and degrades to the same text stub an unreadable payload gets. A count recorded at ingest may only **refuse**, never admit — a `page_count` already over the cap stubs before the fetch, since that document can never be delivered and the walk would otherwise be re-paid every turn; a low claim buys nothing, because the gate still re-derives the count from the bytes.

`pdf_page_count` takes the **larger** of the page-tree walk and the declared `/Pages /Count`, because the caller reads it as an upper bound and a walk is a lower one. `Document::get_pages` silently drops pages four ways (an unresolvable kid or unreadable `/Type`, a traversal that stops after `objects.len()` steps, a subtree past depth 256, a kid that is neither `/Page` nor `/Pages`); any reduction landing inside `1..=12` passed the gate and was both priced low and delivered, so a 40-page document walked as 5 was charged 39,000 while the provider billed 312,000 — every turn, since the request is rebuilt from the whole history. `Document::load_metadata_mem` reads the declared count off the catalog without walking anything.

A byte cap cannot stand in for pages: measured across `cupsfilter`, classic-xref and PDF 1.5 object-stream producers, real documents run **10 to 4,007 bytes per page**, so a 64 KiB cap admits 16 pages or 6,658 depending on nothing an ingester can see. The byte caps above bound the parse and the base64'd request body, nothing more. A payload neither the block nor the probe can measure is not sent — an unpriceable attachment is what "audio costs 100 tokens, no cap" was.

An inlined text document is the one arm whose price *is* its byte count, since those bytes are delivered as prompt text — so `ContentBlock::File` carries a server-derived `size_bytes` and the arm charges `min(escaped size, 16 KiB)` plus the wrapper rather than the flat cap. Charging the cap regardless billed a 400-byte `.md` 17,529 tokens; six of them read as 105,178 and tripped compaction on a 128k window by themselves. A row with no size (persisted before the field, or a blob that would not stat) keeps the cap as its fallback.

`image_tokens(width, height)` is the dearest of the three tilings, because the block is priced once and any of them may be the model on the other end: **Gemini** tiles the raw pixel grid at 768 px with no downscaling, 258 per tile (3024×4032 phone photo → 24 tiles = 6,192); **OpenAI** scales the SHORT side to 768 and bills 85 + 170 per 512-px tile, which is `2 × ceil(long′/512)` tiles — "at most eight" holds only below an 8:3 aspect ratio, and a 1170×23400 iOS scrolling screenshot becomes 768×15360 = 60 tiles = 10,285; **Anthropic** downscales to a 1,568-px long edge and reads 28-px patches, bounded at 56 × 56 = 3,136 but the dearest of the three for a mid-size image (1000×1000 → 1,296 against Gemini's 1,032).

`IMAGE_TOKEN_CEILING` (36 tiles of a 4,096-px square = 9,288) is now **enforced**, not assumed. Delivery re-probes the fetched bytes and stubs any image it cannot measure or that prices above the ceiling, which is what makes the ceiling the honest fallback for a block carrying no dimensions. That gate reads the price **the model in hand would really charge** (`AnyCompletionModel::delivers_image`, whitelist per provider — anything unverified pays the maximum), the way the audio and PDF gates already read the model: the estimate above has to be the cross-provider maximum because it is computed with no model in sight, but gating delivery on that maximum drops an image on the dearest provider's price while the provider actually billed charges a fraction of it. A 24 MP iPhone photo (5712×4284 — the default camera output of an iPhone 15/16 Pro) is 12,384 Gemini tokens, 765 OpenAI ones and 2,352 Anthropic ones, and nothing anywhere in the pipeline downscales, so a cross-provider gate dropped the app's own primary photo on every provider with no signal to the user. `MAX_IMAGE_DOCUMENT_BYTES` cannot do that job: it bounds COMPRESSED bytes while every provider bills pixels, and under 5 MiB it admitted a routine 6000×4000 design export (48 tiles = 12,384), a 48 MP iPhone HEIF (88 tiles = 22,704) and a 12000×9000 flat render (192 tiles = 49,536). An edge cap alone would not have bounded it either — OpenAI's cost is driven by aspect, so a 1×4096 sliver is 12,288 tiles. The only quantity that bounds the price is the price.

Because that gate is per-provider, the ESTIMATE caps at the ceiling rather than falling to the stub above it. Charging the stub would say "delivery refuses this, so it costs nothing" — true only of the provider whose price is the maximum. The same 24 MP photo Gemini refuses is delivered by Anthropic at 2,352, and an image only ever ships where its own biller prices it at or under the ceiling, so the ceiling is a true upper bound on a delivered image and the cap never under-counts one.

### Observability constraints

`LlmResponse` carries provider reasoning/thinking, tool calls, and full output content. The trace layer records all of these: `output_content`, `thinking`, `tool_calls`, and token usage.

### Error handling

Rate-limit retries are not handled in `llm`. They are managed by `AgentLoop` through `ErrorHandler`. Timeout is configurable at the HTTP client level; upper-layer Job monitoring can mark long-running calls as `Stuck`.

## Constraints

- Depends on `model` and `baybo-security` (the latter for the `openai-subscription` OAuth token vault), plus external crates `rig-core`, `reqwest`, `futures`, `serde`, `tokio`, `chrono`, `url`, and similar HTTP/serialization utilities
- Does not depend on `cost` — the dependency is one-directional (`cost` → `llm`): `cost` injects opaque `CostHooks` (admission guard + usage recorder) that `llm`'s `BoundBilledLlm` runs around every call, so a successful return guarantees the spend was recorded
- Does not depend on `baybo-storage` / `baybo-session`
- API keys should use environment-variable placeholders and must not be stored directly in config files

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `AgentLoop` calls `chat()` / `chat_stream()` through the `BillableLlm` / `BoundBilledLlm` billing wrapper and handles retries via `ErrorHandler` |
| `cost` | Consumes `TokenUsage` and `ModelPricing` to calculate per-call cost |
| `context` | Provides compressed message history for `ChatRequest` |
