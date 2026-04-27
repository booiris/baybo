//! `Now` — returns the current wall-clock time and host timezone.
//!
//! Pure system-info readback: no parameters, no capabilities. The LLM uses
//! this to anchor relative-time reasoning (e.g. "schedule for tomorrow at
//! 9am") against the operator's actual clock, rather than training-data
//! drift or UTC assumptions.

use async_trait::async_trait;
use chrono::{Local, Utc};
use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolOutput};

pub struct NowTool;

#[async_trait]
impl Tool for NowTool {
    fn name(&self) -> &str {
        "Now"
    }

    fn description(&self) -> &str {
        "Returns the current date/time in UTC and the host's local timezone. \
         Use this to anchor relative time references (\"tomorrow\", \"in 2 hours\") \
         before scheduling cron jobs or computing deadlines."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "additionalProperties": false })
    }

    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let utc = Utc::now();
        let local = Local::now();
        let offset = local.offset();
        let timezone = iana_time_zone::get_timezone().unwrap_or_else(|_| "unknown".into());

        Ok(ToolOutput::Json(json!({
            "utc": utc.to_rfc3339(),
            "local": local.to_rfc3339(),
            "timezone": timezone,
            "utc_offset": offset.to_string(),
            "utc_offset_seconds": offset.local_minus_utc(),
        })))
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
    async fn returns_expected_fields() {
        let out = NowTool.execute(json!({}), &ctx()).await.unwrap();
        let ToolOutput::Json(v) = out else {
            panic!("expected json");
        };
        assert!(v["utc"].as_str().is_some_and(|s| s.ends_with("+00:00")));
        assert!(v["local"].as_str().is_some());
        assert!(v["timezone"].as_str().is_some());
        assert!(v["utc_offset_seconds"].is_i64());
    }

    #[test]
    fn name_is_now() {
        assert_eq!(NowTool.name(), "Now");
    }
}
