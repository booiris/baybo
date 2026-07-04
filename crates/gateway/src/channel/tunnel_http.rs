//! In-process HTTP surface used by E2E API tunnel forwarding.
//!
//! The tunnel has already authenticated the device through Noise IK. Before
//! dispatch, [`super::api_tunnel`] injects the same device bearer + device-id
//! headers a direct device request would carry, and this router reuses the admin
//! auth middleware to resolve `AuthedClient`.

use axum::Router;
use axum::middleware;

use crate::auth::require_admin_token;

use super::WsChannelState;

pub(crate) fn router(channel_state: WsChannelState) -> Router {
    let tunnel_state = channel_state.tunnel_http.clone();
    let (admin_router, _spec) = crate::api::admin::v1_router_and_spec();
    let admin_router = admin_router.with_state(tunnel_state.admin);
    let blob_router = Router::new()
        .nest("/v1", super::blobs::routes())
        .with_state(channel_state);
    Router::new()
        .merge(admin_router)
        .merge(blob_router)
        .layer(middleware::from_fn_with_state(
            tunnel_state.auth,
            require_admin_token,
        ))
}
