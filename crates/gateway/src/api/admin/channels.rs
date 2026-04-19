//! `/v1/channels` — read-only list of registered channels and their
//! statuses. Lives on the admin listener so operators can see channel
//! registrations; messaging/session routes live on the channel UDS.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use serde::Serialize;
use tokio::sync::RwLock;

use aura_channels::ChannelRegistry;
use aura_model::ChannelType;

use crate::Result;
use crate::api::dto::ListResponse;
use crate::server::AdminState;

pub fn routes() -> Router<AdminState> {
    Router::new().route("/channels", get(list_channels))
}

#[derive(Debug, Serialize)]
pub struct ChannelEntry {
    pub channel_type: ChannelType,
    pub status: String,
}

async fn list_channels(
    State(state): State<AdminState>,
) -> Result<Json<ListResponse<ChannelEntry>>> {
    let items = snapshot(&state.channel_registry).await;
    Ok(Json(ListResponse::new(items)))
}

async fn snapshot(registry: &Arc<RwLock<ChannelRegistry>>) -> Vec<ChannelEntry> {
    let guard = registry.read().await;
    guard
        .list()
        .into_iter()
        .map(|(ct, status)| ChannelEntry {
            channel_type: ct,
            status: format!("{status:?}"),
        })
        .collect()
}
