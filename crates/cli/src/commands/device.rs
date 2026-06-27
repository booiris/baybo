//! `baybo device` subcommand family — the operator surface for the iOS
//! companion's device pairing (orthogonal to `baybo pair`, which gates channel
//! senders).
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
/// Built-in public relay embedded in the pairing QR when the operator has not
/// configured their own `gateway.relay`.
const DEFAULT_GATEWAY_ENDPOINT: &str = "wss://proxy.baybo.space";

pub async fn handle(ctx: &CommandContext, cmd: DeviceCmd) -> Result<CommandOutput> {
    match cmd {
        DeviceCmd::Pair => pair(ctx).await,
        DeviceCmd::List { approved } => list(ctx, approved).await,
        DeviceCmd::Revoke { device_id, yes } => revoke(ctx, device_id, yes).await,
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

/// Owns the spawned self-hosting relay task and stops it on drop (every
/// operator-flow exit path), unless [`finish`](Self::finish) drained it first.
struct StopHosting(Option<tokio::task::JoinHandle<()>>);

impl StopHosting {
    fn new(handle: tokio::task::JoinHandle<()>) -> Self {
        Self(Some(handle))
    }

    /// Wait briefly for the handshake to finish (it sends the sealed
    /// `GatewayWelcome` just after the approved row `wait_for_paired` observes),
    /// so a successful pair isn't cut off before the app receives it.
    async fn finish(mut self) {
        if let Some(h) = self.0.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
        }
    }

    /// Await the hosted handshake task ending. The host loop only returns on a
    /// terminal outcome (the slot is gone, or a side declined), so the task
    /// completing *while the operator is still deciding* is a direct "the phone
    /// gave up" signal — no slot round-trip. Borrows the handle (doesn't consume
    /// it), so `finish`/`drop` still work afterward.
    async fn ended(&mut self) {
        match self.0.as_mut() {
            Some(h) => {
                let _ = h.await;
            }
            None => std::future::pending().await,
        }
    }
}

impl Drop for StopHosting {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            h.abort();
        }
    }
}

/// Percent-encode an instance key for the QR query so `new URL(...).searchParams`
/// round-trips it (keys are usually base64url/hex, but a `+`/`=`/space would
/// otherwise break parsing).
fn qr_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

async fn pair(ctx: &CommandContext) -> Result<CommandOutput> {
    let svc = require_service(ctx)?;

    // 1:1 binding: the gateway binds at most one device (one gateway = one app).
    // If one is already bound, get the operator's informed consent before
    // minting — the actual replacement happens only if/when the new pairing
    // completes (`complete` revokes the old row atomically), so a working binding
    // is never dropped for a pairing that never finishes.
    let existing = svc
        .current_device()
        .await
        .map_err(|e| CliError::Manager(format!("look up current device: {e}")))?;
    if let Some(ref dev) = existing {
        eprintln!(
            "This gateway is already paired with device {}.",
            dev.device_id
        );
        if !prompt::confirm_with_default(
            "Pairing a new device will replace it once the new pairing completes. Continue?",
            false,
        )? {
            return Ok(CommandOutput::structured(
                format!("Kept the existing pairing with device {}.", dev.device_id),
                &json!({ "action": "kept_existing", "device_id": dev.device_id }),
            ));
        }
    }

    // The mint returns two values with opposite handling: the public
    // `rendezvous_id` (the relay routes on it) and the high-entropy `secret`
    // (the Noise PSK). The secret travels *only* in the QR below — never the
    // relay, never a log, never the durable row.
    let (rendezvous_id, secret) = svc
        .mint()
        .await
        .map_err(|e| CliError::Manager(format!("mint device pairing: {e}")))?;

    // Pairing always runs through the relay: the operator supplies its admission
    // key, baked into the QR so the app presents it on its join leg (the relay
    // admits both sides).
    let (endpoint, default_key) = relay_endpoint();
    let key_label = if endpoint == DEFAULT_GATEWAY_ENDPOINT {
        // `guest` is the trial key for the built-in public proxy.
        "Relay instance key (enter `guest` to try it out)"
    } else {
        "Relay instance key"
    };
    let instance_key = prompt::prompt_with_default(key_label, &default_key)?;
    let k = qr_encode(&instance_key);
    // `s=` is the 256-bit secret, hex-encoded; it is bearer credential material.
    // The whole payload is rendered as a QR only — never echoed to the terminal
    // or any log.
    let s = hex::encode(secret.as_bytes());
    let payload = format!("baybo://pair?h={endpoint}&r={rendezvous_id}&s={s}&k={k}");
    eprintln!("Pairing a new device.\n");
    let Some(qr) = render_pairing_qr(&payload) else {
        // The payload carries a one-time secret, so we must NEVER fall back to a
        // shortened/typeable form of it (a short secret is offline-crackable by a
        // hostile relay) or write it to disk. If the QR can't render, bail and ask
        // the operator to retry in a larger terminal.
        return Err(CliError::Io(
            "couldn't render the pairing QR in this terminal — widen the window and re-run \
             `baybo device pair`"
                .into(),
        ));
    };
    // Blank line after as well: the surrounding blank lines stand in for the
    // (dropped) quiet zone so the matrix still scans against the terminal bg.
    eprintln!("{qr}\n");
    eprintln!("Waiting for the device to scan…");

    // Self-contained relay hosting: open our `/pair/host/{rendezvous_id}` leg on
    // the relay and run the XXpsk0 handshake here — pairing needs no
    // `baybo gateway start` daemon. Runs concurrently with the operator flow
    // below (they sync through the in-process slot); the guard stops it on every
    // exit path. The relay endpoint is bound into the handshake prologue, so the
    // QR `h=` (which equals `endpoint`) and the gateway's `relay_url` agree.
    let secret_vault = ctx
        .secret_vault
        .clone()
        .ok_or_else(|| CliError::Config("device pairing needs the secret vault".into()))?;
    let deps = baybo_gateway::PairingHostDeps {
        device_pairing: Arc::clone(svc),
        secret_vault,
        relay_url: endpoint.clone(),
        instance_key: instance_key.clone(),
    };
    let mut host = {
        let (relay_url, key, rid) = (
            endpoint.clone(),
            instance_key.clone(),
            rendezvous_id.clone(),
        );
        StopHosting::new(tokio::spawn(async move {
            if let Err(e) = baybo_gateway::host_pairing_leg(&deps, &relay_url, &key, &rid).await {
                tracing::debug!(error = %e, "device pair: relay host ended");
            }
        }))
    };

    // 1. Wait for the phone to scan + reach the confirm step: the gateway
    //    publishes the confirmation code onto the slot.
    let Some(slot) = wait_for_confirm(svc, &rendezvous_id, SCAN_WAIT).await? else {
        return Ok(CommandOutput::structured(
            "No device scanned the code in time; it has expired.".to_string(),
            &json!({ "action": "timeout", "rendezvous_id": rendezvous_id, "stage": "scan" }),
        ));
    };
    let device_id = slot.device_id.unwrap_or_default();
    let confirm_code = slot.confirm_code.unwrap_or_default();

    // 2. Operator confirms the code matches the phone (Bluetooth-style numeric
    //    comparison). `confirm` requires a terminal, so this is shell-only. The
    //    prompt is interruptible: if the phone declines (or drops) while the
    //    operator is still deciding, bail out instead of holding the prompt for a
    //    device that's already gone.
    eprintln!("\nA device wants to pair:");
    eprintln!("    device:            {device_id}");
    eprintln!("    confirmation code: {confirm_code}");
    let accepted = match confirm_or_device_gone(
        svc,
        &rendezvous_id,
        "Does this match the code on the phone? Pair this device?",
        &mut host,
    )
    .await?
    {
        ConfirmOutcome::Decided(accepted) => accepted,
        ConfirmOutcome::DeviceGone => {
            return Ok(CommandOutput::structured(
                format!("Device {device_id} cancelled pairing on the phone (or it expired)."),
                &json!({ "action": "device_cancelled", "rendezvous_id": rendezvous_id, "device_id": device_id }),
            ));
        }
    };
    svc.set_operator_decision(&rendezvous_id, accepted)
        .await
        .map_err(|e| CliError::Manager(format!("record pairing decision: {e}")))?;
    if !accepted {
        // Let the self-hosted relay leg deliver the `Reject` to the phone before
        // we exit: the host's drive() polls the decision we just wrote, rejects,
        // and the host loop then stops (it sees the decline), so this returns
        // promptly. Without it, dropping the guard would abort the leg mid-flight
        // and the app would see a reset connection instead of "operator declined".
        host.finish().await;
        return Ok(CommandOutput::structured(
            format!("Declined pairing for device {device_id}."),
            &json!({ "action": "declined", "rendezvous_id": rendezvous_id, "device_id": device_id }),
        ));
    }

    // 3. Wait for the gateway to finalize (it requires the phone's confirm too).
    eprintln!("Confirming…");
    let outcome = wait_for_paired(svc, &device_id, &rendezvous_id, OUTCOME_WAIT).await?;
    // Stop our self-hosted relay leg: on success let it finish sending the sealed
    // GatewayWelcome (it goes out right after the approved row `wait_for_paired`
    // just observed); otherwise drop the guard to abort the still-running task.
    match &outcome {
        PairOutcome::Paired => host.finish().await,
        _ => drop(host),
    }
    match outcome {
        PairOutcome::Paired => {
            // Tell the operator what was superseded (the old binding was revoked
            // atomically as the new row was written).
            let replaced_note = existing
                .as_ref()
                .map(|d| format!(" Replaced device {}.", d.device_id))
                .unwrap_or_default();
            Ok(CommandOutput::structured(
                format!("Paired device {device_id}.{replaced_note}"),
                &json!({
                    "action": "paired",
                    "device_id": device_id,
                    "replaced_device_id": existing.as_ref().map(|d| d.device_id.as_str()),
                }),
            ))
        }
        PairOutcome::DeviceDeclined => Ok(CommandOutput::structured(
            format!("Device {device_id} cancelled pairing on the phone."),
            &json!({ "action": "device_cancelled", "rendezvous_id": rendezvous_id, "device_id": device_id }),
        )),
        PairOutcome::TimedOut => Ok(CommandOutput::structured(
            format!("Pairing for device {device_id} did not complete (timed out)."),
            &json!({ "action": "incomplete", "rendezvous_id": rendezvous_id, "device_id": device_id }),
        )),
    }
}

/// Whether the operator decided at the prompt, or the phone backed out first.
enum ConfirmOutcome {
    Decided(bool),
    /// The phone declined or dropped (or the slot expired) before the operator
    /// answered — so there is nothing left to confirm.
    DeviceGone,
}

/// Ask the operator to confirm, but bail out early if the phone declines or
/// drops first — so the operator isn't left holding a prompt for a device that
/// has already gone away.
///
/// The prompt **defaults to no** (`[y/N]`): pairing authorizes a device against
/// a possibly-hostile relay, so an absent-minded Enter must not pair, and the
/// confirm is the backstop if the QR secret ever leaks (shoulder-surf / forward).
///
/// Three concurrent signals decide the outcome:
/// - the operator's answer (a blocking stdin read on a **detached OS thread** —
///   not `spawn_blocking`, since an uncancellable blocking-pool thread stuck on
///   stdin would keep the tokio runtime from shutting down and hang process exit
///   after we bail; a detached thread is just terminated at exit);
/// - the self-hosted relay leg's task ending (a direct phone-gave-up signal);
/// - a slot poll for the phone-side `device_decision` (a belt-and-braces fallback
///   that also catches an expired/consumed slot).
async fn confirm_or_device_gone(
    svc: &DevicePairingService,
    rendezvous_id: &str,
    question: &str,
    host: &mut StopHosting,
) -> Result<ConfirmOutcome> {
    use std::io::{BufRead, IsTerminal, Write};

    // This confirm requires a terminal (the command is shell-only).
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(CliError::Config(
            "interactive confirmation requires a terminal".into(),
        ));
    }
    // Write the `[y/N]` prompt ourselves and release the stderr lock immediately.
    // The blocking read then runs on a detached thread holding *only* the stdin
    // lock, so if we bail (DeviceGone) our `eprintln!` below can't deadlock on a
    // stderr lock the reader is still holding. (`prompt::confirm_with_default`
    // holds stderr across the read, which would hang our bail-out.)
    {
        let mut err = std::io::stderr().lock();
        write!(err, "{question} [y/N]: ")
            .and_then(|()| err.flush())
            .map_err(|e| CliError::Io(format!("write confirm prompt: {e}")))?;
    }
    // Detached OS thread, not `spawn_blocking`: a blocking stdin read can't be
    // cancelled, and a tokio blocking-pool thread stuck on stdin would keep the
    // runtime from shutting down — hanging exit after we bail. A detached thread
    // is just terminated at process exit. Sends `None` on EOF / read error.
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<bool>>();
    std::thread::spawn(move || {
        let mut line = String::new();
        let decision = match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            // `[y/N]`: only an explicit yes pairs; anything else (incl. an empty
            // line) declines. Fail closed — see the doc comment.
            Ok(_) => Some(matches!(
                line.trim().to_ascii_lowercase().as_str(),
                "y" | "yes"
            )),
        };
        let _ = tx.send(decision);
    });
    tokio::pin!(rx);
    let host_ended = host.ended();
    tokio::pin!(host_ended);
    loop {
        tokio::select! {
            answered = &mut rx => {
                // `None` (EOF / read error / sender dropped) → treat as decline.
                let accepted = answered.ok().flatten().unwrap_or(false);
                return Ok(ConfirmOutcome::Decided(accepted));
            }
            // The hosted handshake ended before the operator decided → the phone
            // declined or dropped.
            _ = &mut host_ended => {
                eprintln!();
                return Ok(ConfirmOutcome::DeviceGone);
            }
            _ = tokio::time::sleep(OUTCOME_POLL_INTERVAL) => {
                let gone = match svc
                    .claim_slot(rendezvous_id)
                    .await
                    .map_err(|e| CliError::Manager(format!("poll pairing slot: {e}")))?
                {
                    Some(slot) => slot.device_decision == Some(false),
                    None => true, // expired / consumed
                };
                if gone {
                    // End the dangling prompt line so the caller's message starts
                    // fresh, then bail (the stdin read is abandoned).
                    eprintln!();
                    return Ok(ConfirmOutcome::DeviceGone);
                }
            }
        }
    }
}

/// Poll the slot until the gateway publishes the confirmation code (the phone
/// scanned and reached the confirm step), the slot is gone, or `budget` elapses.
async fn wait_for_confirm(
    svc: &DevicePairingService,
    rendezvous_id: &str,
    budget: Duration,
) -> Result<Option<DevicePairingSlot>> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        match svc
            .claim_slot(rendezvous_id)
            .await
            .map_err(|e| CliError::Manager(format!("poll pairing slot: {e}")))?
        {
            Some(slot) if slot.confirm_code.is_some() => return Ok(Some(slot)),
            Some(_) => {}            // scanned but not yet at the confirm step
            None => return Ok(None), // expired or consumed
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(SCAN_POLL_INTERVAL).await;
    }
}

/// How waiting for the gateway to finalize ended, after the operator confirmed.
enum PairOutcome {
    Paired,
    /// The phone declined or dropped *after* the operator approved — recorded on
    /// the slot by the gateway, so we needn't sit until the timeout.
    DeviceDeclined,
    TimedOut,
}

/// Poll for the finalized (approved) device row after the operator confirmed,
/// short-circuiting if the phone backs out in the meantime.
async fn wait_for_paired(
    svc: &DevicePairingService,
    device_id: &str,
    rendezvous_id: &str,
    budget: Duration,
) -> Result<PairOutcome> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(row) = svc
            .device(device_id)
            .await
            .map_err(|e| CliError::Manager(format!("poll device row: {e}")))?
            && row.status == DeviceStatus::Approved
        {
            return Ok(PairOutcome::Paired);
        }
        // A successful pair deletes the slot and writes the row, so a *gone* slot
        // is benign (the row check above is authoritative); only an explicit
        // device-side decline short-circuits the wait.
        if let Some(slot) = svc
            .claim_slot(rendezvous_id)
            .await
            .map_err(|e| CliError::Manager(format!("poll pairing slot: {e}")))?
            && slot.device_decision == Some(false)
        {
            return Ok(PairOutcome::DeviceDeclined);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(PairOutcome::TimedOut);
        }
        tokio::time::sleep(OUTCOME_POLL_INTERVAL).await;
    }
}

/// The relay endpoint to embed in the pairing QR (the app joins via
/// `/pair/join/<code>` on it), plus the default admission key to offer at the
/// prompt. The relay URL is not operator-configurable yet — always the built-in
/// public proxy (whose trial key is `guest`); the operator may still override the
/// key at the prompt. Both the chosen endpoint and key are recorded on the device
/// row at pairing so the gateway re-uses them for its relay/push connections.
fn relay_endpoint() -> (String, String) {
    (DEFAULT_GATEWAY_ENDPOINT.to_string(), "guest".to_string())
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
        let mut buf = String::from("STATUS\tDEVICE\tRENDEZVOUS\tCREATED_AT\n");
        for r in &rows {
            buf.push_str(&format!(
                "{}\t{}\t{}\t{}\n",
                status_str(r),
                r.device_id,
                r.rendezvous_id.as_deref().unwrap_or("-"),
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
                    "device_id": r.device_id,
                    "rendezvous_id": r.rendezvous_id,
                    "created_at": r.created_at,
                    "approved_at": r.approved_at,
                    "last_seen_at": r.last_seen_at,
                }))
                .collect::<Vec<_>>(),
        }),
    ))
}

async fn revoke(ctx: &CommandContext, device_id: String, yes: bool) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && !yes {
        return Err(CliError::Config(
            "pass --yes to revoke a device from a chat channel".into(),
        ));
    }
    let svc = require_service(ctx)?;
    let changed = svc
        .revoke(&device_id)
        .await
        .map_err(|e| CliError::Manager(format!("revoke device: {e}")))?;
    let human = if changed {
        format!("Revoked device {device_id} (row retained for audit).")
    } else {
        format!("No live device {device_id} to revoke.")
    };
    Ok(CommandOutput::structured(
        human,
        &json!({
            "action": "revoked",
            "changed": changed,
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
        // A realistic payload: relay endpoint, a UUID rendezvous id, a 64-hex
        // (256-bit) secret, and the admission key.
        let payload = format!(
            "baybo://pair?h=wss://proxy.baybo.space&r=11111111-2222-4333-8444-555555555555&s={}&k=guest",
            "ab".repeat(32),
        );
        let qr = render_pairing_qr(&payload).expect("the pairing payload fits a QR");
        assert!(qr.lines().count() > 8, "multi-line QR matrix");
        assert!(
            qr.contains('█') || qr.contains('▀') || qr.contains('▄'),
            "rendered with half-block glyphs"
        );
    }
}
