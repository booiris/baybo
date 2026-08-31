//! Discovery pair for lazily-advertised (deferred) MCP tools.
//!
//! A deferred server's tools register and execute exactly like eager ones —
//! same trust, channel, trigger-scope and approval doors — but their
//! schemas are withheld from the LLM tool block (see
//! [`crate::registry::ToolRegistry::register_dynamic_deferred`]). The model
//! reaches them through this pair:
//!
//! * [`ToolSearchTool`] queries the [`DeferredToolIndex`] and returns each
//!   match's name, description and `parameters_schema`.
//! * `ToolInvoke` is an *envelope*, not a dispatcher: the agent loop unwraps
//!   `{"name", "params"}` at dispatch time and rebinds the call to the inner
//!   tool, so permits, progress captions, trace spans and every executor door
//!   act on the real target. The [`ToolInvokeTool`] registered here only ever
//!   executes when the envelope was malformed, and answers with usage
//!   guidance instead of a bare error.
//!
//! Keeping the advertised tool block byte-stable is the point: deferred
//! servers connecting, dropping or reconnecting never touch
//! `ChatRequest.tools`, so the prompt-cache prefix survives their churn.
//!
//! Subagent scoping: a child with a `tool_allowlist` is only offered this
//! pair when its profile names it — and naming `ToolSearch` deliberately
//! grants discovery of EVERY deferred tool the child's channel/trigger can
//! see, not just the allowlisted ones (the allowlist is a prompt budget,
//! not a boundary — the executor never enforced it for direct calls
//! either). A profile that wants a child limited to specific MCP tools
//! should name those tools (pinning their schemas eager) and omit the pair.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::ToolDefinition;
use crate::registry::{DeferredLookup, DeferredToolIndex};
use crate::{
    Tool, ToolCapability, ToolConcurrency, ToolContext, ToolError, ToolManifest, ToolOutput,
};

pub const TOOL_SEARCH_NAME: &str = "ToolSearch";
pub const TOOL_INVOKE_NAME: &str = "ToolInvoke";

/// Most schemas returned in full per search; further matches are listed by
/// name only so one broad query cannot flood the context.
const DEFAULT_MAX_RESULTS: usize = 8;
const MAX_MAX_RESULTS: usize = 20;

/// Both tools with their governance manifests, ready for
/// `ToolRegistry::register`. Wired in the boot path (not
/// [`super::default_tools`]) because the index view exists only once the
/// registry itself does.
pub fn tools(index: DeferredToolIndex) -> Vec<(std::sync::Arc<dyn Tool>, ToolManifest)> {
    vec![
        super::trusted(ToolSearchTool { index }, Vec::<ToolCapability>::new()),
        super::trusted(ToolInvokeTool, Vec::<ToolCapability>::new()),
    ]
}

pub struct ToolSearchTool {
    index: DeferredToolIndex,
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        TOOL_SEARCH_NAME
    }

    fn description(&self) -> String {
        // Static on purpose: a description that enumerated live servers
        // would change the advertised tool block whenever one connects,
        // which is exactly the cache churn deferral exists to remove.
        "Find deferred MCP tools (not in your tool list); call them via ToolInvoke.".to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search terms; empty lists everything."
                },
                "kind": {
                    "type": "string",
                    "enum": ["fuzzy", "exact"],
                    "default": "fuzzy",
                    "description": "fuzzy ranks by relevance; exact returns schemas for the comma-separated names in query."
                },
                "server": {
                    "type": "string",
                    "description": "Filter by server name."
                },
                "max_results": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_MAX_RESULTS,
                    "default": DEFAULT_MAX_RESULTS,
                    "description": "Full schemas per call; the rest are names only."
                }
            }
        })
    }

    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let query = params
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let server = params
            .get("server")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let max_results = params
            .get("max_results")
            .and_then(Value::as_u64)
            .map(|n| (n as usize).clamp(1, MAX_MAX_RESULTS))
            .unwrap_or(DEFAULT_MAX_RESULTS);
        let exact = params.get("kind").and_then(Value::as_str) == Some("exact");

        let channel = &ctx.user.channel;
        let trigger = &ctx.session_trigger;

        if exact && !query.is_empty() {
            let mut out = Vec::new();
            for raw in query.split(',') {
                let name = raw.trim();
                if name.is_empty() {
                    continue;
                }
                // The same budget as keyword search: full schemas for the
                // first `max_results` names, name-only stubs after, so one
                // call cannot flood the context and the loop's output cap
                // never has to cut JSON mid-schema.
                if out.len() >= max_results {
                    out.push(json!({
                        "name": name,
                        "note": "over max_results budget; fetch in a follow-up exact call",
                    }));
                    continue;
                }
                let entry = match self.index.lookup(name, channel, trigger) {
                    DeferredLookup::Visible(def) => json!({
                        "name": def.name,
                        "description": def.description,
                        "parameters_schema": def.parameters_schema,
                        "call_with": TOOL_INVOKE_NAME,
                    }),
                    DeferredLookup::OutOfScope => json!({
                        "name": name,
                        "error": "registered but not available to this session (channel or trigger scope)",
                    }),
                    DeferredLookup::Eager => json!({
                        "name": name,
                        "note": "not deferred — this tool is already in your tool list; call it directly",
                    }),
                    DeferredLookup::Unknown => json!({
                        "name": name,
                        "error": "no such deferred tool; if it is not in your tool list either, the name is wrong or its server is disconnected",
                    }),
                };
                out.push(entry);
            }
            return Ok(ToolOutput::Json(json!({ "results": out })));
        }

        // Grouping and the `server` filter key off the registration SOURCE
        // (== the `server/` prefix for MCP tools; the batch label for the
        // deferred cron/deck builtins, whose names carry no prefix).
        let visible = self.index.visible_with_source(channel, trigger);
        let candidates: Vec<_> = visible
            .into_iter()
            .filter(|(source, _)| server.is_none_or(|s| source == s))
            .collect();

        if query.is_empty() {
            // Server-level directory: names only, grouped by source.
            let mut by_server: std::collections::BTreeMap<&str, Vec<&str>> =
                std::collections::BTreeMap::new();
            for (source, def) in &candidates {
                let tool = def
                    .name
                    .split_once('/')
                    .map_or(def.name.as_str(), |(_, t)| t);
                by_server.entry(source).or_default().push(tool);
            }
            let servers: Vec<Value> = by_server
                .into_iter()
                .map(|(name, tools)| json!({ "server": name, "tools": tools }))
                .collect();
            return Ok(ToolOutput::Json(json!({
                "deferred_servers": servers,
                "hint": "fuzzy-search with keywords, or kind=\"exact\" with comma-separated names for schemas",
            })));
        }

        let candidates: Vec<ToolDefinition> = candidates.into_iter().map(|(_, def)| def).collect();
        let ranked = bm25_rank(&candidates, query);

        let full: Vec<Value> = ranked
            .iter()
            .take(max_results)
            .map(|def| {
                json!({
                    "name": def.name,
                    "description": def.description,
                    "parameters_schema": def.parameters_schema,
                    "call_with": TOOL_INVOKE_NAME,
                })
            })
            .collect();
        let more: Vec<&str> = ranked
            .iter()
            .skip(max_results)
            .map(|def| def.name.as_str())
            .collect();

        let mut body = json!({ "results": full });
        if !more.is_empty() {
            body["more_matches_by_name"] = json!(more);
        }
        if ranked.is_empty() {
            body["hint"] = json!(
                "no deferred tool matched; an empty query lists every deferred \
                 server, and tools already in your tool list are not indexed here"
            );
        }
        Ok(ToolOutput::Json(body))
    }
}

/// Lowercased alphanumeric tokens; `cloud/modify_firewall_rules` splits on
/// the same boundaries a query naturally uses, and a camelCase name like
/// `CronCreate` splits at case boundaries so "cron create" finds it.
fn tokenize(text: &str) -> Vec<String> {
    let mut spaced = String::with_capacity(text.len() + 8);
    let mut prev_lower = false;
    for c in text.chars() {
        if c.is_ascii_uppercase() && prev_lower {
            spaced.push(' ');
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        spaced.push(c);
    }
    spaced
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Okapi BM25 over the visible deferred set — one document per tool
/// (name + description). The corpus is at most a few hundred tools, so
/// per-call stats cost nothing and stay consistent with the single
/// registry snapshot `visible()` took. A document token matches a query
/// term exactly, or by prefix for terms of 3+ chars, so a truncated
/// keyword ("firew") still finds "firewall". Deterministic: candidates
/// arrive name-sorted and the stable sort breaks score ties in that
/// order.
fn bm25_rank<'a>(candidates: &'a [ToolDefinition], query: &str) -> Vec<&'a ToolDefinition> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    let terms = tokenize(query);
    if terms.is_empty() || candidates.is_empty() {
        return Vec::new();
    }
    let docs: Vec<Vec<String>> = candidates
        .iter()
        .map(|def| tokenize(&format!("{} {}", def.name, def.description)))
        .collect();
    let n = docs.len() as f64;
    let avgdl = docs.iter().map(Vec::len).sum::<usize>() as f64 / n;
    let hit =
        |token: &str, term: &str| token == term || (term.len() >= 3 && token.starts_with(term));
    let df: Vec<f64> = terms
        .iter()
        .map(|term| {
            docs.iter()
                .filter(|doc| doc.iter().any(|t| hit(t, term)))
                .count() as f64
        })
        .collect();
    let mut scored: Vec<(f64, &ToolDefinition)> = docs
        .iter()
        .zip(candidates)
        .filter_map(|(doc, def)| {
            let dl = doc.len() as f64;
            let mut score = 0.0;
            for (term, df_t) in terms.iter().zip(&df) {
                let tf = doc.iter().filter(|t| hit(t, term)).count() as f64;
                if tf == 0.0 {
                    continue;
                }
                let idf = ((n - df_t + 0.5) / (df_t + 0.5) + 1.0).ln();
                score += idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * dl / avgdl));
            }
            (score > 0.0).then_some((score, def))
        })
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    scored.into_iter().map(|(_, def)| def).collect()
}

/// The registered face of the `ToolInvoke` envelope. A well-formed call
/// never reaches this `execute`: the agent loop unwraps the envelope at
/// dispatch and rebinds the call to the inner tool. Executing here means
/// the envelope was malformed, so the answer is usage guidance.
pub struct ToolInvokeTool;

#[async_trait]
impl Tool for ToolInvokeTool {
    fn name(&self) -> &str {
        TOOL_INVOKE_NAME
    }

    fn description(&self) -> String {
        "Call a deferred MCP tool discovered via ToolSearch.".to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "\"server/tool\" from ToolSearch."
                },
                "params": {
                    "type": "object",
                    "description": "Arguments per its parameters_schema."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    fn concurrency(&self) -> ToolConcurrency {
        // Only the malformed-envelope path executes as `ToolInvoke`, and it
        // just returns guidance — never serialize the batch for it.
        ToolConcurrency::Concurrent
    }

    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> crate::Result<ToolOutput> {
        Err(ToolError::InvalidParams(format!(
            "{TOOL_INVOKE_NAME} requires {{\"name\": \"server/tool\", \"params\": {{...}}}} \
             with a namespaced MCP tool name. Builtin tools are called \
             directly, never through {TOOL_INVOKE_NAME}. \
             Use {TOOL_SEARCH_NAME} to find tool names and their schemas."
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::registry::ToolRegistry;
    use crate::{ToolTriggerScope, builtin::trusted};

    struct FakeMcpTool {
        name: &'static str,
        desc: &'static str,
        scope: ToolTriggerScope,
    }

    #[async_trait]
    impl Tool for FakeMcpTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> String {
            self.desc.to_string()
        }
        fn parameters_schema(&self) -> Value {
            json!({ "type": "object", "properties": { "id": { "type": "string" } } })
        }
        fn trigger_scope(&self) -> ToolTriggerScope {
            self.scope
        }
        async fn execute(&self, _p: Value, _c: &ToolContext) -> crate::Result<ToolOutput> {
            Ok(ToolOutput::Text("ok".into()))
        }
    }

    fn registry_with_deferred() -> ToolRegistry {
        let registry = ToolRegistry::new();
        for (name, desc, scope) in [
            (
                "cloud/describe_instances",
                "List cloud instances",
                ToolTriggerScope::Any,
            ),
            (
                "cloud/modify_firewall_rules",
                "Modify firewall rules",
                ToolTriggerScope::Any,
            ),
            (
                "browser/navigate",
                "Navigate the browser",
                ToolTriggerScope::SharedWorkspace,
            ),
        ] {
            let (tool, manifest) = trusted(FakeMcpTool { name, desc, scope }, vec![]);
            let source = name.split('/').next().unwrap().to_string();
            registry.register_dynamic_deferred(&source, tool, manifest);
        }
        registry
    }

    fn owner_ctx() -> ToolContext {
        ToolContext::for_test()
    }

    async fn run_search(registry: &ToolRegistry, params: Value) -> Value {
        let tool = ToolSearchTool {
            index: registry.deferred_tool_index(),
        };
        match tool.execute(params, &owner_ctx()).await.unwrap() {
            ToolOutput::Json(v) => v,
            other => panic!("expected Json output, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn empty_query_lists_servers_and_names() {
        let registry = registry_with_deferred();
        let out = run_search(&registry, json!({})).await;
        let servers = out["deferred_servers"].as_array().unwrap();
        let cloud = servers
            .iter()
            .find(|s| s["server"] == "cloud")
            .expect("cloud server listed");
        let tools = cloud["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        // Names only in the directory — no schemas.
        assert!(out.to_string().contains("describe_instances"));
        assert!(!out.to_string().contains("parameters_schema"));
    }

    #[tokio::test]
    async fn keyword_search_returns_full_schema() {
        let registry = registry_with_deferred();
        let out = run_search(&registry, json!({ "query": "firewall" })).await;
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["name"], "cloud/modify_firewall_rules");
        assert!(results[0]["parameters_schema"].is_object());
        assert_eq!(results[0]["call_with"], TOOL_INVOKE_NAME);

        // BM25 tokens match by prefix too — a truncated keyword still finds
        // the tool.
        let out = run_search(&registry, json!({ "query": "firew" })).await;
        assert_eq!(out["results"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn select_form_distinguishes_miss_kinds() {
        let registry = registry_with_deferred();
        // An eager dynamic tool for the "call directly" bucket.
        let (tool, manifest) = trusted(
            FakeMcpTool {
                name: "cloud/eager_op",
                desc: "Eager op",
                scope: ToolTriggerScope::Any,
            },
            vec![],
        );
        registry.register_dynamic("cloud", tool, manifest);

        let out = run_search(
            &registry,
            json!({ "kind": "exact", "query": "cloud/describe_instances,cloud/eager_op,cloud/nope" }),
        )
        .await;
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        assert!(results[0]["parameters_schema"].is_object());
        assert!(
            results[1]["note"]
                .as_str()
                .unwrap()
                .contains("call it directly")
        );
        assert!(results[2]["error"].as_str().unwrap().contains("no such"));
    }

    #[tokio::test]
    async fn out_of_scope_tools_are_hidden_from_listing_but_named_on_select() {
        // `for_test` contexts are `TriggerSource::User` sessions, which
        // SharedWorkspace allows — so exercise the exclusion with a
        // cron-only tool instead, which a user session must never see.
        let registry = registry_with_deferred();
        let (tool, manifest) = trusted(
            FakeMcpTool {
                name: "cron/only",
                desc: "Cron-scoped op",
                scope: ToolTriggerScope::CronConversation,
            },
            vec![],
        );
        registry.register_dynamic_deferred("cron", tool, manifest);

        let out = run_search(&registry, json!({})).await;
        assert!(
            !out.to_string().contains("cron/only"),
            "out-of-scope deferred tool must not be listed"
        );
        let out = run_search(&registry, json!({ "kind": "exact", "query": "cron/only" })).await;
        let results = out["results"].as_array().unwrap();
        assert!(
            results[0]["error"]
                .as_str()
                .unwrap()
                .contains("not available to this session")
        );
    }

    #[tokio::test]
    async fn unnamespaced_deferred_builtins_group_by_source() {
        let registry = registry_with_deferred();
        let (tool, manifest) = trusted(
            FakeMcpTool {
                name: "CronCreate",
                desc: "Create a scheduled job",
                scope: ToolTriggerScope::Any,
            },
            vec![],
        );
        registry.register_dynamic_deferred("cron", tool, manifest);

        // Directory: grouped under the batch label, full name listed.
        let out = run_search(&registry, json!({})).await;
        let servers = out["deferred_servers"].as_array().unwrap();
        let cron = servers
            .iter()
            .find(|s| s["server"] == "cron")
            .expect("cron group listed");
        assert_eq!(cron["tools"].as_array().unwrap()[0], "CronCreate");

        // Server filter keys off the source, not a name prefix.
        let out = run_search(&registry, json!({ "server": "cron" })).await;
        let servers = out["deferred_servers"].as_array().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0]["server"], "cron");

        // camelCase splits: "cron create" keywords find CronCreate.
        let out = run_search(&registry, json!({ "query": "cron create" })).await;
        assert_eq!(out["results"][0]["name"], "CronCreate");

        // Exact fetch tolerates the source-spelled alias the directory
        // grouping suggests, answering with the canonical name.
        let out = run_search(
            &registry,
            json!({ "kind": "exact", "query": "cron/CronCreate" }),
        )
        .await;
        let results = out["results"].as_array().unwrap();
        assert_eq!(results[0]["name"], "CronCreate");
        assert!(results[0]["parameters_schema"].is_object());
    }

    #[tokio::test]
    async fn select_form_honors_the_max_results_budget() {
        let registry = registry_with_deferred();
        let out = run_search(
            &registry,
            json!({
                "kind": "exact",
                "query": "cloud/describe_instances,cloud/modify_firewall_rules",
                "max_results": 1
            }),
        )
        .await;
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert!(results[0]["parameters_schema"].is_object());
        assert!(
            results[1]["note"]
                .as_str()
                .unwrap()
                .contains("over max_results"),
            "past-budget names come back as stubs"
        );
        assert!(results[1]["parameters_schema"].is_null());
    }

    #[tokio::test]
    async fn malformed_envelope_gets_usage_guidance() {
        let err = ToolInvokeTool
            .execute(json!({ "params": {} }), &owner_ctx())
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("server/tool"),
            "guidance names the shape: {msg}"
        );
        assert!(msg.contains(TOOL_SEARCH_NAME));
    }

    #[test]
    fn the_description_is_compact() {
        let described = ToolSearchTool {
            index: ToolRegistry::new().deferred_tool_index(),
        }
        .description();
        assert!(
            described.len() <= 85,
            "description is too long: {described}"
        );
    }

    #[test]
    fn the_schema_carries_the_empty_and_exact_query_shapes() {
        let schema = ToolSearchTool {
            index: ToolRegistry::new().deferred_tool_index(),
        }
        .parameters_schema();
        let props = &schema["properties"];
        assert!(
            props["query"]["description"]
                .as_str()
                .unwrap()
                .contains("empty lists everything"),
            "{props}"
        );
        assert!(
            props["kind"]["description"]
                .as_str()
                .unwrap()
                .contains("comma-separated"),
            "{props}"
        );
        assert_eq!(props["max_results"]["default"], DEFAULT_MAX_RESULTS);
    }

    #[test]
    fn pair_registers_with_manifests() {
        let registry = ToolRegistry::new();
        let pair = tools(registry.deferred_tool_index());
        assert_eq!(pair.len(), 2);
        let names: Vec<_> = pair.iter().map(|(t, _)| t.name().to_string()).collect();
        assert!(names.contains(&TOOL_SEARCH_NAME.to_string()));
        assert!(names.contains(&TOOL_INVOKE_NAME.to_string()));
        let _ = Arc::clone(&pair[0].0);
    }
}
