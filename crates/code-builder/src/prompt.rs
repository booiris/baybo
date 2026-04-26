use aura_model::{ChatMessage, ContentBlock, Role};

use crate::plan::CallerCaps;

const SYSTEM_PROMPT: &str = "\
You are CodeBuilder, a Python program writer for an automated agent. The user will describe a task. \
Reply with ONLY a single JSON object matching this schema, no prose, no markdown:

{
  \"code\":           string,                    // complete Python 3 program (PEP 723 inline metadata for deps)
  \"network_required\": boolean,                 // true ONLY if the program must reach the network
  \"readable_paths\": string[],                  // absolute paths the program needs to read; empty if none
  \"estimated_runtime_seconds\": integer,        // your honest estimate of wall-clock time
  \"estimated_memory_mb\": integer,              // peak working-set estimate
  \"rationale\":      string                     // one sentence explaining your approach
}

Constraints:
- The program runs under `uv run --isolated --no-project --script script.py` with `python` >= 3.11.
- Declare any third-party PyPI deps via PEP 723 inline metadata at the top of `code`, e.g.:
    # /// script
    # requires-python = \">=3.11\"
    # dependencies = [\"httpx\", \"pandas==2.*\"]
    # ///
  Stdlib-only programs need no header. Installing PyPI deps requires `network_required: true`.
- The only writable directory is the script's CWD.
- To consume external data, list the absolute file paths in `readable_paths` (a subset of the caller's allowlist) and `open()` them inside the program.
- Print the final result to stdout.
- Do NOT fork, daemonize, or open listening sockets.
- If you cannot complete the task safely, set `code` to `raise RuntimeError(\"...\")` and explain in `rationale`.
- Do NOT wrap the JSON in Markdown fences.";

const PARSE_RETRY_NUDGE: &str = "Your previous reply was not a single valid JSON object matching the schema. \
     Reply now with ONLY the JSON object — no markdown, no prose.";

pub(crate) fn build_messages(task: &str, caps: &CallerCaps) -> Vec<ChatMessage> {
    let mut user = String::with_capacity(512);
    user.push_str("# Task\n\n");
    user.push_str(task);
    user.push_str("\n\n# Caller caps\n\n");
    user.push_str(&format!(
        "- max_runtime_seconds: {}\n",
        caps.max_runtime_seconds
            .map(|v| v.to_string())
            .unwrap_or_else(|| "(default)".into())
    ));
    user.push_str(&format!(
        "- max_memory_mb: {}\n",
        caps.max_memory_mb
            .map(|v| v.to_string())
            .unwrap_or_else(|| "(default)".into())
    ));
    user.push_str(&format!("- allow_network: {}\n", caps.allow_network));
    if !caps.extra_readable_paths.is_empty() {
        user.push_str("- extra_readable_paths:\n");
        for p in &caps.extra_readable_paths {
            user.push_str(&format!("  - {}\n", p.display()));
        }
    }

    vec![
        ChatMessage {
            role: Role::System,
            content: vec![ContentBlock::Text(SYSTEM_PROMPT.into())],
        },
        ChatMessage {
            role: Role::User,
            content: vec![ContentBlock::Text(user)],
        },
    ]
}

pub(crate) fn build_retry_messages(
    base: Vec<ChatMessage>,
    previous_reply: &str,
) -> Vec<ChatMessage> {
    let mut msgs = base;
    msgs.push(ChatMessage {
        role: Role::Assistant,
        content: vec![ContentBlock::Text(previous_reply.to_string())],
    });
    msgs.push(ChatMessage {
        role: Role::User,
        content: vec![ContentBlock::Text(PARSE_RETRY_NUDGE.into())],
    });
    msgs
}
