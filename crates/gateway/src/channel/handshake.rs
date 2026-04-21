//! Pure handshake validator for the WS channel server. Separated from
//! the route handler so it can be unit-tested without spinning a
//! TCP/UDS listener.

use aura_channels::sdk::wire::{Frame, PROTOCOL_VERSION};
use aura_gateway_auth::ChannelTokenTable;
use aura_model::ChannelType;

use crate::auth_channel::AuthedClient;

/// Channel type strings reserved for in-process adapters. Sidecars may
/// not claim these — they would shadow the real adapter.
const RESERVED_CHANNEL_TYPES: &[&str] = &[ChannelType::HTTP, ChannelType::TUI];

/// Validate the first frame received on a `/v1/channel-ws` upgrade and
/// produce the `ChannelType` the sidecar is registering as.
///
/// `authed` is the [`AuthedClient`] the auth middleware already attached
/// to the request via [`ChannelAuthState`](crate::auth_channel::ChannelAuthState).
/// `tokens` is the live capability table — we consult it to confirm the
/// `Register.token` the sidecar embedded in the frame names the same
/// subprocess that the header-based auth already validated.
pub(crate) fn validate_register(
    frame: Frame,
    authed: &AuthedClient,
    tokens: &ChannelTokenTable,
) -> Result<ChannelType, String> {
    let Frame::Register {
        token,
        channel_type,
        protocol_version,
    } = frame
    else {
        return Err("expected Register frame".to_string());
    };

    if protocol_version != PROTOCOL_VERSION {
        return Err(format!(
            "protocol version mismatch: server {PROTOCOL_VERSION}, client {protocol_version}"
        ));
    }

    let AuthedClient::Subprocess { pid, label } = authed else {
        return Err("tui psk cannot register via channel-ws".to_string());
    };

    let identity = tokens
        .lookup(&token)
        .ok_or_else(|| "token not registered".to_string())?;
    if identity.pid != *pid || identity.label != *label {
        return Err("token identity mismatch".to_string());
    }

    let normalized = channel_type.as_str().trim().to_string();
    if normalized.is_empty() {
        return Err("channel_type must not be empty".to_string());
    }
    if !normalized.is_ascii() {
        return Err("channel_type must be ascii".to_string());
    }
    if RESERVED_CHANNEL_TYPES.iter().any(|r| *r == normalized) {
        return Err(format!("channel_type '{normalized}' is reserved"));
    }

    Ok(ChannelType::from(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_channels::sdk::wire::Message as WireMessage;
    use aura_gateway_auth::ClientIdentity;

    fn subprocess(pid: u32, label: &str) -> AuthedClient {
        AuthedClient::Subprocess {
            pid,
            label: label.to_string(),
        }
    }

    fn register(token: &str, channel_type: &str, version: u16) -> Frame {
        Frame::Register {
            token: token.to_string(),
            channel_type: ChannelType::from(channel_type),
            protocol_version: version,
        }
    }

    #[test]
    fn accepts_open_channel_type() {
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 42,
            label: "slack".into(),
        });
        let frame = register(handle.token(), "slack", PROTOCOL_VERSION);
        let authed = subprocess(42, "slack");
        let ct = validate_register(frame, &authed, &tokens).unwrap();
        assert_eq!(ct.as_str(), "slack");
    }

    #[test]
    fn rejects_wrong_protocol_version() {
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 1,
            label: "slack".into(),
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
        });
        let frame = register(handle.token(), "slack", PROTOCOL_VERSION);
        let authed = subprocess(42, "discord");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert_eq!(err, "token identity mismatch");
    }

    #[test]
    fn rejects_tui_auth_on_ws() {
        let tokens = ChannelTokenTable::new();
        let frame = register("x", "slack", PROTOCOL_VERSION);
        let err = validate_register(frame, &AuthedClient::Tui, &tokens).unwrap_err();
        assert_eq!(err, "tui psk cannot register via channel-ws");
    }

    #[test]
    fn rejects_reserved_channel_types() {
        for reserved in ["http", "tui"] {
            let tokens = ChannelTokenTable::new();
            let handle = tokens.mint(ClientIdentity {
                pid: 1,
                label: reserved.into(),
            });
            let frame = register(handle.token(), reserved, PROTOCOL_VERSION);
            let authed = subprocess(1, reserved);
            let err = validate_register(frame, &authed, &tokens).unwrap_err();
            assert!(err.contains("reserved"));
        }
    }

    #[test]
    fn rejects_empty_channel_type() {
        let tokens = ChannelTokenTable::new();
        let handle = tokens.mint(ClientIdentity {
            pid: 1,
            label: "slack".into(),
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
        });
        let authed = subprocess(1, "slack");
        let err = validate_register(frame, &authed, &tokens).unwrap_err();
        assert_eq!(err, "expected Register frame");
    }
}
