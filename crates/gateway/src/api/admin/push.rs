//! `/v1/push/*` — admin-side direct-mode device push registration.
//!
//! Scan-to-pair devices bootstrap their push binding over the Noise pairing
//! handshake. The **direct** transport (URL + admin token, no pairing) has no
//! handshake, so a web client bootstraps the same binding over this
//! admin-token-authenticated REST surface instead:
//!
//! 1. `GET /v1/push/params` → the gateway's Ed25519 push verifying key (so the
//!    client can sign a delegation over it) and whether push is configured.
//! 2. `POST /v1/push/register` → the client's Ed25519 public key, APNs token, a
//!    client-generated preview `push_key`, and the delegation. The gateway
//!    verifies the delegation and persists the binding ([`crate::push::web`]).
//!
//! The resulting binding is cryptographically identical to a paired device's, so
//! the dispatcher, the remote host (C), and the APNs NSE path are unchanged. See
//! `docs/modules/mobile/relay-push-security.md` for the (weaker, TLS-bearer)
//! trust model versus the Noise path.

use axum::extract::State;
use axum::http::StatusCode;
use axum::{Extension, Json};
use device_proto::aead;
use device_proto::delegation;
use device_proto::pairing::ApnsEnv;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::dto::ErrorBody;
use crate::auth::AuthedClient;
use crate::push::{DEFAULT_PUSH_RELAY_URL, load_or_create_push_signing_key, web};
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(push_params))
        .routes(routes!(register_push))
        .routes(routes!(update_device_apns_token))
}

/// Response of `GET /v1/push/params`.
#[derive(Debug, Serialize, ToSchema)]
pub struct PushParams {
    /// Lowercase hex of the gateway's 32-byte Ed25519 push verifying key. The
    /// client signs a delegation over this key (authorizing it to manage the
    /// binding at C) before `POST /v1/push/register`.
    pub gateway_push_pubkey: String,
}

#[utoipa::path(
    get,
    path = "/push/params",
    tag = "push",
    responses(
        (status = 200, description = "Gateway push key + whether direct-mode push is configured", body = PushParams),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn push_params(State(state): State<AdminState>) -> Result<Json<PushParams>> {
    let signing_key = load_or_create_push_signing_key(&state.secret_vault)
        .await
        .map_err(|e| GatewayError::Internal(format!("load push signing key: {e}")))?;
    Ok(Json(PushParams {
        gateway_push_pubkey: hex::encode(signing_key.verifying_key().to_bytes()),
    }))
}

/// Request body for `POST /v1/push/register`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct RegisterPushRequest {
    /// The client's self-certifying `device_id` (`device-<hex(ed25519 pub)>`); the
    /// gateway recovers the public key from it and re-derives the canonical id.
    pub device_id: String,
    /// The client's current APNs device token (hex).
    pub apns_token: String,
    /// APNs environment: `"sandbox"` (development-signed) or `"production"`
    /// (distribution-signed).
    pub apns_env: String,
    /// Lowercase hex of the 32-byte preview AEAD key the client also stored in
    /// its App-Group keychain (so its NSE can decrypt). Generated client-side and
    /// delivered over this TLS + admin-token channel.
    pub push_key: String,
    /// Lowercase hex of the 64-byte Ed25519 delegation: the client's device key
    /// authorizing the gateway push key returned by `GET /v1/push/params`.
    pub delegation: String,
}

/// Response of `POST /v1/push/register`.
#[derive(Debug, Serialize, ToSchema)]
pub struct RegisterPushResponse {
    /// The `device-<hex(pub)>` id the binding was stored under (== the push `bid`).
    pub device_id: String,
}

/// Request body for `POST /v1/mobile/apns-token`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateDeviceApnsTokenRequest {
    /// The device's current APNs device token (hex).
    pub apns_token: String,
    /// APNs environment: `"sandbox"` (development-signed) or `"production"`
    /// (distribution-signed).
    pub apns_env: String,
}

/// Refuse a `POST /v1/push/register` with a 400, leaving a gateway-side record:
/// the app only shows a generic error, so this warn is the operator's sole
/// evidence the register reached the gateway and why it was refused. Never log
/// the delegation / push_key / apns_token values themselves.
fn reject_register(device_id: &str, reason: &str) -> GatewayError {
    tracing::warn!(device = %device_id, reason = %reason, "push: direct-mode register rejected");
    GatewayError::BadRequest(reason.to_string())
}

#[utoipa::path(
    post,
    path = "/push/register",
    tag = "push",
    request_body = RegisterPushRequest,
    responses(
        (status = 200, description = "Direct-mode push binding registered", body = RegisterPushResponse),
        (status = 400, description = "Malformed key / delegation that does not verify", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn register_push(
    State(state): State<AdminState>,
    Json(req): Json<RegisterPushRequest>,
) -> Result<Json<RegisterPushResponse>> {
    // Recover the client's Ed25519 public key from its self-certifying device_id
    // (validates prefix / hex / length / point), then re-derive the canonical id.
    let device_pub = delegation::device_pubkey_from_id(req.device_id.trim()).map_err(|_| {
        reject_register(
            &remote_host_protocol::device_id_log(req.device_id.trim()),
            "device_id is not a valid device-<hex> identity",
        )
    })?;
    let device_id = delegation::device_id_for(&device_pub);

    // Verify the delegation authorizes THIS gateway's push key — push is keyless,
    // so the delegation chain is the SOLE authorization C verifies, and a binding
    // without it can't prove ownership to C at all.
    let signing_key = load_or_create_push_signing_key(&state.secret_vault)
        .await
        .map_err(|e| GatewayError::Internal(format!("load push signing key: {e}")))?;
    let deleg_bytes = hex::decode(req.delegation.trim())
        .map_err(|_| reject_register(&device_id, "delegation is not valid hex"))?;
    let deleg_sig = delegation::signature_from_bytes(&deleg_bytes)
        .map_err(|_| reject_register(&device_id, "delegation is not a 64-byte signature"))?;
    if !delegation::verify_delegation(&device_pub, &signing_key.verifying_key(), &deleg_sig) {
        return Err(reject_register(
            &device_id,
            "delegation does not authorize this gateway's push key",
        ));
    }
    let deleg_arr: [u8; delegation::SIGNATURE_LEN] = deleg_bytes
        .as_slice()
        .try_into()
        .map_err(|_| reject_register(&device_id, "delegation is not a 64-byte signature"))?;

    // Decode the preview AEAD key + APNs env.
    let push_key_bytes = hex::decode(req.push_key.trim())
        .map_err(|_| reject_register(&device_id, "push_key is not valid hex"))?;
    let push_key: [u8; aead::KEY_LEN] = push_key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| reject_register(&device_id, "push_key must be 32 bytes"))?;
    let apns_env = match req.apns_env.trim() {
        "production" => ApnsEnv::Production,
        // Default unknown/empty to sandbox — the conservative pick (a sandbox
        // token sent to the production host just fails to deliver).
        _ => ApnsEnv::Sandbox,
    };

    let binding = web::WebPushBinding {
        device_id: device_id.clone(),
        relay_url: DEFAULT_PUSH_RELAY_URL.to_string(),
        created_at: chrono::Utc::now().timestamp(),
    };
    web::store_binding(
        &state.secret_vault,
        &binding,
        &push_key,
        req.apns_token.trim(),
        apns_env,
        &deleg_arr,
    )
    .await
    .map_err(|e| GatewayError::Internal(format!("store push binding: {e}")))?;

    tracing::info!(device = %device_id, "push: registered a direct-mode binding");
    Ok(Json(RegisterPushResponse { device_id }))
}

#[utoipa::path(
    post,
    path = "/mobile/apns-token",
    tag = "push",
    request_body = UpdateDeviceApnsTokenRequest,
    responses(
        (status = 204, description = "Paired device APNs token refreshed"),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Persist failed", body = ErrorBody),
    )
)]
async fn update_device_apns_token(
    State(state): State<AdminState>,
    authed: Option<Extension<AuthedClient>>,
    Json(req): Json<UpdateDeviceApnsTokenRequest>,
) -> Result<StatusCode> {
    let Some(Extension(AuthedClient::Device { device_id })) = authed else {
        return Err(GatewayError::Unauthorized);
    };

    let apns_token = req.apns_token.trim();
    if apns_token.is_empty() {
        tracing::debug!(
            device = %device_id,
            "push: device sent empty APNs token; registration unchanged"
        );
        return Ok(StatusCode::NO_CONTENT);
    }
    let apns_env = match req.apns_env.trim() {
        "production" => ApnsEnv::Production,
        _ => ApnsEnv::Sandbox,
    };
    let reg = crate::push::DeviceApnsRegistration {
        apns_token: apns_token.to_owned(),
        apns_env,
    };
    // The app re-posts its token on every launch/foreground; skip the vault
    // rewrite (and the INFO) when nothing changed. Any doubt — no stored
    // entry, unreadable vault, undecodable bytes — falls open to the write:
    // rewriting is also the recovery path for a malformed stored registration.
    let secret_name = crate::push::device_apns_secret_name(&device_id);
    let stored = state
        .secret_vault
        .get_secret(&secret_name)
        .await
        .ok()
        .flatten()
        .and_then(|s| {
            serde_json::from_slice::<crate::push::DeviceApnsRegistration>(s.as_bytes()).ok()
        });
    if stored.as_ref() == Some(&reg) {
        tracing::debug!(
            device = %device_id,
            "push: device re-posted an unchanged APNs token; registration untouched"
        );
        return Ok(StatusCode::NO_CONTENT);
    }
    let bytes = serde_json::to_vec(&reg)
        .map_err(|e| GatewayError::Internal(format!("encode APNs registration: {e}")))?;
    state
        .secret_vault
        .store_secret(&secret_name, &bytes)
        .await
        .map_err(|e| GatewayError::Internal(format!("persist APNs registration: {e}")))?;

    tracing::info!(
        device = %device_id,
        apns_env = ?apns_env,
        token_len = apns_token.len(),
        "push: device APNs token updated via API"
    );
    Ok(StatusCode::NO_CONTENT)
}
