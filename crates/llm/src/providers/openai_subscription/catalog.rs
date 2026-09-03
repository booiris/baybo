//! Static supplement for the Codex model catalog.
//!
//! `GET <base>/codex/models` reports what the signed-in plan is
//! provisioned for, and stays the authority on that. The rows below make
//! the gpt-5.6 family selectable in the setup wizard and in
//! `baybo llm live-model` without waiting for the endpoint to list it
//! for a given account.
//!
//! Merge order is one-way: a slug the endpoint already returned is left
//! exactly as the endpoint described it, so this table can only ever add
//! ids — never mask, reorder or rewrite a real one. The cost of that
//! asymmetry is that a supplemented id the plan cannot actually serve
//! looks pickable and fails on the first chat call instead of at pick
//! time; the endpoint stays the authority on what a plan is entitled to.

use serde_json::json;

use crate::LiveModelInfo;

/// Model the setup wizard seeds as a subscription entry's `lite_model`.
///
/// The middle of the gpt-5.6 family's three tiers: enough headroom for the
/// auxiliary calls that hang off every turn (the Bash risk judges,
/// WebFetch's page summary, title generation) without spending the
/// plan's quota on the flagship the operator picked for chat.
pub const LITE_MODEL: &str = "gpt-5.6-terra";

/// Context window every gpt-5.6 model advertises.
const GPT_5_6_CONTEXT_WINDOW: usize = 1_050_000;

/// Marks a row as coming from this table rather than the endpoint, both
/// in the picker label and — via `extras` — under `--json`.
const SUPPLEMENT_DESCRIPTION: &str = "gpt-5.6 family (baybo catalog supplement)";
const SUPPLEMENT_SOURCE: &str = "baybo-catalog-supplement";

/// gpt-5.6 slugs and their display names, cheapest tier first.
const GPT_5_6_MODELS: &[(&str, &str)] = &[
    ("gpt-5.6-luna", "GPT-5.6 Luna"),
    ("gpt-5.6-luna-pro", "GPT-5.6 Luna Pro"),
    (LITE_MODEL, "GPT-5.6 Terra"),
    ("gpt-5.6-terra-pro", "GPT-5.6 Terra Pro"),
    ("gpt-5.6-sol", "GPT-5.6 Sol"),
    ("gpt-5.6-sol-pro", "GPT-5.6 Sol Pro"),
];

/// Append every [`GPT_5_6_MODELS`] row the endpoint didn't already list.
pub(super) fn supplement(mut live: Vec<LiveModelInfo>) -> Vec<LiveModelInfo> {
    for (slug, display_name) in GPT_5_6_MODELS {
        if live.iter().any(|m| m.id.eq_ignore_ascii_case(slug)) {
            continue;
        }
        live.push(LiveModelInfo {
            id: (*slug).to_string(),
            display_name: Some((*display_name).to_string()),
            description: Some(SUPPLEMENT_DESCRIPTION.to_string()),
            context_window: Some(GPT_5_6_CONTEXT_WINDOW),
            supports_vision: Some(true),
            supports_tools: Some(true),
            extras: json!({
                "slug": slug,
                "display_name": display_name,
                "context_window": GPT_5_6_CONTEXT_WINDOW,
                "input_modalities": ["text", "image", "file"],
                "source": SUPPLEMENT_SOURCE,
            }),
        });
    }
    live
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(id: &str) -> LiveModelInfo {
        LiveModelInfo {
            id: id.to_string(),
            display_name: Some("from the endpoint".into()),
            description: None,
            context_window: Some(272_000),
            supports_vision: Some(false),
            supports_tools: Some(true),
            extras: json!({ "slug": id }),
        }
    }

    #[test]
    fn an_empty_catalog_gains_the_whole_gpt_5_6_family() {
        let out = supplement(Vec::new());
        assert_eq!(out.len(), GPT_5_6_MODELS.len());
        for (slug, _) in GPT_5_6_MODELS {
            let row = out
                .iter()
                .find(|m| m.id == *slug)
                .unwrap_or_else(|| panic!("{slug} missing"));
            assert_eq!(row.context_window, Some(GPT_5_6_CONTEXT_WINDOW));
            assert_eq!(row.supports_vision, Some(true));
            assert_eq!(row.supports_tools, Some(true));
            assert_eq!(
                row.extras.get("source").and_then(|v| v.as_str()),
                Some(SUPPLEMENT_SOURCE),
                "supplemented rows must be distinguishable from real ones"
            );
        }
    }

    /// The endpoint is the authority: once it serves a slug, its own
    /// metadata is what the picker shows, and the row is not duplicated.
    #[test]
    fn a_slug_the_endpoint_already_serves_is_left_untouched() {
        let out = supplement(vec![remote(LITE_MODEL)]);
        assert_eq!(out.iter().filter(|m| m.id == LITE_MODEL).count(), 1);
        let row = out.iter().find(|m| m.id == LITE_MODEL).expect("present");
        assert_eq!(row.display_name.as_deref(), Some("from the endpoint"));
        assert_eq!(row.context_window, Some(272_000));
        assert_eq!(out.len(), GPT_5_6_MODELS.len());
    }

    #[test]
    fn dedupe_ignores_slug_case() {
        let out = supplement(vec![remote("GPT-5.6-Terra")]);
        assert_eq!(out.len(), GPT_5_6_MODELS.len());
        assert!(!out.iter().any(|m| m.id == LITE_MODEL));
    }

    /// Real models the endpoint reports keep their leading position, so
    /// the picker still opens on what the plan is actually provisioned for.
    #[test]
    fn endpoint_rows_stay_ahead_of_supplemented_ones() {
        let out = supplement(vec![remote("gpt-5"), remote("gpt-5-codex")]);
        assert_eq!(out[0].id, "gpt-5");
        assert_eq!(out[1].id, "gpt-5-codex");
        assert_eq!(out.len(), 2 + GPT_5_6_MODELS.len());
    }

    /// The wizard seeds `lite_model` from this const, so it has to name a
    /// row this table actually offers.
    #[test]
    fn the_lite_model_is_one_of_the_supplemented_slugs() {
        assert!(GPT_5_6_MODELS.iter().any(|(slug, _)| *slug == LITE_MODEL));
    }

    /// `resolve_effort` classifies every slug here through the `gpt-5.2+`
    /// arm — none of them fall through to the generic `low/medium/high`
    /// row, which would quietly drop `xhigh` from the CLI picker.
    #[test]
    fn every_supplemented_slug_lands_on_the_gpt_5_2_plus_effort_row() {
        for (slug, _) in GPT_5_6_MODELS {
            let efforts = super::super::allowed_efforts_for(slug);
            assert!(
                efforts.contains(&"xhigh"),
                "{slug} should offer xhigh, got {efforts:?}"
            );
        }
    }
}
