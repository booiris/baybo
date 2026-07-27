//! Shared dial path for CLI commands that act as a WS channel client of
//! a running `baybo gateway` — the interactive TUI (`baybo tui`) and the
//! headless one-shot prompt (`baybo prompt`).
//!
//! Both connect the same way: resolve the gateway's admin listener from
//! config, read the per-start TUI token the gateway published to the
//! secret vault, and dial [`WsTransport`] against `/v1/channel-ws`. The
//! token is bound to the built-in `tui` channel label, which is what the
//! admin listener's co-hosted auth middleware admits alongside the web
//! chat's admin bearer and a paired device's bearer. Keeping the
//! resolution + token-read + connect logic here means the two
//! entrypoints can't drift on bind-address rewriting or on which dial
//! failure earns which operator-facing message — see
//! [`dial_failure_error`], the one place that decides.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use baybo_channels::ChannelError;
use baybo_config::BayboConfig;
use baybo_gateway::TUI_TOKEN_VAULT_KEY;
use baybo_tui::client::WsTransport;

/// Resolve the admin listener address from the loaded config. When the
/// gateway is bound to a wildcard interface (`0.0.0.0` / `::`), a
/// same-host client rewrites it to loopback — the wildcard is a server-
/// side bind directive, not a dialable target.
pub fn admin_addr_from_config(config: &BayboConfig) -> anyhow::Result<SocketAddr> {
    let host = config.gateway.bind_address.as_str();
    let ip: IpAddr = host
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid gateway.bind_address {host:?}: {e}"))?;
    let dial_ip = match ip {
        IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        other => other,
    };
    Ok(SocketAddr::new(dial_ip, config.gateway.port))
}

/// Best-effort read of the per-start TUI token from the secret vault.
/// Returns `None` if the vault can't be opened (no encryption key,
/// sqlite missing) or the key isn't present yet — both surface to the
/// caller as the same "no live gateway" fallback path. A loud error
/// would only mask the more specific connect-failure message that the
/// dial attempt produces a moment later.
pub async fn read_tui_token(config: &BayboConfig) -> Option<String> {
    let vault = match crate::runtime::build_secret_vault(config).await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "tui token: open vault failed");
            return None;
        }
    };
    match vault.get_secret(TUI_TOKEN_VAULT_KEY).await {
        Ok(Some(value)) => match std::str::from_utf8(value.as_bytes()) {
            Ok(s) => Some(s.to_owned()),
            Err(e) => {
                tracing::warn!(error = %e, "tui token in vault is not valid utf-8");
                None
            }
        },
        Ok(None) => {
            tracing::debug!("tui token: vault key absent (gateway not started yet)");
            None
        }
        Err(e) => {
            tracing::debug!(error = %e, "tui token: vault read failed");
            None
        }
    }
}

/// Dial the gateway's admin listener with the supplied TUI token and pin
/// the connection to `session_id`. A missing token (vault key absent /
/// gateway not running yet) is surfaced as [`ChannelError::NotReachable`]
/// so callers' fallback paths (dev auto-gateway, user-facing error)
/// cover it the same way as a connection refusal.
pub async fn try_connect_with_token(
    admin_addr: SocketAddr,
    tui_token: Option<&str>,
    session_id: &baybo_model::SessionId,
) -> Result<WsTransport, ChannelError> {
    let token = tui_token.ok_or_else(|| {
        ChannelError::NotReachable(format!(
            "no {TUI_TOKEN_VAULT_KEY} in vault (is the gateway running?)",
        ))
    })?;
    WsTransport::connect(admin_addr, token.to_owned(), session_id.clone()).await
}

/// User-facing error for an unreachable gateway, with the one command
/// that fixes it.
pub fn unreachable_gateway_error(admin_addr: SocketAddr, underlying: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "no baybo gateway reachable at {admin_addr}\n  - start it with:       baybo gateway start\n  (underlying error: {underlying})"
    )
}

const REJECTED_TOKEN_HELP: &str = r#"gateway at {{addr}} is running, but it rejected our TUI token
  - the gateway mints a fresh TUI token on every start and publishes it to vault key `{{vault_key}}`
  - most likely it restarted after this client read the token, or it serves a different workspace
  - fix: restart the gateway so both ends hold the same token, then retry
  (underlying error: {{underlying}})"#;

const HANDSHAKE_FAILED_HELP: &str = r#"gateway at {{addr}} answered, but the channel handshake failed
  - the gateway is listening, so this is not a "not running" problem
  - check the gateway log for the matching reject, then retry
  (underlying error: {{underlying}})"#;

/// Turn a dial failure into the right operator-facing message. Every
/// caller routes through here so "unreachable" is only ever printed for
/// an endpoint that genuinely is not listening.
pub fn dial_failure_error(admin_addr: SocketAddr, err: &ChannelError) -> anyhow::Error {
    match err {
        ChannelError::NotReachable(underlying) => unreachable_gateway_error(admin_addr, underlying),
        ChannelError::Unauthorized(underlying) => anyhow::anyhow!(
            REJECTED_TOKEN_HELP
                .replace("{{addr}}", &admin_addr.to_string())
                .replace("{{vault_key}}", TUI_TOKEN_VAULT_KEY)
                .replace("{{underlying}}", underlying)
        ),
        other => anyhow::anyhow!(
            HANDSHAKE_FAILED_HELP
                .replace("{{addr}}", &admin_addr.to_string())
                .replace("{{underlying}}", &other.to_string())
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> SocketAddr {
        "127.0.0.1:8888".parse().expect("literal addr")
    }

    #[test]
    fn not_reachable_keeps_the_start_the_gateway_advice() {
        let msg = dial_failure_error(addr(), &ChannelError::NotReachable("dial: refused".into()))
            .to_string();
        assert!(
            msg.contains("no baybo gateway reachable at 127.0.0.1:8888"),
            "{msg}"
        );
        assert!(msg.contains("baybo gateway start"), "{msg}");
    }

    /// The whole point of the [`ChannelError::Unauthorized`] split: a live
    /// gateway that refused our credential must never be reported as an
    /// absent one, or the operator is sent to start a process already running.
    #[test]
    fn unauthorized_reports_a_running_gateway_not_a_missing_one() {
        let msg =
            dial_failure_error(addr(), &ChannelError::Unauthorized("HTTP 401".into())).to_string();
        assert!(
            msg.contains("is running, but it rejected our TUI token"),
            "{msg}"
        );
        assert!(msg.contains(TUI_TOKEN_VAULT_KEY), "{msg}");
        assert!(
            !msg.contains("no baybo gateway reachable") && !msg.contains("start it with"),
            "must not claim the gateway is absent: {msg}"
        );
    }

    #[test]
    fn other_errors_say_the_endpoint_answered() {
        let msg =
            dial_failure_error(addr(), &ChannelError::Config("bad handshake".into())).to_string();
        assert!(
            msg.contains("answered, but the channel handshake failed"),
            "{msg}"
        );
        assert!(msg.contains("bad handshake"), "{msg}");
        assert!(!msg.contains("no baybo gateway reachable"), "{msg}");
    }
}
