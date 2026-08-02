//! `ToolSearch` — pulls a deferred tool's definition into the conversation.
//!
//! A deferred tool costs nothing until asked for: the session is shown a
//! roster of bare names in a system reminder, and this tool turns a name into
//! the full definition. The definition arrives as *tool output*, which is to
//! say it lands in the message stream — append-only, and therefore free of the
//! prefix invalidation that adding to the tool array would cause.
//!
//! It is an ordinary always-present builtin, for the obvious reason: a tool
//! that had to be loaded before it could load anything would load nothing.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::registry::{SessionToolScope, ToolRegistry};
use crate::{
    Tool, ToolCapability, ToolConcurrency, ToolContext, ToolError, ToolManifest, ToolOutput,
};

pub const TOOL_SEARCH_TOOL_NAME: &str = "ToolSearch";

/// Parameter carrying the query.
const QUERY_FIELD: &str = "query";

/// Prefix that switches the query from search to exact selection.
const SELECT_PREFIX: &str = "select:";

/// How many tools a bare keyword query returns. A search is a guess, and a
/// guess that hands back a dozen definitions costs more than the deferral
/// saved; the model can always run a second, narrower query.
const DEFAULT_SEARCH_LIMIT: usize = 5;

const DESCRIPTION: &str = r#"Load the definitions of tools that are available to you but not yet in your tool list.

Tools kept out of the list are named — one per line — in a system reminder; they cost you nothing until you load one. Their names appear nowhere else, so if a name is not in that reminder, it does not exist.

Two query forms:
- `select:Name` — load these exactly. **Name every tool you expect to need in ONE call** (`select:A,B,C`); one call per tool wastes a round trip each.
- `read a page, fill a form` — search by keyword when you know the capability but not the name. Returns the closest few.

The definitions come back as this call's result, and the tools are callable from your NEXT message onward — not inside the one that loaded them. They stay for the rest of the conversation, so loading the same tool twice is wasted effort."#;

pub struct ToolSearchTool {
    registry: Arc<ToolRegistry>,
}

impl ToolSearchTool {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self { registry }
    }
}

/// Which tools a query asks for, resolved against the roster this session can
/// see. Exact selection reports its misses; a search cannot miss, it just
/// returns fewer.
fn resolve(query: &str, roster: &[String]) -> (Vec<String>, Vec<String>) {
    if let Some(list) = query.strip_prefix(SELECT_PREFIX) {
        let mut hits = Vec::new();
        let mut misses = Vec::new();
        for raw in list.split(',') {
            let wanted = raw.trim();
            if wanted.is_empty() {
                continue;
            }
            match roster.iter().find(|n| n.as_str() == wanted) {
                Some(name) => hits.push(name.clone()),
                None => misses.push(wanted.to_string()),
            }
        }
        return (hits, misses);
    }

    // Keyword search: score by how many of the query's terms the name
    // contains. Case-insensitive, and `_`/`/`/`-` are treated as spaces so
    // "read page" finds `browser/read_page` without the model having to guess
    // the separator.
    let terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect();
    if terms.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut scored: Vec<(usize, &String)> = roster
        .iter()
        .map(|name| {
            let haystack = name.to_lowercase();
            let score = terms.iter().filter(|t| haystack.contains(*t)).count();
            (score, name)
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    // Descending by score, then by name so equal scores are stable.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let hits = scored
        .into_iter()
        .take(DEFAULT_SEARCH_LIMIT)
        .map(|(_, name)| name.clone())
        .collect();
    (hits, Vec::new())
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        TOOL_SEARCH_TOOL_NAME
    }

    fn description(&self) -> String {
        DESCRIPTION.to_string()
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": [QUERY_FIELD],
            "properties": {
                QUERY_FIELD: {
                    "type": "string",
                    "description": "`select:A,B,C` to load those tools exactly, or free text to search by capability."
                }
            }
        })
    }

    /// Reads the registry and flips one per-session set. Nothing else.
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Concurrent
    }

    fn call_label(&self, params: &Value) -> Option<String> {
        params
            .get(QUERY_FIELD)
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> crate::Result<ToolOutput> {
        let query = params
            .get(QUERY_FIELD)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| ToolError::InvalidParams(format!("`{QUERY_FIELD}` is required")))?;

        // No handle means no session behind this context (argv one-shots), so
        // nothing could carry a load forward. Refuse rather than report a
        // success the next call would contradict.
        let handle = ctx.loaded_tools.as_ref().ok_or_else(|| {
            ToolError::Execution(
                "this context has no session to remember a loaded tool, so nothing can be \
                 loaded here"
                    .into(),
            )
        })?;

        let scope = SessionToolScope {
            channel: &ctx.user.channel,
            trigger: &ctx.session_trigger,
            loaded: &handle.snapshot(),
        };
        // The roster ignores what is already loaded, so re-selecting a tool
        // returns its definition again rather than reporting it unknown — the
        // model may have compacted the earlier result away.
        let roster = self.registry.deferred_tool_names(scope);
        let (hits, misses) = resolve(query, &roster);

        if hits.is_empty() {
            return Err(ToolError::InvalidParams(if roster.is_empty() {
                "no tools are deferred in this conversation; everything available is already \
                 in your tool list"
                    .to_string()
            } else {
                format!(
                    "no deferred tool matched '{query}'. Available to load: {}",
                    roster.join(", ")
                )
            }));
        }

        let mut defs = Vec::with_capacity(hits.len());
        for name in &hits {
            if let Some(def) = self.registry.loadable_definition(name, scope) {
                handle.load(name);
                defs.push(json!({
                    "name": def.name,
                    "description": def.description,
                    "input_schema": def.parameters_schema,
                }));
            }
        }

        Ok(ToolOutput::Json(json!({
            "loaded": defs,
            "not_found": misses,
            "note": "These are callable from your next message onward, not inside this one.",
        })))
    }
}

/// The tool plus its manifest, for the runtime's registration list.
pub fn make(registry: Arc<ToolRegistry>) -> (Arc<dyn Tool>, ToolManifest) {
    let tool = ToolSearchTool::new(registry);
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description(),
        trust_level: baybo_model::TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: Vec::<ToolCapability>::new(),
        channels: Vec::new(),
        deferred: false,
    };
    (Arc::new(tool), manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster() -> Vec<String> {
        [
            "browser/click",
            "browser/navigate_page",
            "browser/read_page",
            "CronCreate",
            "CronList",
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    #[test]
    fn select_takes_several_in_one_call() {
        let (hits, misses) = resolve("select:CronCreate,browser/click", &roster());
        assert_eq!(hits, vec!["CronCreate", "browser/click"]);
        assert!(misses.is_empty());
    }

    /// A miss has to be reported rather than silently dropped: the model would
    /// otherwise call a tool it believes it just loaded.
    #[test]
    fn select_reports_a_name_that_is_not_on_the_roster() {
        let (hits, misses) = resolve("select:CronCreate,Nope", &roster());
        assert_eq!(hits, vec!["CronCreate"]);
        assert_eq!(misses, vec!["Nope"]);
    }

    #[test]
    fn select_tolerates_spaces_and_empty_entries() {
        let (hits, _) = resolve("select: CronCreate , , CronList ", &roster());
        assert_eq!(hits, vec!["CronCreate", "CronList"]);
    }

    /// The model knows the capability, not the naming convention — so the
    /// separator must not be something it has to guess right.
    #[test]
    fn search_crosses_the_name_separators() {
        let (hits, _) = resolve("read page", &roster());
        assert_eq!(hits.first().map(String::as_str), Some("browser/read_page"));
    }

    #[test]
    fn search_ranks_more_matching_terms_first() {
        let (hits, _) = resolve("browser navigate", &roster());
        assert_eq!(
            hits.first().map(String::as_str),
            Some("browser/navigate_page")
        );
    }

    #[test]
    fn search_is_case_insensitive() {
        let (hits, _) = resolve("CRONcreate", &roster());
        assert!(hits.iter().any(|h| h == "CronCreate"), "{hits:?}");
    }

    #[test]
    fn search_is_capped() {
        let big: Vec<String> = (0..50).map(|i| format!("browser/thing_{i}")).collect();
        let (hits, _) = resolve("browser", &big);
        assert_eq!(hits.len(), DEFAULT_SEARCH_LIMIT);
    }

    #[test]
    fn a_query_with_no_usable_terms_matches_nothing() {
        let (hits, _) = resolve("!!! ???", &roster());
        assert!(hits.is_empty());
    }
}
