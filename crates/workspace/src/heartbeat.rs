use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Parsed heartbeat configuration from HEARTBEAT.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatSpec {
    /// Global heartbeat interval.
    pub interval: Duration,
    /// Recurring routines to execute.
    pub routines: Vec<RoutineSpec>,
}

/// A single recurring routine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutineSpec {
    /// Unique identifier for this routine.
    pub id: String,
    /// When to run this routine.
    pub schedule: RoutineSchedule,
    /// Prompt to inject when the routine fires.
    pub prompt: String,
    /// Optional channel to notify with results.
    pub notify_channel: Option<String>,
}

/// Schedule for a routine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RoutineSchedule {
    /// Run at a fixed interval.
    Interval(Duration),
    /// Run on a cron expression.
    Cron(String),
}

/// JSON structure expected inside HEARTBEAT.md front-matter or body.
#[derive(Debug, Deserialize)]
struct RawHeartbeatSpec {
    interval_secs: u64,
    #[serde(default)]
    routines: Vec<RawRoutineSpec>,
}

#[derive(Debug, Deserialize)]
struct RawRoutineSpec {
    id: String,
    #[serde(flatten)]
    schedule: RawSchedule,
    prompt: String,
    notify_channel: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawSchedule {
    Interval { interval_secs: u64 },
    Cron { cron: String },
}

/// Parses a HEARTBEAT.md content string into a `HeartbeatSpec`.
///
/// The file is expected to contain a JSON code block (fenced with ```json)
/// or be plain JSON. We extract the first JSON block found, or try parsing
/// the entire content as JSON.
pub fn parse_heartbeat(content: &str) -> aura_core::Result<HeartbeatSpec> {
    let json_str = extract_json_block(content).unwrap_or(content);

    let raw: RawHeartbeatSpec = serde_json::from_str(json_str)
        .map_err(|e| aura_core::AuraError::Config(format!("failed to parse HEARTBEAT.md: {e}")))?;

    let routines = raw
        .routines
        .into_iter()
        .map(|r| {
            let schedule = match r.schedule {
                RawSchedule::Interval { interval_secs } => {
                    RoutineSchedule::Interval(Duration::from_secs(interval_secs))
                }
                RawSchedule::Cron { cron } => RoutineSchedule::Cron(cron),
            };
            RoutineSpec {
                id: r.id,
                schedule,
                prompt: r.prompt,
                notify_channel: r.notify_channel,
            }
        })
        .collect();

    Ok(HeartbeatSpec {
        interval: Duration::from_secs(raw.interval_secs),
        routines,
    })
}

/// Extracts the content of the first ```json ... ``` fenced code block.
fn extract_json_block(content: &str) -> Option<&str> {
    let start_marker = "```json";
    let end_marker = "```";

    let start = content.find(start_marker)?;
    let json_start = start + start_marker.len();
    let rest = &content[json_start..];
    let end = rest.find(end_marker)?;
    Some(rest[..end].trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_plain_json() {
        let content = r#"{"interval_secs": 60, "routines": []}"#;
        let spec = parse_heartbeat(content).unwrap();
        assert_eq!(spec.interval, Duration::from_secs(60));
        assert!(spec.routines.is_empty());
    }

    #[test]
    fn test_parse_fenced_json() {
        let content = r#"
# Heartbeat Config

```json
{
    "interval_secs": 300,
    "routines": [
        {
            "id": "daily-summary",
            "cron": "0 9 * * *",
            "prompt": "Summarize yesterday's activity",
            "notify_channel": "general"
        }
    ]
}
```
"#;
        let spec = parse_heartbeat(content).unwrap();
        assert_eq!(spec.interval, Duration::from_secs(300));
        assert_eq!(spec.routines.len(), 1);
        assert_eq!(spec.routines[0].id, "daily-summary");
        assert!(matches!(&spec.routines[0].schedule, RoutineSchedule::Cron(c) if c == "0 9 * * *"));
        assert_eq!(spec.routines[0].notify_channel.as_deref(), Some("general"));
    }

    #[test]
    fn test_parse_interval_routine() {
        let content = r#"{"interval_secs": 120, "routines": [{"id": "check", "interval_secs": 30, "prompt": "check health"}]}"#;
        let spec = parse_heartbeat(content).unwrap();
        assert_eq!(spec.routines.len(), 1);
        assert!(
            matches!(&spec.routines[0].schedule, RoutineSchedule::Interval(d) if *d == Duration::from_secs(30))
        );
    }

    #[test]
    fn test_parse_invalid_json() {
        let content = "not json at all";
        assert!(parse_heartbeat(content).is_err());
    }
}
