//! `Echo` — debug tool that returns its parameters verbatim.
//!
//! Only compiled under `debug_assertions` and only registered by
//! [`crate::builtin::default_tools`] in debug builds. Useful for smoke-testing
//! the agent's tool-call round-trip without touching the filesystem, shell,
//! or network.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolOutput};

pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }

    fn description(&self) -> &str {
        "Echoes back the input message. Useful for testing tool execution."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "The message to echo back"
                }
            },
            "required": ["message"]
        })
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> crate::Result<ToolOutput> {
        Ok(ToolOutput::Json(params))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aura_model::{ChannelType, User};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            session_id: "t".into(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(1),
            cancellation_token: CancellationToken::new(),
            workspace_root: std::path::PathBuf::from("/tmp"),
            sandbox: None,
            approval: None,
        }
    }

    #[tokio::test]
    async fn echoes_message() {
        let out = EchoTool
            .execute(json!({ "message": "hello world" }), &ctx())
            .await
            .unwrap();
        let ToolOutput::Json(v) = out else {
            panic!("expected json output");
        };
        assert_eq!(v["message"], "hello world");
    }

    #[test]
    fn schema_requires_message() {
        let schema = EchoTool.parameters_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["message"]["type"], "string");
        let required = schema["required"].as_array().expect("required array");
        assert!(required.iter().any(|v| v == "message"));
    }

    #[test]
    fn name_is_echo() {
        assert_eq!(EchoTool.name(), "echo");
    }
}
