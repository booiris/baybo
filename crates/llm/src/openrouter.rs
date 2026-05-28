//! Bundled OpenRouter pricing + capability snapshot, plus a one-shot
//! live-fetch path.
//!
//! `CostManager` is seeded at boot from each built client's
//! `ModelInfo.pricing`, which the provider factories resolve through
//! [`pricing_for`] (snapshot below, then a flat per-provider default).
//! So every configured model attributes spend at the rate OpenRouter had
//! cached the last time someone ran `scripts/regen-openrouter-pricings.sh`,
//! not at a flat per-provider guess — offline, before the live overlay lands.
//!
//! Each snapshot entry also carries the model's `top_provider`
//! capability profile (max context window, max completion tokens) and
//! the multimodal `input_modalities` list. Provider factories consult
//! [`capabilities_for`] when populating `ModelInfo`, so per-model
//! context limits and vision support track OpenRouter's catalog
//! rather than hardcoded per-provider constants.
//!
//! At boot the runtime additionally calls [`fetch_overlay_for`] to
//! pick up any drift since the snapshot was cut, then loops on
//! [`REFRESH_INTERVAL`] so a long-running daemon catches upstream
//! price changes without restarts. Failures (network, OpenRouter
//! down, slug-miss, malformed body) are caller-tolerant — the
//! current value stays in place. This keeps tests, sandboxed
//! builds, and air-gapped deploys working with zero network
//! dependency.
//!
//! OpenRouter publishes prices as per-token USD strings (e.g.
//! `"0.00000125"`). [`ModelPricing`] stores per-1M-token MicroUsd, so
//! the conversion is `parse::<f64>() * 1_000_000.0` then
//! [`MicroUsd::from_usd_decimal`]. Float lives only at the boundary
//! parser; once inside `ModelPricing` everything is integer micros.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use aura_model::MicroUsd;
use serde::Deserialize;

use crate::ModelPricing;
use crate::providers::openrouter_prefix::openrouter_provider_prefix;

const SNAPSHOT_JSON: &str = include_str!("providers/openrouter_pricings.json");
const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";
const LIVE_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Cadence the runtime re-fetches OpenRouter pricing at. Once a day is
/// well below any reasonable rate limit, coarse enough for a catalog
/// whose prices change at a weekly-or-slower rhythm, and tight enough
/// that a long-running daemon picks up upstream price cuts/raises
/// inside a working day rather than only at the next process restart.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// Modality strings OpenRouter advertises in `architecture.input_modalities`
/// that imply a model can natively consume image bytes. Anything else
/// (text, code, embedding-only) falls back to text-only.
const VISION_INPUT_MODALITIES: &[&str] = &["image"];

#[derive(Debug, Deserialize)]
struct SnapshotFile {
    models: HashMap<String, RawModelEntry>,
}

#[derive(Debug, Deserialize)]
struct RawModelEntry {
    pricing: RawPricing,
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    max_completion_tokens: Option<u64>,
    #[serde(default)]
    input_modalities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawPricing {
    prompt: String,
    completion: String,
    #[serde(default)]
    input_cache_read: Option<String>,
    #[serde(default)]
    input_cache_write: Option<String>,
}

impl RawPricing {
    fn to_model_pricing(&self) -> Option<ModelPricing> {
        Some(ModelPricing {
            input_per_1m_tokens: parse_per_token_to_per_million_micros(&self.prompt)?,
            output_per_1m_tokens: parse_per_token_to_per_million_micros(&self.completion)?,
            cached_input_per_1m_tokens: self
                .input_cache_read
                .as_deref()
                .and_then(parse_per_token_to_per_million_micros),
            cache_write_per_1m_tokens: self
                .input_cache_write
                .as_deref()
                .and_then(parse_per_token_to_per_million_micros),
        })
    }
}

/// Per-model capability profile harvested from OpenRouter's
/// `top_provider` and `architecture` blocks. Factories consult this to
/// populate `ModelInfo.context_window` and `supports_vision` instead of
/// hardcoded per-provider constants.
///
/// Every field is optional so a snapshot row that's missing the data
/// leaves the factory's fallback in place rather than silently
/// truncating context to 0 or flipping vision off.
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelCapabilities {
    /// `top_provider.context_length` — the maximum prompt+completion
    /// the model's primary provider accepts. Used as `ModelInfo.context_window`.
    pub context_window: Option<usize>,
    /// `top_provider.max_completion_tokens` — upper bound on a single
    /// completion. Surfaced separately because `context_window` already
    /// includes the prompt; a 1M-context model often caps completions
    /// at ~128k.
    pub max_completion_tokens: Option<usize>,
    /// True iff `architecture.input_modalities` advertises an image
    /// input mode. Other modalities (audio, video, file) imply richer
    /// multimodal support but the LLM-side dispatch only branches on
    /// vision today, so we collapse to a single bool.
    pub supports_vision: Option<bool>,
}

impl RawModelEntry {
    fn to_capabilities(&self) -> ModelCapabilities {
        let supports_vision = if self.input_modalities.is_empty() {
            None
        } else {
            Some(
                self.input_modalities
                    .iter()
                    .any(|m| VISION_INPUT_MODALITIES.contains(&m.as_str())),
            )
        };
        ModelCapabilities {
            context_window: self.context_length.map(|n| n as usize),
            max_completion_tokens: self.max_completion_tokens.map(|n| n as usize),
            supports_vision,
        }
    }
}

fn parse_per_token_to_per_million_micros(s: &str) -> Option<MicroUsd> {
    let v: f64 = s.parse().ok()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    Some(MicroUsd::from_usd_decimal(v * 1_000_000.0))
}

#[derive(Debug, Clone, Copy, Default)]
struct SnapshotEntry {
    pricing: ModelPricing,
    capabilities: ModelCapabilities,
}

static SNAPSHOT: LazyLock<HashMap<String, SnapshotEntry>> =
    LazyLock::new(
        || match serde_json::from_str::<SnapshotFile>(SNAPSHOT_JSON) {
            Ok(raw) => raw
                .models
                .into_iter()
                .filter_map(|(slug, entry)| {
                    let pricing = entry.pricing.to_model_pricing()?;
                    let capabilities = entry.to_capabilities();
                    Some((
                        slug,
                        SnapshotEntry {
                            pricing,
                            capabilities,
                        },
                    ))
                })
                .collect(),
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "openrouter pricing snapshot failed to parse; \
                     falling back to empty map — every advertised model \
                     will attribute at the per-provider flat default. \
                     Run scripts/regen-openrouter-pricings.sh.",
                );
                HashMap::new()
            }
        },
    );

pub(crate) fn snapshot_pricing(slug: &str) -> Option<ModelPricing> {
    SNAPSHOT.get(slug).map(|e| e.pricing)
}

pub(crate) fn snapshot_capabilities(slug: &str) -> Option<ModelCapabilities> {
    SNAPSHOT.get(slug).map(|e| e.capabilities)
}

/// Composes [`slug_for`] + [`snapshot_pricing`] so factories' default
/// `pricing_for_model` doesn't repeat the chain. `None` when the
/// provider isn't OpenRouter-routable or the resolved slug isn't in
/// the bundled snapshot — caller falls back to a per-provider flat
/// default.
pub fn pricing_for(provider: &str, model_id: &str) -> Option<ModelPricing> {
    snapshot_pricing(&slug_for(provider, model_id)?)
}

/// Snapshot capabilities for an Aura `(provider, model_id)` pair.
/// `None` when the slug isn't OpenRouter-routable or isn't in the
/// bundled snapshot — caller keeps its hardcoded default.
pub fn capabilities_for(provider: &str, model_id: &str) -> Option<ModelCapabilities> {
    snapshot_capabilities(&slug_for(provider, model_id)?)
}

/// Enumerate every snapshot model id for an Aura provider, stripped of
/// the `<or_provider>/` slug prefix. Returns the catalog ids OpenRouter
/// publishes for that vendor — useful as a fallback model list when the
/// vendor's own `/v1/models` endpoint is empty or unimplemented.
///
/// The strings are slug-shaped (lowercase, dash-separated) and may not
/// round-trip back to the vendor's preferred display casing
/// (`minimax-m2` vs `MiniMax-M2`), but most vendors accept their model
/// ids case-insensitively. Returns an empty vec when the provider isn't
/// OpenRouter-routable.
pub fn snapshot_model_ids_for(provider: &str) -> Vec<String> {
    let Some(or_provider) = openrouter_provider_prefix(provider) else {
        return Vec::new();
    };
    let prefix = format!("{or_provider}/");
    SNAPSHOT
        .keys()
        .filter_map(|slug| slug.strip_prefix(&prefix).map(str::to_string))
        .collect()
}

/// Map an Aura `(provider, model_id)` pair to its OpenRouter catalog
/// slug. Returns `None` for providers that don't ship through
/// OpenRouter (`openai-subscription` bills against a flat OAuth
/// subscription, not per-token).
///
/// Algorithmic — no per-provider hand tables. Rules in order:
///
/// 1. Provider name maps to OpenRouter's prefix (`gemini` → `google`,
///    everything else passes through).
/// 2. `model_id` is lowercased.
/// 3. A trailing `-` followed by 8 ASCII digits is stripped (covers
///    Anthropic's dated suffixes, e.g. `claude-haiku-4-5-20251001` →
///    `claude-haiku-4-5`).
/// 4. A `-` is rewritten to `.` only when both adjacent characters
///    are ASCII digits **and** each digit run is exactly one
///    character long (covers Aura's dash-separated version style
///    like `claude-opus-4-6` → `claude-opus-4.6`, while leaving
///    OpenRouter's date-stamped slugs intact —
///    `gpt-4o-2024-11-20` stays as `gpt-4o-2024-11-20` because the
///    `2024` and `1106` runs are multi-digit).
///
/// Models the algorithm can't massage into the right slug
/// (`MiniMax-Text-01` lowercases to `minimax-text-01` while
/// OpenRouter has `minimax-01`) just miss the snapshot lookup and
/// fall through to the per-provider flat default — acceptable
/// degradation in exchange for not maintaining a hand table.
pub fn slug_for(provider: &str, model_id: &str) -> Option<String> {
    let or_provider = openrouter_provider_prefix(provider)?;
    let lowered = model_id.to_ascii_lowercase();
    let no_date = strip_trailing_date_suffix(&lowered);
    let normalized = digit_dash_to_dot(no_date);
    Some(format!("{or_provider}/{normalized}"))
}

/// `'-' + YYYYMMDD` — Anthropic's dated-checkpoint suffix shape.
const COMPACT_DATE_SUFFIX_LEN: usize = 9;

fn strip_trailing_date_suffix(s: &str) -> &str {
    if s.len() < COMPACT_DATE_SUFFIX_LEN {
        return s;
    }
    let (head, tail) = s.split_at(s.len() - COMPACT_DATE_SUFFIX_LEN);
    if !tail.starts_with('-') {
        return s;
    }
    if tail[1..].bytes().all(|b| b.is_ascii_digit()) {
        head
    } else {
        s
    }
}

fn digit_dash_to_dot(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'-'
            && is_isolated_digit_at(bytes, i.wrapping_sub(1))
            && is_isolated_digit_at(bytes, i + 1)
        {
            out.push('.');
        } else {
            out.push(b as char);
        }
    }
    out
}

/// Single-digit run at `pos`: a digit byte not flanked by another
/// digit on either side. Multi-digit runs (years like `2024`, build
/// numbers like `1106`) are excluded so dated slugs stay intact.
fn is_isolated_digit_at(bytes: &[u8], pos: usize) -> bool {
    let Some(b) = bytes.get(pos) else {
        return false;
    };
    if !b.is_ascii_digit() {
        return false;
    }
    let prev_is_digit = pos
        .checked_sub(1)
        .and_then(|p| bytes.get(p))
        .is_some_and(|c| c.is_ascii_digit());
    let next_is_digit = bytes.get(pos + 1).is_some_and(|c| c.is_ascii_digit());
    !prev_is_digit && !next_is_digit
}

#[derive(Deserialize)]
struct ApiResponse {
    data: Vec<ApiEntry>,
}

#[derive(Deserialize)]
struct ApiEntry {
    id: String,
    pricing: RawPricing,
    #[serde(default)]
    top_provider: ApiTopProvider,
    #[serde(default)]
    architecture: ApiArchitecture,
}

#[derive(Deserialize, Default)]
struct ApiTopProvider {
    #[serde(default)]
    context_length: Option<u64>,
    #[serde(default)]
    max_completion_tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
struct ApiArchitecture {
    #[serde(default)]
    input_modalities: Vec<String>,
}

impl ApiEntry {
    fn capabilities(&self) -> ModelCapabilities {
        RawModelEntry {
            pricing: RawPricing {
                prompt: String::new(),
                completion: String::new(),
                input_cache_read: None,
                input_cache_write: None,
            },
            context_length: self.top_provider.context_length,
            max_completion_tokens: self.top_provider.max_completion_tokens,
            input_modalities: self.architecture.input_modalities.clone(),
        }
        .to_capabilities()
    }
}

/// Live-fetch overlay covering both pricing and capability drift since
/// the bundled snapshot was cut. Returns `model_id → (pricing, caps)`.
///
/// Single OpenRouter round-trip via [`fetch_catalog`] regardless of
/// how many entries are passed. Caller-tolerant: any failure
/// (network, slug-miss, decode) yields an empty overlay rather than
/// erroring — the caller's snapshot fallback stays in place.
///
/// Each entry's slug is computed via [`slug_for`]; entries whose
/// provider isn't OpenRouter-routable (`openai-subscription`, custom
/// providers) are silently skipped.
pub async fn fetch_overlay_for(
    entries: &[(&str, &str)],
    proxy: Option<&aura_security::http::ProxySettings>,
) -> HashMap<String, (ModelPricing, ModelCapabilities)> {
    let mut wanted: Vec<(String, String)> = Vec::new();
    for (provider, model) in entries {
        let Some(slug) = slug_for(provider, model) else {
            continue;
        };
        wanted.push((slug, (*model).to_string()));
    }
    if wanted.is_empty() {
        return HashMap::new();
    }
    let catalog = match fetch_catalog(proxy).await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(
                error = %e,
                "openrouter live fetch failed; keeping snapshot pricing for all entries",
            );
            return HashMap::new();
        }
    };
    let mut overlay = HashMap::new();
    for (slug, model) in wanted {
        if let Some(entry) = catalog.get(&slug).copied() {
            overlay.insert(model, (entry.pricing, entry.capabilities));
        }
    }
    overlay
}

/// One-shot fetch of OpenRouter's full `/api/v1/models` catalog,
/// reduced to slug → pricing + capability map. Single network
/// round-trip — callers refreshing many configured models should
/// batch through [`fetch_overlay_for`].
///
/// Bounded by [`LIVE_FETCH_TIMEOUT`] so a sluggish OpenRouter doesn't
/// extend boot. Failures are caller-tolerant; the overlay paths
/// swallow them and keep the bundled snapshot.
async fn fetch_catalog(
    proxy: Option<&aura_security::http::ProxySettings>,
) -> crate::Result<HashMap<String, SnapshotEntry>> {
    let body = fetch_models_response(proxy).await?;
    Ok(body
        .data
        .into_iter()
        .filter_map(|e| {
            let pricing = e.pricing.to_model_pricing()?;
            let capabilities = e.capabilities();
            Some((
                e.id,
                SnapshotEntry {
                    pricing,
                    capabilities,
                },
            ))
        })
        .collect())
}

async fn fetch_models_response(
    proxy: Option<&aura_security::http::ProxySettings>,
) -> crate::Result<ApiResponse> {
    let client = aura_security::http::client_builder(proxy)
        .and_then(|b| b.timeout(LIVE_FETCH_TIMEOUT).build())
        .map_err(|e| crate::LlmError::Config(format!("openrouter http client: {e}")))?;

    let resp = client
        .get(OPENROUTER_MODELS_URL)
        .send()
        .await
        .map_err(|e| crate::reqwest_to_error(e, "openrouter GET /models"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(crate::status_to_error(
            status.as_u16(),
            format!("openrouter GET /models returned {status}: {body}"),
        ));
    }

    resp.json()
        .await
        .map_err(|e| crate::LlmError::Decode(format!("openrouter /models: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_snapshot_parses_and_is_non_empty() {
        // If this fires, the JSON checked into providers/ is corrupt
        // or has drifted from the schema this loader expects. Run
        // scripts/regen-openrouter-pricings.sh to regenerate.
        assert!(
            !SNAPSHOT.is_empty(),
            "openrouter snapshot decoded to 0 entries"
        );
    }

    #[test]
    fn snapshot_carries_known_aura_targets() {
        // Pin the four canonical slugs Aura's default factories map
        // to, so a regen that accidentally filters them out is caught.
        for slug in [
            "openai/gpt-5",
            "openai/gpt-5-mini",
            "anthropic/claude-opus-4.6",
            "minimax/minimax-m2",
        ] {
            assert!(
                snapshot_pricing(slug).is_some(),
                "bundled snapshot missing required slug {slug}",
            );
        }
    }

    #[test]
    fn gpt_5_mini_strictly_cheaper_than_gpt_5() {
        // The whole point of per-model snapshot pricing: -mini /
        // -haiku tiers must NOT attribute at the flagship rate.
        // Pin both axes.
        let flagship = snapshot_pricing("openai/gpt-5").unwrap();
        let mini = snapshot_pricing("openai/gpt-5-mini").unwrap();
        assert!(
            mini.input_per_1m_tokens < flagship.input_per_1m_tokens,
            "gpt-5-mini input not cheaper than gpt-5: mini={:?} flagship={:?}",
            mini.input_per_1m_tokens,
            flagship.input_per_1m_tokens,
        );
        assert!(
            mini.output_per_1m_tokens < flagship.output_per_1m_tokens,
            "gpt-5-mini output not cheaper than gpt-5: mini={:?} flagship={:?}",
            mini.output_per_1m_tokens,
            flagship.output_per_1m_tokens,
        );
    }

    #[test]
    fn parse_per_token_handles_typical_openrouter_values() {
        // 0.00000125 USD/token = $1.25 / M tokens = 1_250_000 micros.
        let p = parse_per_token_to_per_million_micros("0.00000125").unwrap();
        assert_eq!(p.into_micros(), 1_250_000);
        // Zero is allowed (free tier filtered upstream, but the parser
        // shouldn't reject it).
        let z = parse_per_token_to_per_million_micros("0").unwrap();
        assert_eq!(z, MicroUsd::ZERO);
        // Garbage in → None, not a panic.
        assert!(parse_per_token_to_per_million_micros("not-a-number").is_none());
        assert!(parse_per_token_to_per_million_micros("-1").is_none());
    }

    #[test]
    fn unknown_slug_returns_none() {
        assert!(snapshot_pricing("openai/does-not-exist-2099").is_none());
        assert!(snapshot_capabilities("openai/does-not-exist-2099").is_none());
    }

    #[test]
    fn snapshot_model_ids_for_strips_provider_prefix() {
        let ids = snapshot_model_ids_for("minimax");
        assert!(
            !ids.is_empty(),
            "expected minimax slugs in bundled snapshot"
        );
        assert!(
            ids.iter().all(|id| !id.contains('/')),
            "ids should be stripped of `<provider>/` prefix: {ids:?}",
        );
        assert!(
            ids.iter().any(|id| id == "minimax-m2"),
            "expected canonical minimax-m2 slug, got {ids:?}",
        );
    }

    #[test]
    fn snapshot_model_ids_for_unknown_provider_is_empty() {
        assert!(snapshot_model_ids_for("openai-subscription").is_empty());
        assert!(snapshot_model_ids_for("not-a-real-provider").is_empty());
    }

    #[test]
    fn snapshot_model_ids_for_gemini_uses_google_prefix() {
        // Aura's `gemini` provider maps to OpenRouter's `google/` prefix —
        // the enumerate path must use the same mapping as `slug_for`,
        // otherwise gemini fallbacks return zero entries.
        let ids = snapshot_model_ids_for("gemini");
        assert!(
            !ids.is_empty(),
            "expected google/* slugs in bundled snapshot"
        );
    }

    #[test]
    fn slug_for_passes_through_simple_ids() {
        assert_eq!(slug_for("openai", "gpt-5").as_deref(), Some("openai/gpt-5"));
        assert_eq!(
            slug_for("openai", "gpt-5-mini").as_deref(),
            Some("openai/gpt-5-mini"),
        );
        assert_eq!(
            slug_for("openai", "gpt-4.1").as_deref(),
            Some("openai/gpt-4.1"),
        );
    }

    #[test]
    fn slug_for_remaps_gemini_to_google_prefix() {
        assert_eq!(
            slug_for("gemini", "gemini-2.5-flash").as_deref(),
            Some("google/gemini-2.5-flash"),
        );
        assert_eq!(
            slug_for("gemini", "gemini-3.1-pro-preview").as_deref(),
            Some("google/gemini-3.1-pro-preview"),
        );
    }

    #[test]
    fn slug_for_rewrites_digit_dash_versions() {
        assert_eq!(
            slug_for("anthropic", "claude-opus-4-6").as_deref(),
            Some("anthropic/claude-opus-4.6"),
        );
        assert_eq!(
            slug_for("anthropic", "claude-sonnet-4-6").as_deref(),
            Some("anthropic/claude-sonnet-4.6"),
        );
        // Single-digit version (no dash run) passes through.
        assert_eq!(
            slug_for("anthropic", "claude-opus-4").as_deref(),
            Some("anthropic/claude-opus-4"),
        );
    }

    #[test]
    fn slug_for_strips_trailing_date_then_rewrites() {
        // claude-haiku-4-5-20251001 → strip date → claude-haiku-4-5 →
        // digit-dash → claude-haiku-4.5. This is the only path that
        // resolves to OpenRouter's actual `claude-haiku-4.5` slug.
        assert_eq!(
            slug_for("anthropic", "claude-haiku-4-5-20251001").as_deref(),
            Some("anthropic/claude-haiku-4.5"),
        );
    }

    #[test]
    fn slug_for_preserves_dated_iso_suffix() {
        // OpenRouter's catalog ships dated checkpoint variants whose
        // slugs keep the YYYY-MM-DD form. The digit-dash rewrite must
        // NOT touch these, otherwise the snapshot lookup misses and
        // a configured user model attributes spend at $0 (budget gate
        // becomes ineffective). Pinned against every dated slug
        // currently in the bundled snapshot.
        let dated: &[(&str, &str, &str)] = &[
            ("openai", "gpt-4o-2024-05-13", "openai/gpt-4o-2024-05-13"),
            ("openai", "gpt-4o-2024-08-06", "openai/gpt-4o-2024-08-06"),
            ("openai", "gpt-4o-2024-11-20", "openai/gpt-4o-2024-11-20"),
            (
                "openai",
                "gpt-4o-mini-2024-07-18",
                "openai/gpt-4o-mini-2024-07-18",
            ),
            (
                "gemini",
                "gemini-2.5-pro-preview-05-06",
                "google/gemini-2.5-pro-preview-05-06",
            ),
            (
                "gemini",
                "gemini-2.5-flash-lite-preview-09-2025",
                "google/gemini-2.5-flash-lite-preview-09-2025",
            ),
        ];
        for (provider, input, expected) in dated {
            assert_eq!(
                slug_for(provider, input).as_deref(),
                Some(*expected),
                "dated slug {input} mangled — digit-dash rewrite over-eager",
            );
        }
    }

    #[test]
    fn slug_for_preserves_multidigit_build_numbers() {
        // Variants where the version-like component on either side of
        // a `-` has more than one digit (build numbers, partial dates)
        // must NOT be rewritten — `gpt-4-0314` is real and would
        // otherwise become `gpt-4.0314`.
        assert_eq!(
            slug_for("openai", "gpt-4-0314").as_deref(),
            Some("openai/gpt-4-0314"),
        );
        assert_eq!(
            slug_for("openai", "gpt-4-1106-preview").as_deref(),
            Some("openai/gpt-4-1106-preview"),
        );
    }

    #[test]
    fn snapshot_carries_cache_tier_rates_for_anthropic() {
        // Anthropic's flagship slugs ship cache_read + cache_write
        // rates on OpenRouter; the regen script keeps them. If this
        // ever fires after a snapshot refresh, OpenRouter dropped the
        // fields upstream — the schema-defaulting in `RawPricing`
        // would silently leave them as None and Anthropic-cache
        // workflows would re-regress to the input rate.
        let p = snapshot_pricing("anthropic/claude-opus-4.6")
            .expect("anthropic/claude-opus-4.6 priced");
        assert!(p.cached_input_per_1m_tokens.is_some());
        assert!(p.cache_write_per_1m_tokens.is_some());
        let input = p.input_per_1m_tokens;
        let cached = p.cached_input_per_1m_tokens.unwrap();
        let write = p.cache_write_per_1m_tokens.unwrap();
        assert!(
            cached < input,
            "cache_read should be cheaper than input: cached={cached:?} input={input:?}",
        );
        assert!(
            write > input,
            "cache_write should be costlier than input: write={write:?} input={input:?}",
        );
    }

    #[test]
    fn dated_slugs_resolve_against_bundled_snapshot() {
        // End-to-end: every dated slug above must resolve to a real
        // bundled snapshot entry. Closes the loop on the runtime
        // reproduction of the bug — the slug rewriter and the
        // snapshot need to stay in sync.
        for (provider, input) in [
            ("openai", "gpt-4o-2024-05-13"),
            ("openai", "gpt-4o-2024-08-06"),
            ("openai", "gpt-4o-2024-11-20"),
            ("openai", "gpt-4o-mini-2024-07-18"),
            ("gemini", "gemini-2.5-pro-preview-05-06"),
        ] {
            assert!(
                pricing_for(provider, input).is_some(),
                "configured dated model {provider}/{input} not priced — \
                 snapshot lookup missed, budget gate would attribute $0",
            );
        }
    }

    #[test]
    fn slug_for_lowercases_minimax_titlecase() {
        assert_eq!(
            slug_for("minimax", "MiniMax-M2").as_deref(),
            Some("minimax/minimax-m2"),
        );
        assert_eq!(
            slug_for("minimax", "MiniMax-M1").as_deref(),
            Some("minimax/minimax-m1"),
        );
    }

    #[test]
    fn slug_for_returns_none_for_subscription_provider() {
        // openai-subscription bills against an OAuth subscription, not
        // per-token, so OpenRouter has no equivalent route.
        assert!(slug_for("openai-subscription", "gpt-5").is_none());
        assert!(slug_for("unknown-provider", "anything").is_none());
    }

    #[test]
    fn pricing_for_resolves_via_slug_then_snapshot() {
        // End-to-end: provider+model_id → real snapshot pricing.
        let p = pricing_for("openai", "gpt-5-mini").expect("gpt-5-mini priced");
        assert!(p.input_per_1m_tokens > MicroUsd::ZERO);
        // claude-opus-4-6 only resolves because slug_for rewrites the dash.
        let p = pricing_for("anthropic", "claude-opus-4-6").expect("opus-4.6 priced");
        assert!(p.output_per_1m_tokens > MicroUsd::ZERO);
        // Subscription provider returns None even for an otherwise-known id.
        assert!(pricing_for("openai-subscription", "gpt-5").is_none());
    }

    #[test]
    fn capabilities_for_resolves_context_window_and_vision() {
        // Pin: Aura's canonical Anthropic flagship resolves to the
        // 1M-context vision-capable profile via the snapshot. If this
        // regresses after a refresh, the factory's hardcoded fallback
        // still works but operators lose per-model context tracking.
        let caps = capabilities_for("anthropic", "claude-opus-4-6")
            .expect("anthropic capabilities present");
        let ctx = caps
            .context_window
            .expect("opus-4.6 must publish context_length");
        assert!(
            ctx >= 200_000,
            "claude-opus-4.6 context_window unexpectedly small: {ctx}",
        );
        assert_eq!(caps.supports_vision, Some(true));

        // openai-subscription is account-tier dependent; OpenRouter has
        // no equivalent route, so capabilities are unresolved.
        assert!(capabilities_for("openai-subscription", "gpt-5").is_none());
    }

    #[test]
    fn capabilities_for_marks_text_only_models_non_vision() {
        // MiniMax-M2 is text-first; the snapshot's input_modalities
        // must collapse to supports_vision=false so the factory sets
        // the right ModelInfo flag.
        let caps = capabilities_for("minimax", "MiniMax-M2").expect("minimax-m2 capabilities");
        assert_eq!(caps.supports_vision, Some(false));
    }
}
