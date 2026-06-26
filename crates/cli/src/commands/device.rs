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
const DEFAULT_GATEWAY_ENDPOINT: &str = "wss://proxy.baybo.space";

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
    /// it), so `finish`/`drop` still work afterward. Never resolves when there is
    /// no self-hosted leg (the direct path), so the caller falls back to the slot
    /// poll there.
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

    let (endpoint, relay) = pairing_endpoint(ctx);
    // On the relay path the operator supplies the relay admission key; it's baked
    // into the QR so the app presents it on its join leg (the relay admits both
    // sides). `guest` is the public trial key. Direct pairing carries no key.
    let instance_key = if relay {
        // `guest` is the trial key for the built-in public proxy; only surface it
        // (hint + default) when that's the endpoint. Any other relay must supply
        // its own admission key.
        let (label, default_key) = if endpoint == DEFAULT_GATEWAY_ENDPOINT {
            ("Relay instance key (enter `guest` to try it out)", "guest")
        } else {
            ("Relay instance key", "")
        };
        Some(prompt::prompt_with_default(label, default_key)?)
    } else {
        None
    };
    let payload = match instance_key.as_deref() {
        Some(key) => {
            let k = qr_encode(key);
            format!("baybo://pair?h={endpoint}&c={code}&k={k}&relay=1")
        }
        None => format!("baybo://pair?h={endpoint}&c={code}"),
    };
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

    // Self-contained relay hosting: open our `/pair/host/{code}` leg on the relay
    // and run the SPAKE2 handshake here, so pairing works with no `baybo gateway
    // start` daemon. Runs concurrently with the operator flow below (they sync
    // through the shared slot); the guard stops it on every exit path.
    let mut host = match instance_key.as_deref() {
        Some(key) => {
            let secret_vault = ctx
                .secret_vault
                .clone()
                .ok_or_else(|| CliError::Config("device pairing needs the secret vault".into()))?;
            let deps = baybo_gateway::PairingHostDeps {
                device_pairing: Arc::clone(svc),
                secret_vault,
                relay_url: endpoint.clone(),
                device_direct_candidates: if ctx.config.gateway.direct.enabled {
                    ctx.config.gateway.direct.advertise.clone()
                } else {
                    Vec::new()
                },
                apns_registrar: None,
            };
            let (relay_url, key, code) = (endpoint.clone(), key.to_string(), code.clone());
            Some(StopHosting::new(tokio::spawn(async move {
                if let Err(e) =
                    baybo_gateway::host_pairing_leg(&deps, &relay_url, &key, &code).await
                {
                    tracing::debug!(error = %e, "device pair: relay host ended");
                }
            })))
        }
        None => None,
    };

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
    //    comparison). `confirm` requires a terminal, so this is shell-only. The
    //    prompt is interruptible: if the phone declines (or drops) while the
    //    operator is still deciding, bail out instead of holding the prompt for a
    //    device that's already gone.
    eprintln!("\nA device wants to pair:");
    eprintln!("    name:              {device_label}");
    eprintln!("    device:            {device_id}");
    eprintln!("    confirmation code: {confirm_code}");
    let accepted = match confirm_or_device_gone(
        svc,
        &code,
        "Does this match the code on the phone? Pair this device?",
        host.as_mut(),
    )
    .await?
    {
        ConfirmOutcome::Decided(accepted) => accepted,
        ConfirmOutcome::DeviceGone => {
            return Ok(CommandOutput::structured(
                format!("\"{device_label}\" cancelled pairing on the phone (or the code expired)."),
                &json!({ "action": "device_cancelled", "code": code, "device_id": device_id }),
            ));
        }
    };
    svc.set_operator_decision(&code, accepted)
        .await
        .map_err(|e| CliError::Manager(format!("record pairing decision: {e}")))?;
    if !accepted {
        // Let the self-hosted relay leg deliver the `Reject` to the phone before
        // we exit: the gateway side polls the decision we just wrote, rejects, and
        // the host loop then stops (it sees the decline), so this returns
        // promptly. Without it, dropping the guard would abort the leg mid-flight
        // and the app would see a reset connection instead of "operator declined".
        // No-op on the direct path — a running daemon serves + rejects.
        if let Some(h) = host {
            h.finish().await;
        }
        return Ok(CommandOutput::structured(
            format!("Declined pairing for \"{device_label}\"."),
            &json!({ "action": "declined", "code": code, "device_id": device_id }),
        ));
    }

    // 3. Wait for the gateway to finalize (it requires the phone's confirm too).
    eprintln!("Confirming…");
    let outcome = wait_for_paired(svc, &user, &device_id, &code, OUTCOME_WAIT).await?;
    // Stop our self-hosted relay leg: on success let it finish sending the sealed
    // GatewayWelcome (it goes out right after the approved row `wait_for_paired`
    // just observed); otherwise drop the guard to abort the still-running task.
    match (host, &outcome) {
        (Some(h), PairOutcome::Paired) => h.finish().await,
        (Some(h), _) => drop(h),
        (None, _) => {}
    }
    match outcome {
        PairOutcome::Paired => Ok(CommandOutput::structured(
            format!("Paired \"{device_label}\" ({user}:{device_id})."),
            &json!({
                "action": "paired",
                "user_id": user,
                "device_id": device_id,
                "label": device_label,
            }),
        )),
        PairOutcome::DeviceDeclined => Ok(CommandOutput::structured(
            format!("\"{device_label}\" cancelled pairing on the phone."),
            &json!({ "action": "device_cancelled", "code": code, "device_id": device_id }),
        )),
        PairOutcome::TimedOut => Ok(CommandOutput::structured(
            format!("Pairing for \"{device_label}\" did not complete (timed out)."),
            &json!({ "action": "incomplete", "code": code, "device_id": device_id }),
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
/// drops first — so the operator isn't left holding a `[Y/n]` prompt for a
/// device that has already gone away.
///
/// Three concurrent signals decide the outcome:
/// - the operator's answer (a blocking stdin read on a **detached OS thread** —
///   not `spawn_blocking`, since an uncancellable blocking-pool thread stuck on
///   stdin would keep the tokio runtime from shutting down and hang process exit
///   after we bail; a detached thread is just terminated at exit);
/// - the self-hosted relay leg's task ending (a direct phone-gave-up signal);
/// - a slot poll for the phone-side `device_decision` (the fallback that also
///   covers the direct path, where there is no self-hosted leg to watch).
async fn confirm_or_device_gone(
    svc: &DevicePairingService,
    code: &str,
    question: &str,
    host: Option<&mut StopHosting>,
) -> Result<ConfirmOutcome> {
    use std::io::{BufRead, IsTerminal, Write};

    // This confirm requires a terminal (the command is shell-only).
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(CliError::Config(
            "interactive confirmation requires a terminal".into(),
        ));
    }
    // Write the `[Y/n]` prompt ourselves and release the stderr lock immediately.
    // The blocking read then runs on a detached thread holding *only* the stdin
    // lock, so if we bail (DeviceGone) our `eprintln!` below can't deadlock on a
    // stderr lock the reader is still holding. (`prompt::confirm_with_default`
    // holds stderr across the read, which would hang our bail-out.)
    {
        let mut err = std::io::stderr().lock();
        write!(err, "{question} [Y/n]: ")
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
            // `[Y/n]`: anything but an explicit no (incl. an empty line) is yes.
            Ok(_) => Some(!matches!(
                line.trim().to_ascii_lowercase().as_str(),
                "n" | "no"
            )),
        };
        let _ = tx.send(decision);
    });
    tokio::pin!(rx);
    let host_ended = async move {
        match host {
            Some(h) => h.ended().await,
            None => std::future::pending::<()>().await,
        }
    };
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
                    .claim_slot(code)
                    .await
                    .map_err(|e| CliError::Manager(format!("poll pairing slot: {e}")))?
                {
                    Some(slot) => slot.device_decision == Some(false),
                    None => true, // expired / consumed
                };
                if gone {
                    // End the dangling `[Y/n]` prompt line so the caller's message
                    // starts fresh, then bail (the stdin read is abandoned).
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
    user: &str,
    device_id: &str,
    code: &str,
    budget: Duration,
) -> Result<PairOutcome> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Some(row) = svc
            .device(user, device_id)
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
            .claim_slot(code)
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

/// The endpoint to embed in the pairing QR, and whether it is the relay (so the
/// app joins via `/pair/join/<code>` on the proxy instead of dialing the gateway
/// directly at `/v1/device/pair`). Prefers a configured direct address; else
/// falls back to the built-in default proxy (relay).
fn pairing_endpoint(ctx: &CommandContext) -> (String, bool) {
    let direct = &ctx.config.gateway.direct;
    if direct.enabled
        && let Some(first) = direct.advertise.first()
    {
        return (first.clone(), false);
    }
    (DEFAULT_GATEWAY_ENDPOINT.to_string(), true)
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
        let qr = render_pairing_qr("baybo://pair?h=wss://proxy.baybo.space&c=5PRX2B")
            .expect("the short pairing payload fits a QR");
        assert!(qr.lines().count() > 8, "multi-line QR matrix");
        assert!(
            qr.contains('█') || qr.contains('▀') || qr.contains('▄'),
            "rendered with half-block glyphs"
        );
    }
}
