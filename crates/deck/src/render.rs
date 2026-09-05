//! The gate's frontend half.
//!
//! `service.js` has always been booted for real and its result checked, while
//! `card.html` was only read, capped and hashed (`bundle.rs`) — so the half of
//! a bundle the user actually looks at was the half nothing verified, and the
//! authoring agent shipped it without ever observing it. Two shipped defects
//! came through that gap: a card that rendered every metric as a placeholder,
//! and one whose maximized view could not scroll.
//!
//! This runs the card's own script against the card's own first snapshot,
//! through the real [`sdkCard.js`] the client injects, and answers the one
//! question a JSON schema cannot: **did handing this card its data change
//! anything it displays?** A card that throws, that reads a field its service
//! never emits, or that never wires `deck.onData` fails that question
//! identically — and all three look on the phone like a page of dashes.
//!
//! It is not a browser. No layout, no cascade, no paint: it cannot judge
//! whether a card looks right, only whether it responded at all. Layout stays
//! the client's problem, and a card is never failed for it here.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;

use crate::bundle::DeckBundle;
use crate::error::{DeckError, Result};

const RENDER_CHECK_JS: &str = include_str!("sdk/render_check.js");
const RENDER_CHECK_FILE: &str = "render_check.js";
/// The card SDK the iOS shell injects ahead of every fragment. Included from
/// the client's own source rather than vendored, so the gate can never drift
/// into checking cards against an SDK the client no longer ships — a copy
/// would rot silently, and a moved file is a compile error instead.
const CARD_SDK_JS: &str = include_str!("../../../app/mobile/web/src/deck/sdkCard.js");
const CARD_SDK_FILE: &str = "sdkCard.js";
/// Outer bound. The harness stops itself at 10s; this only covers a bun that
/// never gets that far.
const RENDER_TIMEOUT: Duration = Duration::from_secs(30);
/// Verdict errors quoted back to the authoring agent.
const MAX_REPORTED_ERRORS: usize = 3;

#[derive(Debug, Deserialize)]
struct Verdict {
    ok: bool,
    #[serde(default)]
    errors: Vec<String>,
    #[serde(default)]
    changed_count: usize,
    #[serde(default)]
    missing_ids: Vec<String>,
}

/// Run the check. A card that demonstrably fails to render its own data is
/// rejected with an agent-facing reason; anything that goes wrong with the
/// CHECK ITSELF (no bun, a crash, unreadable output) is logged and passes —
/// a host fault is not a card fault (`docs/modules/deck.md`), and this must
/// never be the reason a working card cannot be installed.
pub(crate) async fn check_renders_its_data(
    bundle: &DeckBundle,
    snapshot: &Value,
    scratch_dir: &Path,
    process_manager: &Arc<baybo_process::ProcessManager>,
    card_id: &str,
) -> Result<()> {
    let verdict = match run(bundle, snapshot, scratch_dir, process_manager, card_id).await {
        Ok(verdict) => verdict,
        Err(e) => {
            tracing::warn!(card = %card_id, "deck: render check unavailable, skipping: {e}");
            return Ok(());
        }
    };
    if verdict.ok {
        return Ok(());
    }

    let mut reason = String::from("card.html did not render its own first snapshot");
    if !verdict.errors.is_empty() {
        reason.push_str(" — ");
        reason
            .push_str(&verdict.errors[..verdict.errors.len().min(MAX_REPORTED_ERRORS)].join("; "));
    } else if verdict.changed_count == 0 {
        reason.push_str(
            " — delivering the snapshot changed nothing on the page, so every element is still \
             showing its placeholder. Check that the card calls `deck.onData`, and that the \
             fields it reads are the ones `service.js` actually emits",
        );
    }
    if !verdict.missing_ids.is_empty() {
        reason.push_str(&format!(
            ". The script also looked up ids that the markup does not define: {}",
            verdict.missing_ids.join(", ")
        ));
    }
    Err(DeckError::DryRun(reason))
}

async fn run(
    bundle: &DeckBundle,
    snapshot: &Value,
    scratch_dir: &Path,
    process_manager: &Arc<baybo_process::ProcessManager>,
    card_id: &str,
) -> std::result::Result<Verdict, String> {
    std::fs::create_dir_all(scratch_dir).map_err(|e| format!("scratch dir: {e}"))?;
    let harness = scratch_dir.join(RENDER_CHECK_FILE);
    let sdk = scratch_dir.join(CARD_SDK_FILE);
    std::fs::write(&harness, RENDER_CHECK_JS).map_err(|e| format!("write harness: {e}"))?;
    std::fs::write(&sdk, CARD_SDK_JS).map_err(|e| format!("write sdk: {e}"))?;

    let mut sizes: Vec<String> = bundle
        .sizes
        .iter()
        .map(|s| s.as_str().to_string())
        .collect();
    if bundle.maximize {
        sizes.push("max".to_string());
    }

    let bun = baybo_process::HostTool::bun();
    let mut cmd = Command::new(bun.path());
    cmd.arg(&harness)
        .arg(bundle.dir.join(crate::bundle::CARD_FILE))
        .arg(&sdk)
        .env("DECK_RENDER_SIZE", bundle.manifest.size.as_str())
        .env("DECK_RENDER_SIZES", sizes.join(","))
        .current_dir(scratch_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = process_manager
        .spawn(&mut cmd, format!("deck-render-check:{card_id}"))
        .map_err(|e| bun.launch_failure(&e))?;
    if let Some(mut stdin) = child.take_stdin() {
        use tokio::io::AsyncWriteExt;
        let payload = snapshot.to_string();
        let _ = stdin.write_all(payload.as_bytes()).await;
        let _ = stdin.shutdown().await;
    }

    let out = tokio::time::timeout(RENDER_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| format!("render check timed out after {RENDER_TIMEOUT:?}"))?
        .map_err(|e| format!("render check: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .ok_or_else(|| {
            format!(
                "render check produced no verdict (stderr: {})",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        })?;
    serde_json::from_str::<Verdict>(line).map_err(|e| format!("render check verdict: {e}"))
}
