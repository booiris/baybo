//! Admin surface (TCP + bearer token). Hosts config/status/jobs/cron/
//! memory/traces/skills/tools/llm and a read-only channel list. No chat
//! content flows through these endpoints.

pub mod channels;
pub mod config;
pub mod cron;
pub mod jobs;
pub mod llm;
pub mod logs;
pub mod memory;
pub mod sessions;
pub mod skills;
pub mod status;
pub mod tools;
pub mod traces;

use axum::Router;
use utoipa::OpenApi;
use utoipa::openapi::OpenApi as OpenApiDoc;
use utoipa_axum::router::OpenApiRouter;

use crate::api::openapi;
use crate::server::AdminState;

/// Minimal top-level OpenAPI descriptor. Concrete paths and component
/// schemas are folded in by merging the `OpenApiRouter` output from
/// each submodule.
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Aura Admin API",
        description = "TCP + bearer-token surface: config, jobs, cron, memory, traces, skills, tools, channels, LLM."
    ),
    tags(
        (name = "status", description = "Gateway process status"),
        (name = "config", description = "Read and mutate on-disk AuraConfig"),
        (name = "sessions", description = "Conversation sessions (list, detail, fork)"),
        (name = "jobs", description = "Async operation tracking"),
        (name = "cron", description = "Scheduled prompts / tool calls"),
        (name = "memory", description = "Long-term memory entries"),
        (name = "traces", description = "Per-session trace export"),
        (name = "skills", description = "Registered skills"),
        (name = "tools", description = "Registered tool manifests"),
        (name = "channels", description = "Registered channel plugins"),
        (name = "llm", description = "Configured LLM provider"),
        (name = "logs", description = "Recent tracing events (in-memory ring buffer)"),
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
        .merge(sessions::routes())
        .merge(jobs::routes())
        .merge(cron::routes())
        .merge(memory::routes())
        .merge(traces::routes())
        .merge(skills::routes())
        .merge(tools::routes())
        .merge(channels::routes())
        .merge(llm::routes())
        .merge(logs::routes());
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
