# CLI `agent send` — argv Mode

## Problem

`aura agent send --session <id> --message <text>` has its clap grammar and the `AgentSendForbiddenInSlash` slash-mode guard in place, but argv mode currently returns a `CliError::Manager("agent send is not yet available in argv mode …")` stub. The operator-facing contract is documented (`docs/modules/cli.md` §"Command surface" row for `agent`), the handler lives at `crates/cli/src/commands/agent.rs`, and tests cover both the slash guard and the argv deferred-error path. What is missing is the real work: driving a single agent turn from outside a chat session.

## Why it's blocked

`Router::run()` (crates/agent/src/router.rs) is designed as a long-lived `tokio::select!` loop that consumes from `incoming_rx` and `response_rx` channels owned by the supervisor actor system. `AgentLoop::run()` (crates/agent/src/agent_loop.rs) is `async` but expects to be driven by supervisor messages, not by a direct function call. There is no `Router::send_blocking(session_id, blocks) -> String` entry today, and the CLI does not hold the channel handles that would let it hand a message to the actor system.

A shortcut that starts a second `Router` inside the CLI process would violate the invariant that exactly one Router owns a given session — it would corrupt trace trees, double-charge cost tracking, and race against the live chat loop's writes to the session store.

## Proposed direction

Two mutually compatible paths. Either one unblocks `agent send`; both together is the end-state.

**Option A — `Router::send_one_shot`.** Add a dedicated entry point on `Router` that:

1. Takes `(session_id, content: Vec<ContentBlock>)` and returns an assistant reply plus the resulting trace node id.
2. Internally sends a single `IncomingMessage` through the existing router queue and awaits the corresponding `ResponseEnvelope` via a one-shot channel — no new execution path, just a synchronous facade over the existing async loop.
3. Enforces the single-owner invariant: if a chat loop is currently holding `session_id`, reject with a clear error rather than racing. This needs coordination with whatever bookkeeping the supervisor uses for active sessions (see `aura_agent::supervisor` — audit before designing).
4. Wired into `CommandContext::router` as an optional `Arc<dyn AgentSender>` so the CLI only sees the narrow interface it needs.

**Option B — daemon RPC.** If the end goal is operator use against a running `aura` process (not a fresh in-process Router), the right shape is an RPC surface — Unix socket, HTTP admin port, or an MCP-style control channel — that the CLI dials into. The daemon already owns the live Router; RPC just forwards `send` calls and streams back the reply. This sits alongside the deferred `docs/modules/cli.md` §"Deferred command families" ≫ "Service lifecycle: gateway, daemon" work.

## Design constraints

- **`AgentSendForbiddenInSlash` stays**. Inside a chat session the agent loop already owns the session; `agent send` there is not just unnecessary but actively harmful. The existing guard at `crates/cli/src/commands/agent.rs:28-32` must remain before any real argv wiring lands.
- **Trace and cost attribution**. A one-shot turn must produce the same trace/cost/memory records a normal turn does. If it does not, `aura trace show <session>` will quietly skip the turn — a debuggability regression.
- **Error surface**. Timeouts, provider failures, and "session is locked by live loop" all need distinct `CliError` variants so operators can script against them.

## Tests the CLI already ships

- `crates/cli/tests/parser.rs::agent_send_requires_session_and_message`
- `crates/cli/tests/dispatch_smoke.rs::agent_send_slash_mode_is_forbidden`
- `crates/cli/tests/dispatch_smoke.rs::agent_send_argv_mode_reports_deferred`

When the real wiring lands, the third test flips from asserting the deferred-error string to asserting a successful reply.

## Related

- `docs/modules/cli.md` §"Command surface" row for `agent` — partial-ship note
- `crates/agent/src/router.rs` — today's long-running loop
- `crates/agent/src/agent_loop.rs` — message-driven entry point
- `crates/agent/src/supervisor*` — session ownership bookkeeping
- `docs/todo/archives/cli-write-commands.md` — archived parent todo; this one carries the design work item 11 punted on
