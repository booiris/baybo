//! `aura device` subcommand family — the operator surface for the iOS
//! companion's device pairing (orthogonal to `aura pair`, which gates channel
//! senders). See [`mobile-remote-host`](../../../docs/mobile-remote-host.md).

use std::sync::Arc;

use aura_pairing::DevicePairingService;
use aura_store::{DeviceRow, DeviceStatus};
use serde_json::json;

use crate::cli::DeviceCmd;
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: DeviceCmd) -> Result<CommandOutput> {
    match cmd {
        DeviceCmd::Pair { label, user } => pair(ctx, label, user).await,
        DeviceCmd::Approve { code } => approve(ctx, code).await,
        DeviceCmd::List { pending, approved } => list(ctx, pending, approved).await,
        DeviceCmd::Revoke {
            user_id,
            device_id,
            yes,
        } => revoke(ctx, user_id, device_id, yes).await,
    }
}

fn require_service(ctx: &CommandContext) -> Result<&Arc<DevicePairingService>> {
    ctx.device_pairing_service.as_ref().ok_or_else(|| {
        CliError::Config(
            "device pairing service unavailable — run from the workspace root with a valid \
             aura.json"
                .into(),
        )
    })
}

async fn pair(ctx: &CommandContext, label: String, user: String) -> Result<CommandOutput> {
    let svc = require_service(ctx)?;
    let code = svc
        .mint(&user, &label)
        .await
        .map_err(|e| CliError::Manager(format!("mint device pairing: {e}")))?;
    let human = format!(
        "Pairing code for \"{label}\": {code}\n\
         Scan it in the Aura iOS app to start pairing, then approve with:\n  \
         aura device approve {code}",
    );
    Ok(CommandOutput::structured(
        human,
        &json!({
            "action": "minted",
            "code": code,
            "user_id": user,
            "label": label,
        }),
    ))
}

async fn approve(ctx: &CommandContext, code: String) -> Result<CommandOutput> {
    let svc = require_service(ctx)?;
    let row = svc
        .approve(&code)
        .await
        .map_err(|e| CliError::Manager(format!("approve device: {e}")))?;
    match row {
        Some(r) => {
            let human = format!(
                "Approved device \"{}\" ({}:{}).",
                r.label, r.user_id, r.device_id,
            );
            Ok(CommandOutput::structured(
                human,
                &json!({
                    "action": "approved",
                    "user_id": r.user_id,
                    "device_id": r.device_id,
                    "label": r.label,
                    "approved_at": r.approved_at,
                }),
            ))
        }
        None => Err(CliError::Config(format!(
            "code '{code}' not found (it may have expired, been approved already, or been revoked)"
        ))),
    }
}

async fn list(ctx: &CommandContext, pending: bool, approved: bool) -> Result<CommandOutput> {
    let svc = require_service(ctx)?;
    let filter = if pending {
        Some(DeviceStatus::Pending)
    } else if approved {
        Some(DeviceStatus::Approved)
    } else {
        None
    };
    let rows = svc
        .list(filter)
        .await
        .map_err(|e| CliError::Manager(format!("list devices: {e}")))?;
    let human = if rows.is_empty() {
        "(no devices)".to_string()
    } else {
        let mut buf = String::from("STATUS\tUSER\tDEVICE\tLABEL\tCODE\tCREATED_AT\n");
        for r in &rows {
            buf.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                status_str(r),
                r.user_id,
                r.device_id,
                r.label,
                r.pairing_code.as_deref().unwrap_or("-"),
                r.created_at,
            ));
        }
        buf.trim_end().to_string()
    };
    Ok(CommandOutput::structured(
        human,
        &json!({
            "devices": rows
                .iter()
                .map(|r| json!({
                    "status": status_str(r),
                    "user_id": r.user_id,
                    "device_id": r.device_id,
                    "label": r.label,
                    "pairing_code": r.pairing_code,
                    "created_at": r.created_at,
                    "approved_at": r.approved_at,
                    "last_seen_at": r.last_seen_at,
                }))
                .collect::<Vec<_>>(),
        }),
    ))
}

async fn revoke(
    ctx: &CommandContext,
    user_id: String,
    device_id: String,
    yes: bool,
) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && !yes {
        return Err(CliError::Config(
            "pass --yes to revoke a device from a chat channel".into(),
        ));
    }
    let svc = require_service(ctx)?;
    let changed = svc
        .revoke(&user_id, &device_id)
        .await
        .map_err(|e| CliError::Manager(format!("revoke device: {e}")))?;
    let human = if changed {
        format!("Revoked device {user_id}:{device_id} (row retained for audit).")
    } else {
        format!("No live device {user_id}:{device_id} to revoke.")
    };
    Ok(CommandOutput::structured(
        human,
        &json!({
            "action": "revoked",
            "changed": changed,
            "user_id": user_id,
            "device_id": device_id,
        }),
    ))
}

fn status_str(row: &DeviceRow) -> &'static str {
    match row.status {
        DeviceStatus::Pending => "PENDING",
        DeviceStatus::Approved => "APPROVED",
        DeviceStatus::Revoked => "REVOKED",
    }
}
