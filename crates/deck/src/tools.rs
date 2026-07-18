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

/// Build the deck tools with manifests, ready for a `ToolRegistry`. All
/// `Trusted`; the authoring pair (`Create`/`Update`) carries `ReadFile`
/// because it reads the agent's staged bundle from disk, while the
/// discovery pair (`List`/`Get`) reads through the manager and needs no
/// filesystem capability. Everything else runs through the manager's own
/// gates. Owner-channel-only: the deck is the owner's surface, so sessions
/// on any other channel (telegram, tui, subagent, …) neither see these
/// tools in their LLM list nor may execute them.
pub fn agent_tools(manager: Arc<DeckManager>) -> Vec<(Arc<dyn Tool>, ToolManifest)> {
    let entries: Vec<(Arc<dyn Tool>, Vec<baybo_tools::ToolCapability>)> = vec![
        (
            Arc::new(DeckCardListTool {
                manager: manager.clone(),
            }),
            vec![],
        ),
        (
            Arc::new(DeckCardGetTool {
                manager: manager.clone(),
            }),
            vec![],
        ),
        (
            Arc::new(DeckCardCreateTool {
                manager: manager.clone(),
            }),
            vec![baybo_tools::ToolCapability::ReadFile],
        ),
        (
            Arc::new(DeckCardUpdateTool { manager }),
            vec![baybo_tools::ToolCapability::ReadFile],
        ),
    ];
    entries
        .into_iter()
        .map(|(tool, capabilities)| {
            let manifest = ToolManifest {
                name: tool.name().to_string(),
                description: tool.description(),
                trust_level: baybo_model::TrustLevel::Trusted,
                parameters_schema: tool.parameters_schema(),
                capabilities,
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

pub struct DeckCardListTool {
    manager: Arc<DeckManager>,
}

#[async_trait]
impl Tool for DeckCardListTool {
    fn name(&self) -> &str {
        "DeckCardList"
    }

    fn description(&self) -> String {
        "List the user's live deck cards (card_id, title, size, enabled, spec_hash) — use it to resolve the user's description to a card_id before updating.".to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let view = self.manager.deck_view().await.map_err(map_err)?;
        let cards: Vec<Value> = view
            .cards
            .iter()
            .map(|c| {
                json!({
                    "card_id": c.id,
                    "title": c.title,
                    "size": c.size.as_str(),
                    "enabled": c.enabled,
                    "spec_hash": c.spec_hash,
                })
            })
            .collect();
        Ok(ToolOutput::Json(json!({ "cards": cards })))
    }
}

pub struct DeckCardGetTool {
    manager: Arc<DeckManager>,
}

#[derive(Deserialize)]
struct GetParams {
    card_id: String,
}

#[async_trait]
impl Tool for DeckCardGetTool {
    fn name(&self) -> &str {
        "DeckCardGet"
    }

    fn description(&self) -> String {
        "Return a live card's current four source files verbatim, so you can edit from the real source before DeckCardUpdate (rather than re-authoring blind). See the /deck skill.".to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "card_id": {"type": "string", "description": "The card's uuid (from DeckCardList)"}
            },
            "required": ["card_id"]
        })
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        params
            .get("card_id")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    async fn execute(&self, params: Value, _ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let params: GetParams =
            serde_json::from_value(params).map_err(|e| ToolError::InvalidParams(e.to_string()))?;
        let files = self
            .manager
            .bundle_files(&params.card_id)
            .await
            .map_err(map_err)?;
        Ok(ToolOutput::Json(json!({
            "card_id": params.card_id,
            "files": {
                "manifest.json": files.manifest_json,
                "openapi.json": files.openapi_json,
                "service.js": files.service_js,
                "card.html": files.card_html,
            }
        })))
    }
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
        "Install a new deck card from a staged bundle directory (absolute path). Runs the dry-run gate and returns any failure (including the service's stderr) so you can fix and retry. The /deck skill carries the bundle contract and templates.".to_string()
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
        "Replace an existing card's bundle (card_id + staged directory of the same four files). Runs the same dry-run gate and restarts the service; the user's title/size/layout are preserved. See the /deck skill.".to_string()
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
