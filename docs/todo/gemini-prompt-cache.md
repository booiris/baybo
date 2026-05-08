# Gemini Prompt Cache — Implicit Cache Disabled by `tools`

## Problem

Gemini implicit caching does not engage when the `generateContent` request carries
`tools` (function declarations). Every call comes back with
`usageMetadata.cachedContentTokenCount` absent (or 0), so `cost_records.cached_input_tokens`
stays at 0 across an entire session even when the prefix is byte-identical and well
above the model's minimum cache threshold.

Reproduced on:

- `gemini-3.1-flash-lite-preview` — 4 calls, prefix ≈ 8.7k tokens, all cached=0.
- `gemini-2.5-flash` — 2 calls, prefix ≈ 8.4k tokens, second call cached=0.

Both runs send 41 `function_declarations` (Bash, Read, Write, browser/*, …).
Aura-side prefix stability has been audited and is not the cause:

- `crates/agent/src/soul.rs:73` env block contains only workdir + platform — no
  timestamps, no UUIDs.
- `crates/tools/src/registry.rs:151` already sorts the tool list by name so the
  serialised `tools` array is byte-identical across calls.
- The rig path at `crates/llm/src/lib.rs:373` (and `:279` for streaming) already
  works around rig 0.34's missing `cached_input_tokens` mapping by reading
  `usage_metadata.cached_content_token_count` directly off the raw response —
  so a 0 here is what Google returned, not a parse loss.

Matches the open community report at
[vercel/ai #11513](https://github.com/vercel/ai/issues/11513): "Implicit caching
not working with Gemini 3 Flash when tools are defined." Our data extends the
scope to the 2.5 series.

## Why it's blocked

Gemini's *explicit* cache (`cachedContents` resource) is the only path to a
cache hit while tools are in play, but the API constraint is positional, not
existential: `system_instruction` and `tools` must live **inside** the
`CachedContent` itself, and the per-turn `generateContent` request must then
*not* re-send them. rig 0.34 has no plumbing for either side:

- `GenerateContentRequest` has no `cachedContent` field —
  `~/.cargo/registry/src/.../rig-core-0.34.0/src/providers/gemini/completion.rs:1945`
  is a literal commented-out `// cachedContent: Optional<String>`.
- `create_request_body` (same file, line 281) unconditionally writes
  `system_instruction` and `tools` into the request, with no bypass for the
  cached-content case.
- No client API for `POST /v1beta/cachedContents` resource creation.

Pushing `cachedContent` through `additional_params` (which is `flatten`-ed) is
half a workaround — it gets the field on the wire, but the request still also
carries `tools`/`system_instruction`, which Google rejects.

Implicit caching has no application-side fix; the API has to start honouring
prefixes that include tools. That is upstream-only.

## Proposed direction

Three options, ordered by cost:

**A — Wait + log.** Add a `warn!` at the rig-Gemini bridge (`crates/llm/src/lib.rs`
near the `cached_input_tokens` workaround) that fires once per session when
`cached_content_token_count` is 0/absent on a call with non-empty `tools` on a
2.5-or-newer Gemini model. Documents the gap in operator logs without committing
engineering effort. Revisit when rig 0.35 ships or Google fixes implicit caching.

**B — Explicit-cache POC.** Behind a feature flag, bypass rig for the Gemini
path: write a `GeminiExplicitCacheClient` that

1. Maintains a `(model, system_hash, tools_hash) -> cachedContents/<name>` map.
2. On first call per (system, tools) tuple, `POST /v1beta/cachedContents` with
   the system + tools + any stable initial documents; cache the resource name
   for the configured TTL (Gemini default 1h, billed per token-hour).
3. On subsequent calls, send `generateContent` with only `contents` +
   `cachedContent: <name>`, no `systemInstruction`/`tools`.
4. Refresh / recreate when TTL expires or system/tools rotate.

Cacheable surface is the ~7-8k stable prefix (system + 41 tool schemas); the
chat history continues to be billed at full rate. At 25% cached-input pricing
that is ~60% off the *prefix* portion, ~40-50% off total input on multi-turn
sessions. Verify storage cost (token-hour) does not eat the saving for
short-lived sessions before generalising.

**C — Status quo.** Accept zero cache savings on Gemini until upstream fixes
implicit caching. Cheapest; only viable if Gemini stays a side experiment.

## Design constraints

- **No silent regressions**. Whatever path lands, `cost_records.cached_input_tokens`
  must remain accurate — the column is what `aura cost` and any future quota
  logic key on. Don't fabricate cache hits we didn't get.
- **TTL accounting**. Explicit cache costs *whether or not it gets hit*. A
  session that ends after one turn pays storage for nothing. Cache lifecycle
  must be tied to either active-session set or a short TTL, not "create and
  hope."
- **Single-source guard for tools/system**. If we go down option B, the rig
  fork or bypass must enforce that `tools`/`systemInstruction` appear in
  exactly one of (cache resource, per-turn request), never both — the API
  silently degrades or 4xx's depending on which fields collide.

## Open questions

- Does Google's implicit caching ever start working with tools? Watch
  vercel/ai #11513 and the Gemini API release notes.
- Does rig 0.35+ expose `cachedContent`? If so, option B collapses to "wire it
  up + write the resource manager."
- Storage-cost break-even point — at what number of turns does a 7-8k cache
  resource pay back its 1-hour storage fee? Run the math against current
  Gemini pricing before committing to B.

## Related

- `crates/llm/src/lib.rs:264-294, :340-384` — rig 0.34 cache-token workaround.
- `crates/llm/src/providers/gemini.rs` — provider factory; explicit-cache work
  would land alongside.
- `crates/tools/src/registry.rs:144-152` — tool ordering rationale (Anthropic
  cache parity).
- [vercel/ai #11513](https://github.com/vercel/ai/issues/11513) — community
  report; subscribe for upstream resolution.
- [Gemini context caching docs](https://ai.google.dev/gemini-api/docs/caching).
