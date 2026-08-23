//! Admin surface (TCP + bearer token). Hosts config/status/turns/cron/
//! traces/skills/tools/llm, a read-only channel list, and the web chat
//! REST surface.

pub mod agents;
pub mod analytics;
pub mod channels;
pub mod chat;
pub mod config;
pub mod cron;
pub mod deck;
pub mod llm;
pub mod logs;
pub mod project_team;
pub mod projects;
pub mod push;
pub mod skills;
pub mod status;
pub mod tools;
pub mod traces;
pub mod turns;

use axum::Router;
use baybo_model::{LlmEntryName, LlmPin};
use utoipa::OpenApi;
use utoipa::openapi::OpenApi as OpenApiDoc;
use utoipa_axum::router::OpenApiRouter;

use crate::api::openapi;
use crate::server::AdminState;
use crate::{GatewayError, Result};

/// Validate a whole LLM pin — entry, the model within it, and the thinking
/// rung — against the live pool. The single home of that rule: the session
/// model switch, the agent-profile pin and the hire form all come through
/// here, so what a client may send cannot drift between them.
///
/// Three checks, and each rejects rather than degrades, because every one of
/// them is a pick that would otherwise be silently discarded at run time and
/// leave the UI showing a choice nobody is running:
///
/// - an unknown entry is a 400 (`resolve` would fall back safely on a
///   stranded name, which is exactly what makes it invisible);
/// - a model is only meaningful *within* an entry, so `model` without `llm`
///   is refused rather than dropped, and a model outside that entry's
///   `[model] + model_candidates` is refused rather than degraded to the
///   entry default;
/// - a rung outside baybo's ladder is refused so a typo surfaces here
///   instead of every turn. It is canonicalised on the way in, so `none` and
///   `off` cannot persist as two spellings of one rung.
///
/// Empty strings read as absent throughout: a cleared `<select>` posts `""`.
pub(crate) fn validate_llm_pin(
    state: &AdminState,
    llm: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<LlmPin> {
    let entry = match llm.map(str::trim) {
        None | Some("") => None,
        Some(name) => {
            let known = state
                .llm_pool
                .read()
                .entry_names()
                .iter()
                .any(|e| e.as_str() == name);
            if !known {
                return Err(GatewayError::BadRequest(format!(
                    "unknown LLM entry {name:?}; see GET /v1/llm/models for valid names"
                )));
            }
            Some(LlmEntryName::from(name))
        }
    };

    let model = match (&entry, model.map(str::trim)) {
        (_, None | Some("")) => None,
        (None, Some(_)) => {
            return Err(GatewayError::BadRequest(
                "model pick requires an llm entry; send llm together with model".to_string(),
            ));
        }
        (Some(entry), Some(model)) => {
            let pool = state.llm_pool.read();
            match pool.entry_model_ids(entry) {
                Some(ids) if ids.iter().any(|m| m == model) => {}
                _ => {
                    return Err(GatewayError::BadRequest(format!(
                        "model {model:?} is not a configured model of LLM entry {entry:?}; \
                         see GET /v1/llm/models for its model + model_candidates"
                    )));
                }
            }
            Some(model.to_string())
        }
    };

    let effort = match effort.map(str::trim) {
        None | Some("") => None,
        Some(level) => match baybo_llm::effort::ReasoningEffort::parse(level) {
            Some(rung) => Some(rung.as_str().to_string()),
            None => {
                return Err(GatewayError::BadRequest(format!(
                    "unknown reasoning_effort {level:?}; expected one of {}",
                    effort_ladder()
                )));
            }
        },
    };

    Ok(LlmPin {
        entry,
        model,
        effort,
    })
}

/// The rungs a pin accepts, for the 400's message. The ladder itself lives in
/// `baybo_llm::effort` — mirroring it here is how the gateway ended up two
/// rungs behind it.
fn effort_ladder() -> String {
    baybo_llm::effort::ReasoningEffort::ALL
        .iter()
        .map(|l| l.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Minimal top-level OpenAPI descriptor. Concrete paths and component
/// schemas are folded in by merging the `OpenApiRouter` output from
/// each submodule.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Baybo Admin API",
        description = "TCP + bearer-token surface: config, turns, cron, traces, skills, tools, channels, LLM."
    ),
    tags(
        (name = "status", description = "Gateway process status"),
        (name = "config", description = "Read and mutate on-disk BayboConfig"),
        (name = "turns", description = "Agent turn tracking"),
        (name = "jobs", description = "In-flight background jobs"),
        (name = "cron", description = "Scheduled prompts / tool calls"),
        (name = "traces", description = "Per-session trace export"),
        (name = "analytics", description = "Aggregated cost / session activity dashboards"),
        (name = "skills", description = "Registered skills"),
        (name = "tools", description = "Registered tool manifests"),
        (name = "channels", description = "Registered channel plugins"),
        (name = "chat", description = "Web and device chat sessions"),
        (name = "push", description = "Direct-mode device push registration"),
        (name = "llm", description = "Configured LLM provider"),
        (name = "logs", description = "Recent tracing events (in-memory ring buffer)"),
        (name = "agents", description = "User-managed agent profiles (chat personas)"),
        (name = "deck", description = "Agent-authored live cards (docs/modules/deck.md)"),
    )
)]
pub struct AdminApiDoc;

/// Build the admin v1 router together with its OpenAPI document.
///
/// Returns an axum `Router` already carrying `/v1/*` paths (including a
/// `/v1/openapi.json` endpoint that serves `spec`) plus the same
/// `OpenApi` document. The caller only has to wire state and auth
/// middleware — no external `nest("/v1", …)` required.
pub fn v1_router_and_spec() -> (Router<AdminState>, OpenApiDoc) {
    let v1 = OpenApiRouter::new()
        .merge(status::routes())
        .merge(config::routes())
        .merge(turns::routes())
        .merge(cron::routes())
        .merge(traces::routes())
        .merge(analytics::routes())
        .merge(skills::routes())
        .merge(tools::routes())
        .merge(channels::routes())
        .merge(chat::routes())
        .merge(push::routes())
        .merge(llm::routes())
        .merge(logs::routes())
        .merge(agents::routes())
        .merge(deck::routes())
        .merge(projects::routes())
        .merge(project_team::routes());
    let (router, spec) = OpenApiRouter::with_openapi(AdminApiDoc::openapi())
        .nest("/v1", v1)
        .split_for_parts();
    let router = router.merge(openapi::spec_route(spec.clone()));
    (router, spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_v1_router_assembles_without_panic() {
        let _ = v1_router_and_spec();
    }
}
