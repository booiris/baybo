//! Shared contract between the `chat_smoke` probe (`src/bin/chat_smoke.rs`)
//! and the real-terminal test (`tests/chat_render.rs`): the scenario
//! keywords the test types and the strings the stub gateway renders for
//! each, in one place so the two sides can't drift.
//!
//! Gated behind `test-support` (see `lib.rs`) — never ships in a release
//! build.

/// Scenario selectors. The probe dispatches on the trimmed message text;
/// the test types these to trigger each scenario.
pub const SAY_TOOL: &str = "tool";
pub const SAY_SUBAGENT: &str = "subagent";
pub const SAY_APPROVAL: &str = "approval";
pub const SAY_TASK: &str = "task";
pub const SAY_MARKDOWN: &str = "markdown";
pub const SAY_MARKDOWN_TOOL: &str = "mdtool";

/// Default-echo prefix for any non-scenario message.
pub const REPLY_PREFIX: &str = "stub-reply for: ";

// --- `tool` scenario: a plain tool call lifecycle. ---
pub const TOOL_NAME: &str = "Read";
pub const TOOL_LABEL: &str = "src/lib.rs";
pub const TOOL_SUMMARY: &str = "120 lines";
pub const TOOL_REPLY: &str = "read the file for you";

// --- `subagent` scenario: a subagent spawn, which reaches the TUI as a
// `Task` tool call (the TUI has no dedicated subagent surface). ---
pub const SUBAGENT_TOOL: &str = "Task";
pub const SUBAGENT_LABEL: &str = "explore-login-bug";
pub const SUBAGENT_SUMMARY: &str = "3 candidates";
pub const SUBAGENT_REPLY: &str = "subagent finished exploring";

// --- `approval` scenario: a tool-approval modal the user resolves. ---
pub const APPROVAL_TOOL: &str = "Bash";
pub const APPROVAL_COMMAND: &str = "ls -la /tmp";
pub const APPROVAL_DESC: &str = "list the temp dir";
/// Sent after the user resolves the approval (the TUI echoes a
/// `ResolveApproval` frame back to the stub).
pub const APPROVAL_REPLY: &str = "command finished";

// --- `markdown` scenario: an answer using every block kind, streamed in
// several deltas so the block scanner is exercised across chunk boundaries.
// Split so no delta lands on a line boundary. ---
pub const MARKDOWN_DELTAS: &[&str] = &[
    "## Findi",
    "ngs\n\nThe parser is **fast** and uses `pulldown-cmark`.\n\n- first point\n- second po",
    "int\n\n1. step one\n1. step two\n\n```rust\nfn main() {}\n```\n\n| lang | speed |\n|---|---|\n| rust | fast |\n\n> a quoted aside\n\n---\n\n中文段落，用来验证宽字符换行。\n",
];
/// Heading text, which must render without its `##` markers stripped away.
pub const MARKDOWN_HEADING: &str = "Findings";
pub const MARKDOWN_EMPHASISED: &str = "fast";
/// Last rendered content of the answer — waited on to know the turn landed.
/// The scenario's closing `Message` carries the concatenated deltas, exactly as
/// a real gateway does, so `finalize_stream` correctly renders no body again.
pub const MARKDOWN_TAIL: &str = "中文段落";

// --- `mdtool` scenario: a tool call landing while a markdown block is still
// buffered. `insert_before` can only append above the live viewport, so the
// held-back prose must commit BEFORE the tool block or it would surface under
// it and invert the turn's reading order. ---
/// Streamed before the tool call. The second delta is a **complete** line, which
/// the block scanner holds as an open paragraph — so what must be flushed is a
/// buffered block, not merely a trailing partial line.
pub const MDTOOL_BEFORE: &[&str] = &["Checking the parser.\n\n", "Held in an open block\n"];
/// Streamed after the tool completes.
pub const MDTOOL_AFTER: &str = "\n\nAll done.\n";
pub const MDTOOL_HELD: &str = "Held in an open block";
pub const MDTOOL_TAIL: &str = "All done.";

// --- `task` scenario: a planning checklist. The TUI deliberately DROPS
// `Frame::TaskList` (the checklist is web-dashboard-only), so this subject
// must NOT appear on screen — only the trailing reply does. ---
pub const TASK_SUBJECT: &str = "VERIFY_LOGIN_FLOW";
pub const TASK_REPLY: &str = "updated the plan";
