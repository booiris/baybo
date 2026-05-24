#!/usr/bin/env bash
# Refresh the OpenRouter pricing snapshot bundled into `aura-llm`.
#
# Why a snapshot at all: `CostManager` seeds `compute_cost_usd` from
# `LlmProviderRegistry::all_known_pricings()` at boot. The runtime
# also tries a live fetch for the active model, but every other
# model attribution and any boot where the live fetch fails (offline,
# OpenRouter down, no egress) falls back to this snapshot. Keeping it
# in-tree means tests, sandboxed builds, and air-gapped deploys all
# resolve to a real per-model rate instead of a flat per-provider
# guess.
#
# Output: crates/llm/src/providers/openrouter_pricings.json
#
# Filtering rules:
#   * Store the FULL OpenRouter catalog (every provider), not just the
#     ones Aura ships factories for. Pricing lookups are gated by
#     `openrouter::openrouter_provider_prefix` — an unmapped provider's
#     rows are simply never queried — so bundling them is harmless and
#     means adding a new provider factory needs no edit here. (The
#     bundled JSON is only the offline fallback anyway: at boot the
#     runtime live-fetches accurate pricing for the configured models.)
#   * Drop `:free` route variants (their prompt/completion are 0 and
#     we never want to attribute spend at $0 just because someone
#     happens to be configured against the free tier).
#   * Drop entries where both prompt and completion are 0 (likely
#     demos / placeholders).
#   * Under `pricing`, keep only `prompt` / `completion` /
#     `input_cache_read` / `input_cache_write` — the four fields
#     `ModelPricing` actually consumes. Image / audio / web_search /
#     internal_reasoning prices are not yet wired through
#     `compute_cost_usd`.
#   * Lift `top_provider.context_length` /
#     `top_provider.max_completion_tokens` to top-level
#     `context_length` / `max_completion_tokens`, and
#     `architecture.input_modalities` to `input_modalities` —
#     factories read these via `openrouter::capabilities_for` to
#     populate `ModelInfo.context_window` and `supports_vision`
#     instead of hardcoded per-provider constants.
#
# Run after a model add or whenever upstream prices drift; commit the
# resulting JSON change alongside the slug-table update.
set -euo pipefail

REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$REPO_ROOT/crates/llm/src/providers/openrouter_pricings.json"
SOURCE_URL="https://openrouter.ai/api/v1/models"

raw="$(mktemp)"
trap 'rm -f "$raw"' EXIT

echo "[*] Fetching $SOURCE_URL ..."
http_code="$(curl -fsSL -o "$raw" -w "%{http_code}" "$SOURCE_URL")"
if [[ "$http_code" != "200" ]]; then
    echo "[!] OpenRouter returned HTTP $http_code" >&2
    exit 1
fi

fetched_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# `-S` sorts keys so the bundled JSON has a deterministic model order:
# OpenRouter returns `.data[]` in a churn-prone recency order, which
# would otherwise make every regen a giant reshuffle diff instead of
# just the rows whose price/caps actually changed.
jq -S --arg t "$fetched_at" --arg src "$SOURCE_URL" '
  {
    fetched_at: $t,
    source: $src,
    models: (
      [.data[]
        | select(.id | test(":free$") | not)
        | select((.pricing.prompt | tonumber) > 0 or (.pricing.completion | tonumber) > 0)
        | { (.id): {
              pricing: {
                prompt:            .pricing.prompt,
                completion:        .pricing.completion,
                input_cache_read:  (.pricing.input_cache_read  // null),
                input_cache_write: (.pricing.input_cache_write // null),
              },
              context_length:        (.top_provider.context_length        // null),
              max_completion_tokens: (.top_provider.max_completion_tokens // null),
              input_modalities:      (.architecture.input_modalities      // []),
            } }
      ] | add
    )
  }
' "$raw" > "$OUT"

count="$(jq '.models | length' "$OUT")"
bytes="$(wc -c < "$OUT")"
echo "[*] Wrote $count models / $bytes bytes → $OUT"
