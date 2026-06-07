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

// --- `task` scenario: a planning checklist. The TUI deliberately DROPS
// `Frame::TaskList` (the checklist is web-dashboard-only), so this subject
// must NOT appear on screen — only the trailing reply does. ---
pub const TASK_SUBJECT: &str = "VERIFY_LOGIN_FLOW";
pub const TASK_REPLY: &str = "updated the plan";
