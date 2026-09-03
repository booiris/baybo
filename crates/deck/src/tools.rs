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
use baybo_tools::{Tool, ToolContext, ToolError, ToolManifest, ToolOutput, ToolTriggerScope};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manager::{CardView, DeckManager};

const MAX_FIRST_SNAPSHOT_KEYS: usize = 32;
const MAX_FIRST_SNAPSHOT_KEY_CHARS: usize = 128;

/// Build the deck tools with manifests, ready for a `ToolRegistry`. All
/// `Trusted`; the authoring pair (`Create`/`Update`) carries `ReadFile`
/// because it reads the agent's staged bundle from disk, while the
/// discovery pair (`List`/`Get`) reads through the manager and needs no
/// filesystem capability. Everything else runs through the manager's own
/// gates. Owner-channel-only: the deck is the owner's surface, so sessions
/// on any other channel (telegram, tui, subagent, …) neither see these
/// tools in their LLM list nor may execute them.
/// The `ToolSearch` source label the deferred deck tools register under —
/// also the group name in its directory and the `server` filter value.
pub const DEFERRED_SOURCE: &str = "deck";

/// Register the deck batch on `registry`, all DEFERRED: low-frequency,
/// ~1.6 KB of schemas, reachable via `ToolSearch` + `ToolInvoke` with
/// names unchanged.
pub fn install_agent_tools(registry: &mut baybo_tools::ToolRegistry, manager: Arc<DeckManager>) {
    for (tool, manifest) in agent_tools(manager) {
        registry.register_dynamic_deferred(DEFERRED_SOURCE, tool, manifest);
    }
}

/// This batch's row in the deferred-tools notice: `(source, description,
/// trigger_scope)`. The scope mirrors the tools' own `SharedWorkspace`, so
/// the door that keeps `DeckCard*` out of a session keeps its row out too.
pub fn deferred_notice_spec() -> (String, Option<String>, ToolTriggerScope) {
    (
        DEFERRED_SOURCE.to_string(),
        Some("Deck card management: create/get/update/list".to_string()),
        ToolTriggerScope::SharedWorkspace,
    )
}

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
        "sizes": card.sizes.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        "maximize": card.maximize,
        "position": card.position,
        "spec_hash": card.spec_hash,
    })
}

/// Count the snapshot's leaf scalars, and how many of them carry anything.
/// Zero, empty string, false and null are all "carries nothing" — the point is
/// to answer "did this card actually read its data source" WITHOUT copying a
/// user's numbers into the agent transcript. A card wired to the wrong
/// workspace, or one whose service silently swallowed an upstream failure,
/// returns a perfectly schema-valid snapshot of zeros, and every other field
/// of this summary looks identical to a healthy one.
fn count_leaves(value: &Value, leaves: &mut u64, populated: &mut u64) {
    match value {
        Value::Object(map) => {
            for nested in map.values() {
                count_leaves(nested, leaves, populated);
            }
        }
        Value::Array(items) => {
            for nested in items {
                count_leaves(nested, leaves, populated);
            }
        }
        scalar => {
            *leaves += 1;
            let carries = match scalar {
                Value::Null => false,
                Value::Bool(b) => *b,
                Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
                Value::String(s) => !s.is_empty(),
                _ => false,
            };
            if carries {
                *populated += 1;
            }
        }
    }
}

fn first_snapshot_summary(snapshot: &Value) -> Value {
    let bytes = snapshot.to_string().len();
    // `serde_json::Map` is a `BTreeMap` here (no `preserve_order` feature in
    // the workspace), so `keys()` is already sorted and the cap takes the
    // lexicographically first ones.
    let keys = snapshot
        .as_object()
        .map(|object| {
            object
                .keys()
                .take(MAX_FIRST_SNAPSHOT_KEYS)
                .map(|key| truncate_chars(key, MAX_FIRST_SNAPSHOT_KEY_CHARS))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let keys_truncated = snapshot
        .as_object()
        .is_some_and(|object| object.len() > MAX_FIRST_SNAPSHOT_KEYS);
    let json_type = match snapshot {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    let (mut leaves, mut populated) = (0, 0);
    count_leaves(snapshot, &mut leaves, &mut populated);
    json!({
        "bytes": bytes,
        "json_type": json_type,
        "top_level_keys": keys,
        "top_level_keys_truncated": keys_truncated,
        "leaf_values": leaves,
        "populated_values": populated,
    })
}

fn mutation_output(card_field: &str, card: &CardView, snapshot: &Value) -> Value {
    let mut fields = serde_json::Map::new();
    fields.insert(card_field.to_string(), card_summary(card));
    fields.insert("first_snapshot".into(), first_snapshot_summary(snapshot));
    Value::Object(fields)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        truncated.push('…');
    }
    truncated
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

    fn trigger_scope(&self) -> ToolTriggerScope {
        ToolTriggerScope::SharedWorkspace
    }

    fn description(&self) -> String {
        "List the user's live deck cards (card_id, title, size, sizes, maximize, enabled, spec_hash) — use it to resolve the user's description to a card_id before updating. See the deck skill.".to_string()
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
                    "sizes": c.sizes.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    "maximize": c.maximize,
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

    fn trigger_scope(&self) -> ToolTriggerScope {
        ToolTriggerScope::SharedWorkspace
    }

    fn description(&self) -> String {
        "Return a live card's current source verbatim (the four bundle files plus any src/ pre-build sources), so you can edit from the real source before DeckCardUpdate (rather than re-authoring blind). See the deck skill.".to_string()
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
        let mut all = json!({
            "manifest.json": files.manifest_json,
            "openapi.json": files.openapi_json,
            "service.js": files.service_js,
            "card.html": files.card_html,
        });
        // Fold any src/ pre-build sources into the same map, keyed by their
        // bundle-relative path (`src/…`), so the agent sees the real inputs.
        if let Some(obj) = all.as_object_mut() {
            for (rel, contents) in files.src {
                obj.insert(rel, Value::String(contents));
            }
        }
        Ok(ToolOutput::Json(json!({
            "card_id": params.card_id,
            "files": all,
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

    fn trigger_scope(&self) -> ToolTriggerScope {
        ToolTriggerScope::SharedWorkspace
    }

    fn description(&self) -> String {
        "Install a new deck card from a staged bundle directory (absolute path). Runs the dry-run gate, returns any failure (including the service's stderr) for repair, and returns a schema-checked first-snapshot summary on success. The deck skill carries the bundle contract and templates.".to_string()
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
        let result = self.manager.install(&params.path).await.map_err(map_err)?;
        Ok(ToolOutput::Json(mutation_output(
            "installed",
            &result.card,
            &result.first_snapshot,
        )))
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

    fn trigger_scope(&self) -> ToolTriggerScope {
        ToolTriggerScope::SharedWorkspace
    }

    fn description(&self) -> String {
        "Replace an existing card's bundle (card_id + staged directory of the same four files). Runs the same dry-run gate, returns a schema-checked first-snapshot summary, and restarts the service; the user's title/size/layout are preserved. See the deck skill.".to_string()
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
        let result = self
            .manager
            .update(&params.card_id, &params.path)
            .await
            .map_err(map_err)?;
        Ok(ToolOutput::Json(mutation_output(
            "updated",
            &result.card,
            &result.first_snapshot,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_snapshot_summary_reports_shape_without_value() {
        let snapshot = json!({"count": 3, "status": "ok"});
        let summary = first_snapshot_summary(&snapshot);

        assert_eq!(summary["json_type"], "object");
        assert_eq!(summary["top_level_keys"], json!(["count", "status"]));
        assert!(summary.get("value").is_none());
    }

    /// The signal that separates a working card from one reading the wrong
    /// data source: both pass the schema and have identical shape, so without
    /// this the agent has nothing to tell them apart and reports success.
    #[test]
    fn first_snapshot_summary_counts_populated_leaves_without_showing_them() {
        let healthy = first_snapshot_summary(&json!({
            "today": {"calls": 49, "total": 5708754},
            "days": [{"total": 0}, {"total": 5708754}],
        }));
        assert_eq!(healthy["leaf_values"], 4);
        assert_eq!(healthy["populated_values"], 3);

        let empty = first_snapshot_summary(&json!({
            "today": {"calls": 0, "total": 0},
            "days": [{"total": 0}, {"total": 0}],
        }));
        assert_eq!(empty["leaf_values"], 4);
        assert_eq!(empty["populated_values"], 0);
        assert_eq!(
            empty["top_level_keys"], healthy["top_level_keys"],
            "shape alone cannot tell these apart — that is why the count exists"
        );

        for summary in [&healthy, &empty] {
            let text = summary.to_string();
            assert!(!text.contains("5708754"), "must not copy values: {text}");
            assert!(!text.contains("49"), "must not copy values: {text}");
        }
    }

    #[test]
    fn first_snapshot_summary_bounds_top_level_keys() {
        let snapshot = Value::Object(
            (0..=MAX_FIRST_SNAPSHOT_KEYS)
                .map(|index| (format!("key-{index:02}"), json!(index)))
                .collect(),
        );
        let summary = first_snapshot_summary(&snapshot);

        assert_eq!(
            summary["top_level_keys"].as_array().map(Vec::len),
            Some(MAX_FIRST_SNAPSHOT_KEYS)
        );
        assert_eq!(summary["top_level_keys_truncated"], true);
    }
}
