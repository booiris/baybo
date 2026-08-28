//! `/v1/push/*` — admin-side direct-mode device push registration.
//!
//! Scan-to-pair devices bootstrap their push identity/key/delegation over the
//! Noise pairing handshake, then post their provider target through the device
//! API. The **direct** transport (URL + admin token, no pairing) bootstraps the same binding over this
//! admin-token-authenticated REST surface instead:
//!
//! 1. `GET /v1/push/params` → the gateway's Ed25519 push verifying key (so the
//!    client can sign a delegation over it) and whether push is configured.
//! 2. `POST /v1/push/register` → the client's Ed25519 public key, push target, a
//!    client-generated preview `push_key`, and the delegation. The gateway
//!    verifies the delegation and persists the binding ([`crate::push::web`]).
//!
//! The resulting binding is cryptographically identical to a paired device's, so
//! the dispatcher, the remote host (C), and platform decrypt paths are unchanged. See
//! `docs/modules/mobile/relay-push-security.md` for the (weaker, TLS-bearer)
//! trust model versus the Noise path.

use axum::extract::State;
use axum::http::StatusCode;
use axum::{Extension, Json};
use device_proto::aead;
use device_proto::delegation;
use remote_host_protocol::push::{ApnsEnvironment, PushTarget};
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
        .routes(routes!(update_device_push_token))
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
    /// The client's current provider-tagged token.
    pub target: PushTargetRequest,
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

#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApnsEnvironmentRequest {
    Sandbox,
    Production,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum PushTargetRequest {
    Apns {
        token: String,
        environment: ApnsEnvironmentRequest,
    },
    Fcm {
        token: String,
    },
}

impl PushTargetRequest {
    fn into_target(self) -> Option<PushTarget> {
        fn valid_token(token: String) -> Option<String> {
            let token = token.trim().to_string();
            (!token.is_empty() && token.len() <= remote_host_protocol::push::PUSH_TOKEN_MAX_LEN)
                .then_some(token)
        }

        match self {
            Self::Apns { token, environment } => valid_token(token).map(|token| PushTarget::Apns {
                token,
                environment: match environment {
                    ApnsEnvironmentRequest::Sandbox => ApnsEnvironment::Sandbox,
                    ApnsEnvironmentRequest::Production => ApnsEnvironment::Production,
                },
            }),
            Self::Fcm { token } => valid_token(token).map(|token| PushTarget::Fcm { token }),
        }
    }
}

/// Request body for `POST /v1/mobile/push-token`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateDevicePushTokenRequest {
    pub target: PushTargetRequest,
}

/// Refuse a `POST /v1/push/register` with a 400, leaving a gateway-side record:
/// the app only shows a generic error, so this warn is the operator's sole
/// evidence the register reached the gateway and why it was refused. Never log
/// the delegation, push key, or provider token values themselves.
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

    // Verify the delegation authorizes THIS gateway's push key. The remote API
    // key marks admitted traffic but does not prove device ownership, so a
    // binding without this delegation cannot prove ownership to C.
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

    // Decode the preview AEAD key and validate the provider target.
    let push_key_bytes = hex::decode(req.push_key.trim())
        .map_err(|_| reject_register(&device_id, "push_key is not valid hex"))?;
    let push_key: [u8; aead::KEY_LEN] = push_key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| reject_register(&device_id, "push_key must be 32 bytes"))?;
    let target = req
        .target
        .into_target()
        .ok_or_else(|| reject_register(&device_id, "push token has invalid length"))?;

    let binding = web::WebPushBinding {
        device_id: device_id.clone(),
        relay_url: DEFAULT_PUSH_RELAY_URL.to_string(),
        remote_api_key: remote_host_protocol::DEFAULT_REMOTE_API_KEY.to_string(),
        created_at: chrono::Utc::now().timestamp(),
    };
    web::store_binding(
        &state.secret_vault,
        &binding,
        &push_key,
        &target,
        &deleg_arr,
    )
    .await
    .map_err(|e| GatewayError::Internal(format!("store push binding: {e}")))?;

    tracing::info!(device = %device_id, "push: registered a direct-mode binding");
    Ok(Json(RegisterPushResponse { device_id }))
}

#[utoipa::path(
    post,
    path = "/mobile/push-token",
    tag = "push",
    request_body = UpdateDevicePushTokenRequest,
    responses(
        (status = 204, description = "Paired device push token refreshed"),
        (status = 400, description = "Provider token has invalid length", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
        (status = 500, description = "Persist failed", body = ErrorBody),
    )
)]
async fn update_device_push_token(
    State(state): State<AdminState>,
    authed: Option<Extension<AuthedClient>>,
    Json(req): Json<UpdateDevicePushTokenRequest>,
) -> Result<StatusCode> {
    let Some(Extension(AuthedClient::Device { device_id })) = authed else {
        return Err(GatewayError::Unauthorized);
    };

    let target = req
        .target
        .into_target()
        .ok_or_else(|| GatewayError::BadRequest("push token has invalid length".to_string()))?;
    let reg = crate::push::DevicePushRegistration {
        target: target.clone(),
    };
    // The app re-posts its target on every launch/foreground; skip the vault
    // rewrite (and the INFO) when nothing changed. Any doubt — no stored
    // entry, unreadable vault, undecodable bytes — falls open to the write:
    // rewriting is also the recovery path for a malformed stored registration.
    let secret_name = crate::push::device_push_registration_secret_name(&device_id);
    let stored = state
        .secret_vault
        .get_secret(&secret_name)
        .await
        .ok()
        .flatten()
        .and_then(|s| {
            serde_json::from_slice::<crate::push::DevicePushRegistration>(s.as_bytes()).ok()
        });
    if stored.as_ref() == Some(&reg) {
        tracing::debug!(
            device = %device_id,
            "push: device re-posted an unchanged target; registration untouched"
        );
        return Ok(StatusCode::NO_CONTENT);
    }
    let bytes = serde_json::to_vec(&reg)
        .map_err(|e| GatewayError::Internal(format!("encode push registration: {e}")))?;
    state
        .secret_vault
        .store_secret(&secret_name, &bytes)
        .await
        .map_err(|e| GatewayError::Internal(format!("persist push registration: {e}")))?;

    tracing::info!(
        device = %device_id,
        provider = target.provider().as_str(),
        token_len = target.token().len(),
        "push: device target updated via API"
    );
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_request_preserves_provider_specific_metadata() {
        assert_eq!(
            PushTargetRequest::Apns {
                token: " token ".into(),
                environment: ApnsEnvironmentRequest::Production,
            }
            .into_target(),
            Some(PushTarget::Apns {
                token: "token".into(),
                environment: ApnsEnvironment::Production,
            })
        );
        assert_eq!(
            PushTargetRequest::Fcm {
                token: " fcm-token ".into(),
            }
            .into_target(),
            Some(PushTarget::Fcm {
                token: "fcm-token".into(),
            })
        );
    }

    #[test]
    fn target_request_rejects_empty_and_oversized_tokens() {
        assert!(
            PushTargetRequest::Fcm { token: "  ".into() }
                .into_target()
                .is_none()
        );
        assert!(
            PushTargetRequest::Fcm {
                token: "x".repeat(remote_host_protocol::push::PUSH_TOKEN_MAX_LEN + 1),
            }
            .into_target()
            .is_none()
        );
    }
}
