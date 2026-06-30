//! The runtime [`DashboardBackend`] implementation.
//!
//! This is the only place the operator dashboard's typed transport calls are
//! resolved against the live components: the admission write path
//! ([`crate::admission_db`]), the relay connection / control / broker / traffic
//! registries, the per-IP traffic registry, the read-only traffic ledger
//! ([`crate::traffic_query`]), and (when push is on) the device binding store.
//!
//! Secrets never reach the wire: a `remote_api_key` is masked to its last four
//! characters before any [`KeyRow`] / [`RelayKeyTotal`] is serialized, the APNs
//! token is masked inside the push crate, and every mutation logs only the
//! masked key. Stat reads degrade to empty/zeroed values rather than 500 when
//! the ledger is absent or a query fails.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use remote_host_admission::Tier;
use remote_host_dashboard::{
    AdmitKeyOutcome, AdmitKeyRequest, BucketUnit, DashboardBackend, DashboardError, DeviceRow,
    EditKeyRequest, IpBucket, IpEndpointTotal, IpTotal, IpTotals, IpTrafficSeries, KeyRow, KeyTier,
    KickOutcome, OverviewSnapshot, PushBucket, PushDeviceTotal, PushTotals, PushTrafficSeries,
    Range, RelayBucket, RelayKeyTotal, RelayTotals, RelayTrafficSeries, RevealedKey,
};
use remote_host_push::{ApnsEnv, DeviceTokenStore};

use crate::admission_db::{
    AdmissionDb, AdmissionDbError, KeyLimits, NewKey, generate_remote_api_key,
};
use crate::traffic_query::{TrafficQueryError, TrafficReader};

/// Trailing characters of a `remote_api_key` retained when masking it for the
/// wire (`key_last4`) or an audit line.
const KEY_MASK_TAIL: usize = 4;
/// Top-N cap for every traffic breakdown (matches the frontend's 10 + "more").
const TRAFFIC_TOP_N: u32 = 10;

/// Required dependencies for [`RuntimeDashboardBackend`], populated by struct
/// literal at the one server call site (required-deps-in-config rule).
pub(crate) struct DashboardBackendConfig {
    pub admission_db: Arc<AdmissionDb>,
    pub traffic_reader: Option<TrafficReader>,
    pub conns: Arc<remote_host_relay::ConnectionRegistry>,
    pub control: Arc<remote_host_relay::ControlRegistry>,
    pub broker: Arc<remote_host_relay::RelayBroker>,
    pub relay_traffic: Arc<remote_host_relay::TrafficRegistry>,
    pub ip_traffic: Arc<remote_host_edge::IpTrafficRegistry>,
    pub devices: Option<Arc<dyn DeviceTokenStore>>,
    pub push_enabled: bool,
    pub started_at: std::time::Instant,
    pub build_version: &'static str,
}

/// The live read + control backend the dashboard router proxies to.
pub(crate) struct RuntimeDashboardBackend {
    admission_db: Arc<AdmissionDb>,
    traffic_reader: Option<TrafficReader>,
    conns: Arc<remote_host_relay::ConnectionRegistry>,
    control: Arc<remote_host_relay::ControlRegistry>,
    broker: Arc<remote_host_relay::RelayBroker>,
    relay_traffic: Arc<remote_host_relay::TrafficRegistry>,
    ip_traffic: Arc<remote_host_edge::IpTrafficRegistry>,
    devices: Option<Arc<dyn DeviceTokenStore>>,
    push_enabled: bool,
    started_at: std::time::Instant,
    build_version: &'static str,
}

impl RuntimeDashboardBackend {
    pub(crate) fn from_config(config: DashboardBackendConfig) -> Self {
        let DashboardBackendConfig {
            admission_db,
            traffic_reader,
            conns,
            control,
            broker,
            relay_traffic,
            ip_traffic,
            devices,
            push_enabled,
            started_at,
            build_version,
        } = config;
        Self {
            admission_db,
            traffic_reader,
            conns,
            control,
            broker,
            relay_traffic,
            ip_traffic,
            devices,
            push_enabled,
            started_at,
            build_version,
        }
    }

    /// Resolve a row's `id` (SQLite rowid) to its full secret, or [`NotFound`].
    ///
    /// [`NotFound`]: DashboardError::NotFound
    async fn resolve_key(&self, id: i64) -> Result<String, DashboardError> {
        self.admission_db
            .reveal_key(id)
            .await
            .map_err(db_err)?
            .ok_or(DashboardError::NotFound)
    }
}

#[async_trait]
impl DashboardBackend for RuntimeDashboardBackend {
    async fn overview(&self) -> Result<OverviewSnapshot, DashboardError> {
        let uptime_secs = self.started_at.elapsed().as_secs();
        let now = chrono::Utc::now();
        let started =
            now - chrono::Duration::seconds(i64::try_from(uptime_secs).unwrap_or(i64::MAX));
        Ok(OverviewSnapshot {
            keys_admitted: self.admission_db.admission().len(),
            devices_bound: self.devices.as_ref().map_or(0, |d| d.len()),
            push_enabled: self.push_enabled,
            gateways_connected: self.control.connected(),
            relay_legs_pending: self.broker.pending_len(),
            relay_conns_live: self.conns.live_total(),
            relay_keys_tracked: self.relay_traffic.tracked_len(),
            ip_entries_tracked: self.ip_traffic.tracked_len(),
            build_version: self.build_version.to_string(),
            started_at: started.to_rfc3339(),
            uptime_secs,
            server_time: now.to_rfc3339(),
        })
    }

    async fn list_keys(&self) -> Result<Vec<KeyRow>, DashboardError> {
        let records = self.admission_db.list_keys().await.map_err(db_err)?;
        let live = self.conns.live_counts();
        // SQLite's `datetime('now')` shape; lexicographically comparable, so an
        // `expires_at < now` check needs no parsing.
        let now_sql = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        Ok(records
            .into_iter()
            .map(|r| {
                let conns_live = live.get(&r.remote_api_key).copied().unwrap_or(0);
                let expired = r
                    .expires_at
                    .as_deref()
                    .is_some_and(|e| e < now_sql.as_str());
                KeyRow {
                    id: r.rowid,
                    label: r.label,
                    key_last4: key_last4(&r.remote_api_key),
                    tier: tier_to_dto(r.tier),
                    max_conns: r.max_conns,
                    max_bps: r.max_bps,
                    per_server_max_bps: r.per_server_max_bps,
                    expires_at: r.expires_at,
                    created_at: r.created_at,
                    conns_live,
                    expired,
                }
            })
            .collect())
    }

    async fn reveal_key(&self, id: i64) -> Result<RevealedKey, DashboardError> {
        match self.admission_db.reveal_key(id).await.map_err(db_err)? {
            Some(remote_api_key) => {
                audit(DashboardAction::Reveal, &remote_api_key);
                Ok(RevealedKey { remote_api_key })
            }
            None => Err(DashboardError::NotFound),
        }
    }

    async fn admit_key(&self, req: AdmitKeyRequest) -> Result<AdmitKeyOutcome, DashboardError> {
        let remote_api_key = req.key.unwrap_or_else(generate_remote_api_key);
        let new = NewKey {
            remote_api_key: remote_api_key.clone(),
            label: req.label,
            tier: tier_from_dto(req.tier),
            max_conns: req.max_conns,
            max_bps: req.max_bps,
            per_server_max_bps: req.per_server_max_bps,
            expires_at: req.expires_at,
        };
        self.admission_db.admit_key(&new).await.map_err(admit_err)?;
        self.admission_db.force_reload().await.map_err(db_err)?;
        // The rowid is assigned by the INSERT; resolve it from the freshly
        // reloaded table (the secret is the unique key).
        let id = self
            .admission_db
            .list_keys()
            .await
            .map_err(db_err)?
            .into_iter()
            .find(|r| r.remote_api_key == remote_api_key)
            .map(|r| r.rowid)
            .ok_or_else(|| DashboardError::Backend("admitted key not found after reload".into()))?;
        audit(DashboardAction::Admit, &remote_api_key);
        Ok(AdmitKeyOutcome { id, remote_api_key })
    }

    async fn edit_key(&self, id: i64, req: EditKeyRequest) -> Result<(), DashboardError> {
        let key = self.resolve_key(id).await?;
        let limits = KeyLimits {
            label: req.label,
            tier: tier_from_dto(req.tier),
            max_conns: req.max_conns,
            max_bps: req.max_bps,
            per_server_max_bps: req.per_server_max_bps,
            expires_at: req.expires_at,
        };
        self.admission_db
            .edit_key(&key, &limits)
            .await
            .map_err(db_err)?;
        self.admission_db.force_reload().await.map_err(db_err)?;
        audit(DashboardAction::Update, &key);
        Ok(())
    }

    async fn revoke_key(&self, id: i64) -> Result<(), DashboardError> {
        let key = self.resolve_key(id).await?;
        self.admission_db.revoke_key(&key).await.map_err(db_err)?;
        // Delete-then-reload: the reload diff yields the revoked key only because
        // the row is already gone, firing `on_revoke` (kick + bandwidth.forget).
        self.admission_db.force_reload().await.map_err(db_err)?;
        audit(DashboardAction::Revoke, &key);
        Ok(())
    }

    async fn kick_key(&self, id: i64) -> Result<KickOutcome, DashboardError> {
        let key = self.resolve_key(id).await?;
        // Kick only — the key stays admitted; the gateway may reconnect. Bandwidth
        // buckets re-tune on the next leg (only a revoke forgets them).
        let kicked = self.conns.kick(&HashSet::from([key.clone()]));
        audit(DashboardAction::Kick, &key);
        Ok(KickOutcome { kicked })
    }

    async fn traffic_relay(&self, range: Range) -> Result<RelayTrafficSeries, DashboardError> {
        let reader = match self.traffic_reader.as_ref() {
            Some(r) => r,
            None => return Ok(empty_relay(range)),
        };
        match collect_relay(reader, range).await {
            Ok(series) => Ok(series),
            Err(e) => {
                warn_traffic("relay", &e);
                Ok(empty_relay(range))
            }
        }
    }

    async fn traffic_push(&self, range: Range) -> Result<PushTrafficSeries, DashboardError> {
        let reader = match self.traffic_reader.as_ref() {
            Some(r) => r,
            None => return Ok(empty_push(range)),
        };
        match collect_push(reader, range).await {
            Ok(series) => Ok(series),
            Err(e) => {
                warn_traffic("push", &e);
                Ok(empty_push(range))
            }
        }
    }

    async fn traffic_ip(&self, range: Range) -> Result<IpTrafficSeries, DashboardError> {
        let reader = match self.traffic_reader.as_ref() {
            Some(r) => r,
            None => return Ok(empty_ip(range)),
        };
        match collect_ip(reader, range).await {
            Ok(series) => Ok(series),
            Err(e) => {
                warn_traffic("ip", &e);
                Ok(empty_ip(range))
            }
        }
    }

    async fn list_devices(&self) -> Result<Vec<DeviceRow>, DashboardError> {
        let devices = match self.devices.as_ref() {
            Some(d) => d,
            None => return Ok(Vec::new()),
        };
        let summaries = devices.summaries();
        // Join cumulative push volume from the ledger; degrade to zeros when the
        // ledger is off or the query fails.
        let totals = match self.traffic_reader.as_ref() {
            Some(reader) => match reader.push_device_totals().await {
                Ok(t) => t,
                Err(e) => {
                    warn_traffic("push device totals", &e);
                    std::collections::HashMap::new()
                }
            },
            None => std::collections::HashMap::new(),
        };
        Ok(summaries
            .into_iter()
            .map(|s| {
                let (sends_total, bytes_total) =
                    totals.get(&s.device_id).copied().unwrap_or((0, 0));
                DeviceRow {
                    device_id: s.device_id,
                    apns_env: apns_env_str(s.env).to_string(),
                    token_masked: s.token_masked,
                    gateway_pubkey_hex: hex::encode(s.gateway_pubkey),
                    last_counter: s.last_counter,
                    sends_total,
                    bytes_total,
                }
            })
            .collect())
    }
}

// --- Traffic assembly (mask top-N, fold window totals from the bucket series) ---

async fn collect_relay(
    reader: &TrafficReader,
    range: Range,
) -> Result<RelayTrafficSeries, TrafficQueryError> {
    let buckets: Vec<RelayBucket> = reader
        .relay_series(range)
        .await?
        .into_iter()
        .map(|b| RelayBucket {
            t: b.bucket,
            bytes_up: b.bytes_up,
            bytes_down: b.bytes_down,
            frames_up: b.frames_up,
            frames_down: b.frames_down,
        })
        .collect();
    let by_key = reader
        .relay_top_keys(range, TRAFFIC_TOP_N)
        .await?
        .into_iter()
        .map(|k| RelayKeyTotal {
            key_last4: key_last4(&k.remote_api_key),
            bytes_up: k.bytes_up,
            bytes_down: k.bytes_down,
            frames_up: k.frames_up,
            frames_down: k.frames_down,
        })
        .collect();
    let mut totals = RelayTotals {
        bytes_up: 0,
        bytes_down: 0,
        frames_up: 0,
        frames_down: 0,
    };
    for b in &buckets {
        totals.bytes_up = totals.bytes_up.saturating_add(b.bytes_up);
        totals.bytes_down = totals.bytes_down.saturating_add(b.bytes_down);
        totals.frames_up = totals.frames_up.saturating_add(b.frames_up);
        totals.frames_down = totals.frames_down.saturating_add(b.frames_down);
    }
    Ok(RelayTrafficSeries {
        range,
        bucket: bucket_unit(range),
        buckets,
        by_key,
        totals,
    })
}

async fn collect_push(
    reader: &TrafficReader,
    range: Range,
) -> Result<PushTrafficSeries, TrafficQueryError> {
    let buckets: Vec<PushBucket> = reader
        .push_series(range)
        .await?
        .into_iter()
        .map(|b| PushBucket {
            t: b.bucket,
            sends: b.sends,
            bytes: b.bytes,
        })
        .collect();
    let by_device = reader
        .push_top_devices(range, TRAFFIC_TOP_N)
        .await?
        .into_iter()
        .map(|d| PushDeviceTotal {
            device_id: d.device_id,
            sends: d.sends,
            bytes: d.bytes,
        })
        .collect();
    let mut totals = PushTotals { sends: 0, bytes: 0 };
    for b in &buckets {
        totals.sends = totals.sends.saturating_add(b.sends);
        totals.bytes = totals.bytes.saturating_add(b.bytes);
    }
    Ok(PushTrafficSeries {
        range,
        bucket: bucket_unit(range),
        buckets,
        by_device,
        totals,
    })
}

async fn collect_ip(
    reader: &TrafficReader,
    range: Range,
) -> Result<IpTrafficSeries, TrafficQueryError> {
    let buckets: Vec<IpBucket> = reader
        .ip_series(range)
        .await?
        .into_iter()
        .map(|b| IpBucket {
            t: b.bucket,
            requests: b.requests,
            bytes: b.bytes,
        })
        .collect();
    let by_endpoint = reader
        .ip_top_endpoints(range, TRAFFIC_TOP_N)
        .await?
        .into_iter()
        .map(|e| IpEndpointTotal {
            endpoint: e.endpoint,
            requests: e.requests,
            bytes: e.bytes,
        })
        .collect();
    let by_ip = reader
        .ip_top_sources(range, TRAFFIC_TOP_N)
        .await?
        .into_iter()
        .map(|s| IpTotal {
            ip: s.ip,
            requests: s.requests,
            bytes: s.bytes,
        })
        .collect();
    let mut totals = IpTotals {
        requests: 0,
        bytes: 0,
    };
    for b in &buckets {
        totals.requests = totals.requests.saturating_add(b.requests);
        totals.bytes = totals.bytes.saturating_add(b.bytes);
    }
    Ok(IpTrafficSeries {
        range,
        bucket: bucket_unit(range),
        buckets,
        by_endpoint,
        by_ip,
        totals,
    })
}

fn empty_relay(range: Range) -> RelayTrafficSeries {
    RelayTrafficSeries {
        range,
        bucket: bucket_unit(range),
        buckets: Vec::new(),
        by_key: Vec::new(),
        totals: RelayTotals {
            bytes_up: 0,
            bytes_down: 0,
            frames_up: 0,
            frames_down: 0,
        },
    }
}

fn empty_push(range: Range) -> PushTrafficSeries {
    PushTrafficSeries {
        range,
        bucket: bucket_unit(range),
        buckets: Vec::new(),
        by_device: Vec::new(),
        totals: PushTotals { sends: 0, bytes: 0 },
    }
}

fn empty_ip(range: Range) -> IpTrafficSeries {
    IpTrafficSeries {
        range,
        bucket: bucket_unit(range),
        buckets: Vec::new(),
        by_endpoint: Vec::new(),
        by_ip: Vec::new(),
        totals: IpTotals {
            requests: 0,
            bytes: 0,
        },
    }
}

/// Hourly buckets for ≤7d windows, daily beyond (matches the reader's grouping).
fn bucket_unit(range: Range) -> BucketUnit {
    match range {
        Range::H24 | Range::D7 => BucketUnit::Hour,
        Range::D30 | Range::D60 => BucketUnit::Day,
    }
}

fn warn_traffic(which: &str, e: &TrafficQueryError) {
    tracing::warn!(target: "remote_host", error = %e, "dashboard: {which} traffic read failed; serving empty");
}

// --- Mapping + masking helpers ---

fn tier_to_dto(tier: Tier) -> KeyTier {
    match tier {
        Tier::Guest => KeyTier::Guest,
        Tier::Registered => KeyTier::Registered,
    }
}

fn tier_from_dto(tier: KeyTier) -> Tier {
    match tier {
        KeyTier::Guest => Tier::Guest,
        KeyTier::Registered => Tier::Registered,
    }
}

fn apns_env_str(env: ApnsEnv) -> &'static str {
    match env {
        ApnsEnv::Sandbox => "sandbox",
        ApnsEnv::Production => "production",
    }
}

/// Last [`KEY_MASK_TAIL`] characters of a key (char-safe — pasted keys may not be
/// ASCII). The client renders this as `····<last4>`; the full secret never ships.
fn key_last4(key: &str) -> String {
    let count = key.chars().count();
    key.chars()
        .skip(count.saturating_sub(KEY_MASK_TAIL))
        .collect()
}

/// Map an [`AdmissionDbError`] from an *admit* into the dashboard error: a PK
/// collision (the only realistic write failure here) is a 409 conflict; a missing
/// registered limit / out-of-range value is a 400.
fn admit_err(e: AdmissionDbError) -> DashboardError {
    match e {
        AdmissionDbError::MissingRegisteredLimits | AdmissionDbError::LimitOutOfRange { .. } => {
            DashboardError::BadRequest(e.to_string())
        }
        AdmissionDbError::Db(_) => DashboardError::Conflict(e.to_string()),
        AdmissionDbError::NotFound => DashboardError::Backend(e.to_string()),
    }
}

/// Map an [`AdmissionDbError`] from a non-admit call: a row miss is 404, a bad
/// limit is 400, and a libsql failure is a 500.
fn db_err(e: AdmissionDbError) -> DashboardError {
    match e {
        AdmissionDbError::NotFound => DashboardError::NotFound,
        AdmissionDbError::MissingRegisteredLimits | AdmissionDbError::LimitOutOfRange { .. } => {
            DashboardError::BadRequest(e.to_string())
        }
        AdmissionDbError::Db(_) => DashboardError::Backend(e.to_string()),
    }
}

// --- Audit (masked key only; never the full secret) ---

#[derive(Clone, Copy)]
enum DashboardAction {
    Admit,
    Revoke,
    Update,
    Kick,
    Reveal,
}

impl DashboardAction {
    fn as_str(self) -> &'static str {
        match self {
            DashboardAction::Admit => "admit",
            DashboardAction::Revoke => "revoke",
            DashboardAction::Update => "update",
            DashboardAction::Kick => "kick",
            DashboardAction::Reveal => "reveal",
        }
    }
}

fn audit(action: DashboardAction, key: &str) {
    tracing::info!(
        target: "remote_host",
        action = action.as_str(),
        key = %format!("…{}", key_last4(key)),
        "dashboard: mutation",
    );
}
