# web-search - Pluggable Web Search

## Overview

`baybo-search` owns the `WebSearch` tool and its Tavily, Brave, and SearXNG
providers. The tool returns ranked titles, URLs, and snippets; it never fetches
result pages.

The crate defines the provider-neutral query/result types, validates tool
arguments, enforces domain filters, bounds and renders results, maps provider
errors, and resolves credentials at boot. It depends on `baybo-tools` for the
tool contract and `baybo-config` for provider configuration, so it remains a
domain crate rather than adding a config dependency to `baybo-tools`.

`crates/baybo/src/runtime.rs` registers it after config, proxy, and the vault
are available. A disabled or unusable provider registers no tool; failures are
logged without preventing Baybo from starting.

## Search and fetch remain separate

Search results contain attacker-influenced URLs. `WebSearch` exposes them as
text only; `WebFetch` remains the single path for reading a result and applying
redirect pinning, content-type checks, response limits, and the SSRF floor.

## Domain filtering

Providers receive native filters or `site:` operator hints when supported, but
the tool re-checks every returned URL. This is required because provider-side
filters may be approximate or silently ignored.

Hosts are normalized to bare punycoded names. Matching respects label
boundaries: `rust-lang.org` admits `blog.rust-lang.org`, not
`evil-rust-lang.org`; malformed result URLs fail closed. A filter list with no
usable host is rejected instead of silently admitting or discarding results.
Suppression caused by the caller's filter is reported separately from the
operator's `blocked_domains` policy.

## Endpoint and credentials

Provider endpoints are compile-time constants or operator-configured. The
model supplies a query, never a destination, so this is the fixed-endpoint
exception documented in [`security.md`](security.md#ssrf-floor): constructors
require HTTP(S), reject embedded credentials, query strings, and fragments,
and disable redirects, but allow private addresses for self-hosted SearXNG.

`web_search.api_key_name` names a user secret; it never contains the key. Boot
resolves `user_env.<name>` from the vault, then the same process environment
variable. Tavily and Brave default to `TAVILY_API_KEY` and `BRAVE_API_KEY`;
SearXNG is keyless. Invalid environment-variable names are rejected during
config validation.

## Providers

| Provider | Request/auth | Domain filter | Freshness | Operational notes |
|---|---|---|---|---|
| `tavily` | `POST /search`, bearer token | native | native `time_range` | Default provider; publication age is usually absent outside news results. |
| `brave` | `GET /res/v1/web/search`, `X-Subscription-Token` | `site:` hints | `pd`/`pw`/`pm`/`py` | Query is capped at 400 characters and 50 words; oversized filter hints are omitted. |
| `searxng` | `GET <base_url>/search`, no key | `site:` hints | `day`/`month`/`year` | Requires `base_url` and JSON output enabled in SearXNG. A week maps to `month`; suspended engines are logged. |

Brave's terms restrict persistent storage and use of results for model
evaluation or improvement. Baybo persists tool results in session transcripts,
so operators must confirm their use is permitted and must not select Brave for
search-quality benchmarks.

## Configuration

```json
"web_search": {
  "enabled": false,
  "provider": "tavily",
  "max_results": 8,
  "blocked_domains": []
}
```

Search is opt-in. `max_results`, `country`, `language`, and
`blocked_domains` are deployment settings, not model arguments. Locale values
are forwarded verbatim because provider vocabularies differ: for example,
Brave expects a two-letter country code while Tavily expects a lowercase
country name.

`base_url` may contain a path prefix but no credentials, query, or fragment;
it is required for SearXNG. All fields are hot-reloadable. Reload atomically
replaces the dynamic `web_search` tool source, including on vault-only key
rotations; see [`config-hot-reload.md`](../config-hot-reload.md).

## Tool contract

Arguments are `query` (required), `allowed_domains`, `blocked_domains`, and
`freshness`. Domain lists are limited to 20 entries and are mutually exclusive.
`max_results` remains operator-controlled.

`freshness: "any"` is the explicit unfiltered value. It exists because a
strict-schema caller may materialize every optional enum; choosing `day` merely
as filler would silently turn an ordinary search into a recent-only search.

The description is static so the tool array remains stable across turns and
resumed sessions. The output is a numbered `ToolOutput::Text` block of title,
URL, optional age, and snippet.

### Error mapping

| Situation | Result |
|---|---|
| Invalid query, domain list, or freshness | `ToolError::InvalidParams` |
| HTTP 401/403 | terminal `ToolError::Execution` with credential guidance when applicable |
| Other HTTP status | `ToolOutput::Error`, allowing the model to retry or degrade |
| Decode or transport failure | `ToolError::Execution` without exposing raw request URLs |
| Deadline exceeded | `ToolError::Timeout` |
| Response over 1 MiB | `ToolError::Execution` |
| No results | successful explanatory text |

The tool owns the deadline and cancellation race so every provider has the
same bound; dropping the provider future cancels the request.

## Implementation constraints

- Credentials travel in HTTP headers, never URLs.
- Provider text fields are flattened and length-capped before rendering to
  prevent forged list entries or one result evicting the rest.
- Providers own only request encoding and response mapping; the tool owns
  policy, deadlines, rendering, and error classification. Transport traces
  contain status, byte count, and bounded body previews, not request URLs.

## Known gaps

- Search calls have no per-session budget and do not enter token-denominated
  `cost_records`; concurrent research can therefore consume provider quota
  without local cost visibility.
