# tools - Tool System

## Overview

The `tools` crate provides Aura's tool abstraction, registration, and runtime routing. It exposes a uniform `Tool` interface so Agent does not care how a particular tool is implemented.

Core responsibilities:

- Define `Tool` trait and `ToolOutput`
- Manage `ToolRegistry` — both built-in tools (registered at startup) and **dynamic** tools sourced from external providers like MCP servers (registered/unregistered at runtime via `register_dynamic` / `unregister_for_source`)
- Generate tool definitions for the LLM (name, description, parameters schema only — no secrets or governance details)
- Carry source, capability, trust, and runtime metadata in `ToolManifest`

### Builtin tool set

Modeled after Claude Code's
[tools reference](https://code.claude.com/docs/en/tools-reference). Tool
names match the strings the LLM uses in function calls and operators use in
permission rules.

| Tool                                                                                                                                                                                                                                                                  | Status      | Notes                                                                                                       |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------- | ----------------------------------------------------------------------------------------------------------- |
| `Read`, `Write`, `Edit`                                                                                                                                                                                                                                               | implemented | file I/O on absolute paths                                                                                  |
| `Bash`                                                                                                                                                                                                                                                                | implemented | `sh -c` inside the OS sandbox in **permissive filesystem** mode capped at `workspace_root + $HOME` (FHS roots RO; nothing outside that union is visible — no full host-root bind), with `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.config/gh`, `~/.docker`, `~/.kube`, `$AURA_HOME`, … masked by per-call tmpfs. Network enabled. Approval gate fires only on file-delete (`rm`/`rmdir`/`find -delete`) or destructive `git` (`reset --hard`, `push --force`, `branch -D`, …). No env/cwd persistence across calls. See `docs/modules/sandbox.md#filesystem-policy-workspace-vs-permissive`. |
| `Glob`, `Grep`                                                                                                                                                                                                                                                        | implemented | basic walkdir + regex; will be upgraded if throughput becomes an issue                                      |
| `WebFetch`                                                                                                                                                                                                                                                            | implemented | returns raw body; no side-channel LLM extraction yet                                                        |
| `SendFile`                                                                                                                                                                                                                                                            | implemented | streams a local file into `BlobStore` and returns a channel attachment                                      |
| `Echo`                                                                                                                                                                                                                                                                | debug-only  | returns params verbatim; registered only under `debug_assertions` for round-trip smoke-testing              |
| `CronCreate`, `CronDelete`, `CronList`                                                                                                                                                                                                                                | implemented | live in `aura-cron::tools` (not `aura-tools::builtin`) because they hold `Arc<CronScheduler>`; registered from `src/runtime.rs` after the scheduler is constructed |
| `Skill`                                                                                                                                                                                                                                                               | implemented | lives in `aura-skills::tools` (parallel to `aura-cron::tools`) because it holds `Arc<SkillRegistry>` + `Arc<dyn SkillRiskCheck>`; registered from `src/runtime.rs` after the assessor is constructed. Mode 1 (no `file_path`) returns the SKILL.md body plus a categorized inventory of helper files (`references/`, `templates/`, `scripts/`, `other`). Mode 2 (`file_path` set) returns a sub-file's contents with path-traversal protections. Risk assessor and `required_env` approval gate fire on every call. |
| `Agent`, `AskUserQuestion`, `SendMessage`, `EnterPlanMode`/`ExitPlanMode`, `EnterWorktree`/`ExitWorktree`, `LSP`, `Monitor`, `NotebookEdit`, `Task*`/`TodoWrite`, `ToolSearch`, `WebSearch`, `Team*`                                                                   | TODO stub   | lives in `builtin::todo`; not auto-registered — each depends on a backing subsystem that has not yet landed |

`ToolRegistry::with_defaults(blob_store)` registers the implemented set with
`TrustLevel::Trusted` manifests declaring their capabilities
(`ReadFile`, `WriteFile`, `Http`, `ExecCommand`). `SendFile` is part of this
default set and uses the supplied `BlobStore` to stage channel attachments.
Stubs exist so downstream can register them once their backing subsystem is
ready without having to invent the tool name/schema at that point.

## Design Decisions

### Unified abstraction across execution surfaces

`ToolRegistry` exposes a single `Tool` interface: Rust tools implement it directly. This keeps `AgentLoop` independent of execution shape.

### Secret access (deferred)

Tool-level secret declaration and runtime injection were removed pending the
final tool-system design. `ToolContext` currently carries no secrets; a future
iteration will reintroduce per-tool secret access on top of the finalized
`Tool` trait and governance model.

### MCP client support

The `mcp` submodule (`crates/tools/src/mcp/`) implements an MCP **client**
that surfaces every tool advertised by a configured MCP server through the
agent loop's `Tool` path. Per the workspace's "MCP scope is agent-loop only"
rule, MCP tools never bridge to slash, mention, or elicitation surfaces.

- **Configuration** lives in `<workspace>/.mcp.json` (loaded/written by
  `aura_tools::mcp::McpFile`). Each entry carries a `name`, a transport
  (`stdio { command, args }` or `http { url }`), a `trust_level`, an
  optional `capabilities` set, and an optional `oauth { client_id,
  callback_port }` block. **Nothing secret lives in this file** — env
  bags, header bags, OAuth client secrets, and OAuth refresh/access
  tokens all live in `SecretVault` under the `mcp.<name>.…` namespace
  (`aura_tools::mcp::vault_keys`).
- **Tool wrapping** — every server-side tool descriptor becomes an
  `aura_tools::mcp::McpTool` named `<server>/<tool>` so MCP names cannot
  collide with builtins. Each `McpTool` carries an `Arc`-cloned
  `Peer<RoleClient>` that proxies `call_tool` over the connected
  rmcp transport.
- **Reconciler** (`McpReconciler`) re-reads `.mcp.json` every 5 seconds,
  computes a per-entry identity hash (transport + trust + capabilities +
  OAuth client_id), and connects/disconnects accordingly. Connections
  are torn down + re-established when the identity hash changes;
  `register_dynamic` / `unregister_for_source` keep the registry in
  sync. Cancelled via the shared shutdown signal.
- **OAuth** — the `oauth` submodule (`aura_tools::mcp::oauth`) drives
  OAuth 2.1 + PKCE + Dynamic Client Registration via rmcp's
  `OAuthState`. The flow runs **inline inside `aura mcp add`** for HTTP
  transports when any OAuth flag (`--client-id`, `--client-secret`,
  `--callback-port`) is passed: discovery → DCR (if no client_id) →
  PKCE → browser launch via `open::that` → localhost callback listener
  (axum) on `--callback-port` (or an ephemeral port) → token exchange →
  vault persistence. Failed authorization → no `.mcp.json` mutation.
- **Trust + capabilities** — the entry's `trust_level` becomes the
  `ToolManifest`'s ceiling; defaults are `[Http]` for HTTP and
  `[Http, ExecCommand]` for stdio. The existing
  `ToolExecutor::validate_trust` rule still fires (e.g. an `installed`
  server may not declare `WriteFile` or `ExecCommand`). Each `McpTool`
  reports a single `ResourceAccess::Http { host }` (HTTP) or
  `ResourceAccess::ExecCommand { command }` (stdio) so the approval
  gate can prompt per host or per command.

### Skill tool

The `Skill` builtin is the LLM's entry point for declarative skills.
Lives in `aura-skills::tools` so it can take `Arc<SkillRegistry>`
without `aura-tools` gaining a dep edge into `aura-skills`.

- **Visibility:** the per-turn system reminder in `AgentLoop` lists
  every `agent_invocable && trust_level != Untrusted` skill. The
  `Skill` tool itself is always registered; when the registry is
  empty the reminder is skipped, so the LLM never sees a usable list
  and won't call.
- **Slash sugar:** `/<cmd> [args]` synthesizes a deterministic
  iteration-0 `Skill(name, args)` call before iter-1, so slash and
  LLM-driven invocations share one execution path (risk assessor,
  env-var approval, trace provenance).
- **Manifest:** `TrustLevel::Trusted`, no capabilities — the tool
  itself only renders metadata and reads files inside the
  operator-controlled skill directory; outbound side effects all
  happen through whatever tools the skill body subsequently prompts.
- **Output:** `ToolOutput::Json` with `name`, `description`,
  `content`, `path`, `skill_dir`, `linked_files{references,templates,scripts,other}`,
  optional `args`, optional `risk_warning`, and a `usage_hint`. Mode 2
  (`file_path` set) collapses to `{name, file, content, file_type}`.
- **Risk:** verdict from `Arc<dyn SkillRiskCheck>` (impl in
  `aura-skills-assessor`). `Dangerous` → `ToolError::Denied`;
  `Suspicious` → response carries `risk_warning` and a
  `SessionNotifier` warn (when wired); `Safe` runs silently.
- **Env-var gate:** `SkillRequirements::required_env` is checked
  *before* prompting — any missing var fails with
  `ToolError::Execution`. If everything is set, an approval prompt
  fires using the new `ResourceAccess::Env { vars }` variant. Env
  *values* are never templated into the response; the skill body is
  expected to instruct downstream tool calls on how to read them.

See [`skills.md`](./skills.md#selection-pipeline) for how the agent
loop publishes the per-turn skill list.

### Capability-driven governance

`ToolManifest` carries coarse capability ceilings (`ToolCapability`): `ReadFile`, `WriteFile`, `Http`, `ExecCommand`. The manifest answers "what *kind* of thing may this tool do"; the concrete resource per call comes from `Tool::accessed_resources(params)` as [`ResourceAccess`] and is what the approval gate routes on. Trust level is a separate axis enforced before execution.

Typical rules:

- `Untrusted` tools may not auto-execute
- `Installed` tools may not declare `WriteFile` or `ExecCommand` (requires `Trusted`)
- Concrete paths/hosts/commands are gated by user approval, not by manifest

### User-approval gate

`ToolExecutor` holds an `Arc<ApprovalGateMap>` shared with `ChannelRegistry`. At execution time it calls `gate_map.get(user.channel)` to resolve the right gate for the session's channel; if no gate is registered, `AutoDenyGate` (fail-closed) is returned. Matching:

- `ReadFile` / `WriteFile` — component-aware path prefix (`Path::starts_with`). Approving `/tmp/a` covers `/tmp/a/b` but not `/tmp/ab`. Read and write are independent (an approved read does not cover a write). `ReadFile` is unconditionally bypassed by `ToolExecutor` (read is non-destructive; per-path prompting is friction without a safety win), but the matching rule is still defined for tools that gate writes via this mechanism.
- `Http` — `HostPattern::Exact` is case-insensitive equality; `HostPattern::Wildcard("foo.com")` covers `foo.com` and any subdomain but not `barfoo.com`. `ResourceAccess::to_approved()` produces `Exact` only — wildcards are operator-authored.
- `ExecCommand` — exact full-command string match (no shell tokenization). `BashTool` only declares this `ResourceAccess` (and therefore only triggers an approval prompt) when the command tokens contain a file-delete (`rm`, `rmdir`, `unlink`, `shred`, `srm`, `wipe`, `find … -delete`) or a destructive `git` invocation (`clean -f`, `reset --hard`, `branch -d`/`-D`/`--delete`, `tag -d`, `push -f`/`--force`/`--force-with-lease`/`--delete`/`-d`, `stash drop`/`clear`, `worktree remove`, `update-ref -d`, `filter-branch`, `filter-repo`). Non-destructive shell commands run through the OS sandbox without any pre-execution prompt.

`ApprovalDecision::ApproveAlways` promotes every `ResourceAccess` the call touched into `ApprovedResource` entries that the executor pushes directly into the shared `Mutex<Vec<ApprovedResource>>` provided by `AgentLoop`. After all tool calls in a turn complete, `AgentLoop` flushes the contents back into `SessionState::approved_resources` so they survive session replay.

#### What each builtin actually declares

Tools decide what `ResourceAccess` to declare from their parameters; the matching rules above only describe coverage *given* a declaration. Two builtins deliberately suppress declarations to skip prompts that wouldn't add safety:

- **`Read`** declares `ReadFile`, but `ToolExecutor` unconditionally drops it from the uncovered set. The tool still runs through `aura_security::is_sensitive_path` for the actual access decision.
- **`WebFetch`** is host-shape conditional (see below). `SafeResolver` + `is_blocked_ip` at DNS resolution time and `validate_url_with` at parse time are the load-bearing SSRF guards; the approval gate exists only for the *one* shape those checks cannot decide on.

#### WebFetch host-shape policy

`WebFetchTool::accessed_resources` returns one of three things based on the URL's host:

1. **Hostname URL** (`https://example.com/`) → `[]` → no approval. The `SafeResolver` (custom `reqwest::dns::Resolve`) runs `is_blocked_ip` over every resolved address and drops any that land in reserved ranges; if all addresses are blocked the connection fails before any byte goes out. Per-fetch hostname prompts add friction without catching anything the resolver doesn't already catch, and the LLM's normal "fetch a doc URL" path is the dominant case.
2. **Literal IP that the SSRF floor would reject** (RFC1918, loopback, link-local, CGNAT, IPv6 ULA, link-local v6, unspecified, IPv4-mapped-v6) → `[]` → no approval. `validate_url_with` fails the call at parse time so the prompt would just stack a click in front of an error. WHATWG-canonicalised forms (`http://2130706433/`, `http://0x7f000001/`, `http://0177.0.0.1/`, `http://127.1/`) reach this branch via `url::Url::host_str` returning the dotted form.
3. **Literal *public* IP** (`http://1.2.3.4/`, `http://[2001:db8::1]/`) → `ResourceAccess::Http { host }` → approval prompt. RFC range checks can't tell a routable IP that belongs to internal infrastructure from a real public service, so this is the only shape where human-in-the-loop adds something `is_blocked_ip` can't.

This is a deliberate departure from "concrete HTTP hosts are gated by approval" — `WebFetch` is the only HTTP-emitting builtin where the per-call resolver guarantee makes the prompt redundant. MCP HTTP transports still declare `ResourceAccess::Http { host }` and go through approval normally, because their target hosts are operator-configured at install time and the LLM doesn't pick them.

Cross-host redirects are still rejected inside the redirect policy (with a "re-issue WebFetch on the new URL" error) so a host change is always visible in the call trace, not silently followed inside `reqwest`. Per-hop SSRF re-validation runs on every redirect target regardless.

`ChannelApprovalGate` + `ApprovalQueue` (`crates/tools/src/approval.rs`) extract the common queue-and-oneshot pattern so each channel only supplies a sync waker callback (e.g. `|| event_tx.try_send(WakeUp)`). The queue exposes `peek_head` / `resolve_head` / `len` so the channel's event loop can render and dismiss inline prompts without touching oneshot internals.

`ApprovalGateMap` is a sync `HashMap<ChannelType, Arc<dyn ApprovalGate>>` behind a `std::sync::RwLock`. `ChannelRegistry` populates it at `register()` time by reading `Channel::approval_gate()` on the newly-registered handle and evicts the entry on `unregister()`; `ToolExecutor` reads it per-call. Both hold an `Arc` to the same map, so gates registered after `ToolExecutor` construction are visible immediately. Adding a new channel with approval support requires only wiring an `Arc<dyn ApprovalGate>` into the `Channel` at construction time — no changes to `ToolExecutor` or bootstrap code.

### LLM visibility boundary

`tool_definitions()` exposes only `name`, `description`, and `parameters_schema` to the model. Capabilities and trust level are never exposed.

### Output control

Tool output should prefer structured `Json`, use `LargeText` for long text with truncation, and be sanitized before entering Job or Trace.

## Constraints

- Depends on `model`, `session`, `registry`, plus `rmcp` + `oauth2` + `axum` (callback listener) for the MCP client
- Does not install third-party artifacts (that's `registry`)
- Defines the `ApprovalGate` trait but never implements the user-facing UX — the per-connection gate is built by the gateway's WS sidecar (`ChannelApprovalGate` backed by an `ApprovalQueue`), and the TUI renders the resulting prompts inline in its scrollback
- `artifact_hash` must be recorded in `trace::ExecutionProvenance`

## Collaboration

| Module     | Role                                                                                               |
| ---------- | -------------------------------------------------------------------------------------------------- |
| `agent`    | `ToolExecutor` validates trust/capability, executes tools, records observability                   |
| `security` | Upper layers inject secrets and network policy (no direct dependency)                              |
| `registry` | Provides verified third-party tool artifacts; `TrustLevel` will govern MCP tools once reintroduced |
| `trace`    | Records tool parameters, results, artifact hash, and source                                        |
| `llm`      | Consumes tool definitions for function calling                                                     |
| `rmcp`     | (Removed) External SDK for MCP client transports — to be restored with MCP support                 |
