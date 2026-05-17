---
name: aura-cli
version: 0.1.0
description: "Inspect the running Aura instance — sessions, jobs, traces, cron, costs, config, channels, MCP servers, gateway and sidecar logs — by invoking the `aura` CLI through the Bash tool. Use whenever the user asks about Aura's own state (what's running, what failed, what spent money, what was said) instead of answering from memory."
user-invocable: false
disable-model-invocation: false
allowed-tools:
  - Bash
---

# aura CLI

You are operating against the **running Aura instance** via its CLI.
The binary is at the absolute path shown below — use that path
verbatim, not bare `aura`, because the agent's Bash subshell may
not have the binary on `$PATH`. The Bash tool also pre-exports
`AURA_HELP_AGENT=1` and `AURA_CONFIG_PATH=<active config>`, so
commands you compose:

- See the **extended help inventory** (every hidden subcommand)
- Point at the **same config** the running gateway is reading

You never need to pass `--config` yourself.

## Current session

The conversation you are answering inside right now is
**session `{{session_id}}`**. Use this id whenever a subcommand
takes `<id>` and the user is asking about "this session" / "the
current chat" / "now" — e.g. `{{AURA_BIN}} session show {{session_id}}`,
`{{AURA_BIN}} session history {{session_id}}`,
`{{AURA_BIN}} cost show --session {{session_id}}`. Only swap in a
different id if the user names one explicitly.

## Decision tree

Pick the family that matches the question. `{{AURA_BIN}}` is the
absolute path to the running binary, baked in at boot time.

| Question | Command |
|---|---|
| "Is anything running / failing / costing now?" | `{{AURA_BIN}} status --live` |
| "Which sessions exist?" | `{{AURA_BIN}} session list` |
| "What's the metadata for session X?" | `{{AURA_BIN}} session show <id>` |
| "What did the user/agent say?" | `{{AURA_BIN}} session history <id>` |
| "What got dropped by compaction?" | `{{AURA_BIN}} session history <id> --include-superseded` (or `--superseded-only`) |
| "What LLM calls / tool calls happened?" | `{{AURA_BIN}} session export <id>` (full JSON tree) |
| "Which jobs are in-flight / failed?" | `{{AURA_BIN}} job list [--status in-progress \| failed \| stuck]` |
| "What's the state of job X?" | `{{AURA_BIN}} job show <id>` |
| "Cancel job X" | `{{AURA_BIN}} job cancel <id> --yes` |
| "What's scheduled in cron?" | `{{AURA_BIN}} cron list` |
| "What does cron job X actually run?" | `{{AURA_BIN}} cron show <id>` (returns the prompt body) |
| "How much are we spending today?" | `{{AURA_BIN}} cost show` |
| "How much did session X cost?" | `{{AURA_BIN}} cost show --session <id>` |
| "How much did user X cost this month?" | `{{AURA_BIN}} cost show --user <id> --since 2026-04-01 --until 2026-05-01` |
| "Show me recent gateway logs" | `{{AURA_BIN}} log main -n 200` |
| "Show me telegram sidecar logs" | `{{AURA_BIN}} log channel telegram -n 200` |
| "What's in the config?" | `{{AURA_BIN}} config show` or `{{AURA_BIN}} config get <path>` |
| "Is the runtime healthy?" | `{{AURA_BIN}} doctor` |
| "What channels are registered?" | `{{AURA_BIN}} channel list` |
| "Which skills do I have?" | `{{AURA_BIN}} skills list` |
| "What does skill X declare?" | `{{AURA_BIN}} skills info <name>` |
| "Find skills matching <topic>" | `{{AURA_BIN}} skills search <query>` |
| "Are skill prereqs satisfied?" | `{{AURA_BIN}} skills check [name]` |
| "What MCP servers are wired?" | `{{AURA_BIN}} mcp list` |
| "Probe an MCP server" | `{{AURA_BIN}} mcp get <name>` |
| "Probe an LLM entry" | `{{AURA_BIN}} llm probe <name>` |
| "What models can provider X serve?" | `{{AURA_BIN}} llm live-model <name>` |

## Output conventions

- **Human format (default)**: short tabular or key/value blocks. Good
  for relaying back to the user verbatim. `session export` produces
  enormous JSON — pipe through `jq` for slicing or pass `--out <path>`
  to save to a file.
- **JSON format**: pass `--json` for machine-readable output. Useful
  when you need a specific field or aggregate across rows.
- **Mutating commands** (`cancel`, `config set/unset`, `pair revoke`,
  …) require `--yes` in slash mode. In your Bash-tool context they
  don't, but pass `--yes` anyway for explicitness.

## Avoid

- `{{AURA_BIN}} tui` / `{{AURA_BIN}} gateway start` / `{{AURA_BIN}} setup`
  — these open interactive UIs or daemon processes and will hang
  the tool.
- `{{AURA_BIN}} log --follow` — streams until Ctrl-C, which the Bash
  tool can't deliver. Use the bounded `-n` form.
- Trying to mutate cron via CLI — `add` / `remove` / `enable` /
  `disable` / `run` are LLM-only via the `CronCreate` / `CronDelete`
  tools you already have.
- Inferring state from one command when another is more direct.
  Prefer `{{AURA_BIN}} cost show --session <id>` over piping
  `session export` through `jq`.

## When in doubt

Run `{{AURA_BIN}} --help` for the family inventory, then
`{{AURA_BIN}} <family> --help` for the verb inventory. Every
subcommand's `--help` documents its flags, output shape, and (when
relevant) the underlying store it reads.
