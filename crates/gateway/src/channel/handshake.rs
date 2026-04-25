//! Pure handshake validator for the WS channel server. Separated
//! from the route handler so it can be unit-tested without spinning
//! a TCP listener.

use aura_channels::wire::{Frame, PROTOCOL_VERSION};
use aura_model::ChannelType;

use crate::auth::{AuthedClient, ChannelTokenTable};

/// Channel type strings reserved for in-process adapters. Sidecars may
/// not claim these — they would shadow the real adapter. `tui` is not
/// reserved here because the bundled TUI registers as that exact
/// channel type via the vault-token auth path; subprocess sidecars are
/// blocked from claiming `"tui"` further down inside `validate_register`.
const RESERVED_CHANNEL_TYPES: &[&str] = &[ChannelType::HTTP];

/// Validate the first frame received on a `/v1/channel-ws` upgrade and
/// produce the `ChannelType` the sidecar is registering as.
///
/// `authed` is the [`AuthedClient`] the auth middleware already attached
/// to the request via [`ChannelAuthState`](crate::auth::channel::ChannelAuthState).
/// `tokens` is the live capability table — we consult it to confirm the
/// `Register.token` that a subprocess embedded in the frame names the
/// same identity that the header-based auth already validated. The
/// built-in TUI authenticates with the vault-issued TUI token, which
/// the middleware already verified, so its `Register.token` field is
/// ignored and it must claim the `"tui"` channel type.
/// Outcome of a successful Register handshake.
///
/// `session_id` is `Some` only for session-scoped clients (today: the
/// built-in TUI). Sidecars leave it `None` and register as
/// type-level channels — the registry enforces the historical 1:1
/// `ChannelType → Channel` mapping for those.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegisterOutcome {
    pub channel_type: ChannelType,
    pub session_id: Option<String>,
}

pub(crate) fn validate_register(
    frame: Frame,
    authed: &AuthedClient,
    tokens: &ChannelTokenTable,
) -> Result<RegisterOutcome, String> {
    let Frame::Register {
        token,
        channel_type,
        protocol_version,
        session_id,
    } = frame
    else {
        return Err("expected Register frame".to_string());
    };

    if protocol_version != PROTOCOL_VERSION {
        return Err(format!(
            "protocol version mismatch: server {PROTOCOL_VERSION}, client {protocol_version}"
        ));
    }

    let normalized = channel_type.as_str().trim().to_string();
    if normalized.is_empty() {
        return Err("channel_type must not be empty".to_string());
    }
    if !normalized.is_ascii() {
        return Err("channel_type must be ascii".to_string());
    }

    match authed {
        AuthedClient::Tui => {
            if normalized != ChannelType::TUI {
                return Err(format!(
                    "tui token must register as channel_type '{}', got '{normalized}'",
                    ChannelType::TUI
                ));
            }
            if session_id.as_deref().is_none_or(str::is_empty) {
                return Err(
                    "tui clients must declare a session_id in the Register frame".to_string(),
                );
            }
        }
        AuthedClient::Subprocess { pid, label } => {
            let identity = tokens
                .lookup(&token)
                .ok_or_else(|| "token not registered".to_string())?;
            if identity.pid != *pid || identity.label != *label {
                return Err("token identity mismatch".to_string());
            }
            // Token is bound to a single channel_type at mint time
            // (see `ChannelSpawner::spawn`). A compromised sidecar
            // must not be able to claim a different channel and
            // collect that channel's bot secrets.
            if let Some(bound) = identity.bound_channel_type.as_deref()
                && bound != normalized
            {
                return Err(format!(
                    "token bound to channel_type '{bound}', cannot register as '{normalized}'",
                ));
            }
            if RESERVED_CHANNEL_TYPES.iter().any(|r| *r == normalized) {
                return Err(format!("channel_type '{normalized}' is reserved"));
            }
            if normalized == ChannelType::TUI {
                return Err("channel_type 'tui' is reserved for the built-in TUI".to_string());
            }
            // Subprocess sidecars are type-scoped, not session-scoped.
            // Letting them self-supply a session_id would (a) bypass
            // the pairing gate and (b) let them attach to or impersonate
            // any session whose id they can guess. The Register frame
            // must therefore leave session_id empty.
            if session_id.as_deref().is_some_and(|s| !s.trim().is_empty()) {
                return Err(
                    "subprocess sidecars must not declare a session_id in Register".to_string(),
                );
            }
        }
    }

    let session_id = session_id.and_then(|s| {
        let trimmed = s.trim().to_owned();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    });

    Ok(RegisterOutcome {
        channel_type: ChannelType::from(normalized),
        session_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::ClientIdentity;
    use aura_channels::wire::Message as WireMessage;

    fn subprocess(pid: u32, label: &str) -> AuthedClient {
        AuthedClient::Subprocess {
            pid,
            label: label.to_string(),
        }
    }

    fn register(token: &str, channel_type: &str, version: u16) -> Frame {
        register_with_session(token, channel_type, version, None)
    }

    fn register_with_session(
        token: &str,
        channel_type: &str,
        version: u16,
        session_id: Option<&str>,
    ) -> Frame {
        Frame::Register {
            token: token.to_string(),
            channel_type: ChannelType::from(channel_type),
            protocol_version: version,
            session_id: session_id.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn accepts_open_channel_type() {
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 42,
            label: "slack".into(),
            bound_channel_type: Some("slack".into()),
        });
        let frame = register(handle.token(), "slack", PROTOCOL_VERSION);
        let authed = subprocess(42, "slack");
        let outcome = validate_register(frame, &authed, &tokens).unwrap();
        assert_eq!(outcome.channel_type.as_str(), "slack");
        assert!(outcome.session_id.is_none(), "sidecars register type-level");
    }

    #[test]
    fn rejects_wrong_protocol_version() {
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 1,
            label: "slack".into(),
            bound_channel_type: Some("slack".into()),
        });
        let frame = register(handle.token(), "slack", PROTOCOL_VERSION + 1);
        let authed = subprocess(1, "slack");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert!(err.contains("protocol version"));
    }

    #[test]
    fn rejects_unknown_token() {
        let tokens = ChannelTokenTable::new();
        let frame = register("deadbeef", "slack", PROTOCOL_VERSION);
        let authed = subprocess(1, "slack");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert_eq!(err, "token not registered");
    }

    #[test]
    fn rejects_pid_mismatch() {
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 42,
            label: "slack".into(),
            bound_channel_type: Some("slack".into()),
        });
        let frame = register(handle.token(), "slack", PROTOCOL_VERSION);
        let authed = subprocess(999, "slack");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert_eq!(err, "token identity mismatch");
    }

    #[test]
    fn rejects_label_mismatch() {
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 42,
            label: "slack".into(),
            bound_channel_type: Some("slack".into()),
        });
        let frame = register(handle.token(), "slack", PROTOCOL_VERSION);
        let authed = subprocess(42, "discord");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert_eq!(err, "token identity mismatch");
    }

    #[test]
    fn accepts_tui_auth_claiming_tui_channel() {
        let tokens = ChannelTokenTable::new();
        let frame = register_with_session("", ChannelType::TUI, PROTOCOL_VERSION, Some("sess-123"));
        let outcome = validate_register(frame, &AuthedClient::Tui, &tokens).unwrap();
        assert_eq!(outcome.channel_type.as_str(), ChannelType::TUI);
        assert_eq!(outcome.session_id.as_deref(), Some("sess-123"));
    }

    #[test]
    fn rejects_tui_auth_without_session_id() {
        let tokens = ChannelTokenTable::new();
        let frame = register("", ChannelType::TUI, PROTOCOL_VERSION);
        let err = validate_register(frame, &AuthedClient::Tui, &tokens).unwrap_err();
        assert!(err.contains("must declare a session_id"));
    }

    #[test]
    fn rejects_tui_auth_claiming_other_channel() {
        let tokens = ChannelTokenTable::new();
        let frame = register("", "slack", PROTOCOL_VERSION);
        let err = validate_register(frame, &AuthedClient::Tui, &tokens).unwrap_err();
        assert!(err.contains("tui token must register"));
    }

    #[test]
    fn rejects_subprocess_claiming_tui() {
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 1,
            label: "tui".into(),
            bound_channel_type: Some("tui".into()),
        });
        let frame = register(handle.token(), ChannelType::TUI, PROTOCOL_VERSION);
        let authed = subprocess(1, "tui");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert!(err.contains("reserved for the built-in TUI"));
    }

    #[test]
    fn rejects_reserved_channel_types() {
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 1,
            label: "http".into(),
            bound_channel_type: Some("http".into()),
        });
        let frame = register(handle.token(), "http", PROTOCOL_VERSION);
        let authed = subprocess(1, "http");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert!(err.contains("reserved"));
    }

    #[test]
    fn rejects_empty_channel_type() {
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 1,
            label: "slack".into(),
            bound_channel_type: Some("slack".into()),
        });
        let frame = register(handle.token(), "   ", PROTOCOL_VERSION);
        let authed = subprocess(1, "slack");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn rejects_non_register_frame() {
        let tokens = ChannelTokenTable::new();
        let frame = Frame::Message(WireMessage {
            content: String::new(),
            session_id: String::new(),
            user_id: String::new(),
            channel_type: ChannelType::from("slack"),
            bot_id: String::new(),
        });
        let authed = subprocess(1, "slack");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert_eq!(err, "expected Register frame");
    }

    #[test]
    fn rejects_subprocess_claiming_other_bound_channel_type() {
        // Token was minted for "slack" (the supervisor knows the
        // channel_type at spawn time). The sidecar must not be able
        // to register as "discord" and steal that channel's bot
        // secrets.
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 7,
            label: "sidecar-slack".into(),
            bound_channel_type: Some("slack".into()),
        });
        let frame = register(handle.token(), "discord", PROTOCOL_VERSION);
        let authed = subprocess(7, "sidecar-slack");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert!(
            err.contains("bound to channel_type 'slack'") && err.contains("'discord'"),
            "got: {err}",
        );
    }

    #[test]
    fn accepts_subprocess_claiming_bound_channel_type() {
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 7,
            label: "sidecar-slack".into(),
            bound_channel_type: Some("slack".into()),
        });
        let frame = register(handle.token(), "slack", PROTOCOL_VERSION);
        let authed = subprocess(7, "sidecar-slack");
        let outcome = validate_register(frame, &authed, &tokens).unwrap();
        assert_eq!(outcome.channel_type.as_str(), "slack");
        assert!(outcome.session_id.is_none());
    }

    #[test]
    fn rejects_subprocess_supplying_session_id() {
        // Subprocess sidecars are type-scoped, never session-scoped:
        // letting them name a session_id would bypass pairing AND
        // let them attach to any guessable session.
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 7,
            label: "sidecar-slack".into(),
            bound_channel_type: Some("slack".into()),
        });
        let frame = register_with_session(
            handle.token(),
            "slack",
            PROTOCOL_VERSION,
            Some("session-i-want-to-hijack"),
        );
        let authed = subprocess(7, "sidecar-slack");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert!(err.contains("must not declare a session_id"), "got: {err}");
    }

    #[test]
    fn rejects_subprocess_supplying_whitespace_session_id() {
        // Whitespace-only is treated as empty by the trimmer below
        // the auth checks, but we want the explicit rejection to
        // happen *before* trimming so an attacker can't slip a value
        // past the gate by padding it with spaces.
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 7,
            label: "sidecar-slack".into(),
            bound_channel_type: Some("slack".into()),
        });
        let frame = register_with_session(
            handle.token(),
            "slack",
            PROTOCOL_VERSION,
            Some(" some-session "),
        );
        let authed = subprocess(7, "sidecar-slack");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert!(err.contains("must not declare a session_id"), "got: {err}");
    }
}
