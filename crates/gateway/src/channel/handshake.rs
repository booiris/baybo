//! Pure handshake validator for the WS channel server. Separated
//! from the route handler so it can be unit-tested without spinning
//! a TCP listener.
//!
//! After the Channel / Connection / Subscription refactor a Register
//! frame names only the channel a connection serves; per-session
//! interest is expressed by later `Subscribe` / `Unsubscribe` frames
//! on the same connection. The validator therefore no longer parses
//! or returns a `session_id`.

use baybo_channels::wire::Frame;
use baybo_model::ChannelType;

use crate::auth::{AuthedClient, ChannelTokenTable, TOOL_CLIENT_LABEL_PREFIX};

/// Channel type strings that subprocess sidecars may not claim — they
/// would shadow an in-process adapter. `"tui"` is enforced separately
/// (only the vault-token TUI auth path may claim it). `"owner"` is reserved
/// so a subprocess sidecar can't register onto the owner's own chat pool —
/// only the web ([`AuthedClient::Web`]) and device ([`AuthedClient::Device`])
/// auth paths may claim it.
const RESERVED_CHANNEL_TYPES: &[&str] = &[ChannelType::OWNER];

/// Outcome of a successful Register handshake. Only the channel type
/// is reported; subscriptions are negotiated post-handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegisterOutcome {
    pub channel_type: ChannelType,
}

pub(crate) fn validate_register(
    frame: Frame,
    authed: &AuthedClient,
    tokens: &ChannelTokenTable,
) -> Result<RegisterOutcome, String> {
    let Frame::Register {
        token,
        channel_type,
    } = frame
    else {
        return Err("expected Register frame".to_string());
    };

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
        }
        // Tool sidecars (browser MCP server today; future code_exec,
        // db_query, …) live in the same `ChannelTokenTable` for
        // `/v1/blobs` auth, but must never register on `/v1/channel-ws`
        // — a leaked tool token would otherwise impersonate a channel.
        AuthedClient::Tool { label } => {
            return Err(format!(
                "label '{label}' is reserved for tool sidecars and may not register on /v1/channel-ws",
            ));
        }
        // Web and device both register onto the single shared `owner` chat
        // pool (their surface identity comes from the auth path — the admin
        // bearer vs the approved device token — not the channel claim). The
        // claim is pinned to `owner` so a leaked web/device token still can't
        // be redirected onto another channel's session stream (tui/telegram).
        AuthedClient::Web => {
            if normalized != ChannelType::OWNER {
                return Err(format!(
                    "web chat must register as channel_type '{}', got '{normalized}'",
                    ChannelType::OWNER,
                ));
            }
        }
        AuthedClient::Device { device_id, .. } => {
            if normalized != ChannelType::OWNER {
                return Err(format!(
                    "device token must register as channel_type '{}', got '{normalized}'",
                    ChannelType::OWNER,
                ));
            }
            let _ = device_id;
        }
        AuthedClient::Subprocess { pid, label, .. } => {
            if label.starts_with(TOOL_CLIENT_LABEL_PREFIX) {
                return Err(format!(
                    "label '{label}' is reserved for tool sidecars and may not register on /v1/channel-ws",
                ));
            }
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
        }
    }

    Ok(RegisterOutcome {
        channel_type: ChannelType::from(normalized),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::ClientIdentity;
    use baybo_channels::MessageRole;
    use baybo_channels::wire::Message as WireMessage;

    fn subprocess(pid: u32, label: &str) -> AuthedClient {
        AuthedClient::Subprocess {
            pid,
            label: label.to_string(),
            channel_type: Some(label.to_string()),
        }
    }

    fn register(token: &str, channel_type: &str) -> Frame {
        Frame::Register {
            token: token.to_string(),
            channel_type: ChannelType::from(channel_type),
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
        let frame = register(handle.token(), "slack");
        let authed = subprocess(42, "slack");
        let outcome = validate_register(frame, &authed, &tokens).unwrap();
        assert_eq!(outcome.channel_type.as_str(), "slack");
    }

    #[test]
    fn rejects_unknown_token() {
        let tokens = ChannelTokenTable::new();
        let frame = register("deadbeef", "slack");
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
        let frame = register(handle.token(), "slack");
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
        let frame = register(handle.token(), "slack");
        let authed = subprocess(42, "discord");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert_eq!(err, "token identity mismatch");
    }

    #[test]
    fn accepts_tui_auth_claiming_tui_channel() {
        let tokens = ChannelTokenTable::new();
        let frame = register("", ChannelType::TUI);
        let outcome = validate_register(frame, &AuthedClient::Tui, &tokens).unwrap();
        assert_eq!(outcome.channel_type.as_str(), ChannelType::TUI);
    }

    #[test]
    fn rejects_tui_auth_claiming_other_channel() {
        let tokens = ChannelTokenTable::new();
        let frame = register("", "slack");
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
        let frame = register(handle.token(), ChannelType::TUI);
        let authed = subprocess(1, "tui");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert!(err.contains("reserved for the built-in TUI"));
    }

    #[test]
    fn rejects_reserved_channel_types() {
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 1,
            label: "owner".into(),
            bound_channel_type: Some("owner".into()),
        });
        let frame = register(handle.token(), "owner");
        let authed = subprocess(1, "owner");
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
        let frame = register(handle.token(), "   ");
        let authed = subprocess(1, "slack");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert!(err.contains("empty"));
    }

    #[test]
    fn rejects_non_register_frame() {
        let tokens = ChannelTokenTable::new();
        let frame = Frame::Message(WireMessage {
            content: String::new(),
            session_id: baybo_model::SessionId::from(""),
            user_id: String::new(),
            channel_type: ChannelType::from("slack"),
            bot_id: String::new(),
            attachments: Vec::new(),
            platform_msg_id: String::new(),
            role: MessageRole::User,
            ordinal: None,
        });
        let authed = subprocess(1, "slack");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert_eq!(err, "expected Register frame");
    }

    #[test]
    fn rejects_subprocess_claiming_other_bound_channel_type() {
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 7,
            label: "sidecar-slack".into(),
            bound_channel_type: Some("slack".into()),
        });
        let frame = register(handle.token(), "discord");
        let authed = subprocess(7, "sidecar-slack");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert!(
            err.contains("bound to channel_type 'slack'") && err.contains("'discord'"),
            "got: {err}",
        );
    }

    #[test]
    fn rejects_subprocess_with_tool_label_on_channel_ws() {
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 0,
            label: "tool/browser".into(),
            bound_channel_type: None,
        });
        let frame = register(handle.token(), "telegram");
        let authed = subprocess(0, "tool/browser");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert!(
            err.contains("tool/browser") && err.contains("reserved for tool sidecars"),
            "got: {err}",
        );
    }

    #[test]
    fn accepts_device_auth_claiming_owner_channel() {
        let tokens = ChannelTokenTable::new();
        let frame = register("", ChannelType::OWNER);
        let authed = AuthedClient::Device {
            device_id: "d1".into(),
        };
        let outcome = validate_register(frame, &authed, &tokens).unwrap();
        assert_eq!(outcome.channel_type.as_str(), ChannelType::OWNER);
    }

    #[test]
    fn rejects_device_auth_claiming_other_channel() {
        let tokens = ChannelTokenTable::new();
        let frame = register("", "telegram");
        let authed = AuthedClient::Device {
            device_id: "d1".into(),
        };
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert!(err.contains("device token must register"), "got: {err}");
    }

    #[test]
    fn rejects_subprocess_claiming_owner() {
        // `owner` is reserved: a sidecar can't register onto the owner's own
        // chat pool, even with a token bound to it.
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 5,
            label: "sidecar-owner".into(),
            bound_channel_type: Some("owner".into()),
        });
        let frame = register(handle.token(), "owner");
        let authed = subprocess(5, "sidecar-owner");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert!(err.contains("reserved"), "got: {err}");
    }

    #[test]
    fn accepts_web_auth_claiming_owner_channel() {
        // The admin listener turns a valid admin bearer into AuthedClient::Web.
        // Web and device auth are the *only* paths allowed to claim the
        // otherwise-reserved "owner" chat channel.
        let tokens = ChannelTokenTable::new();
        let frame = register("", ChannelType::OWNER);
        let authed = AuthedClient::Web;
        let outcome = validate_register(frame, &authed, &tokens).unwrap();
        assert_eq!(outcome.channel_type.as_str(), ChannelType::OWNER);
    }

    #[test]
    fn rejects_web_auth_claiming_other_channel() {
        // Web chat must not be redirectable to telegram (or any
        // other channel). The handshake fixes the channel type for
        // this auth variant.
        let tokens = ChannelTokenTable::new();
        let frame = register("", "telegram");
        let authed = AuthedClient::Web;
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert!(
            err.contains("web chat must register as channel_type"),
            "got: {err}",
        );
    }

    #[test]
    fn rejects_subprocess_claiming_owner_even_with_matching_bound_type() {
        // Reserved-channel-types is a hard floor: even a sidecar
        // whose token was minted with bound_channel_type=Some("owner")
        // can't claim it. Only the web/device auth paths are allowed past
        // the RESERVED gate.
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 99,
            label: "sidecar-owner".into(),
            bound_channel_type: Some("owner".into()),
        });
        let frame = register(handle.token(), "owner");
        let authed = subprocess(99, "sidecar-owner");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert!(err.contains("reserved"), "got: {err}");
    }

    #[test]
    fn accepts_subprocess_claiming_bound_channel_type() {
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 7,
            label: "sidecar-slack".into(),
            bound_channel_type: Some("slack".into()),
        });
        let frame = register(handle.token(), "slack");
        let authed = subprocess(7, "sidecar-slack");
        let outcome = validate_register(frame, &authed, &tokens).unwrap();
        assert_eq!(outcome.channel_type.as_str(), "slack");
    }
}
