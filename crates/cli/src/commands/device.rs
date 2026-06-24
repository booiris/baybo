//! `baybo device` subcommand family — the operator surface for the iOS
//! companion's device pairing (orthogonal to `baybo pair`, which gates channel
//! senders). See [`mobile-remote-host`](../../../docs/mobile-remote-host.md).
//!
//! `pair` is a live, terminal-only flow: it mints a code, waits for the phone to
//! scan and reach the confirm step, shows the Bluetooth-style confirmation code,
//! and asks the operator to approve — both sides confirm before any token
//! activates. There is no separate `approve` step.

use std::sync::Arc;
use std::time::Duration;

use baybo_pairing::DevicePairingService;
use baybo_store::device_pairing::DevicePairingSlot;
use baybo_store::{DeviceRow, DeviceStatus};
use serde_json::json;

use crate::cli::DeviceCmd;
use crate::commands::prompt;
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

/// Poll cadence while waiting for the phone to scan.
const SCAN_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Poll cadence while waiting for the gateway to finalize after the operator
/// confirms.
const OUTCOME_POLL_INTERVAL: Duration = Duration::from_millis(400);
/// How long to wait for the phone to scan + reach the confirm step (the
/// operator may walk to their phone; the slot's own TTL is the hard bound).
const SCAN_WAIT: Duration = Duration::from_secs(300);
/// How long to wait for the gateway to finalize once the operator confirmed.
const OUTCOME_WAIT: Duration = Duration::from_secs(125);
/// Built-in default endpoint embedded in the pairing QR when the operator has
/// not configured a reachable `gateway.direct.advertise` address.
const DEFAULT_GATEWAY_ENDPOINT: &str = "wss://proxy.baybo.space:7777";

pub async fn handle(ctx: &CommandContext, cmd: DeviceCmd) -> Result<CommandOutput> {
    match cmd {
        DeviceCmd::Pair { label, user } => pair(ctx, label, user).await,
        DeviceCmd::List { approved } => list(ctx, approved).await,
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
             baybo.json"
                .into(),
        )
    })
}

/// The local operator's user id — the same `$USER`/`$USERNAME` derivation
/// `prompt`/`tui` turns run as, so a device paired with no `--user` is owned by
/// the identity whose completed turns should push to it.
fn operator_user_id() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "baybo-cli".to_string())
}

async fn pair(
    ctx: &CommandContext,
    label: Option<String>,
    user: Option<String>,
) -> Result<CommandOutput> {
    let svc = require_service(ctx)?;
    let user = user.unwrap_or_else(operator_user_id);
    // The label is optional: an empty slot label tells the gateway to use the
    // name the device reports in its DeviceHello during the handshake.
    let operator_label = label.as_deref().unwrap_or("");
    let code = svc
        .mint(&user, operator_label)
        .await
        .map_err(|e| CliError::Manager(format!("mint device pairing: {e}")))?;

    let endpoint = pairing_endpoint(ctx);
    let payload = format!("baybo://pair?h={endpoint}&c={code}");
    eprintln!("Pairing a new device.\n");
    if let Some(qr) = render_pairing_qr(&payload) {
        // Blank line after as well: the surrounding blank lines stand in for the
        // (dropped) quiet zone so the matrix still scans against the terminal bg.
        eprintln!("{qr}\n");
    } else {
        // QR couldn't render (shouldn't happen for this short payload) — fall
        // back to the raw code so the operator can still pair manually.
        eprintln!("endpoint: {endpoint}\ncode:     {code}\n");
    }
    eprintln!("Waiting for the device to scan…");

    // 1. Wait for the phone to scan + reach the confirm step: the gateway
    //    publishes the confirmation code + the device's name onto the slot.
    let Some(slot) = wait_for_confirm(svc, &code, SCAN_WAIT).await? else {
        return Ok(CommandOutput::structured(
            "No device scanned the code in time; it has expired.".to_string(),
            &json!({ "action": "timeout", "code": code, "stage": "scan" }),
        ));
    };
    let device_id = slot.device_id.unwrap_or_default();
    let confirm_code = slot.confirm_code.unwrap_or_default();
    // Resolved during the handshake: the device's reported name (or the
    // operator's override if one was passed).
    let device_label = slot.label;

    // 2. Operator confirms the code matches the phone (Bluetooth-style numeric
    //    comparison). `confirm` requires a terminal, so this is shell-only.
    eprintln!("\nA device wants to pair:");
    eprintln!("    name:              {device_label}");
    eprintln!("    device:            {device_id}");
    eprintln!("    confirmation code: {confirm_code}");
    let accepted = prompt::confirm("Does this match the code on the phone? Pair this device?")?;
    svc.set_operator_decision(&code, accepted)
        .await
        .map_err(|e| CliError::Manager(format!("record pairing decision: {e}")))?;
    if !accepted {
        return Ok(CommandOutput::structured(
            format!("Declined pairing for \"{device_label}\"."),
            &json!({ "action": "declined", "code": code, "device_id": device_id }),
        ));
    }

    // 3. Wait for the gateway to finalize (it requires the phone's confirm too).
    eprintln!("Confirming…");
    if wait_for_paired(svc, &user, &device_id, OUTCOME_WAIT).await? {
        Ok(CommandOutput::structured(
            format!("Paired \"{device_label}\" ({user}:{device_id})."),
            &json!({
                "action": "paired",
                "user_id": user,
                "device_id": device_id,
                "label": device_label,
            }),
        ))
    } else {
        Ok(CommandOutput::structured(
            format!(
                "Pairing for \"{device_label}\" did not complete (the device may have declined or timed out)."
            ),
            &json!({ "action": "incomplete", "code": code, "device_id": device_id }),
        ))
    }
}

/// Poll the slot until the gateway publishes the confirmation code (the phone
/// scanned and reached the confirm step), the slot is gone, or `budget` elapses.
async fn wait_for_confirm(
    svc: &DevicePairingService,
    code: &str,
    budget: Duration,
) -> Result<Option<DevicePairingSlot>> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        match svc
            .claim_slot(code)
            .await
            .map_err(|e| CliError::Manager(format!("poll pairing slot: {e}")))?
        {
            Some(slot) if slot.confirm_code.is_some() => return Ok(Some(slot)),
            Some(_) => {}             // scanned but not yet at the confirm step
            None => return Ok(None),  // expired or consumed
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(SCAN_POLL_INTERVAL).await;
    }
}

/// Poll for the finalized (approved) device row after the operator confirmed.
async fn wait_for_paired(
    svc: &DevicePairingService,
    user: &str,
    device_id: &str,
    budget: Duration,
) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(row) = svc
            .device(user, device_id)
            .await
            .map_err(|e| CliError::Manager(format!("poll device row: {e}")))?
            && row.status == DeviceStatus::Approved
        {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(OUTCOME_POLL_INTERVAL).await;
    }
}

/// The endpoint to embed in the pairing QR: the operator's first advertised
/// direct address when direct reachability is configured, else the built-in
/// default proxy.
fn pairing_endpoint(ctx: &CommandContext) -> String {
    let direct = &ctx.config.gateway.direct;
    if direct.enabled
        && let Some(first) = direct.advertise.first()
    {
        return first.clone();
    }
    DEFAULT_GATEWAY_ENDPOINT.to_string()
}

/// Render `payload` as a QR for the terminal (unicode half-blocks). `None` if
/// the payload won't fit a QR.
///
/// Natural polarity (dark modules filled `█`, light modules empty) so it shows
/// as a normal black-on-white QR on a light terminal and an inverted (still
/// scannable) one on a dark terminal. The built-in quiet zone is dropped so the
/// matrix renders flush — the caller frames it with blank lines, which serve as
/// the scan margin against the terminal background.
fn render_pairing_qr(payload: &str) -> Option<String> {
    use qrcode::QrCode;
    use qrcode::render::unicode;
    let code = QrCode::new(payload).ok()?;
    Some(
        code.render::<unicode::Dense1x2>()
            .dark_color(unicode::Dense1x2::Dark)
            .light_color(unicode::Dense1x2::Light)
            .quiet_zone(false)
            .build(),
    )
}

async fn list(ctx: &CommandContext, approved: bool) -> Result<CommandOutput> {
    let svc = require_service(ctx)?;
    let filter = approved.then_some(DeviceStatus::Approved);
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
        DeviceStatus::Approved => "APPROVED",
        DeviceStatus::Revoked => "REVOKED",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_scannable_qr() {
        let qr = render_pairing_qr("baybo://pair?h=wss://proxy.baybo.space:7777&c=5PRX2B")
            .expect("the short pairing payload fits a QR");
        assert!(qr.lines().count() > 8, "multi-line QR matrix");
        assert!(
            qr.contains('█') || qr.contains('▀') || qr.contains('▄'),
            "rendered with half-block glyphs"
        );
    }
}
