//! `/v1/channels` — read-only list of registered channels and their
//! statuses. Lives on the admin listener so operators can see channel
//! registrations; messaging/session routes live on the channel UDS.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use aura_channels::ChannelRegistry;

use crate::Result;
use crate::api::dto::{ChannelEntry, ErrorBody, ListResponse};
use crate::server::AdminState;

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new().routes(routes!(list_channels))
}

#[utoipa::path(
    get,
    path = "/channels",
    tag = "channels",
    responses(
        (status = 200, description = "Registered channels", body = inline(ListResponse<ChannelEntry>)),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn list_channels(
    State(state): State<AdminState>,
) -> Result<Json<ListResponse<ChannelEntry>>> {
    let items = snapshot(&state.channel_registry);
    Ok(Json(ListResponse::new(items)))
}

fn snapshot(registry: &Arc<ChannelRegistry>) -> Vec<ChannelEntry> {
    registry
        .list()
        .into_iter()
        .map(|ct| ChannelEntry {
            channel_type: ct.into(),
            status: "running".to_string(),
        })
        .collect()
}
