---
name: baybo-cli
version: 0.1.0
description: "Inspect the running Baybo instance — sessions, turns, traces, cron, costs, config, channels, MCP servers, gateway and sidecar logs — by invoking the `baybo` CLI through the Bash tool. Use whenever the user asks about Baybo's own state (what's running, what failed, what spent money, what was said) instead of answering from memory."
user-invocable: false
disable-model-invocation: false
allowed-tools:
  - Bash
---

# baybo CLI

You are operating against the **running Baybo instance** via its CLI.
The binary is at the absolute path shown below — use that path
verbatim, not bare `baybo`, because the agent's Bash subshell may
not have the binary on `$PATH`. The Bash tool also pre-exports
`BAYBO_HELP_AGENT=1` and `BAYBO_CONFIG_PATH=<active config>`, so
commands you compose:

- See the **extended help inventory** (every hidden subcommand)
- Point at the **same config** the running gateway is reading

You never need to pass `--config` yourself.

## Current session

The conversation you are answering inside right now is
**session `{{session_id}}`**. Use this id whenever a subcommand
takes `<id>` and the user is asking about "this session" / "the
current chat" / "now" — e.g. `{{BAYBO_BIN}} session show {{session_id}}`,
`{{BAYBO_BIN}} session history {{session_id}}`,
`{{BAYBO_BIN}} cost show --session {{session_id}}`. Only swap in a
different id if the user names one explicitly.

## Decision tree

Pick the family that matches the question. `{{BAYBO_BIN}}` is the
absolute path to the running binary, baked in at boot time.

| Question | Command |
|---|---|
| "Is anything running / failing / costing now?" | `{{BAYBO_BIN}} status --live` |
| "Which sessions exist?" | `{{BAYBO_BIN}} session list` |
| "What's the metadata for session X?" | `{{BAYBO_BIN}} session show <id>` |
| "What did the user/agent say?" | `{{BAYBO_BIN}} session history <id>` |
| "What got dropped by compaction?" | `{{BAYBO_BIN}} session history <id> --include-superseded` (or `--superseded-only`) |
| "What LLM calls / tool calls happened?" | `{{BAYBO_BIN}} session export <id>` (full JSON tree) |
| "Which turns are in-flight / failed?" | `{{BAYBO_BIN}} turn list [--status in-progress \| failed \| stuck]` |
| "What's the state of turn X?" | `{{BAYBO_BIN}} turn show <id>` |
| "Cancel turn X" | `{{BAYBO_BIN}} turn cancel <id> --yes` |
| "What's scheduled in cron?" | `{{BAYBO_BIN}} cron list` |
| "What does cron job X actually run?" | `{{BAYBO_BIN}} cron show <id>` (returns the prompt body) |
| "How much are we spending today?" | `{{BAYBO_BIN}} cost show` |
| "How much did session X cost?" | `{{BAYBO_BIN}} cost show --session <id>` |
| "How much did user X cost this month?" | `{{BAYBO_BIN}} cost show --user <id> --since 2026-04-01 --until 2026-05-01` |
| "Show me recent gateway logs" | `{{BAYBO_BIN}} log main -n 200` |
| "Show me telegram sidecar logs" | `{{BAYBO_BIN}} log channel telegram -n 200` |
| "What's in the config?" | `{{BAYBO_BIN}} config show` or `{{BAYBO_BIN}} config get <path>` |
| "Is the runtime healthy?" | `{{BAYBO_BIN}} doctor` |
| "What channels are registered?" | `{{BAYBO_BIN}} channel list` |
| "Which skills do I have?" | `{{BAYBO_BIN}} skills list` |
| "What does skill X declare?" | `{{BAYBO_BIN}} skills info <name>` |
| "Find skills matching <topic>" | `{{BAYBO_BIN}} skills search <query>` |
| "Are skill prereqs satisfied?" | `{{BAYBO_BIN}} skills check [name]` |
| "What MCP servers are wired?" | `{{BAYBO_BIN}} mcp list` |
| "Probe an MCP server" | `{{BAYBO_BIN}} mcp get <name>` |
| "Probe an LLM entry" | `{{BAYBO_BIN}} llm probe <name>` |
| "What models can provider X serve?" | `{{BAYBO_BIN}} llm live-model <name>` |

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

- `{{BAYBO_BIN}} tui` / `{{BAYBO_BIN}} gateway start` / `{{BAYBO_BIN}} setup`
  — these open interactive UIs or daemon processes and will hang
  the tool.
- `{{BAYBO_BIN}} log --follow` — streams until Ctrl-C, which the Bash
  tool can't deliver. Use the bounded `-n` form.
- Trying to mutate cron via CLI — `add` / `remove` / `enable` /
  `disable` / `run` are LLM-only via the `CronCreate` / `CronDelete`
  tools you already have.
- Inferring state from one command when another is more direct.
  Prefer `{{BAYBO_BIN}} cost show --session <id>` over piping
  `session export` through `jq`.

## When in doubt

Run `{{BAYBO_BIN}} --help` for the family inventory, then
`{{BAYBO_BIN}} <family> --help` for the verb inventory. Every
subcommand's `--help` documents its flags, output shape, and (when
relevant) the underlying store it reads.
