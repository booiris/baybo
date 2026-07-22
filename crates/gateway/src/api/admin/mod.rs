//! Admin surface (TCP + bearer token). Hosts config/status/jobs/cron/
//! traces/skills/tools/llm, a read-only channel list, and the web chat
//! REST surface.

pub mod agents;
pub mod analytics;
pub mod channels;
pub mod chat;
pub mod config;
pub mod cron;
pub mod deck;
pub mod jobs;
pub mod llm;
pub mod logs;
pub mod push;
pub mod skills;
pub mod status;
pub mod tools;
pub mod traces;

use axum::Router;
use baybo_model::LlmEntryName;
use utoipa::OpenApi;
use utoipa::openapi::OpenApi as OpenApiDoc;
use utoipa_axum::router::OpenApiRouter;

use crate::api::openapi;
use crate::server::AdminState;
use crate::{GatewayError, Result};

/// Validate an optional LLM pin against the live pool. `None`/empty clears
/// the pin; an unknown name is a 400 so the operator gets a crisp error at
/// write time instead of a silent fallback later (`resolve` would fall back
/// safely on a stranded name). Shared by the session model switch and the
/// agent-profile endpoints so the contract and wording can't drift.
pub(crate) fn validate_llm_pin(
    state: &AdminState,
    llm: Option<&str>,
) -> Result<Option<LlmEntryName>> {
    match llm.map(str::trim) {
        None | Some("") => Ok(None),
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
            Ok(Some(LlmEntryName::from(name)))
        }
    }
}

/// Validate that `model` is one of `entry`'s pinnable models (its default
/// `[model] + model_candidates` that actually built a client). Call after
/// [`validate_llm_pin`] has confirmed the entry. A model outside that set is
/// a `BadRequest` — the client picked something the entry can't serve.
pub(crate) fn validate_llm_model(
    state: &AdminState,
    entry: &LlmEntryName,
    model: &str,
) -> Result<()> {
    let pool = state.llm_pool.read();
    match pool.entry_model_ids(entry) {
        Some(ids) if ids.iter().any(|m| m == model) => Ok(()),
        _ => Err(GatewayError::BadRequest(format!(
            "model {model:?} is not a configured model of LLM entry {entry:?}; \
             see GET /v1/llm/models for its model + model_candidates"
        ))),
    }
}

/// Minimal top-level OpenAPI descriptor. Concrete paths and component
/// schemas are folded in by merging the `OpenApiRouter` output from
/// each submodule.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Baybo Admin API",
        description = "TCP + bearer-token surface: config, jobs, cron, traces, skills, tools, channels, LLM."
    ),
    tags(
        (name = "status", description = "Gateway process status"),
        (name = "config", description = "Read and mutate on-disk BayboConfig"),
        (name = "jobs", description = "Async operation tracking"),
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
        .merge(jobs::routes())
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
        .merge(deck::routes());
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
