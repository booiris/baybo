//! Per-bot encrypted secret namespace exposed to sidecars via the
//! `Frame::SecretRequest` / `Frame::SecretReply` pair, plus the
//! gateway-side helper that merges a bot's plaintext config with its
//! vault-encrypted `config.*` credentials before `Frame::StartBot`.

use std::collections::HashMap;
use std::sync::Arc;

use aura_channels::vault_keys;
use aura_channels::wire::{Frame, SecretOp, secret_limits};
use aura_model::ChannelType;
use aura_security::SecretVault;
use aura_storage::ChannelBotRow;

use super::state::WsChannelState;

const ERR_BOT_UNKNOWN: &str = "bot_unknown";
const ERR_KEY_TOO_LONG: &str = "key_too_long";
const ERR_KEY_REQUIRED: &str = "key_required";
const ERR_VALUE_REQUIRED: &str = "value_required";
const ERR_VALUE_TOO_LARGE: &str = "value_too_large";
const ERR_QUOTA_EXCEEDED: &str = "quota_exceeded";
const ERR_INTERNAL: &str = "internal";

fn err_reply(request_id: String, error: &'static str) -> Frame {
    Frame::SecretReply {
        request_id,
        ok: false,
        value: None,
        keys: None,
        error: Some(error.into()),
    }
}

fn ok_value(request_id: String, value: Option<String>) -> Frame {
    Frame::SecretReply {
        request_id,
        ok: true,
        value,
        keys: None,
        error: None,
    }
}

fn ok_keys(request_id: String, keys: Vec<String>) -> Frame {
    Frame::SecretReply {
        request_id,
        ok: true,
        value: None,
        keys: Some(keys),
        error: None,
    }
}

fn ok_empty(request_id: String) -> Frame {
    Frame::SecretReply {
        request_id,
        ok: true,
        value: None,
        keys: None,
        error: None,
    }
}

/// Merge a bot's plaintext config with its `config.*` vault entries
/// into the single map the sidecar receives on
/// `Frame::StartBot.metadata`. Vault values shadow plaintext on key
/// collision so a credential can't accidentally regress to plaintext.
///
/// A single decrypt failure drops only that key from the merged map
/// — the remaining values still let a partial-feature sidecar start
/// (e.g. Lark read flows still work without `verification_token`); the
/// operator gets the warning to investigate.
pub(crate) async fn load_start_metadata(
    vault: &Arc<SecretVault>,
    row: &ChannelBotRow,
) -> HashMap<String, String> {
    let mut merged = row.metadata.clone();
    let prefix = vault_keys::config_prefix(&row.channel_type, &row.bot_id);
    let names = match vault.list_secrets_with_prefix(&prefix).await {
        Ok(names) => names,
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                channel_type = %row.channel_type,
                bot_id = %row.bot_id,
                "list config secrets for StartBot failed; continuing with plaintext only",
            );
            return merged;
        }
    };
    for name in names {
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        match vault.get_secret(&name).await {
            Ok(Some(value)) => match String::from_utf8(value.as_bytes().to_vec()) {
                Ok(plaintext) => {
                    merged.insert(rest.to_owned(), plaintext);
                }
                Err(_) => tracing::warn!(
                    %name,
                    "config secret was not valid UTF-8; omitting from StartBot metadata",
                ),
            },
            Ok(None) => {
                tracing::debug!(%name, "config secret listed but missing on read; skipping");
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    %name,
                    "decrypt config secret failed; omitting from StartBot metadata",
                );
            }
        }
    }
    merged
}

pub(crate) async fn handle_request(
    state: &WsChannelState,
    channel_type: &ChannelType,
    request_id: String,
    bot_id: String,
    op: SecretOp,
    key: Option<String>,
    value: Option<String>,
) -> Frame {
    match state.channel_bot_store.get(channel_type, &bot_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return err_reply(request_id, ERR_BOT_UNKNOWN),
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                %channel_type,
                %bot_id,
                "secret request: bot lookup failed",
            );
            return err_reply(request_id, ERR_INTERNAL);
        }
    }

    match op {
        SecretOp::Get => match validate_key(key.as_deref()) {
            Ok(()) => {
                let name = vault_keys::user(channel_type, &bot_id, key.as_deref().unwrap_or(""));
                match state.secret_vault.get_secret(&name).await {
                    Ok(Some(v)) => match String::from_utf8(v.as_bytes().to_vec()) {
                        Ok(plaintext) => ok_value(request_id, Some(plaintext)),
                        Err(_) => err_reply(request_id, ERR_INTERNAL),
                    },
                    Ok(None) => ok_value(request_id, None),
                    Err(e) => {
                        tracing::warn!(
                            error = %format!("{e:#}"),
                            %channel_type,
                            %bot_id,
                            "secret get failed",
                        );
                        err_reply(request_id, ERR_INTERNAL)
                    }
                }
            }
            Err(tag) => err_reply(request_id, tag),
        },
        SecretOp::Set => {
            if let Err(tag) = validate_key(key.as_deref()) {
                return err_reply(request_id, tag);
            }
            let Some(value) = value else {
                return err_reply(request_id, ERR_VALUE_REQUIRED);
            };
            if value.len() > secret_limits::MAX_VALUE_BYTES {
                return err_reply(request_id, ERR_VALUE_TOO_LARGE);
            }
            let name = vault_keys::user(channel_type, &bot_id, key.as_deref().unwrap_or(""));
            // Quota is enforced before insert. The check is racy with
            // concurrent writers from the same sidecar but the point
            // is short-circuit a runaway loop, not strict accounting.
            let already_exists = matches!(state.secret_vault.get_secret(&name).await, Ok(Some(_)));
            if !already_exists
                && let Some(over) =
                    scope_quota_check(&state.secret_vault, channel_type, &bot_id).await
            {
                return err_reply(request_id, over);
            }
            match state
                .secret_vault
                .store_secret(&name, value.as_bytes())
                .await
            {
                Ok(()) => ok_empty(request_id),
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        %channel_type,
                        %bot_id,
                        "secret set failed",
                    );
                    err_reply(request_id, ERR_INTERNAL)
                }
            }
        }
        SecretOp::Delete => match validate_key(key.as_deref()) {
            Ok(()) => {
                let name = vault_keys::user(channel_type, &bot_id, key.as_deref().unwrap_or(""));
                match state.secret_vault.delete_secret(&name).await {
                    Ok(()) => ok_empty(request_id),
                    Err(e) => {
                        tracing::warn!(
                            error = %format!("{e:#}"),
                            %channel_type,
                            %bot_id,
                            "secret delete failed",
                        );
                        err_reply(request_id, ERR_INTERNAL)
                    }
                }
            }
            Err(tag) => err_reply(request_id, tag),
        },
        SecretOp::List => {
            let prefix = vault_keys::user_prefix(channel_type, &bot_id);
            match state.secret_vault.list_secrets_with_prefix(&prefix).await {
                Ok(names) => {
                    let keys = names
                        .into_iter()
                        .filter_map(|name| name.strip_prefix(&prefix).map(str::to_owned))
                        .collect();
                    ok_keys(request_id, keys)
                }
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        %channel_type,
                        %bot_id,
                        "secret list failed",
                    );
                    err_reply(request_id, ERR_INTERNAL)
                }
            }
        }
    }
}

fn validate_key(key: Option<&str>) -> Result<(), &'static str> {
    let Some(key) = key else {
        return Err(ERR_KEY_REQUIRED);
    };
    if key.len() > secret_limits::MAX_KEY_BYTES {
        return Err(ERR_KEY_TOO_LONG);
    }
    Ok(())
}

async fn scope_quota_check(
    vault: &Arc<SecretVault>,
    channel_type: &ChannelType,
    bot_id: &str,
) -> Option<&'static str> {
    let prefix = vault_keys::user_prefix(channel_type, bot_id);
    match vault.list_secrets_with_prefix(&prefix).await {
        Ok(names) if names.len() >= secret_limits::MAX_KEYS_PER_SCOPE => Some(ERR_QUOTA_EXCEEDED),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                %channel_type,
                %bot_id,
                "scope quota check failed; allowing through",
            );
            None
        }
    }
}
