//! In-process HTTP surface used by E2E API tunnel forwarding.
//!
//! The tunnel has already authenticated the device through Noise IK. This router
//! deliberately omits bearer middleware and is only called from
//! [`super::api_tunnel`], which allowlists the specific paths a device may reach.

use axum::Router;
use utoipa_axum::router::OpenApiRouter;

use crate::server::AdminState;

use super::WsChannelState;

pub(crate) fn router(admin_state: AdminState, channel_state: WsChannelState) -> Router {
    let (chat_router, _spec) = OpenApiRouter::new()
        .merge(crate::api::admin::chat::routes())
        .split_for_parts();
    let chat_router = Router::new()
        .nest("/v1", chat_router)
        .with_state(admin_state);
    let blob_router = Router::new()
        .nest("/v1", super::blobs::routes())
        .with_state(channel_state);
    Router::new().merge(chat_router).merge(blob_router)
}
