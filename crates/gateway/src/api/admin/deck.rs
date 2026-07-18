//! `/v1/deck/*` — the deck management + dynamic per-card service surface
//! (docs/modules/deck.md).
//!
//! These management routes are part of the static OpenAPI document. The
//! per-card ops (`POST /v1/deck/services/{card_id}/{op}`) are a
//! deliberately separate dynamic surface addressed by uuid: each card
//! publishes its own `openapi.json`, served below and enforced as the
//! admission contract by the deck manager — per-card ops can never merge
//! into the snapshot-tested static document.

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use baybo_deck::{CardView, DeckError};
use baybo_store::{DeckLayoutEntry, DeckSize};

use crate::api::dto::ErrorBody;
use crate::server::AdminState;
use crate::{GatewayError, Result};

pub fn routes() -> OpenApiRouter<AdminState> {
    OpenApiRouter::new()
        .routes(routes!(get_deck))
        .routes(routes!(put_layout))
        .routes(routes!(get_recycle))
        .routes(routes!(enable_card))
        .routes(routes!(disable_card))
        .routes(routes!(delete_card))
        .routes(routes!(restore_card))
        .routes(routes!(purge_card))
        .routes(routes!(get_bundle))
        .routes(routes!(get_service_openapi))
        .routes(routes!(call_service_op))
}

fn deck_err(e: DeckError) -> GatewayError {
    match e {
        DeckError::NotFound(id) => GatewayError::NotFound(format!("deck card {id}")),
        DeckError::InvalidBundle(_)
        | DeckError::DryRun(_)
        | DeckError::OpRejected(_)
        | DeckError::DeckFull(_)
        | DeckError::ServiceUnavailable(_) => GatewayError::BadRequest(e.to_string()),
        DeckError::Storage(e) => GatewayError::Internal(format!("deck storage: {e}")),
        other => GatewayError::Internal(other.to_string()),
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeckCardDto {
    pub card_id: String,
    pub title: String,
    pub position: i64,
    /// `small` (1×1) | `wide` (2×1) | `large` (2×2).
    pub size: String,
    /// The grid sizes the card implements; the client's ⤢ resize cycle stays
    /// inside this set and a single entry hides the control. Always contains
    /// `size`.
    pub sizes: Vec<String>,
    /// Whether the card declares a maximized (full-screen) layout — the ⛶
    /// affordance in the top-right.
    pub maximize: bool,
    pub enabled: bool,
    pub quarantined: bool,
    /// Unix millis; present only in the recycle listing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at_ms: Option<i64>,
    pub spec_hash: String,
    /// The card's persisted push counter — the client's seq cursor seed.
    pub last_seq: i64,
    pub created_at_ms: i64,
    /// Ops the card declared replay-safe (`x-baybo-retryable: true`); the
    /// phone's relay transport replays a silent-death call only for these.
    pub retryable_ops: Vec<String>,
}

impl From<CardView> for DeckCardDto {
    fn from(c: CardView) -> Self {
        Self {
            card_id: c.id,
            title: c.title,
            position: c.position,
            size: c.size.as_str().to_string(),
            sizes: c.sizes.iter().map(|s| s.as_str().to_string()).collect(),
            maximize: c.maximize,
            enabled: c.enabled,
            quarantined: c.quarantined_at.is_some(),
            deleted_at_ms: c.deleted_at.map(|t| t.timestamp_millis()),
            spec_hash: c.spec_hash,
            last_seq: c.last_seq,
            created_at_ms: c.created_at.timestamp_millis(),
            retryable_ops: c.retryable_ops,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeckSnapshotDto {
    pub card_id: String,
    pub seq: i64,
    /// The card's own JSON text; empty when `error` is present.
    pub payload: String,
    pub fetched_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeckResponse {
    /// Live cards ordered by position.
    pub cards: Vec<DeckCardDto>,
    /// Latest snapshot per card (the paint source).
    pub snapshots: Vec<DeckSnapshotDto>,
}

#[utoipa::path(
    get,
    path = "/deck",
    tag = "deck",
    responses(
        (status = 200, description = "Cards + layout + latest snapshots + seqs", body = DeckResponse),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn get_deck(State(state): State<AdminState>) -> Result<Json<DeckResponse>> {
    let view = state.deck_manager.deck_view().await.map_err(deck_err)?;
    Ok(Json(DeckResponse {
        cards: view.cards.into_iter().map(DeckCardDto::from).collect(),
        snapshots: view
            .snapshots
            .into_iter()
            .map(|s| DeckSnapshotDto {
                card_id: s.card_id,
                seq: s.seq,
                payload: s.payload,
                fetched_at_ms: s.fetched_at.timestamp_millis(),
                error: s.error,
            })
            .collect(),
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct DeckLayoutEntryDto {
    pub card_id: String,
    pub position: i64,
    /// `small` | `wide` | `large`.
    pub size: String,
}

#[utoipa::path(
    put,
    path = "/deck/layout",
    tag = "deck",
    request_body = Vec<DeckLayoutEntryDto>,
    responses(
        (status = 204, description = "Layout applied"),
        (status = 400, description = "Bad entry", body = ErrorBody),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn put_layout(
    State(state): State<AdminState>,
    Json(entries): Json<Vec<DeckLayoutEntryDto>>,
) -> Result<axum::http::StatusCode> {
    let entries: Vec<DeckLayoutEntry> = entries
        .into_iter()
        .map(|e| {
            let size = DeckSize::parse(&e.size).ok_or_else(|| {
                GatewayError::BadRequest(format!("size must be small|wide|large, got `{}`", e.size))
            })?;
            Ok(DeckLayoutEntry {
                id: e.card_id,
                position: e.position,
                size,
            })
        })
        .collect::<Result<_>>()?;
    state
        .deck_manager
        .set_layout(&entries)
        .await
        .map_err(deck_err)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/deck/recycle",
    tag = "deck",
    responses(
        (status = 200, description = "Soft-deleted cards, most recent first", body = Vec<DeckCardDto>),
        (status = 401, description = "Unauthorized", body = ErrorBody),
    )
)]
async fn get_recycle(State(state): State<AdminState>) -> Result<Json<Vec<DeckCardDto>>> {
    let cards = state.deck_manager.recycle_view().await.map_err(deck_err)?;
    Ok(Json(cards.into_iter().map(DeckCardDto::from).collect()))
}

#[utoipa::path(
    post,
    path = "/deck/cards/{card_id}/enable",
    tag = "deck",
    params(("card_id" = String, Path, description = "Card uuid")),
    responses(
        (status = 204, description = "Card re-passed the dry-run gate and its service started"),
        (status = 400, description = "Gate failed; card left quarantined", body = ErrorBody),
        (status = 404, description = "Unknown card", body = ErrorBody),
    )
)]
async fn enable_card(
    State(state): State<AdminState>,
    Path(card_id): Path<String>,
) -> Result<axum::http::StatusCode> {
    state
        .deck_manager
        .enable(&card_id)
        .await
        .map_err(deck_err)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/deck/cards/{card_id}/disable",
    tag = "deck",
    params(("card_id" = String, Path, description = "Card uuid")),
    responses(
        (status = 204, description = "Service stopped"),
        (status = 404, description = "Unknown card", body = ErrorBody),
    )
)]
async fn disable_card(
    State(state): State<AdminState>,
    Path(card_id): Path<String>,
) -> Result<axum::http::StatusCode> {
    state
        .deck_manager
        .disable(&card_id)
        .await
        .map_err(deck_err)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/deck/cards/{card_id}",
    tag = "deck",
    params(("card_id" = String, Path, description = "Card uuid")),
    responses(
        (status = 204, description = "Soft-deleted into the recycle bin"),
        (status = 404, description = "Unknown card", body = ErrorBody),
    )
)]
async fn delete_card(
    State(state): State<AdminState>,
    Path(card_id): Path<String>,
) -> Result<axum::http::StatusCode> {
    state
        .deck_manager
        .soft_delete(&card_id)
        .await
        .map_err(deck_err)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/deck/cards/{card_id}/restore",
    tag = "deck",
    params(("card_id" = String, Path, description = "Card uuid")),
    responses(
        (status = 200, description = "Card re-passed the gate and left the bin", body = DeckCardDto),
        (status = 400, description = "Gate failed; card stays in the bin", body = ErrorBody),
        (status = 404, description = "Unknown card", body = ErrorBody),
    )
)]
async fn restore_card(
    State(state): State<AdminState>,
    Path(card_id): Path<String>,
) -> Result<Json<DeckCardDto>> {
    let card = state
        .deck_manager
        .restore(&card_id)
        .await
        .map_err(deck_err)?;
    Ok(Json(card.into()))
}

#[utoipa::path(
    post,
    path = "/deck/cards/{card_id}/purge",
    tag = "deck",
    params(("card_id" = String, Path, description = "Card uuid")),
    responses(
        (status = 204, description = "Row, snapshots, and bundle files removed"),
        (status = 400, description = "Card is not in the recycle bin", body = ErrorBody),
        (status = 404, description = "Unknown card", body = ErrorBody),
    )
)]
async fn purge_card(
    State(state): State<AdminState>,
    Path(card_id): Path<String>,
) -> Result<axum::http::StatusCode> {
    state.deck_manager.purge(&card_id).await.map_err(deck_err)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize, ToSchema)]
pub struct DeckBundleDto {
    /// The card's frontend, rendered by the deck shell in a sandboxed
    /// iframe. Reaches clients via their transport (never a direct URL).
    pub card_html: String,
}

#[utoipa::path(
    get,
    path = "/deck/cards/{card_id}/bundle",
    tag = "deck",
    params(("card_id" = String, Path, description = "Card uuid")),
    responses(
        (status = 200, description = "The card's frontend", body = DeckBundleDto),
        (status = 404, description = "Unknown card", body = ErrorBody),
    )
)]
async fn get_bundle(
    State(state): State<AdminState>,
    Path(card_id): Path<String>,
) -> Result<Json<DeckBundleDto>> {
    let card_html = state
        .deck_manager
        .card_html(&card_id)
        .await
        .map_err(deck_err)?;
    Ok(Json(DeckBundleDto { card_html }))
}

#[utoipa::path(
    get,
    path = "/deck/services/{card_id}/openapi.json",
    tag = "deck",
    params(("card_id" = String, Path, description = "Card uuid")),
    responses(
        (status = 200, description = "The card's op contract (its own OpenAPI document)"),
        (status = 404, description = "Unknown card", body = ErrorBody),
    )
)]
async fn get_service_openapi(
    State(state): State<AdminState>,
    Path(card_id): Path<String>,
) -> Result<Json<Value>> {
    let doc = state
        .deck_manager
        .openapi_json(&card_id)
        .await
        .map_err(deck_err)?;
    Ok(Json(doc))
}

#[utoipa::path(
    post,
    path = "/deck/services/{card_id}/{op}",
    tag = "deck",
    params(
        ("card_id" = String, Path, description = "Card uuid"),
        ("op" = String, Path, description = "Op name declared in the card's openapi.json"),
    ),
    request_body = Value,
    responses(
        (status = 200, description = "The op's JSON result"),
        (status = 400, description = "Off-schema call (rejected before reaching card code)", body = ErrorBody),
        (status = 404, description = "Unknown card", body = ErrorBody),
    )
)]
async fn call_service_op(
    State(state): State<AdminState>,
    Path((card_id, op)): Path<(String, String)>,
    Json(params): Json<Value>,
) -> Result<Json<Value>> {
    let value = state
        .deck_manager
        .call_op(&card_id, &op, params)
        .await
        .map_err(deck_err)?;
    Ok(Json(value))
}
