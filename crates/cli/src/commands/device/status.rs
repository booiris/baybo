//! `baybo device status`: each approved device's live legs and last direct
//! offer, read from the running gateway's link table
//! (`GET /v1/mobile/links`). It is the one device command that asks the
//! gateway rather than the stores; without an answer it prints the device
//! rows alone and says why.

use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use baybo_gateway::AdminToken;
use baybo_gateway::api::admin::mobile::{DeviceLink, LinkCarrier, MOBILE_LINKS_PATH, MobileLinks};
use baybo_gateway::config::admin_dial_addr;
use baybo_security::SecretVault;
use baybo_store::{DeviceRow, DeviceStatus};
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::StatusCode;
use serde_json::json;

use super::require_service;
use crate::context::CommandContext;
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

/// How long the command waits for the running gateway's answer.
const LINKS_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// Shown where a device row has no time for a field, or a leg no carrier.
const ABSENT: &str = "-";

pub(super) async fn status(ctx: &CommandContext) -> Result<CommandOutput> {
    let rows = require_service(ctx)?
        .list(Some(DeviceStatus::Approved))
        .await
        .map_err(|e| CliError::Manager(format!("list devices: {e}")))?;
    let gateway = GatewayLinks::fetch(ctx).await;
    Ok(render(&rows, &gateway))
}

/// The running gateway's link table, or why the command has none.
enum GatewayLinks {
    Live {
        address: SocketAddr,
        links: MobileLinks,
    },
    Unavailable {
        address: Option<SocketAddr>,
        why: Unavailable,
    },
}

enum Unavailable {
    /// `gateway.bind_address` is not an address to dial.
    Address(String),
    /// The vault holds no admin token to present, or cannot be read.
    NoToken(String),
    /// Nothing answered at the admin address in time.
    Unreachable(String),
    /// The gateway answered, but not with its link table.
    Refused(StatusCode),
    Unreadable(String),
}

impl GatewayLinks {
    async fn fetch(ctx: &CommandContext) -> Self {
        let address = match admin_dial_addr(&ctx.config.gateway) {
            Ok(address) => address,
            Err(e) => {
                return Self::Unavailable {
                    address: None,
                    why: Unavailable::Address(e.to_string()),
                };
            }
        };
        let fetched = match admin_token(ctx.secret_vault.as_ref()).await {
            Ok(token) => request(address, &token).await,
            Err(why) => Err(why),
        };
        match fetched {
            Ok(links) => Self::Live { address, links },
            Err(why) => Self::Unavailable {
                address: Some(address),
                why,
            },
        }
    }

    fn address(&self) -> Option<SocketAddr> {
        match self {
            Self::Live { address, .. } => Some(*address),
            Self::Unavailable { address, .. } => *address,
        }
    }

    fn link(&self, device_id: &str) -> Option<Option<&DeviceLink>> {
        match self {
            Self::Live { links, .. } => Some(
                links
                    .devices
                    .iter()
                    .find(|link| link.device_id == device_id),
            ),
            Self::Unavailable { .. } => None,
        }
    }

    /// Why the table is missing, as one line naming the address asked.
    fn problem(&self) -> Option<String> {
        let Self::Unavailable { address, why } = self else {
            return None;
        };
        let at = address.map_or_else(|| ABSENT.to_owned(), |address| address.to_string());
        Some(match why {
            Unavailable::Address(reason) => format!("gateway address unknown: {reason}"),
            Unavailable::NoToken(reason) => format!("cannot ask the gateway at {at}: {reason}"),
            Unavailable::Unreachable(reason) => format!("gateway not reachable at {at}: {reason}"),
            Unavailable::Refused(StatusCode::UNAUTHORIZED) => format!(
                "the gateway at {at} refused the vault's admin token (does it serve another workspace?)"
            ),
            Unavailable::Refused(status) => format!("the gateway at {at} answered {status}"),
            Unavailable::Unreadable(reason) => {
                format!("the gateway at {at} sent an unreadable link table: {reason}")
            }
        })
    }
}

async fn admin_token(vault: Option<&Arc<SecretVault>>) -> std::result::Result<String, Unavailable> {
    let Some(vault) = vault else {
        return Err(Unavailable::NoToken(
            "the workspace's secret vault is unavailable".to_owned(),
        ));
    };
    match AdminToken::new(Arc::clone(vault)).get().await {
        Ok(Some(token)) => Ok(token),
        Ok(None) => Err(Unavailable::NoToken(
            "the vault holds no admin token; the gateway has never started in this workspace"
                .to_owned(),
        )),
        Err(e) => Err(Unavailable::NoToken(e.to_string())),
    }
}

async fn request(
    address: SocketAddr,
    token: &str,
) -> std::result::Result<MobileLinks, Unavailable> {
    let client = reqwest::Client::builder()
        .timeout(LINKS_REQUEST_TIMEOUT)
        .no_proxy()
        .build()
        .map_err(|e| Unavailable::Unreachable(format!("build HTTP client: {e}")))?;
    let response = client
        .get(format!("http://{address}{MOBILE_LINKS_PATH}"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| {
            Unavailable::Unreachable(if e.is_timeout() {
                format!("no answer within {LINKS_REQUEST_TIMEOUT:?}")
            } else if e.is_connect() {
                "nothing accepted the connection; start it with `baybo gateway start`".to_owned()
            } else {
                e.to_string()
            })
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(Unavailable::Refused(status));
    }
    response
        .json::<MobileLinks>()
        .await
        .map_err(|e| Unavailable::Unreadable(e.to_string()))
}

fn render(rows: &[DeviceRow], gateway: &GatewayLinks) -> CommandOutput {
    let mut human = String::new();
    if let Some(problem) = gateway.problem() {
        let _ = writeln!(human, "{problem}; showing the device rows alone");
    }
    if rows.is_empty() {
        human.push_str("(no approved devices)");
    }
    for row in rows {
        let _ = writeln!(
            human,
            "{}\n  approved {} · last seen {}",
            row.device_id,
            unix_seconds(row.approved_at),
            unix_seconds(row.last_seen_at),
        );
        let Some(link) = gateway.link(&row.device_id) else {
            continue;
        };
        let legs = link.map_or(&[][..], |link| link.legs.as_slice());
        if legs.is_empty() {
            human.push_str("  no live legs\n");
        }
        for leg in legs {
            let _ = writeln!(
                human,
                "  {}\t{}\tsince {}",
                leg.class.as_str(),
                leg.carrier.map_or(ABSENT, LinkCarrier::as_str),
                rfc3339(leg.started_at),
            );
        }
        match link.and_then(|link| link.last_offer.as_ref()) {
            Some(offer) => {
                let _ = writeln!(
                    human,
                    "  last offer: {} at {}",
                    offer.outcome.as_str(),
                    rfc3339(offer.at)
                );
            }
            None => human.push_str("  last offer: none\n"),
        }
    }
    let devices: Vec<_> = rows
        .iter()
        .map(|row| {
            let link = gateway.link(&row.device_id);
            json!({
                "device_id": row.device_id,
                "approved_at": row.approved_at,
                "last_seen_at": row.last_seen_at,
                "legs": link.map(|link| link.map(|link| link.legs.clone()).unwrap_or_default()),
                "last_offer": link.flatten().and_then(|link| link.last_offer.clone()),
            })
        })
        .collect();
    CommandOutput::structured(
        human.trim_end().to_owned(),
        &json!({
            "gateway": {
                "address": gateway.address().map(|address| address.to_string()),
                "error": gateway.problem(),
            },
            "devices": devices,
        }),
    )
}

fn rfc3339(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn unix_seconds(at: Option<i64>) -> String {
    at.and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .map_or_else(|| ABSENT.to_owned(), rfc3339)
}
