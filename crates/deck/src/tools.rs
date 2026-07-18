//! LLM-facing deck tools (the domain-owns-its-tools pattern, like cron).
//!
//! The agent authors a card bundle in a staging directory with its
//! ordinary file tools, then calls `DeckCardCreate(path)` /
//! `DeckCardUpdate(card_id, path)`. Install *is* the dry-run gate:
//! failures come back in the tool result (with the child's stderr) so
//! the agent iterates in the same turn, and the user's first sight of a
//! card is a populated one.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use baybo_tools::{Tool, ToolContext, ToolError, ToolManifest, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manager::{CardView, DeckManager};

/// Build the deck tools with manifests, ready for a `ToolRegistry`.
/// `Trusted` with `ReadFile` — they read the agent's staged bundle from
/// disk; everything else they do runs through the manager's own gates.
/// Owner-channel-only: the deck is the owner's surface, so sessions on
/// any other channel (telegram, tui, subagent, …) neither see these
/// tools in their LLM list nor may execute them.
pub fn agent_tools(manager: Arc<DeckManager>) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(DeckCardCreateTool {
            manager: manager.clone(),
        }),
        Arc::new(DeckCardUpdateTool { manager }),
    ];
    tools
        .into_iter()
        .map(|tool| {
            let manifest = ToolManifest {
                name: tool.name().to_string(),
                description: tool.description(),
                trust_level: baybo_model::TrustLevel::Trusted,
                parameters_schema: tool.parameters_schema(),
                capabilities: vec![baybo_tools::ToolCapability::ReadFile],
                channels: vec![baybo_model::ChannelType::owner()],
            };
            (tool, manifest)
        })
        .collect()
}

fn card_summary(card: &CardView) -> Value {
    json!({
        "card_id": card.id,
        "title": card.title,
        "size": card.size.as_str(),
        "position": card.position,
        "spec_hash": card.spec_hash,
    })
}

fn map_err(e: crate::error::DeckError) -> ToolError {
    ToolError::Execution(e.to_string())
}

pub struct DeckCardCreateTool {
    manager: Arc<DeckManager>,
}

#[derive(Deserialize)]
struct CreateParams {
    /// Absolute path to the staged bundle directory.
    path: PathBuf,
}

#[async_trait]
impl Tool for DeckCardCreateTool {
    fn name(&self) -> &str {
        "DeckCardCreate"
    }

    fn description(&self) -> String {
        r#"Install a new deck card from a staged bundle directory.

The directory must contain exactly these four files (author them with your normal file tools first — the /card skill expansion in this conversation carries the contract and templates):
- manifest.json  {"title", "size": "small|wide|large", "refresh": {"op", "params"?, "min_emit_interval_secs"}}
- openapi.json   the card's op contract (paths./<op>.get|post with typed parameters)
- service.js     backend: `export const ops = {...}` + optional `export function start(ctx)`
- card.html      frontend, rendered in a sandboxed iframe on the phone

Install runs the dry-run gate: static validation, a real sandboxed boot, one invocation of the refresh op, and a checked first snapshot — all before the card goes live. Failures are returned here (including the service's stderr) so you can fix the bundle and retry. On success the card appears on the user's deck already populated."#
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the staged bundle directory"
                }
            },
            "required": ["path"]
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("path")
            .and_then(Value::as_str)
            .map(|p| p.to_string())
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let params: CreateParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        if !params.path.is_absolute() {
            return Err(ToolError::InvalidParams("path must be absolute".into()));
        }
        let card = self.manager.install(&params.path).await.map_err(map_err)?;
        Ok(ToolOutput::Json(json!({
            "installed": card_summary(&card),
            "note": "The card is live on the user's deck with its first snapshot."
        })))
    }
}

pub struct DeckCardUpdateTool {
    manager: Arc<DeckManager>,
}

#[derive(Deserialize)]
struct UpdateParams {
    card_id: String,
    /// Absolute path to the staged replacement bundle.
    path: PathBuf,
}

#[async_trait]
impl Tool for DeckCardUpdateTool {
    fn name(&self) -> &str {
        "DeckCardUpdate"
    }

    fn description(&self) -> String {
        r#"Replace an existing deck card's bundle with a staged directory (same four files as DeckCardCreate).

Runs the same dry-run gate before anything goes live; on success the card's service restarts on the new code. The card's title, size, and position are owned by the row after install — the new manifest's values do NOT overwrite the user's layout."#
            .to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "card_id": {"type": "string", "description": "The card's uuid"},
                "path": {
                    "type": "string",
                    "description": "Absolute path to the staged replacement bundle"
                }
            },
            "required": ["card_id", "path"]
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("card_id")
            .and_then(Value::as_str)
            .map(|c| c.to_string())
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let params: UpdateParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        if !params.path.is_absolute() {
            return Err(ToolError::InvalidParams("path must be absolute".into()));
        }
        let card = self
            .manager
            .update(&params.card_id, &params.path)
            .await
            .map_err(map_err)?;
        Ok(ToolOutput::Json(json!({
            "updated": card_summary(&card),
        })))
    }
}
