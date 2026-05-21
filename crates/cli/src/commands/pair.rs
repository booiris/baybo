//! `aura pair` subcommand family.
//!
//! Surfaces the per-user pairing gate to operators. See
//! `docs/modules/pairing.md` for the gate's semantics.

use std::sync::Arc;

use aura_model::ChannelType;
use aura_storage::retry::retry_on_busy;
use aura_store::{ChannelPairingRow, ChannelPairingStore, PairingStatus};
use chrono::Utc;
use serde_json::json;

use crate::cli::PairCmd;
use crate::context::{CommandContext, Invocation};
use crate::error::{CliError, Result};
use crate::format::CommandOutput;

pub async fn handle(ctx: &CommandContext, cmd: PairCmd) -> Result<CommandOutput> {
    match cmd {
        PairCmd::List { pending, approved } => list(ctx, pending, approved).await,
        PairCmd::Approve { code } => approve(ctx, code).await,
        PairCmd::Revoke {
            channel_type,
            bot_id,
            user_id,
            yes,
        } => revoke(ctx, channel_type, bot_id, user_id, yes).await,
    }
}

fn require_store(ctx: &CommandContext) -> Result<&Arc<dyn ChannelPairingStore>> {
    ctx.channel_pairing_store.as_ref().ok_or_else(|| {
        CliError::Config(
            "pairing store unavailable — run from the workspace root with a valid aura.json".into(),
        )
    })
}

async fn list(ctx: &CommandContext, pending: bool, approved: bool) -> Result<CommandOutput> {
    let store = require_store(ctx)?;
    let filter = if pending {
        Some(PairingStatus::Pending)
    } else if approved {
        Some(PairingStatus::Approved)
    } else {
        None
    };
    let rows = retry_on_busy("channel_pairings.list", || async {
        store.list(filter).await
    })
    .await
    .map_err(|e| CliError::Manager(format!("list pairings: {e}")))?;
    let now = Utc::now().timestamp();
    let human = if rows.is_empty() {
        "(no pairings)".to_string()
    } else {
        let mut buf = String::from("STATUS\tCHANNEL\tBOT\tUSER\tCODE\tCREATED_AT\tEXPIRES_AT\n");
        for r in &rows {
            let status = render_status(r, now);
            let exp = r
                .expires_at
                .map(|e| e.to_string())
                .unwrap_or_else(|| "-".to_string());
            buf.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                status,
                r.channel_type,
                if r.bot_id.is_empty() { "-" } else { &r.bot_id },
                r.user_id,
                r.code,
                r.created_at,
                exp,
            ));
        }
        buf.trim_end().to_string()
    };
    Ok(CommandOutput::structured(
        human,
        &json!({
            "pairings": rows
                .iter()
                .map(|r| json!({
                    "status": render_status(r, now),
                    "channel_type": r.channel_type.as_str(),
                    "bot_id": r.bot_id,
                    "user_id": r.user_id,
                    "code": r.code,
                    "created_at": r.created_at,
                    "expires_at": r.expires_at,
                    "approved_at": r.approved_at,
                }))
                .collect::<Vec<_>>(),
        }),
    ))
}

async fn approve(ctx: &CommandContext, code: String) -> Result<CommandOutput> {
    let store = require_store(ctx)?;
    let now = Utc::now().timestamp();
    let row = retry_on_busy("channel_pairings.approve", || async {
        store.approve_by_code(&code, now).await
    })
    .await
    .map_err(|e| CliError::Manager(format!("approve pairing: {e}")))?;
    match row {
        Some(r) => {
            let human = format!(
                "Approved pairing for {}:{}:{} (code {}).",
                r.channel_type,
                if r.bot_id.is_empty() { "-" } else { &r.bot_id },
                r.user_id,
                r.code,
            );
            Ok(CommandOutput::structured(
                human,
                &json!({
                    "action": "approved",
                    "channel_type": r.channel_type.as_str(),
                    "bot_id": r.bot_id,
                    "user_id": r.user_id,
                    "code": r.code,
                    "approved_at": r.approved_at,
                }),
            ))
        }
        None => Err(CliError::Config(format!(
            "code '{code}' not found (it may have expired, been approved already, or been revoked)"
        ))),
    }
}

async fn revoke(
    ctx: &CommandContext,
    channel_type: String,
    bot_id: String,
    user_id: String,
    yes: bool,
) -> Result<CommandOutput> {
    if ctx.invocation == Invocation::Slash && !yes {
        return Err(CliError::Config(
            "pass --yes to revoke a pairing from a chat channel".into(),
        ));
    }
    let store = require_store(ctx)?;
    let ct = ChannelType::from(channel_type.as_str());
    let ct_for_delete = ct.clone();
    let bot_owned = bot_id.clone();
    let uid_owned = user_id.clone();
    retry_on_busy("channel_pairings.delete", || {
        let ct = ct_for_delete.clone();
        let bot = bot_owned.clone();
        let uid = uid_owned.clone();
        async move { store.delete(&ct, &bot, &uid).await }
    })
    .await
    .map_err(|e| CliError::Manager(format!("revoke pairing: {e}")))?;
    let human = format!(
        "Revoked pairing for {}:{}:{}.",
        ct.as_str(),
        if bot_id.is_empty() {
            "-"
        } else {
            bot_id.as_str()
        },
        user_id,
    );
    Ok(CommandOutput::structured(
        human,
        &json!({
            "action": "revoked",
            "channel_type": ct.as_str(),
            "bot_id": bot_id,
            "user_id": user_id,
        }),
    ))
}

fn render_status(row: &ChannelPairingRow, now: i64) -> &'static str {
    match row.status {
        PairingStatus::Approved => "APPROVED",
        PairingStatus::Pending if row.is_expired(now) => "EXPIRED",
        PairingStatus::Pending => "PENDING",
    }
}
