//! `spawn_subagent` builtin — typed-subagent dispatch.
//!
//! Blocking tool that:
//!
//! 1. Resolves `subagent_type` against
//!    [`crate::SubagentRegistry`].
//! 2. Packs the resolved profile name (`subagent_type`) and the parent's
//!    `prompt` brief into a [`SubagentSpawnRequest`]; the child's
//!    `ContextManager` resolves the name back to a system prompt.
//! 3. Hands the [`SubagentSpawnRequest`] to the actor-backed
//!    [`crate::SubagentSpawner`] (impl in `aura-agent`) via a late-set
//!    slot, and returns the [`SubagentResult`] it produces — the child's
//!    terminal for a foreground spawn, or the dispatch ack for a
//!    background one.
//!
//! Registered by the runtime wiring code (not
//! [`aura_tools::builtin::default_tools`]) because it needs the
//! runtime-owned spawner slot AND the live `SubagentRegistry` so its
//! description can enumerate available types every LLM turn.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use aura_model::{
    AURA_BACKEND_TAG, ExternalAgentKind, ModelTier, OnTimeout, SPAWN_SUBAGENT_TOOL_NAME, SessionId,
    SubagentBackend, SubagentParentContext, SubagentResult, SubagentSpawnRequest, TrustLevel,
};
use aura_session::SessionManager;
use aura_tools::{Tool, ToolConcurrency, ToolContext, ToolError, ToolManifest, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{SubagentDispatchLimiter, SubagentRegistry, SubagentSpawner};

/// Late-set handle to the actor-backed spawner. The tool is built while
/// the tool registry is assembled (before the supervisor + actor spawner
/// exist), so the runtime injects an empty slot here and fills it once the
/// spawner is constructed. Mirrors how the background-job manager receives
/// its supervisor.
pub type SubagentSpawnerSlot = Arc<OnceLock<Arc<dyn SubagentSpawner>>>;

/// Default recursion cap. A typed subagent that itself dispatches a
/// subagent is fine (`parent → child → grandchild`), but past depth 3
/// the model is usually fan-out for its own sake. Override via the
/// constructor for deployments that need a different ceiling.
pub const DEFAULT_MAX_SUBAGENT_DEPTH: u32 = 3;

/// Default cap on concurrent subagents under a single root session.
/// Distinct from [`DEFAULT_MAX_SUBAGENT_DEPTH`]: depth bounds the
/// lineage chain, fan-out bounds breadth. Eight covers "dispatch a
/// few specialists in parallel" without letting a confused model
/// spin up a hundred siblings.
pub const DEFAULT_MAX_SUBAGENTS_PER_ROOT: u32 = 8;

/// Cycle-protection cap on the lineage walk. A corrupt chain (e.g.
/// from a buggy migration) shouldn't be allowed to spin the executor
/// forever. Pegged well above any realistic depth.
const MAX_LINEAGE_WALK_HOPS: u32 = 128;

const STATIC_DESCRIPTION: &str = r#"Spawn a child subagent — a fresh, independent agent session — to investigate
or perform a focused sub-task. Use when the work is decomposable: the
parent agent stays focused on the conversation while the subagent
runs to completion and returns a single final message.

# How subagents see the world

The subagent does NOT see your transcript. It starts with an empty
context, a system prompt fixed by its `subagent_type` profile, and the
self-contained `prompt` you authored. Everything it needs to know must
be inside that prompt.

This call BLOCKS until the subagent's final assistant message arrives.
The parent then sees the final message text as this tool's result.
Intermediate tool output and streaming deltas inside the subagent are
not surfaced.

# Picking a subagent_type

`subagent_type` is a free string but must match one of the registered
profiles. Each profile has its own description ("when to use / when
NOT to use") shown below in the live registry section. Pick the most
specific match; fall back to `general-purpose` only when none fits.

If you emit an unknown type the call returns a hard error listing the
catalogue — there's no soft fallback.

# Writing the prompt

Brief the subagent like a teammate who just sat down: it hasn't seen
this conversation and doesn't know what you've tried.

- State the goal in one or two sentences, then the relevant facts.
- Include exact paths and line numbers (`crate/foo.rs:123`) — the
  subagent should not have to grep for what you already located.
- If the task involves a fix, write what specifically to change. Do
  NOT push the synthesis onto the subagent (avoid phrases like
  "based on your findings, decide what to do"); that nullifies the
  point of dispatching one in the first place.
- If you need short output, say so ("under 200 words").
- Include the relevant span ids or job ids when the subagent should
  hop across traces.

# Cost and parallelism

Each spawn starts a fresh LLM cost stream. Use sparingly. If you have
multiple independent sub-tasks, emit multiple `spawn_subagent`
tool_use blocks IN ONE message — the runtime executes parallel tool
calls concurrently. Sequential spawns serialise needlessly.

# Background mode

`background: true` returns immediately with a handle and lets the
parent continue talking. The subagent's result is delivered as an
out-of-band notification (a system reminder) prepended to your next
user turn. Works for any backend — especially handy for long external
(claude/codex/gemini) runs you don't want to block on.

# Trust but verify

A subagent's final message reports what it INTENDED to do, not
necessarily what landed. When it claims to have made a change, verify
with `Read` / `Bash(git diff)` before acting on the report.

# Recursion

Subagents may themselves call `spawn_subagent`, but the runtime
enforces a hard depth cap. Use the parent's perspective: most useful
work is "parent dispatches one or two specialists", not deep trees.
"#;

/// Backstop for the parent's wait on a `spawn_subagent` call. Subagent
/// execution is no longer wall-clock-bounded — aura subagents stop at
/// `max_iterations`, external ones at their own internal safety timeout —
/// so this exists only because the tool executor requires a `max_timeout`.
/// Pegged high enough never to fire in practice.
const TOOL_WAIT_BACKSTOP: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Raw JSON shape the LLM emits as `spawn_subagent`'s arguments.
/// `subagent_type` and `prompt` are required; everything else is
/// optional with documented defaults.
#[derive(Debug, Clone, Deserialize)]
struct SpawnParams {
    subagent_type: String,
    /// 3-5 word task summary. Surfaced in traces for operator UI. The
    /// subagent itself does NOT see this — its brief lives in `prompt`.
    description: String,
    /// Self-contained brief the child sees as its first user message.
    prompt: String,
    /// `"aura"` (default), `"claude"`, `"codex"`, or `"gemini"`.
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    model_tier: Option<String>,
    #[serde(default)]
    background: bool,
    /// `"background"` (default) or `"kill"`: what to do when a foreground
    /// spawn exceeds its 2-minute foreground wait in a user session.
    #[serde(default)]
    on_timeout: Option<String>,
    #[serde(default)]
    resume_session_id: Option<String>,
    /// Barrier cohort. Subagents sharing a `group` (spawned together in one
    /// turn) run in the background and deliver one merged notification only
    /// once they all finish.
    #[serde(default)]
    group: Option<String>,
}

/// Outcome of parameter parsing — the fully-built request, or a
/// structured error the executor surfaces as `ToolError::InvalidParams`.
#[derive(Debug)]
struct ParsedSpawn {
    request: SubagentSpawnRequest,
}

/// The two subagent-admission gate inputs [`SpawnSubagentTool::lineage_gate_inputs`]
/// derives from the parent: the parent's `depth` (the depth gate compares it
/// against the nesting cap) and the `root_session_id` (the fan-out gate keys
/// on it). Only `depth` needs the lineage walk; the root is denormalized on
/// the parent row.
struct LineageGateInputs {
    depth: u32,
    root_session_id: SessionId,
}

fn parse_spawn_request(value: &Value, registry: &SubagentRegistry) -> Result<ParsedSpawn, String> {
    let p: SpawnParams = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;

    if p.subagent_type.trim().is_empty() {
        return Err("`subagent_type` must be a non-empty string".into());
    }
    let profile = match registry.get(&p.subagent_type) {
        Some(profile) => profile,
        None => {
            let available: Vec<String> = registry
                .all_summaries_sorted()
                .into_iter()
                .map(|s| s.name)
                .collect();
            return Err(format!(
                "unknown subagent_type '{}'; available types: {}",
                p.subagent_type,
                if available.is_empty() {
                    "<none registered>".to_string()
                } else {
                    available.join(", ")
                }
            ));
        }
    };

    if p.description.trim().is_empty() {
        return Err("`description` must be a non-empty 3-5 word task summary".into());
    }
    if p.prompt.trim().is_empty() {
        return Err("`prompt` must be a non-empty self-contained brief".into());
    }

    let model_tier = match p.model_tier.as_deref() {
        Some(label) => match ModelTier::parse(label) {
            Some(t) => Some(t),
            None => {
                return Err(format!(
                    "unknown model_tier '{label}'; expected fast|balanced|deep"
                ));
            }
        },
        // Precedence at this boundary: explicit > profile default.
        // The runtime's `LlmClientPool` falls back to its pool default
        // when neither is set.
        None => profile.default_tier,
    };

    let backend = match p.backend.as_deref().map(str::trim) {
        None | Some("") => SubagentBackend::Aura,
        Some(t) if t == AURA_BACKEND_TAG => SubagentBackend::Aura,
        Some(other) => {
            if let Some(kind) = ExternalAgentKind::parse(other) {
                SubagentBackend::External {
                    external_kind: kind,
                }
            } else {
                return Err(format!("unknown backend {other:?}"));
            }
        }
    };
    let resume_session_id = p
        .resume_session_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(SessionId::from);

    let on_timeout = match p.on_timeout.as_deref().map(str::trim) {
        None | Some("") | Some("background") => OnTimeout::Background,
        Some("kill") => OnTimeout::Kill,
        Some(other) => {
            return Err(format!(
                "unknown on_timeout {other:?}; expected background|kill"
            ));
        }
    };

    let group = p
        .group
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    // A grouped subagent is background-from-start: its result is barrier-held
    // and delivered with the whole group, so it never blocks the turn.
    let background = p.background || group.is_some();

    Ok(ParsedSpawn {
        request: SubagentSpawnRequest {
            subagent_type: profile.name.clone(),
            task_summary: p.description,
            prompt: p.prompt,
            model_tier,
            background,
            on_timeout,
            // Filled in by the tool after the lineage walk; the
            // synthesized request leaves it `None` so test fixtures
            // can build a `SubagentSpawnRequest` without knowing the
            // dispatch limiter.
            fan_out_root: None,
            backend,
            resume_session_id,
            group,
        },
    })
}

/// Construction bundle for [`SpawnSubagentTool`]. The number of
/// required deps crossed the "two or three" threshold once the
/// fan-out cap landed; per the workspace style guide a sibling
/// config struct makes every required value show up by name at the
/// call site.
pub struct SpawnSubagentToolConfig {
    pub spawner: SubagentSpawnerSlot,
    pub registry: Arc<SubagentRegistry>,
    pub sessions: Arc<SessionManager>,
    pub dispatch_limiter: Arc<dyn SubagentDispatchLimiter>,
    pub max_depth: u32,
    pub max_subagents_per_root: u32,
}

pub struct SpawnSubagentTool {
    spawner: SubagentSpawnerSlot,
    registry: Arc<SubagentRegistry>,
    sessions: Arc<SessionManager>,
    dispatch_limiter: Arc<dyn SubagentDispatchLimiter>,
    max_depth: u32,
    max_subagents_per_root: u32,
    /// Cache of the rendered `description()` string keyed by
    /// `SubagentRegistry::version()`. The LLM sees this catalogue
    /// every turn but the registry only mutates at boot / on
    /// `reload`, so a per-turn rebuild is wasted. Holds an
    /// `Arc<String>` so reads clone a pointer, not the body.
    description_cache: parking_lot::Mutex<Option<(u64, Arc<String>)>>,
}

impl SpawnSubagentTool {
    pub fn from_config(config: SpawnSubagentToolConfig) -> Self {
        let SpawnSubagentToolConfig {
            spawner,
            registry,
            sessions,
            dispatch_limiter,
            max_depth,
            max_subagents_per_root,
        } = config;
        Self {
            spawner,
            registry,
            sessions,
            dispatch_limiter,
            max_depth,
            max_subagents_per_root,
            description_cache: parking_lot::Mutex::new(None),
        }
    }

    /// Walk the parent's lineage chain, returning the number of
    /// ancestors above it AND the root session id. `0` ⇒ parent is a
    /// root session (root_id == parent_id). A corrupt chain that
    /// exceeds [`MAX_LINEAGE_WALK_HOPS`] surfaces as
    /// `ToolError::Execution` so the LLM sees a clean failure
    /// instead of hanging the executor.
    async fn lineage_gate_inputs(
        &self,
        parent_session_id: &SessionId,
    ) -> aura_tools::Result<LineageGateInputs> {
        let parent = self
            .sessions
            .get(parent_session_id)
            .await
            .map_err(|e| ToolError::Execution(format!("load session for depth check: {e}")))?
            .ok_or_else(|| {
                ToolError::Execution(format!(
                    "lineage walk hit unknown session '{parent_session_id}' — store inconsistency"
                ))
            })?;
        // `Session.root_session_id` is denormalized at create-time, so
        // the root can be read off the parent row without traversing
        // the chain. Depth still needs the walk.
        let root_session_id = parent.root_session_id.clone();
        let mut current_id = parent_session_id.clone();
        let mut depth: u32 = 0;
        let mut current = Some(parent);
        for _ in 0..MAX_LINEAGE_WALK_HOPS {
            let session = match current.take() {
                Some(s) => s,
                None => self
                    .sessions
                    .get(&current_id)
                    .await
                    .map_err(|e| {
                        ToolError::Execution(format!("load session for depth check: {e}"))
                    })?
                    .ok_or_else(|| {
                        ToolError::Execution(format!(
                            "lineage walk hit unknown session '{current_id}' — store inconsistency"
                        ))
                    })?,
            };
            let Some(parent_link) = session.lineage else {
                return Ok(LineageGateInputs {
                    depth,
                    root_session_id,
                });
            };
            depth = depth.saturating_add(1);
            current_id = parent_link.parent_session_id;
        }
        Err(ToolError::Execution(format!(
            "lineage walk exceeded {MAX_LINEAGE_WALK_HOPS} hops starting at '{parent_session_id}'; possible cycle"
        )))
    }

    /// Return the rendered description (static body + dynamic
    /// catalogue) for the LLM. Cached against the registry's
    /// `version()`; the rebuild only fires on `register` /
    /// `register_builtins` / `reload` / `remove`. The shared
    /// `Arc<String>` is cheap to clone for read-only consumers
    /// (`description()` clones the body once at the trait boundary,
    /// which the tool registry serialises into the per-turn `tools`
    /// payload).
    fn cached_description(&self) -> Arc<String> {
        let registry_version = self.registry.version();
        let mut cache = self.description_cache.lock();
        if let Some((cached_version, body)) = &*cache
            && *cached_version == registry_version
        {
            return Arc::clone(body);
        }
        let mut out = String::with_capacity(STATIC_DESCRIPTION.len() + 256);
        out.push_str(STATIC_DESCRIPTION);
        out.push_str(&self.render_type_catalogue());
        let body = Arc::new(out);
        *cache = Some((registry_version, Arc::clone(&body)));
        body
    }

    /// Render the dynamic "available types" tail appended to the
    /// static description every LLM turn. Pulled out so unit tests can
    /// assert on the exact shape without spinning up the full tool.
    fn render_type_catalogue(&self) -> String {
        let mut out = String::from("\n# Available subagent_type values\n\n");
        let summaries = self.registry.all_summaries_sorted();
        if summaries.is_empty() {
            out.push_str(
                "(none registered — `spawn_subagent` cannot be called until the workspace's `agents/` directory contains at least one profile, or `register_builtins` has run.)\n",
            );
            return out;
        }
        for s in summaries {
            out.push_str("- **");
            out.push_str(&s.name);
            out.push_str("**");
            if let Some(tier) = s.default_tier {
                out.push_str(" (default tier: ");
                out.push_str(tier.as_str());
                out.push(')');
            }
            out.push_str(" — ");
            // Description is multi-line in literal blocks; indent
            // continuation lines so the bullet stays readable.
            let mut first = true;
            for line in s.description.lines() {
                if first {
                    first = false;
                } else {
                    out.push_str("\n  ");
                }
                out.push_str(line);
            }
            out.push('\n');
        }
        out
    }
}

fn parameters_schema() -> Value {
    json!({
        "type": "object",
        "required": ["subagent_type", "description", "prompt"],
        "properties": {
            "subagent_type": {
                "type": "string",
                "description": "Name of a registered SubagentProfile (see live list in the description). Unknown values return an error."
            },
            "description": {
                "type": "string",
                "description": "Short (3-5 word) task summary. Trace/UI display only — does NOT seed the subagent's context."
            },
            "prompt": {
                "type": "string",
                "description": "Self-contained brief the subagent sees as its first user message. The subagent does NOT see the parent transcript — include every fact, path, and line number it needs."
            },
            "model_tier": {
                "type": "string",
                "enum": ["fast", "balanced", "deep"],
                "description": "Coarse model selection. Falls back to the profile's default_tier, then the pool default. Only applies to backend='aura'."
            },
            "backend": {
                "type": "string",
                "enum": ["aura", "claude", "codex", "gemini"],
                "description": "Backend that runs the subagent. 'aura' (default) spawns a full in-process aura agent that uses the configured LLMs/tools/skills. 'claude' delegates to a local Claude Code subprocess; 'codex' delegates to OpenAI's codex CLI; 'gemini' delegates to Google's gemini CLI — these three are one-shot, run their own internal tool loops with bypassed permissions, and are best for heavy autonomous tasks where you want that external agent (not aura) to drive."
            },
            "background": {
                "type": "boolean",
                "description": "When true, returns a handle immediately and surfaces the subagent's final result as an out-of-band notification prepended to your next user turn, letting the parent keep working. Works for any backend; especially useful for long external (claude/codex/gemini) runs."
            },
            "on_timeout": {
                "type": "string",
                "enum": ["background", "kill"],
                "description": "What to do if a foreground (background=false) subagent is still running after a 2-minute foreground wait. 'background' (default) detaches it — you get a handle now and a notification when it finishes — so a slow subagent never blocks you. 'kill' cancels it and returns a timeout instead. Only applies to foreground spawns from a top-level user chat; ignored when background=true."
            },
            "group": {
                "type": "string",
                "description": "Barrier cohort name. Spawn several subagents with the SAME group in one turn to run them in parallel and get a SINGLE merged notification only once they ALL finish — instead of a separate notification per subagent. Grouped subagents always run in the background (they never block this turn). Use when you fan out a batch of related tasks and want to react to the whole set at once."
            },
            "resume_session_id": {
                "type": "string",
                "description": "Continue a prior subagent's conversation. Use a child_session_id value the user previously saw in a 'subagent_session_id: ...' tail on this same parent session — passing an arbitrary id will be rejected. The new prompt is appended to that child's existing context (for claude / codex, via the agent's own --resume). Leave unset to start a fresh subagent."
            }
        }
    })
}

#[async_trait]
impl Tool for SpawnSubagentTool {
    fn name(&self) -> &str {
        SPAWN_SUBAGENT_TOOL_NAME
    }

    fn description(&self) -> String {
        (*self.cached_description()).clone()
    }

    fn parameters_schema(&self) -> Value {
        parameters_schema()
    }

    fn max_timeout(&self) -> Duration {
        TOOL_WAIT_BACKSTOP
    }

    /// Independent of the per-response tool pool: a foreground spawn
    /// blocks on its child for the child's whole lifetime, so holding a
    /// shared permit would serialize the parent's fan-out. Concurrency
    /// is bounded out-of-band by the `SubagentDispatchLimiter` (per-root
    /// fan-out cap), not the loop's permit pool.
    fn concurrency(&self) -> ToolConcurrency {
        ToolConcurrency::Independent
    }

    fn progress_label(&self, params: &Value) -> Option<String> {
        let kind = params.get("subagent_type").and_then(Value::as_str)?;
        let summary = params
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let label = if summary.is_empty() {
            kind.to_string()
        } else {
            format!("{kind}: {summary}")
        };
        aura_tools::progress::preview_arg(&label)
    }

    async fn execute(&self, params: Value, ctx: &ToolContext) -> aura_tools::Result<ToolOutput> {
        let ParsedSpawn { request } =
            parse_spawn_request(&params, &self.registry).map_err(ToolError::InvalidParams)?;

        let LineageGateInputs {
            depth,
            root_session_id,
        } = self.lineage_gate_inputs(&ctx.session_id).await?;
        if depth >= self.max_depth {
            return Err(ToolError::SubagentDepthExceeded {
                current_depth: depth,
                cap: self.max_depth,
            });
        }

        // Reserve a fan-out slot under the root BEFORE shipping the
        // envelope. Failure paths below release it; on the happy
        // path the router's wait task releases on terminal.
        if let Err(current_count) = self
            .dispatch_limiter
            .try_reserve(&root_session_id, self.max_subagents_per_root)
        {
            return Err(ToolError::SubagentFanoutExceeded {
                current_count,
                cap: self.max_subagents_per_root,
            });
        }

        let parent = SubagentParentContext {
            session_id: ctx.session_id.clone(),
            job_id: ctx.job_id,
            span_id: ctx.span_id,
            cancel_token: ctx.cancellation_token.clone(),
        };
        let mut request = request;
        request.fan_out_root = Some(root_session_id.clone());
        // Foreground results carry the resume-id tail so the parent can
        // continue the child via `resume_session_id`. The background ack
        // is a dispatch notice, not a final result — its escorted
        // delivery surfaces the id separately, so it stays tail-free.
        let background = request.background;
        let result = match self.spawner.get() {
            // The spawner reserves nothing and releases the fan-out slot on
            // the child's terminal (every path inside `spawn`). The tool
            // reserved above; only the not-yet-wired case below needs the
            // tool to release, since `spawn` is never reached.
            Some(spawner) => spawner.spawn(parent, request).await,
            None => {
                self.dispatch_limiter.release(&root_session_id);
                SubagentResult::failed("subagent spawner not wired")
            }
        };
        // Foreground results carry the resume-id tail; the background ack
        // is a dispatch notice whose escorted delivery surfaces the id
        // separately, so it stays tail-free. The parent receives the
        // subagent's textual report; raw media never crosses the boundary.
        let text = if background {
            result.result_text()
        } else {
            result.to_tool_result_text()
        };
        Ok(ToolOutput::Text(text))
    }
}

pub fn make(config: SpawnSubagentToolConfig) -> (Arc<dyn Tool>, ToolManifest) {
    let tool = SpawnSubagentTool::from_config(config);
    let manifest = ToolManifest {
        name: tool.name().to_string(),
        description: tool.description(),
        trust_level: TrustLevel::Trusted,
        parameters_schema: tool.parameters_schema(),
        capabilities: vec![],
    };
    (Arc::new(tool), manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SubagentRegistry, unbounded_limiter};
    use aura_model::{
        ChannelType, Lineage, LineageKind, Session, SessionId, SubagentExitStatus, User,
    };
    use aura_session::SessionStore;
    use aura_session::test_support::{
        MemorySessionFolderStore, MemorySessionStore, MemorySessionSummaryStore,
    };
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    const TEST_PARENT_SESSION: &str = "parent-sess";

    fn ctx() -> ToolContext {
        ctx_with_session(TEST_PARENT_SESSION)
    }

    fn ctx_with_session(session_id: &str) -> ToolContext {
        ToolContext {
            session_id: session_id.into(),
            job_id: aura_model::JobId::default(),
            span_id: aura_model::SpanId::default(),
            user: User {
                id: "u".into(),
                name: None,
                channel: ChannelType::tui(),
            },
            timeout: Duration::from_secs(5),
            cancellation_token: CancellationToken::new(),
            workspace_root: PathBuf::from("/tmp"),
            workspace_paths: aura_workspace::WorkspacePaths::new("/tmp"),
            sandbox: None,
            approval: None,
            notifier: None,
            events: aura_tools::noop_event_sink(),
            llm: None,
            secrets: None,
            virtual_reads: None,
            background_jobs: None,
            background_control: None,
        }
    }

    fn registry_with_builtins() -> Arc<SubagentRegistry> {
        let r = SubagentRegistry::new();
        r.register_builtins();
        Arc::new(r)
    }

    /// Build an in-memory `SessionManager` seeded with a single root
    /// session at `TEST_PARENT_SESSION`. The tests that don't care
    /// about lineage call this; the depth tests build a longer chain
    /// directly against the underlying store.
    async fn sessions_with_root() -> Arc<SessionManager> {
        let store = Arc::new(MemorySessionStore::new());
        let summary_store = Arc::new(MemorySessionSummaryStore::new());
        let folder_store = Arc::new(MemorySessionFolderStore::new());
        let manager = Arc::new(SessionManager::new(
            store.clone(),
            summary_store,
            folder_store,
        ));
        let user = User {
            id: "u".into(),
            name: None,
            channel: ChannelType::tui(),
        };
        let session = manager
            .create_session(user, ChannelType::tui())
            .await
            .unwrap();
        // Override the auto-generated id so ctx() lines up.
        let mut renamed = session;
        renamed.id = SessionId::from(TEST_PARENT_SESSION);
        renamed.root_session_id = renamed.id.clone();
        store.save(&renamed).await.unwrap();
        manager
    }

    /// Persist a child session whose lineage points at `parent_id`.
    /// Returns the child's id so callers can chain further levels.
    async fn save_child_session(
        sessions: &SessionManager,
        child_id: &str,
        parent_id: &str,
    ) -> SessionId {
        let parent = sessions
            .get(&SessionId::from(parent_id))
            .await
            .unwrap()
            .expect("parent must exist before linking child");
        let child = Session {
            id: SessionId::from(child_id),
            user: parent.user.clone(),
            channel: ChannelType::from("subagent"),
            created_at: chrono::Utc::now(),
            last_active: chrono::Utc::now(),
            state: Default::default(),
            root_session_id: parent.root_session_id.clone(),
            trigger: parent.trigger.clone(),
            lineage: Some(Lineage {
                parent_session_id: parent.id.clone(),
                parent_job_id: aura_model::JobId::default(),
                parent_span_id: None,
                kind: LineageKind::Subagent,
            }),
            hidden: false,
            pinned: false,
            folder_id: None,
        };
        sessions.store().save(&child).await.unwrap();
        SessionId::from(child_id)
    }

    /// Build an empty in-memory `SessionManager` for tests that
    /// reach `execute()` but don't care about lineage depth. Returns
    /// a manager with no sessions persisted — the depth check will
    /// fall through to "parent session unknown" unless the test
    /// pre-saves a root.
    fn empty_session_manager() -> Arc<SessionManager> {
        Arc::new(SessionManager::new(
            Arc::new(MemorySessionStore::new()),
            Arc::new(MemorySessionSummaryStore::new()),
            Arc::new(MemorySessionFolderStore::new()),
        ))
    }

    /// Convenience constructor used by every test that doesn't care
    /// about depth or fan-out — pre-seeds a root session at
    /// `TEST_PARENT_SESSION` and instantiates the tool with the
    /// defaults plus an `UnboundedDispatchLimiter` so test runs can't
    /// trip the fan-out cap.
    async fn tool_with_default(spawner: SubagentSpawnerSlot) -> SpawnSubagentTool {
        SpawnSubagentTool::from_config(SpawnSubagentToolConfig {
            spawner,
            registry: registry_with_builtins(),
            sessions: sessions_with_root().await,
            dispatch_limiter: unbounded_limiter(),
            max_depth: DEFAULT_MAX_SUBAGENT_DEPTH,
            max_subagents_per_root: DEFAULT_MAX_SUBAGENTS_PER_ROOT,
        })
    }

    /// Shared bridge for tests that DO care about a specific dispatch
    /// limiter / max_depth / max fan-out — every other knob falls
    /// back to the same defaults `tool_with_default` uses.
    async fn tool_with(
        spawner: SubagentSpawnerSlot,
        sessions: Arc<SessionManager>,
        dispatch_limiter: Arc<dyn SubagentDispatchLimiter>,
        max_depth: u32,
        max_subagents_per_root: u32,
    ) -> SpawnSubagentTool {
        SpawnSubagentTool::from_config(SpawnSubagentToolConfig {
            spawner,
            registry: registry_with_builtins(),
            sessions,
            dispatch_limiter,
            max_depth,
            max_subagents_per_root,
        })
    }

    type CapturedSpawn = Arc<parking_lot::Mutex<Option<(SubagentSpawnRequest, SessionId)>>>;

    /// Stand-in for the actor-backed spawner: records the (request, parent
    /// session id) it was handed and returns a canned result.
    struct FakeSubagentSpawner {
        response: SubagentResult,
        captured: CapturedSpawn,
    }

    #[async_trait]
    impl SubagentSpawner for FakeSubagentSpawner {
        async fn spawn(
            &self,
            parent: SubagentParentContext,
            request: SubagentSpawnRequest,
        ) -> SubagentResult {
            *self.captured.lock() = Some((request, parent.session_id));
            self.response.clone()
        }
    }

    /// A slot pre-filled with a fake spawner, plus the capture handle a test
    /// reads after `execute` to assert what reached the spawner.
    fn fake_spawner(response: SubagentResult) -> (SubagentSpawnerSlot, CapturedSpawn) {
        let captured: CapturedSpawn = Arc::new(parking_lot::Mutex::new(None));
        let slot: SubagentSpawnerSlot = Arc::new(OnceLock::new());
        let _ = slot.set(Arc::new(FakeSubagentSpawner {
            response,
            captured: Arc::clone(&captured),
        }));
        (slot, captured)
    }

    /// A slot that was never filled — models the tool being called before
    /// the runtime wired the spawner.
    fn unwired_spawner() -> SubagentSpawnerSlot {
        Arc::new(OnceLock::new())
    }

    #[tokio::test]
    async fn forwards_resolved_profile_and_brief_to_channel() {
        let (spawner, captured) = fake_spawner(SubagentResult {
            child_session_id: aura_model::SessionId::from("child-1"),
            final_content: Some(vec![aura_model::ContentBlock::Text("done".into())]),
            status: SubagentExitStatus::Completed,
        });

        let tool = tool_with_default(spawner).await;
        let out = tool
            .execute(
                json!({
                    "subagent_type": "general-purpose",
                    "description": "look up X",
                    "prompt": "Investigate X and report what you find.",
                    "model_tier": "fast",
                }),
                &ctx(),
            )
            .await
            .unwrap();
        match out {
            ToolOutput::Text(s) => {
                assert!(s.contains("done"), "got: {s}");
                assert!(
                    s.contains("[subagent_session_id: child-1]"),
                    "missing resume-id tail: {s}",
                );
            }
            _ => panic!("expected Text output"),
        }
        let (req, parent_id) = captured.lock().take().expect("spawner saw a request");
        assert_eq!(req.subagent_type, "general-purpose");
        assert_eq!(req.task_summary, "look up X");
        assert_eq!(req.prompt, "Investigate X and report what you find.");
        assert_eq!(req.model_tier, Some(ModelTier::Fast));
        assert!(!req.background);
        assert!(
            matches!(&req.backend, SubagentBackend::Aura),
            "default backend should be Aura, got {:?}",
            req.backend
        );
        assert_eq!(parent_id.as_ref(), "parent-sess");
    }

    #[tokio::test]
    async fn unknown_subagent_type_returns_helpful_error() {
        let tool = tool_with_default(unwired_spawner()).await;
        let err = tool
            .execute(
                json!({
                    "subagent_type": "ghost",
                    "description": "x",
                    "prompt": "x",
                }),
                &ctx(),
            )
            .await
            .expect_err("ghost type must error");
        match err {
            ToolError::InvalidParams(msg) => {
                assert!(msg.contains("ghost"));
                assert!(msg.contains("general-purpose"));
            }
            other => panic!("expected InvalidParams, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_required_fields_surface_as_tool_error() {
        let tool = tool_with_default(unwired_spawner()).await;

        let cases = [
            json!({"description": "x", "prompt": "x"}), // missing subagent_type
            json!({"subagent_type": "general-purpose", "prompt": "x"}), // missing description
            json!({"subagent_type": "general-purpose", "description": "x"}), // missing prompt
        ];
        for case in cases {
            let err = tool
                .execute(case.clone(), &ctx())
                .await
                .expect_err("missing required field must error");
            assert!(matches!(err, ToolError::InvalidParams(_)));
        }
    }

    #[tokio::test]
    async fn background_flag_routes_through_envelope() {
        // The tool surface for background mode is identical to foreground
        // from the *tool's* perspective — it hands the request to the
        // spawner and renders whatever `SubagentResult` comes back. The
        // spawner-side branch (background dispatch ack + escorted
        // `BackgroundJobFinished` on terminal) is tested in the agent crate
        // alongside the supervisor fixtures. Here we just confirm the tool
        // propagates `background: true` and the ack lands as Text output.
        let ack_text = "[background subagent dispatched]";
        let (spawner, captured) = fake_spawner(SubagentResult {
            child_session_id: aura_model::SessionId::from("child"),
            final_content: Some(vec![aura_model::ContentBlock::Text(ack_text.into())]),
            status: SubagentExitStatus::Completed,
        });
        let tool = tool_with_default(spawner).await;
        let out = tool
            .execute(
                json!({
                    "subagent_type": "general-purpose",
                    "description": "x",
                    "prompt": "x",
                    "background": true,
                }),
                &ctx(),
            )
            .await
            .unwrap();
        match out {
            ToolOutput::Text(s) => assert_eq!(s, ack_text),
            other => panic!("expected Text, got {other:?}"),
        }
        let (req, _) = captured.lock().take().expect("spawner saw a request");
        assert!(req.background, "tool must propagate background=true");
    }

    #[tokio::test]
    async fn images_in_result_are_dropped_text_only() {
        // A subagent hands its parent a textual report, never raw media:
        // even if its final_content carries an image, the parent boundary
        // surfaces text only.
        let (spawner, _captured) = fake_spawner(SubagentResult {
            child_session_id: aura_model::SessionId::from("child"),
            final_content: Some(vec![
                aura_model::ContentBlock::Text("here's what I found".into()),
                aura_model::ContentBlock::Image {
                    blob: aura_model::BlobRef {
                        blob_id: "b-1".into(),
                    },
                    mime_type: "image/png".into(),
                },
            ]),
            status: SubagentExitStatus::Completed,
        });
        let tool = tool_with_default(spawner).await;
        let out = tool
            .execute(
                json!({
                    "subagent_type": "general-purpose",
                    "description": "x",
                    "prompt": "x",
                }),
                &ctx(),
            )
            .await
            .unwrap();
        match out {
            ToolOutput::Text(s) => assert!(s.starts_with("here's what I found"), "got: {s}"),
            other => panic!("expected Text (images dropped at the parent boundary), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn text_only_result_stays_as_text_variant() {
        let (spawner, _captured) = fake_spawner(SubagentResult {
            child_session_id: aura_model::SessionId::from("child"),
            final_content: Some(vec![aura_model::ContentBlock::Text("just text".into())]),
            status: SubagentExitStatus::Completed,
        });
        let tool = tool_with_default(spawner).await;
        let out = tool
            .execute(
                json!({
                    "subagent_type": "general-purpose",
                    "description": "x",
                    "prompt": "x",
                }),
                &ctx(),
            )
            .await
            .unwrap();
        match out {
            ToolOutput::Text(s) => assert!(s.starts_with("just text"), "got: {s}"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unwired_spawner_renders_failed_result() {
        // The slot is never filled (the tool was called before the runtime
        // wired the spawner) — the spawn surfaces as a failed result, not a
        // panic or a hang.
        let tool = tool_with_default(unwired_spawner()).await;
        let out = tool
            .execute(
                json!({
                    "subagent_type": "general-purpose",
                    "description": "x",
                    "prompt": "x",
                }),
                &ctx(),
            )
            .await
            .unwrap();
        match out {
            ToolOutput::Text(s) => assert!(s.contains("subagent spawner not wired"), "got: {s}"),
            _ => panic!("expected Text output"),
        }
    }

    #[tokio::test]
    async fn lineage_depth_at_cap_rejects_spawn() {
        let sessions = sessions_with_root().await;
        // Build a chain: parent → c1 → c2 → c3.  Depth of c3 is 3.
        save_child_session(&sessions, "c1", TEST_PARENT_SESSION).await;
        save_child_session(&sessions, "c2", "c1").await;
        save_child_session(&sessions, "c3", "c2").await;

        let tool = tool_with(
            unwired_spawner(),
            sessions,
            unbounded_limiter(),
            /* max_depth */ 3,
            DEFAULT_MAX_SUBAGENTS_PER_ROOT,
        )
        .await;
        let err = tool
            .execute(
                json!({
                    "subagent_type": "general-purpose",
                    "description": "x",
                    "prompt": "x",
                }),
                &ctx_with_session("c3"),
            )
            .await
            .expect_err("at-cap parent must be rejected");
        match err {
            ToolError::SubagentDepthExceeded { current_depth, cap } => {
                assert_eq!(current_depth, 3);
                assert_eq!(cap, 3);
            }
            other => panic!("expected SubagentDepthExceeded, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn lineage_depth_below_cap_is_accepted() {
        let (spawner, _captured) = fake_spawner(SubagentResult {
            child_session_id: aura_model::SessionId::from("grandchild"),
            final_content: Some(vec![aura_model::ContentBlock::Text("done".into())]),
            status: SubagentExitStatus::Completed,
        });
        let sessions = sessions_with_root().await;
        save_child_session(&sessions, "c1", TEST_PARENT_SESSION).await;
        // Spawning from c1 with cap=3 must succeed (depth 1 < 3).
        let tool = tool_with(
            spawner,
            sessions,
            unbounded_limiter(),
            /* max_depth */ 3,
            DEFAULT_MAX_SUBAGENTS_PER_ROOT,
        )
        .await;
        let out = tool
            .execute(
                json!({
                    "subagent_type": "general-purpose",
                    "description": "x",
                    "prompt": "x",
                }),
                &ctx_with_session("c1"),
            )
            .await
            .unwrap();
        match out {
            ToolOutput::Text(s) => assert!(s.starts_with("done"), "got: {s}"),
            _ => panic!("expected Text output"),
        }
    }

    #[tokio::test]
    async fn unknown_parent_session_surfaces_as_execution_error() {
        let tool = tool_with(
            unwired_spawner(),
            empty_session_manager(),
            unbounded_limiter(),
            DEFAULT_MAX_SUBAGENT_DEPTH,
            DEFAULT_MAX_SUBAGENTS_PER_ROOT,
        )
        .await;
        let err = tool
            .execute(
                json!({
                    "subagent_type": "general-purpose",
                    "description": "x",
                    "prompt": "x",
                }),
                &ctx_with_session("not-persisted"),
            )
            .await
            .expect_err("unknown parent must surface as execution error");
        assert!(matches!(err, ToolError::Execution(_)));
    }

    #[test]
    fn description_appends_type_catalogue() {
        let tool = SpawnSubagentTool::from_config(SpawnSubagentToolConfig {
            spawner: unwired_spawner(),
            registry: registry_with_builtins(),
            sessions: empty_session_manager(),
            dispatch_limiter: unbounded_limiter(),
            max_depth: DEFAULT_MAX_SUBAGENT_DEPTH,
            max_subagents_per_root: DEFAULT_MAX_SUBAGENTS_PER_ROOT,
        });
        let desc = tool.description();
        assert!(desc.starts_with(STATIC_DESCRIPTION));
        assert!(desc.contains("# Available subagent_type values"));
        // Every built-in profile must appear in the catalogue so the
        // parent LLM has a stable enum of names to pick from.
        for expected in ["general-purpose", "explorer", "planner", "reviewer"] {
            assert!(
                desc.contains(expected),
                "catalogue missing built-in `{expected}` — full text:\n{desc}"
            );
        }
        // Tier hints surface so the LLM can read the default tier
        // without guessing.
        assert!(desc.contains("(default tier: fast)"));
        assert!(desc.contains("(default tier: deep)"));
        // Sorted by name: explorer < general-purpose < planner < reviewer.
        let pos = |needle: &str| desc.find(needle).expect("present");
        let p_explorer = pos("- **explorer**");
        let p_general = pos("- **general-purpose**");
        let p_planner = pos("- **planner**");
        let p_reviewer = pos("- **reviewer**");
        assert!(
            p_explorer < p_general && p_general < p_planner && p_planner < p_reviewer,
            "catalogue must be sorted alphabetically"
        );
    }

    /// Limiter that records every reserve / release event so tests
    /// can assert on the dispatcher's bookkeeping.
    #[derive(Default)]
    struct RecordingLimiter {
        events: parking_lot::Mutex<Vec<&'static str>>,
        reserved: parking_lot::Mutex<u32>,
        deny_after: u32,
    }

    impl RecordingLimiter {
        fn new(deny_after: u32) -> Self {
            Self {
                events: parking_lot::Mutex::new(Vec::new()),
                reserved: parking_lot::Mutex::new(0),
                deny_after,
            }
        }
        fn events(&self) -> Vec<&'static str> {
            self.events.lock().clone()
        }
    }

    impl SubagentDispatchLimiter for RecordingLimiter {
        fn try_reserve(
            &self,
            _root_session_id: &aura_model::SessionId,
            _cap: u32,
        ) -> Result<(), u32> {
            let mut held = self.reserved.lock();
            if *held >= self.deny_after {
                self.events.lock().push("deny");
                return Err(*held);
            }
            *held += 1;
            self.events.lock().push("reserve");
            Ok(())
        }
        fn release(&self, _root_session_id: &aura_model::SessionId) {
            let mut held = self.reserved.lock();
            *held = held.saturating_sub(1);
            self.events.lock().push("release");
        }
        fn in_flight(&self, _root_session_id: &aura_model::SessionId) -> u32 {
            *self.reserved.lock()
        }
    }

    #[tokio::test]
    async fn fan_out_cap_rejects_with_helpful_error() {
        let limiter = Arc::new(RecordingLimiter::new(/* deny_after */ 0));
        let tool = tool_with(
            unwired_spawner(),
            sessions_with_root().await,
            limiter.clone() as Arc<dyn SubagentDispatchLimiter>,
            DEFAULT_MAX_SUBAGENT_DEPTH,
            /* max_subagents_per_root */ 4,
        )
        .await;
        let err = tool
            .execute(
                json!({
                    "subagent_type": "general-purpose",
                    "description": "x",
                    "prompt": "x",
                }),
                &ctx(),
            )
            .await
            .expect_err("limiter must reject when over cap");
        match err {
            ToolError::SubagentFanoutExceeded { current_count, cap } => {
                assert_eq!(current_count, 0);
                assert_eq!(cap, 4);
            }
            other => panic!("expected SubagentFanoutExceeded, got {other:?}"),
        }
        // Tool must not have leaked a reservation: only the deny was
        // recorded; no matching release happened.
        assert_eq!(limiter.events(), vec!["deny"]);
    }

    #[tokio::test]
    async fn unwired_spawner_after_reserve_releases_slot() {
        let limiter = Arc::new(RecordingLimiter::new(/* deny_after */ 8));
        let tool = tool_with(
            unwired_spawner(),
            sessions_with_root().await,
            limiter.clone() as Arc<dyn SubagentDispatchLimiter>,
            DEFAULT_MAX_SUBAGENT_DEPTH,
            8,
        )
        .await;
        let _ = tool
            .execute(
                json!({
                    "subagent_type": "general-purpose",
                    "description": "x",
                    "prompt": "x",
                }),
                &ctx(),
            )
            .await
            .unwrap();
        // Reserve happened, then the spawner was missing (slot empty). The
        // tool's fallback must release the slot itself — `spawn` was never
        // reached, so nothing else will release.
        assert_eq!(limiter.events(), vec!["reserve", "release"]);
    }

    #[test]
    fn parse_spawn_request_backend_claude() {
        let reg = registry_with_builtins();
        let v = json!({
            "subagent_type": "general-purpose",
            "description": "investigate",
            "prompt": "investigate",
            "backend": "claude",
        });
        let req = parse_spawn_request(&v, &reg).unwrap().request;
        assert_eq!(
            req.backend,
            SubagentBackend::External {
                external_kind: ExternalAgentKind::Claude,
            }
        );
    }

    #[test]
    fn parse_spawn_request_unknown_backend_rejected() {
        let reg = registry_with_builtins();
        let v = json!({
            "subagent_type": "general-purpose",
            "description": "x",
            "prompt": "x",
            "backend": "nope",
        });
        assert!(parse_spawn_request(&v, &reg).is_err());
    }

    #[test]
    fn parse_spawn_request_on_timeout_defaults_to_background() {
        let reg = registry_with_builtins();
        let v = json!({
            "subagent_type": "general-purpose",
            "description": "x",
            "prompt": "x",
        });
        let req = parse_spawn_request(&v, &reg).unwrap().request;
        assert_eq!(req.on_timeout, OnTimeout::Background);
    }

    #[test]
    fn parse_spawn_request_on_timeout_kill() {
        let reg = registry_with_builtins();
        let v = json!({
            "subagent_type": "general-purpose",
            "description": "x",
            "prompt": "x",
            "on_timeout": "kill",
        });
        let req = parse_spawn_request(&v, &reg).unwrap().request;
        assert_eq!(req.on_timeout, OnTimeout::Kill);
    }

    #[test]
    fn parse_spawn_request_unknown_on_timeout_rejected() {
        let reg = registry_with_builtins();
        let v = json!({
            "subagent_type": "general-purpose",
            "description": "x",
            "prompt": "x",
            "on_timeout": "explode",
        });
        assert!(parse_spawn_request(&v, &reg).is_err());
    }

    #[test]
    fn parse_spawn_request_group_forces_background() {
        let reg = registry_with_builtins();
        let v = json!({
            "subagent_type": "general-purpose",
            "description": "x",
            "prompt": "x",
            "group": "research",
        });
        let req = parse_spawn_request(&v, &reg).unwrap().request;
        assert_eq!(req.group.as_deref(), Some("research"));
        assert!(
            req.background,
            "a grouped subagent must run in the background"
        );
    }

    #[test]
    fn parse_spawn_request_blank_group_is_none() {
        let reg = registry_with_builtins();
        let v = json!({
            "subagent_type": "general-purpose",
            "description": "x",
            "prompt": "x",
            "group": "   ",
        });
        let req = parse_spawn_request(&v, &reg).unwrap().request;
        assert_eq!(req.group, None);
        assert!(!req.background, "blank group must not force background");
    }

    #[test]
    fn parse_spawn_request_with_resume_session_id() {
        let reg = registry_with_builtins();
        let v = json!({
            "subagent_type": "general-purpose",
            "description": "follow-up",
            "prompt": "follow-up",
            "resume_session_id": "child-abc",
        });
        let req = parse_spawn_request(&v, &reg).unwrap().request;
        assert_eq!(
            req.resume_session_id.as_ref().map(|s| s.as_ref()),
            Some("child-abc"),
        );
    }

    #[test]
    fn description_handles_empty_registry() {
        let tool = SpawnSubagentTool::from_config(SpawnSubagentToolConfig {
            spawner: unwired_spawner(),
            registry: Arc::new(SubagentRegistry::new()),
            sessions: empty_session_manager(),
            dispatch_limiter: unbounded_limiter(),
            max_depth: DEFAULT_MAX_SUBAGENT_DEPTH,
            max_subagents_per_root: DEFAULT_MAX_SUBAGENTS_PER_ROOT,
        });
        let desc = tool.description();
        assert!(desc.contains("none registered"));
    }

    #[test]
    fn parse_spawn_request_rejects_unknown_model_tier() {
        let registry = registry_with_builtins();
        let v = json!({
            "subagent_type": "general-purpose",
            "description": "x",
            "prompt": "x",
            "model_tier": "ultradeep",
        });
        assert!(parse_spawn_request(&v, &registry).is_err());
    }
}
