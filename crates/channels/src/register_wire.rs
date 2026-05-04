use std::collections::HashMap;

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
    ts(export, export_to = "../../../sdks/channel-ts/src/generated/")
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
    ts(export, export_to = "../../../sdks/channel-ts/src/generated/")
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
        /// Non-secret auxiliary configuration persisted as plaintext
        /// JSON on the `channel_bots.metadata` row (Lark `base_url`,
        /// Discord intents bitmask, …). Empty for single-secret
        /// sidecars; older sidecars that don't emit the field decode
        /// it as `{}`. See [`crate::registration::RegistrationResult`]
        /// for the full split-storage contract.
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        metadata: HashMap<String, String>,
        /// Secret-valued auxiliary credentials beyond the primary
        /// `token` (Lark `app_secret` / `encrypt_key` /
        /// `verification_token`, Slack signing-secret). Routed by the
        /// CLI into [`aura_security::SecretVault`] under per-bot keys
        /// so they're encrypted at rest and redacted in logs the same
        /// way `token` is. Empty for single-secret sidecars.
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        secrets: HashMap<String, String>,
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
    ts(export, export_to = "../../../sdks/channel-ts/src/generated/")
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
            metadata: HashMap::new(),
            secrets: HashMap::new(),
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"type\":\"result\""));
        // Empty maps stay off the wire so older CLIs that predate the
        // fields round-trip cleanly.
        assert!(!json.contains("metadata"));
        assert!(!json.contains("secrets"));
        assert_eq!(frame, serde_json::from_str(&json).unwrap());
    }

    #[test]
    fn roundtrip_result_with_metadata_and_secrets() {
        let mut metadata = HashMap::new();
        metadata.insert("base_url".into(), "https://open.feishu.cn".into());
        let mut secrets = HashMap::new();
        secrets.insert("app_secret".into(), "deadbeef".into());
        secrets.insert("encrypt_key".into(), "cafebabe".into());
        let frame = RegisterOut::Result {
            bot_id: "lark-bot".into(),
            token: "app-id-token".into(),
            metadata,
            secrets,
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"app_secret\""));
        assert!(json.contains("\"base_url\""));
        // Both maps round-trip with content preserved.
        assert_eq!(frame, serde_json::from_str(&json).unwrap());
    }

    #[test]
    fn legacy_result_without_metadata_decodes_empty() {
        // Old sidecars predating the new fields write only the two
        // original keys. The new CLI must still parse it.
        let json = r#"{"type":"result","bot_id":"123","token":"tok"}"#;
        let frame: RegisterOut = serde_json::from_str(json).unwrap();
        let RegisterOut::Result {
            bot_id,
            token,
            metadata,
            secrets,
        } = frame
        else {
            panic!("expected Result variant");
        };
        assert_eq!(bot_id, "123");
        assert_eq!(token, "tok");
        assert!(metadata.is_empty());
        assert!(secrets.is_empty());
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
