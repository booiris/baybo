use serde::Serialize;
use serde_json::Value;

/// Output rendering preference for a command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-friendly text, possibly with ANSI color. Default for TTY stdout.
    #[default]
    Human,
    /// `serde_json::to_string_pretty` payload; stable, machine-readable.
    Json,
    /// Uncolored single block of text. Used when rendering to a chat channel.
    Plain,
}

/// A command's structured result.
///
/// Commands return this instead of pre-rendered strings so the sink can
/// decide between human text, JSON, or plain text without duplication.
#[derive(Debug)]
pub struct CommandOutput {
    pub human: String,
    pub data: Option<Value>,
}

impl CommandOutput {
    pub fn text(msg: impl Into<String>) -> Self {
        Self {
            human: msg.into(),
            data: None,
        }
    }

    pub fn structured<T: Serialize>(human: impl Into<String>, data: &T) -> Self {
        let value = serde_json::to_value(data).unwrap_or(Value::Null);
        Self {
            human: human.into(),
            data: Some(value),
        }
    }

    pub fn json_only<T: Serialize>(data: &T) -> Self {
        let value = serde_json::to_value(data).unwrap_or(Value::Null);
        let human = serde_json::to_string_pretty(&value).unwrap_or_default();
        Self {
            human,
            data: Some(value),
        }
    }

    pub fn render(&self, format: OutputFormat) -> String {
        match format {
            OutputFormat::Human | OutputFormat::Plain => self.human.clone(),
            OutputFormat::Json => match &self.data {
                Some(v) => serde_json::to_string_pretty(v).unwrap_or_default(),
                None => self.human.clone(),
            },
        }
    }
}
