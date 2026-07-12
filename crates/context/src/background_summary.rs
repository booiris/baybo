//! Background summary pass.
//!
//! Iterative pass driven from the session's in-actor background step:
//!
//! 1. Load the session's transcript up to a pinned ordinal.
//! 2. Make sure `<workspace>/state/sessions/<session>/summary.md` exists
//!    on disk — first pass writes `DEFAULT_NOTES_TEMPLATE` so the
//!    model's `Edit` calls always land against a real file with the
//!    canonical section scaffold.
//! 3. Hand the transcript + a session-notes prompt to the LLM with the
//!    `Read` / `Edit` tools available. The model is expected to issue
//!    one or more `Edit` calls against the notes path; each handler
//!    rewrites `summary.md` in place. The loop runs at most
//!    [`MAX_BACKGROUND_SUMMARY_ITERATIONS`] turns; tool errors come
//!    back to the model as `tool_result` bodies (prefixed `ERROR:`)
//!    so it can retry with corrected arguments.
//! 4. Record success / failure on `session_summaries`. The summary
//!    file itself was already updated by the model's `Edit` calls — no
//!    final atomic-write happens here.
//!
//! [`run_background_summary`] is the entry point. The chat callback is
//! the only agent-layer-coupled piece — it bundles the LLM client +
//! cost + trace recording the inline path also uses, and returns the
//! LLM response together with the span id and billed cost. Tool
//! execution lives here (not in the agent's `ToolExecutor`), so the
//! `Read` / `Edit` calls deliberately bypass the resource-approval
//! gate; the handlers' path scoping (only the session-notes path is
//! served) is the security boundary.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use baybo_llm::{ChatRequest, LlmResponse, ToolCallInfo, ToolDefinitionForLlm};
use baybo_model::{ChannelType, ChatMessage, ContentBlock, SessionId, User};
use baybo_session::SessionManager;
use baybo_tools::builtin::{EditTool, ReadTool};
use baybo_tools::{Tool, ToolContext, ToolError, ToolOutput};
use baybo_workspace::WorkspacePaths;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::error::ContextError;
use crate::tokenizer::Tokenizer;
use baybo_trace::LlmCallInputs;

/// Total file-size budget the summary is asked to fit within. When
/// `summary.md` already exceeds this, the prompt appends a "CRITICAL"
/// condense-now directive. Mirrors Claude Code's session-notes budget.
const MAX_TOTAL_SESSION_MEMORY_TOKENS: usize = 12_000;

/// Per-section soft cap. Sections over this trigger a per-section
/// condense directive in the prompt's appendix.
const MAX_SECTION_LENGTH: usize = 2_000;

/// Hard upper bound on tool-loop iterations per pass. The model
/// normally makes a fistful of parallel `Edit` calls in a single turn
/// then stops; this cap exists so a misbehaving model can't burn the
/// budget chasing its own tail.
const MAX_BACKGROUND_SUMMARY_ITERATIONS: usize = 10;

const READ_TOOL_NAME: &str = "Read";
const EDIT_TOOL_NAME: &str = "Edit";

/// Default scaffold written to the prompt as `currentNotes` when the
/// the session has no `summary.md` yet — gives the model fixed section
/// headers + italic descriptors to fill in. The italic descriptors are
/// preserved verbatim across passes (see the `STRUCTURE PRESERVATION
/// REMINDER` block of the prompt).
const DEFAULT_NOTES_TEMPLATE: &str = "# Session Title
_A short and distinctive 5-10 word descriptive title for the session. Super info dense, no filler_

# Current State
_What is actively being worked on right now? Pending tasks not yet completed. Immediate next steps._

# Task specification
_What did the user ask to build? Any design decisions or other explanatory context_

# Files and Functions
_What are the important files? In short, what do they contain and why are they relevant?_

# Workflow
_What bash commands are usually run and in what order? How to interpret their output if not obvious?_

# Errors & Corrections
_Errors encountered and how they were fixed. What did the user correct? What approaches failed and should not be tried again?_

# Codebase and System Documentation
_What are the important system components? How do they work/fit together?_

# Learnings
_What has worked well? What has not? What to avoid? Do not duplicate items from other sections_

# Key results
_If the user asked a specific output such as an answer to a question, a table, or other document, repeat the exact result here_

# Worklog
_Step by step, what was attempted, done? Very terse summary for each step_
";

/// Result of one chat call inside the background-summary flow. Wraps
/// the sanitized [`LlmResponse`] with the trace span id + post-pricing
/// cost the call billed — `run_background_summary` writes both into
/// `session_summaries` so per-pass telemetry is queryable without
/// re-reading the trace store.
///
/// Same shape the inline path's `CompressionRunner::run` returns; the
/// inline closure discards everything but `response`.
pub struct SummaryChatRun {
    pub response: LlmResponse,
    pub span_id: String,
    pub cost_micros: i64,
}

pub type BackgroundSummaryFuture =
    Pin<Box<dyn Future<Output = std::result::Result<SummaryChatRun, ContextError>> + Send>>;

/// Multi-turn chat hook handed to [`run_background_summary`]. Invoked
/// once per tool-loop iteration: the closure clones whatever Arcs it
/// needs into a fresh per-call LLM-bridge, fires one chat request,
/// returns the sanitized response + span/cost telemetry. Each call
/// gets its own `Compression` step + `LlmCall` span; the loop
/// accumulates `cost_micros` and reports the *last* `span_id` on the
/// outcome.
pub type BackgroundSummaryCallback =
    Box<dyn FnMut(ChatRequest, LlmCallInputs) -> BackgroundSummaryFuture + Send>;

/// Required inputs for one [`run_background_summary`] pass. `model_id`
/// is the LLM the chat callback will hit — recorded against
/// `session_summaries` for telemetry. `tokenizer` is the same one the
/// the session's `ContextManager` uses (see
/// [`crate::ContextManager::tokenizer`]) so the prompt's per-section /
/// total-budget appendices match the rest of the system's accounting.
/// `cancel_token` is the detached pass's own token (a fresh
/// `CancellationToken`, NOT derived from the session actor's token, so an
/// idle reap can't tear down an in-flight pass); the loop
/// hands it to each tool call's [`ToolContext`] so the pass's own cancel
/// cascades into any in-flight `Read` / `Edit` along with the LLM call.
pub struct BackgroundSummaryConfig {
    pub workspace: Arc<WorkspacePaths>,
    pub sessions: Arc<SessionManager>,
    pub tokenizer: Arc<dyn Tokenizer>,
    pub session_id: SessionId,
    pub up_to_ordinal: i64,
    pub model_id: String,
    pub cancel_token: CancellationToken,
}

/// Result of one [`run_background_summary`] pass. The agent-side background
/// task records its LLM calls as `Compression` steps under the triggering job
/// and logs this outcome on failure/success boundaries; no separate
/// maintenance job is created for the pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundSummaryOutcome {
    pub cursor: i64,
    pub model_id: String,
    pub span_id: String,
    pub cost_micros: i64,
}

/// Body of the trailing user prompt. `{{notesPath}}` and
/// `{{currentNotes}}` are mustache-style placeholders the builder
/// substitutes via `String::replace`; `format!` is avoided so the
/// template stays as a clean raw string with literal `"` and newlines.
const PROMPT_TEMPLATE: &str = r#"IMPORTANT: This message and these instructions are NOT part of the actual user conversation. Do NOT include any references to "note-taking", "session notes extraction", or these update instructions in the notes content.

Based on the user conversation above (EXCLUDING this note-taking instruction message as well as system prompt, claude.md entries, or any past session summaries), update the session notes file.

The file {{notesPath}} has already been read for you. Here are its current contents:
<current_notes_content>
{{currentNotes}}
</current_notes_content>

Your ONLY task is to use the Edit tool to update the notes file, then stop. You can make multiple edits (update every section as needed) - make all Edit tool calls in parallel in a single message. Do not call any other tools.

CRITICAL RULES FOR EDITING:
- The file must maintain its exact structure with all sections, headers, and italic descriptions intact
-- NEVER modify, delete, or add section headers (the lines starting with '#' like # Task specification)
-- NEVER modify or delete the italic _section description_ lines (these are the lines in italics immediately following each header - they start and end with underscores)
-- The italic _section descriptions_ are TEMPLATE INSTRUCTIONS that must be preserved exactly as-is - they guide what content belongs in each section
-- ONLY update the actual content that appears BELOW the italic _section descriptions_ within each existing section
-- Do NOT add any new sections, summaries, or information outside the existing structure
- Do NOT reference this note-taking process or instructions anywhere in the notes
- It's OK to skip updating a section if there are no substantial new insights to add. Do not add filler content like "No info yet", just leave sections blank/unedited if appropriate.
- Write DETAILED, INFO-DENSE content for each section - include specifics like file paths, function names, error messages, exact commands, technical details, etc.
- For "Key results", include the complete, exact output the user requested (e.g., full table, full answer, etc.)
- Do not include information that's already in the CLAUDE.md files included in the context
- Keep each section under ~2000 tokens/words - if a section is approaching this limit, condense it by cycling out less important details while preserving the most critical information
- Focus on actionable, specific information that would help someone understand or recreate the work discussed in the conversation
- IMPORTANT: Always update "Current State" to reflect the most recent work - this is critical for continuity after compaction

Use the Edit tool with file_path: {{notesPath}}

STRUCTURE PRESERVATION REMINDER:
Each section has TWO parts that must be preserved exactly as they appear in the current file:
1. The section header (line starting with #)
2. The italic description line (the _italicized text_ immediately after the header - this is a template instruction)

You ONLY update the actual content that comes AFTER these two preserved lines. The italic description lines starting and ending with underscores are part of the template structure, NOT content to be edited or removed.

REMEMBER: Use the Edit tool in parallel and stop. Do not continue after the edits. Only include insights from the actual user conversation, never from these note-taking instructions. Do not delete or change section headers or italic _section descriptions_."#;

/// Walk `notes` and return `(section_header, token_count)` for each
/// `# Header`-rooted section. The count covers the header line + body
/// up to (excluding) the next header. Tokens are measured with the
/// caller's tokenizer so this matches the rest of the system's
/// accounting (no chars-per-token heuristic drift).
fn section_token_counts(notes: &str, tokenizer: &dyn Tokenizer) -> Vec<(String, usize)> {
    let mut out: Vec<(String, usize)> = Vec::new();
    let mut current_header: Option<String> = None;
    let mut current_buf = String::new();
    for line in notes.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            if let Some(prev) = current_header.take() {
                out.push((prev, tokenizer.count_text(&current_buf)));
                current_buf.clear();
            }
            current_header = Some(rest.to_string());
        }
        current_buf.push_str(line);
        current_buf.push('\n');
    }
    if let Some(prev) = current_header {
        out.push((prev, tokenizer.count_text(&current_buf)));
    }
    out
}

/// Trailing user prompt appended to the session's transcript before the
/// LLM call. Substitutes `{{notesPath}}` / `{{currentNotes}}` into
/// [`PROMPT_TEMPLATE`], then optionally appends size-budget directives
/// when the existing notes already exceed
/// [`MAX_TOTAL_SESSION_MEMORY_TOKENS`] or any section runs past
/// [`MAX_SECTION_LENGTH`].
fn build_summary_prompt(
    notes_path: &Path,
    current_notes: &str,
    tokenizer: &dyn Tokenizer,
) -> String {
    let path_str = notes_path.display().to_string();
    let mut prompt = PROMPT_TEMPLATE
        .replace("{{notesPath}}", &path_str)
        .replace("{{currentNotes}}", current_notes);

    let total_tokens = tokenizer.count_text(current_notes);
    let oversized: Vec<(String, usize)> = section_token_counts(current_notes, tokenizer)
        .into_iter()
        .filter(|(_, count)| *count > MAX_SECTION_LENGTH)
        .collect();
    let over_total = total_tokens > MAX_TOTAL_SESSION_MEMORY_TOKENS;

    if over_total {
        prompt.push_str(&format!(
            r#"

CRITICAL: The session memory file is currently ~{total_tokens} tokens, which exceeds the maximum of {MAX_TOTAL_SESSION_MEMORY_TOKENS} tokens. You MUST condense the file to fit within this budget. Aggressively shorten oversized sections by removing less important details, merging related items, and summarizing older entries. Prioritize keeping "Current State" and "Errors & Corrections" accurate and detailed."#
        ));
    }
    if !oversized.is_empty() {
        let header = if over_total {
            "Oversized sections to condense:"
        } else {
            "IMPORTANT: The following sections exceed the per-section limit and MUST be condensed:"
        };
        prompt.push_str("\n\n");
        prompt.push_str(header);
        prompt.push('\n');
        for (name, tokens) in &oversized {
            prompt.push_str(&format!(
                "- \"# {name}\" is ~{tokens} tokens (limit: {MAX_SECTION_LENGTH})\n"
            ));
        }
    }

    prompt
}

async fn read_existing_summary(
    paths: &WorkspacePaths,
    session_id: &SessionId,
) -> std::io::Result<Option<String>> {
    let path = paths.session_summary_file(session_id.as_str());
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Atomic write — tempfile + rename. Creates the per-session
/// directory if absent. Same crash-safety guarantee as
/// `baybo_workspace::identity::write_identity_file`: readers see
/// either the prior file or the new one, never a partial.
async fn atomic_write_summary(
    paths: &WorkspacePaths,
    session_id: &SessionId,
    body: &str,
) -> std::io::Result<PathBuf> {
    let dir = paths.session_state_dir(session_id.as_str());
    tokio::fs::create_dir_all(&dir).await?;
    let target = paths.session_summary_file(session_id.as_str());
    let tmp = paths.session_summary_tmp_file(session_id.as_str());
    tokio::fs::write(&tmp, body).await?;
    tokio::fs::rename(&tmp, &target).await?;
    Ok(target)
}

async fn load_session_transcript_up_to(
    sessions: &SessionManager,
    session_id: &SessionId,
    up_to_ordinal: i64,
) -> Result<Vec<ChatMessage>, ContextError> {
    // SQL pushes both the supersede filter and the ordinal upper
    // bound, so neither superseded rows nor the post-snapshot tail
    // cross the wire.
    let out = sessions
        .load_active_session_messages_up_to(session_id, up_to_ordinal)
        .await
        .map_err(|e| {
            ContextError::Compression(format!(
                "load_active_session_messages_up_to({session_id}): {e}"
            ))
        })?;
    debug!(
        session_id = %session_id,
        up_to_ordinal,
        loaded = out.len(),
        "background summary: loaded session transcript slice"
    );
    Ok(out)
}

/// Translate a builtin [`Tool`]'s metadata into the LLM-side tool
/// definition. Reuses the canonical `name` / `description` / schema so
/// the model sees the same tools it would in any other agent loop;
/// path-scoping defense lives in [`execute_tool_call`].
fn tool_def_from(tool: &dyn Tool) -> ToolDefinitionForLlm {
    ToolDefinitionForLlm {
        name: tool.name().to_string(),
        description: tool.description(),
        parameters_schema: tool.parameters_schema(),
    }
}

/// Stub [`ToolContext`] for the in-loop `Read` / `Edit` calls. Both
/// builtin tools mark `ctx` as unused, so the field values just have
/// to compile — no real session/user/sandbox is exposed. The cancel
/// token is the loop's own so a cancel cascades into any
/// in-flight tool call.
fn make_tool_context(workspace_paths: &WorkspacePaths, cancel: CancellationToken) -> ToolContext {
    ToolContext {
        session_id: "background-summary".into(),
        job_id: baybo_model::JobId::default(),
        span_id: baybo_model::SpanId::default(),
        user: User {
            id: "background-summary".into(),
            name: None,
            channel: ChannelType::tui(),
        },
        timeout: Duration::from_secs(30),
        cancellation_token: cancel,
        workspace_root: workspace_paths.root().to_path_buf(),
        workspace_paths: workspace_paths.clone(),
        sandbox: None,
        sandbox_bypass_reason: None,
        approval: None,
        notifier: None,
        events: baybo_tools::noop_event_sink(),
        llm: None,
        secrets: None,
        virtual_reads: None,
        // No read-before-write enforcement: this pass legitimately rewrites
        // `summary.md` in place from the transcript it was handed, without a
        // prior `Read`. Path scoping (only the session-notes file is served)
        // is the boundary here, not the read contract.
        read_tracker: None,
        background_jobs: None,
        background_control: None,
    }
}

/// Reject any tool call whose `file_path` argument doesn't point at
/// the session-notes file. Returns `Ok(())` when the path matches or
/// when the call has no `file_path` argument (let the underlying
/// builtin tool surface the missing-arg error).
fn enforce_notes_scope(notes_path: &Path, args: &serde_json::Value) -> Result<(), String> {
    let Some(raw) = args.get("file_path").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    if Path::new(raw) == notes_path {
        Ok(())
    } else {
        Err(format!(
            "only the session-notes path is supported (got {raw}, expected {})",
            notes_path.display()
        ))
    }
}

/// Convert a [`ToolOutput`] into the plain text body the next-turn
/// `tool_result` content block carries. Attachment / multimodal
/// variants are flattened to their text — the model isn't going to
/// receive an image attachment back through this loop.
fn tool_output_text(out: ToolOutput) -> String {
    match out {
        ToolOutput::Text(s) => s,
        ToolOutput::Json(v) => v.to_string(),
        ToolOutput::Error(s) => s,
        ToolOutput::WithAttachments { text, .. } => text,
        ToolOutput::MultiModalText { text, .. } => text,
    }
}

/// Apply one `tool_use` block against the session-notes file by
/// dispatching to the corresponding builtin tool. The agent's
/// `ToolExecutor` is bypassed deliberately — calling
/// [`Tool::execute`] directly skips the resource-approval gate (the
/// background-summary loop is a system-internal pass). Errors come
/// back as the `tool_result` body (prefixed `ERROR:`) so the model
/// can retry with corrected arguments instead of the loop hard-
/// aborting.
async fn execute_tool_call(
    notes_path: &Path,
    call: &ToolCallInfo,
    ctx: &ToolContext,
) -> ContentBlock {
    let outcome: Result<String, String> = async {
        enforce_notes_scope(notes_path, &call.arguments)?;
        let edit = EditTool::new(ctx.workspace_paths.clone());
        let tool: &dyn Tool = match call.name.as_str() {
            READ_TOOL_NAME => &ReadTool,
            EDIT_TOOL_NAME => &edit,
            other => {
                return Err(format!(
                    "{other}: tool not available in the background-summary loop"
                ));
            }
        };
        match tool.execute(call.arguments.clone(), ctx).await {
            Ok(out) => Ok(tool_output_text(out)),
            Err(ToolError::Execution(msg)) | Err(ToolError::InvalidParams(msg)) => Err(msg),
            Err(e) => Err(e.to_string()),
        }
    }
    .await;

    let body = match outcome {
        Ok(s) => s,
        Err(e) => format!("ERROR: {e}"),
    };
    ContentBlock::ToolResult {
        tool_use_id: call.id.clone(),
        content: body,
        // The summary pass disables read-before-write tracking, so its tool
        // results carry no fingerprint.
        meta: None,
    }
}

/// Ensure `summary.md` exists on disk and return its contents. When
/// missing, write [`DEFAULT_NOTES_TEMPLATE`] atomically (tempfile +
/// rename, same crash-safety as later writes) so the upcoming `Edit`
/// tool calls have a real file with the section scaffold in place.
async fn ensure_notes_file(
    workspace: &WorkspacePaths,
    session_id: &SessionId,
) -> std::io::Result<String> {
    match read_existing_summary(workspace, session_id).await? {
        Some(content) => Ok(content),
        None => {
            atomic_write_summary(workspace, session_id, DEFAULT_NOTES_TEMPLATE).await?;
            Ok(DEFAULT_NOTES_TEMPLATE.to_string())
        }
    }
}

/// Execute one background-summary pass. Returns the structured outcome
/// on success; on failure, increments the session's `error_count` and
/// returns the underlying error.
///
/// The chat callback is the only agent-layer-coupled piece — it bundles
/// the LLM client + cost + trace recording machinery the inline path
/// also uses. Everything else (transcript load, prior-summary read,
/// prompt build, parse, atomic write, metadata record) is owned here.
pub async fn run_background_summary(
    config: BackgroundSummaryConfig,
    chat: BackgroundSummaryCallback,
) -> Result<BackgroundSummaryOutcome, ContextError> {
    let BackgroundSummaryConfig {
        workspace,
        sessions,
        tokenizer,
        session_id,
        up_to_ordinal,
        model_id,
        cancel_token,
    } = config;

    // `up_to_ordinal` pins the snapshot the trigger fired against so
    // concurrent appends to the session's transcript don't bleed into
    // this pass' input.
    let transcript =
        match load_session_transcript_up_to(&sessions, &session_id, up_to_ordinal).await {
            Ok(m) => m,
            Err(e) => {
                let _ = sessions
                    .record_summary_failure(&session_id, &model_id, "", Utc::now())
                    .await;
                return Err(e);
            }
        };
    if transcript.is_empty() {
        warn!(
            session_id = %session_id,
            "background summary: session transcript is empty after load — skipping pass"
        );
        return Ok(BackgroundSummaryOutcome {
            cursor: up_to_ordinal,
            model_id,
            span_id: String::new(),
            cost_micros: 0,
        });
    }

    // Pre-create `summary.md` from the default scaffold when missing
    // so the model's first `Edit` call lands against a real file with
    // the canonical section headers in place. Read failures fall back
    // to the in-memory template so the prompt still ships valid
    // content even if the disk read itself flakes.
    let current_notes = match ensure_notes_file(&workspace, &session_id).await {
        Ok(body) => body,
        Err(e) => {
            warn!(
                session_id = %session_id,
                error = %e,
                "background summary: failed to read or seed summary.md; \
                 inlining default template into the prompt"
            );
            DEFAULT_NOTES_TEMPLATE.to_string()
        }
    };

    let notes_path = workspace.session_summary_file(session_id.as_str());
    // The pinned session transcript is the persisted prefix every
    // iteration's trace span references by `up_to_ordinal`; everything
    // appended past this index (the summary prompt + the sub-loop's own
    // turns) is not in `session_messages` and rides inline as the suffix.
    let transcript_len = transcript.len();
    let mut messages: Vec<ChatMessage> = transcript;
    messages.push(ChatMessage::agent_context(vec![ContentBlock::Text(
        build_summary_prompt(&notes_path, &current_notes, tokenizer.as_ref()),
    )]));
    let tool_defs = vec![
        tool_def_from(&ReadTool),
        tool_def_from(&EditTool::new(workspace.as_ref().clone())),
    ];
    // Hang `Read` / `Edit` off the pass's cancel token so cancelling it
    // cascades into any in-flight tool call alongside the LLM request.
    let tool_ctx = make_tool_context(&workspace, cancel_token);

    let mut chat = chat;
    let mut span_id = String::new();
    let mut cost_micros: i64 = 0;
    let mut iterations: usize = 0;
    let mut applied_any_edit = false;
    loop {
        if iterations >= MAX_BACKGROUND_SUMMARY_ITERATIONS {
            let _ = sessions
                .record_summary_failure(&session_id, &model_id, &span_id, Utc::now())
                .await;
            return Err(ContextError::Compression(format!(
                "background summary loop exceeded {MAX_BACKGROUND_SUMMARY_ITERATIONS} \
iterations without terminating"
            )));
        }
        iterations += 1;

        let input_marker = LlmCallInputs::Persisted {
            last_ordinal: up_to_ordinal,
            prefix_len: transcript_len,
            suffix: messages[transcript_len..].to_vec(),
        };
        let request = ChatRequest {
            messages: messages.clone(),
            temperature: None,
            tools: tool_defs.clone(),
        };
        let SummaryChatRun {
            response,
            span_id: turn_span,
            cost_micros: turn_cost,
        } = match chat(request, input_marker).await {
            Ok(run) => run,
            Err(e) => {
                let _ = sessions
                    .record_summary_failure(&session_id, &model_id, &span_id, Utc::now())
                    .await;
                return Err(e);
            }
        };
        span_id = turn_span;
        cost_micros = cost_micros.saturating_add(turn_cost);

        let assistant_blocks = if response.content_blocks.is_empty() {
            vec![ContentBlock::Text(response.content.clone())]
        } else {
            response.content_blocks.clone()
        };
        messages.push(ChatMessage::assistant(assistant_blocks));

        if response.tool_calls.is_empty() {
            break;
        }

        let mut tool_results: Vec<ContentBlock> = Vec::with_capacity(response.tool_calls.len());
        for call in &response.tool_calls {
            if call.name == EDIT_TOOL_NAME {
                applied_any_edit = true;
            }
            tool_results.push(execute_tool_call(&notes_path, call, &tool_ctx).await);
        }
        messages.push(ChatMessage::agent_context(tool_results));
    }

    if !applied_any_edit {
        debug!(
            session_id = %session_id,
            iterations,
            "background summary: pass produced no Edit calls — notes left unchanged"
        );
    }

    // On metadata failure the file lands but the row doesn't —
    // an FS orphan that the next successful pass overwrites or
    // the startup reaper cleans up. Return success regardless.
    if let Err(e) = sessions
        .record_summary_success(
            &session_id,
            up_to_ordinal,
            cost_micros,
            &model_id,
            &span_id,
            Utc::now(),
        )
        .await
    {
        warn!(
            session_id = %session_id,
            error = %e,
            "background summary: metadata write failed; FS orphan possible"
        );
    }

    info!(
        session_id = %session_id,
        cursor = up_to_ordinal,
        cost_micros,
        iterations,
        applied_any_edit,
        "background summary: pass succeeded"
    );

    Ok(BackgroundSummaryOutcome {
        cursor: up_to_ordinal,
        model_id,
        span_id,
        cost_micros,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `chars / 4`-style tokenizer matching the SimpleTokenizer used in
    /// `lib.rs`'s tests. Stable across architectures so test thresholds
    /// don't drift when the production `TiktokenTokenizer` is updated.
    struct SimpleTokenizer;

    impl Tokenizer for SimpleTokenizer {
        fn count_text(&self, text: &str) -> usize {
            text.len() / 4 + 1
        }
        fn count_image(&self, _w: u32, _h: u32) -> usize {
            100
        }
        fn count_message(&self, _msg: &ChatMessage) -> usize {
            0
        }
    }

    fn tk() -> SimpleTokenizer {
        SimpleTokenizer
    }

    #[test]
    fn build_prompt_substitutes_path_and_notes() {
        let path = Path::new("/tmp/state/sessions/abc/summary.md");
        let prompt = build_summary_prompt(path, "# Section\n_desc_\nbody", &tk());
        assert!(prompt.contains("/tmp/state/sessions/abc/summary.md"));
        assert!(prompt.contains("<current_notes_content>"));
        assert!(prompt.contains("# Section"));
        assert!(prompt.contains("body"));
        // No mustache placeholders survive substitution.
        assert!(!prompt.contains("{{notesPath}}"));
        assert!(!prompt.contains("{{currentNotes}}"));
        // No size warnings on small input.
        assert!(!prompt.contains("CRITICAL: The session memory"));
        assert!(!prompt.contains("MUST be condensed"));
    }

    #[test]
    fn build_prompt_appends_section_oversize_warning() {
        let path = Path::new("/x");
        // SimpleTokenizer: chars/4+1, so ~2010 tokens needs ~8040 chars.
        let big_body = "x".repeat(MAX_SECTION_LENGTH * 4 + 40);
        let notes = format!("# Big\n_desc_\n{big_body}");
        let prompt = build_summary_prompt(path, &notes, &tk());
        assert!(prompt.contains("# Big"));
        assert!(prompt.contains(
            "IMPORTANT: The following sections exceed the per-section limit and MUST be condensed:"
        ));
        assert!(prompt.contains("\"# Big\""));
        assert!(!prompt.contains("CRITICAL: The session memory"));
    }

    #[test]
    fn build_prompt_appends_total_oversize_warning_when_huge() {
        let path = Path::new("/x");
        // Many small sections so total > 12k but no single section > 2k.
        let small_section = "# S\n_d_\nbody\n";
        let n = (MAX_TOTAL_SESSION_MEMORY_TOKENS * 4 / small_section.len()) + 20;
        let notes = small_section.repeat(n);
        let prompt = build_summary_prompt(path, &notes, &tk());
        assert!(prompt.contains("CRITICAL: The session memory"));
    }

    #[test]
    fn build_prompt_uses_oversized_to_condense_header_when_both() {
        let path = Path::new("/x");
        // A single huge section trips both gates.
        let huge_body = "y".repeat(MAX_TOTAL_SESSION_MEMORY_TOKENS * 4 + 100);
        let notes = format!("# Huge\n_d_\n{huge_body}");
        let prompt = build_summary_prompt(path, &notes, &tk());
        assert!(prompt.contains("CRITICAL: The session memory"));
        assert!(prompt.contains("Oversized sections to condense:"));
        // Don't double-emit the per-section-only header.
        assert!(!prompt.contains(
            "IMPORTANT: The following sections exceed the per-section limit and MUST be condensed:"
        ));
    }

    #[test]
    fn section_token_counts_splits_by_h1_headers() {
        let notes = "# A\nbody-a\n# B\nbody-b body-b body-b\n";
        let counts = section_token_counts(notes, &tk());
        assert_eq!(counts.len(), 2);
        assert_eq!(counts[0].0, "A");
        assert_eq!(counts[1].0, "B");
        assert!(counts[1].1 >= counts[0].1);
    }

    fn write_notes(dir: &tempfile::TempDir, body: &str) -> PathBuf {
        let path = dir.path().join("summary.md");
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Construct a [`ToolCallInfo`] for the test loop — ID is fixed
    /// so assertions can match on it; arguments come in pre-shaped.
    fn tc(name: &str, arguments: serde_json::Value) -> ToolCallInfo {
        ToolCallInfo {
            id: "test-call".into(),
            name: name.into(),
            arguments,
            signature: None,
        }
    }

    fn test_ctx() -> ToolContext {
        make_tool_context(
            &WorkspacePaths::new("/tmp/baybo-test"),
            CancellationToken::new(),
        )
    }

    /// Pull the `tool_result` body string out of a returned ContentBlock.
    fn body_of(block: &ContentBlock) -> &str {
        match block {
            ContentBlock::ToolResult { content, .. } => content.as_str(),
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_call_returns_file_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_notes(&dir, "# A\nbody\n");
        let block = execute_tool_call(
            &path,
            &tc(
                READ_TOOL_NAME,
                serde_json::json!({"file_path": path.to_str().unwrap()}),
            ),
            &test_ctx(),
        )
        .await;
        let body = body_of(&block);
        assert!(body.contains("body"), "expected body in: {body}");
        assert!(!body.starts_with("ERROR:"), "unexpected error: {body}");
    }

    #[tokio::test]
    async fn read_call_rejects_unrelated_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_notes(&dir, "x");
        let block = execute_tool_call(
            &path,
            &tc(
                READ_TOOL_NAME,
                serde_json::json!({"file_path": "/etc/passwd"}),
            ),
            &test_ctx(),
        )
        .await;
        let body = body_of(&block);
        assert!(body.starts_with("ERROR:"), "expected scope error: {body}");
        assert!(body.contains("only the session-notes path"), "got: {body}");
    }

    #[tokio::test]
    async fn edit_call_replaces_unique_match_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_notes(&dir, "# A\nold\n");
        let block = execute_tool_call(
            &path,
            &tc(
                EDIT_TOOL_NAME,
                serde_json::json!({
                    "file_path": path.to_str().unwrap(),
                    "old_string": "old",
                    "new_string": "new",
                }),
            ),
            &test_ctx(),
        )
        .await;
        let body = body_of(&block);
        assert!(!body.starts_with("ERROR:"), "unexpected error: {body}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# A\nnew\n");
    }

    #[tokio::test]
    async fn edit_call_rejects_ambiguous_match_without_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_notes(&dir, "x x x");
        let block = execute_tool_call(
            &path,
            &tc(
                EDIT_TOOL_NAME,
                serde_json::json!({
                    "file_path": path.to_str().unwrap(),
                    "old_string": "x",
                    "new_string": "y",
                }),
            ),
            &test_ctx(),
        )
        .await;
        let body = body_of(&block);
        assert!(body.starts_with("ERROR:"), "expected error: {body}");
        // File untouched after failed edit.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x x x");
    }

    #[tokio::test]
    async fn edit_call_replace_all_rewrites_every_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_notes(&dir, "x x x");
        let block = execute_tool_call(
            &path,
            &tc(
                EDIT_TOOL_NAME,
                serde_json::json!({
                    "file_path": path.to_str().unwrap(),
                    "old_string": "x",
                    "new_string": "y",
                    "replace_all": true,
                }),
            ),
            &test_ctx(),
        )
        .await;
        let body = body_of(&block);
        assert!(!body.starts_with("ERROR:"), "unexpected error: {body}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "y y y");
    }

    #[tokio::test]
    async fn edit_call_rejects_unrelated_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_notes(&dir, "x");
        let block = execute_tool_call(
            &path,
            &tc(
                EDIT_TOOL_NAME,
                serde_json::json!({
                    "file_path": "/etc/passwd",
                    "old_string": "x",
                    "new_string": "y",
                }),
            ),
            &test_ctx(),
        )
        .await;
        let body = body_of(&block);
        assert!(body.starts_with("ERROR:"), "expected scope error: {body}");
        assert!(body.contains("only the session-notes path"), "got: {body}");
    }

    #[tokio::test]
    async fn unknown_tool_call_surfaces_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_notes(&dir, "x");
        let block = execute_tool_call(&path, &tc("Bash", serde_json::json!({})), &test_ctx()).await;
        let body = body_of(&block);
        assert!(body.contains("not available"), "got: {body}");
    }
}
