use serde::{Deserialize, Serialize};

/// Maximum size of a single line-delimited JSON frame on the
/// registration wire, in either direction. Sized to admit a `Result`
/// frame carrying a token up to ~1 MiB plus envelope overhead
/// (`{"type":"result","bot_id":"...","token":"..."}` + newline).
pub const MAX_FRAME_BYTES: usize = 1024 * 1024 + 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
)]
pub enum PromptKind {
    Input,
    Password,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
)]
pub enum RegisterOut {
    Prompt {
        id: String,
        label: String,
        kind: PromptKind,
        required: bool,
    },
    Result {
        bot_id: String,
        token: String,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS))]
#[cfg_attr(
    feature = "ts-export",
    ts(export, export_to = "../../../sidecars/sdk/channel-ts/src/generated/")
)]
pub enum RegisterIn {
    PromptReply { id: String, value: String },
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_prompt() {
        let frame = RegisterOut::Prompt {
            id: "abc".into(),
            label: "bot token".into(),
            kind: PromptKind::Password,
            required: true,
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"type\":\"prompt\""));
        assert!(json.contains("\"kind\":\"password\""));
        assert_eq!(frame, serde_json::from_str(&json).unwrap());
    }

    #[test]
    fn roundtrip_result() {
        let frame = RegisterOut::Result {
            bot_id: "123456789".into(),
            token: "123456789:hunter2".into(),
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"type\":\"result\""));
        assert_eq!(frame, serde_json::from_str(&json).unwrap());
    }

    #[test]
    fn roundtrip_error() {
        let frame = RegisterOut::Error {
            message: "user cancelled".into(),
        };
        assert_eq!(
            frame,
            serde_json::from_str(&serde_json::to_string(&frame).unwrap()).unwrap()
        );
    }

    #[test]
    fn roundtrip_prompt_reply() {
        let frame = RegisterIn::PromptReply {
            id: "abc".into(),
            value: "hello".into(),
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"type\":\"prompt_reply\""));
        assert_eq!(frame, serde_json::from_str(&json).unwrap());
    }

    #[test]
    fn roundtrip_cancel() {
        let frame = RegisterIn::Cancel;
        let json = serde_json::to_string(&frame).unwrap();
        assert_eq!(json, "{\"type\":\"cancel\"}");
        assert_eq!(frame, serde_json::from_str(&json).unwrap());
    }

    #[test]
    fn unknown_kind_rejected() {
        let err = serde_json::from_str::<RegisterOut>(r#"{"type":"nope"}"#).unwrap_err();
        assert!(err.to_string().contains("unknown variant"));
    }
}
