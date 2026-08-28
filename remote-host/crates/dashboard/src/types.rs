//! Dashboard wire DTOs and the crate-local error type.
//!
//! Every type here is serde-only: counts are `u64`/`usize`, byte and frame
//! totals never use floating point, and no payload bytes or APNs token secrets
//! cross this boundary. The backend masks keys/devices before populating them.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

/// Time window selector shared by all three traffic getters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Range {
    #[default]
    #[serde(rename = "24h")]
    H24,
    #[serde(rename = "7d")]
    D7,
    #[serde(rename = "30d")]
    D30,
    #[serde(rename = "60d")]
    D60,
}

impl Range {
    pub fn hours(self) -> u32 {
        match self {
            Self::H24 => 24,
            Self::D7 => 168,
            Self::D30 => 720,
            Self::D60 => 1440,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct RangeQuery {
    #[serde(default)]
    pub range: Range,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BucketUnit {
    Hour,
    Day,
}

// --- Overview (Page 1) ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverviewSnapshot {
    pub keys_admitted: usize,
    pub devices_bound: usize,
    pub push_enabled: bool,
    pub gateways_connected: usize,
    pub relay_legs_pending: usize,
    pub relay_conns_live: usize,
    pub relay_keys_tracked: usize,
    pub ip_entries_tracked: usize,
    pub build_version: String,
    pub started_at: String,
    pub uptime_secs: u64,
    pub server_time: String,
}

// --- Keys (Page 2). NO secret in this payload. ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyRow {
    pub id: i64,
    pub label: Option<String>,
    pub key_last4: String,
    pub max_conns: Option<u32>,
    pub max_bps: Option<u64>,
    pub per_server_max_bps: Option<u64>,
    pub expires_at: Option<String>,
    pub created_at: String,
    pub conns_live: usize,
    pub expired: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevealedKey {
    pub remote_api_key: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdmitKeyRequest {
    pub key: Option<String>,
    pub label: Option<String>,
    pub max_conns: Option<u32>,
    pub max_bps: Option<u64>,
    pub per_server_max_bps: Option<u64>,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdmitKeyOutcome {
    pub id: i64,
    pub remote_api_key: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EditKeyRequest {
    pub label: Option<String>,
    pub max_conns: Option<u32>,
    pub max_bps: Option<u64>,
    pub per_server_max_bps: Option<u64>,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KickOutcome {
    pub kicked: usize,
}

// --- Traffic (Page 3); top-N masked to last4, never the secret ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayTrafficSeries {
    pub range: Range,
    pub bucket: BucketUnit,
    pub buckets: Vec<RelayBucket>,
    pub by_key: Vec<RelayKeyTotal>,
    pub totals: RelayTotals,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayBucket {
    pub t: String,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub frames_up: u64,
    pub frames_down: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayKeyTotal {
    pub key_last4: String,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub frames_up: u64,
    pub frames_down: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RelayTotals {
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub frames_up: u64,
    pub frames_down: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushTrafficSeries {
    pub range: Range,
    pub bucket: BucketUnit,
    pub buckets: Vec<PushBucket>,
    pub by_device: Vec<PushDeviceTotal>,
    pub totals: PushTotals,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushBucket {
    pub t: String,
    pub sends: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushDeviceTotal {
    pub device_id: String,
    pub sends: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PushTotals {
    pub sends: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpTrafficSeries {
    pub range: Range,
    pub bucket: BucketUnit,
    pub buckets: Vec<IpBucket>,
    pub by_endpoint: Vec<IpEndpointTotal>,
    pub by_ip: Vec<IpTotal>,
    pub totals: IpTotals,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpBucket {
    pub t: String,
    pub requests: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpEndpointTotal {
    pub endpoint: String,
    pub requests: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpTotal {
    pub ip: String,
    pub requests: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct IpTotals {
    pub requests: u64,
    pub bytes: u64,
}

/// One source IP's per-endpoint breakdown over a window — the drill-down behind
/// clicking a row on the dashboard's IPs page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpEndpointBreakdown {
    pub ip: String,
    pub endpoints: Vec<IpEndpointTotal>,
    pub totals: IpTotals,
}

// --- Devices (Page 4, read-only) ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRow {
    pub device_id: String,
    pub provider: String,
    pub environment: Option<String>,
    pub token_masked: String,
    pub gateway_pubkey_hex: String,
    pub last_counter: u64,
    pub sends_total: u64,
    pub bytes_total: u64,
}

/// The one error the [`DashboardBackend`](crate::DashboardBackend) trait
/// surfaces; each arm maps to an HTTP status via [`IntoResponse`].
#[derive(Debug, thiserror::Error)]
pub enum DashboardError {
    #[error("not found")]
    NotFound,
    #[error("invalid request: {0}")]
    BadRequest(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("backend failure: {0}")]
    Backend(String),
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for DashboardError {
    fn into_response(self) -> Response {
        let status = match &self {
            DashboardError::NotFound => StatusCode::NOT_FOUND,
            DashboardError::BadRequest(_) => StatusCode::BAD_REQUEST,
            DashboardError::Conflict(_) => StatusCode::CONFLICT,
            DashboardError::Backend(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let body = ErrorBody {
            error: self.to_string(),
        };
        (status, Json(body)).into_response()
    }
}
