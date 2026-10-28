//! `/v1/skills` — list the skills one agent can actually invoke.

use axum::Json;
use axum::extract::{Query, State};
use baybo_skills::UNIVERSAL_SKILLS;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::Result;
use crate::api::dto::{ErrorBody, ListResponse};
use crate::server::AdminState;

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new().routes(routes!(list_skills))
}

/// One registered skill: its invocation name and a short blurb of what it
/// does (the `SKILL.md` `description`).
#[derive(Debug, Serialize, ToSchema)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    /// True for a skill every agent has regardless of persona (runtime
    /// infrastructure, not a granted capability). Lets a client show what a
    /// not-yet-created agent would start with, without hard-coding the list.
    pub universal: bool,
}

/// Query for [`list_skills`].
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct ListSkillsQuery {
    /// Whose scope to list. Absent = the default scope, which is the
    /// built-in agent's. Any other id lists that agent's own directory plus
    /// the universal skills — no agent inherits another's.
    ///
    /// The id need not name an existing profile: this asks what a scope can
    /// invoke, not whether a row exists, so a client can preview what a
    /// freshly-created agent would get.
    #[serde(default)]
    pub agent_id: Option<String>,
}

#[utoipa::path(
    get,
    path = "/skills",
    tag = "skills",
    params(("agent_id" = Option<String>, Query, description = "List this agent's scope instead of the default one")),
    responses(
        (status = 200, description = "Skills this scope can invoke", body = inline(ListResponse<SkillInfo>)),
        (status = 400, description = "Malformed agent id", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_skills(
    State(state): State<AdminState>,
    Query(query): Query<ListSkillsQuery>,
) -> Result<Json<ListResponse<SkillInfo>>> {
    let agent = query
        .agent_id
        .as_deref()
        .map(crate::api::admin::agents::parse_agent_id)
        .transpose()?;
    if let Some(agent) = agent.as_ref() {
        state
            .skill_registry
            .ensure_agent_skills(agent, &state.workspace_paths);
    }
    let items = state
        .skill_registry
        .summaries_for(agent.as_ref())
        .into_iter()
        .map(|s| SkillInfo {
            universal: UNIVERSAL_SKILLS.contains(&s.name.as_str()),
            name: s.name,
            description: s.description,
        })
        .collect();
    Ok(Json(ListResponse::new(items)))
}
