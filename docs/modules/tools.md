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
| `Read`, `Write`, `Edit`                                                                                                                                                                                                                                               | implemented | file I/O on absolute paths. `Edit` carries extra guards when `file_path` resolves under `<workspace>/profile/`: writes are restricted to the three identity files (`SOUL.md`, `USER.md`, `IDENTITY.md`), the existing file is capped at 1 MiB, and after a successful write the change is staged and committed to `profile/`'s standalone git repo with a fixed `Aura <aura@local>` author and `--no-verify` (audit history, not a hand-curated repo). Detached HEAD or commit failure leaves the file write in place and surfaces a `commit_warning` in the tool output. A profile edit does **not** change the running session's system prompt mid-turn; `ContextManager` re-resolves it on the next compaction (see [`agent.md`](agent.md)), so the edit takes effect from then on. |
| `Bash`                                                                                                                                                                                                                                                                | implemented | `sh -c` inside the OS sandbox in **permissive filesystem** mode capped at `workspace_root + $HOME` (FHS roots RO; nothing outside that union is visible — no full host-root bind), with `~/.ssh`, `~/.aws`, `~/.gnupg`, `~/.config/gh`, `~/.docker`, `~/.kube`, `$AURA_HOME`, … masked by per-call tmpfs. Network enabled. Approval gate fires only on file-delete (`rm`/`rmdir`/`find -delete`) or destructive `git` (`reset --hard`, `push --force`, `branch -D`, …). No env/cwd persistence across calls. See `docs/modules/sandbox.md#filesystem-policy-workspace-vs-permissive`. |
| `Glob`, `Grep`                                                                                                                                                                                                                                                        | implemented | basic walkdir + regex; will be upgraded if throughput becomes an issue                                      |
| `WebFetch`                                                                                                                                                                                                                                                            | implemented | renders the response as Markdown; when `prompt` is supplied, the agent layer has bound a side LLM into `ToolContext::llm` (gateway/runtime path binds `Some`, argv-mode leaves `None`), AND the rendered content is at least `SUMMARY_MIN_CHARS` (2048 chars), runs a fixed-system extraction pass and returns the model's reply instead of the raw body. Shorter pages and LLM-less builds fall through to raw markdown — the prompt is silently ignored. |
| `SendFile`                                                                                                                                                                                                                                                            | implemented | streams a local file into `BlobStore` and returns a channel attachment                                      |
| `Now`                                                                                                                                                                                                                                                                 | implemented | returns the current UTC + host-local time so the LLM can anchor relative-time reasoning; no parameters, no capabilities |
| `Echo`                                                                                                                                                                                                                                                                | debug-only  | returns params verbatim; registered only under `debug_assertions` for round-trip smoke-testing              |
| `CronCreate`, `CronDelete`, `CronList`                                                                                                                                                                                                                                | implemented | live in `aura-cron::tools` (not `aura-tools::builtin`) because they hold `Arc<CronScheduler>`; registered from `src/runtime.rs` after the scheduler is constructed |
| `Skill`                                                                                                                                                                                                                                                               | implemented | lives in `aura-skills::tools` (parallel to `aura-cron::tools`) because it holds `Arc<SkillRegistry>` + `Arc<dyn SkillRiskCheck>`; registered from `src/runtime.rs` after the assessor is constructed. Mode 1 (no `file_path`) returns the SKILL.md body plus a categorized inventory of helper files (`references/`, `templates/`, `scripts/`, `other`). Mode 2 (`file_path` set) returns a sub-file's contents with path-traversal protections. Risk assessor and `required_env` approval gate fire on every call. |
| `SkillInstall`                                                                                                                                                                                                                                                        | implemented | lives in `aura-skills::tools` alongside `Skill`. Validates a source directory (must contain a parseable SKILL.md, must be outside the workspace skills dir, must not collide with an existing install), runs the risk assessor (`Dangerous` aborts with `ToolError::Denied`), copies the tree to `<workspace>/skills/<name>/` via a temp-dir-and-rename for atomicity, then triggers `SkillRegistry::reload()` so the new skill is available next turn. Declares `WriteFile` capability scoped to the skills dir. |
| `SkillUninstall`                                                                                                                                                                                                                                                      | implemented | symmetric counterpart to `SkillInstall`. Looks up the skill by name, refuses if it has no on-disk source or its canonicalized `source_path` doesn't sit under the workspace skills dir (so registry-only or third-party-mounted skills aren't deletable), removes the directory recursively, then triggers `SkillRegistry::reload()`. Same `WriteFile` capability scoping. |
| `TaskCreate`, `TaskGet`, `TaskList`, `TaskUpdate`                                                                                                                                                                                                                      | implemented | the session planning checklist — live in `aura-task::tools` (hold `Arc<dyn TaskStore>`); registered from `src/runtime.rs`. The agent loop re-injects the list every turn and emits it to the web checklist. See [`task.md`](task.md). |
| `Agent`, `AskUserQuestion`, `SendMessage`, `EnterPlanMode`/`ExitPlanMode`, `EnterWorktree`/`ExitWorktree`, `LSP`, `Monitor`, `NotebookEdit`, `TaskStop`/`TaskOutput`, `ToolSearch`, `WebSearch`, `Team*`                                                               | TODO stub   | lives in `builtin::todo`; not auto-registered — each depends on a backing subsystem that has not yet landed (`TaskStop`/`TaskOutput` need the background-task runtime) |

`ToolRegistry::with_defaults(blob_store, workspace_paths)` registers the
implemented set with `TrustLevel::Trusted` manifests declaring their
capabilities (`ReadFile`, `WriteFile`, `Http`, `ExecCommand`). No LLM handle is
threaded through the constructor or the `default_tools(blob_store, workspace_paths)`
factory; `WebFetch`'s prompt-driven extraction reads its LLM from the per-call
`ToolContext::llm` slot the agent layer binds at tool-call time. `workspace_paths`
is forwarded to `Edit` (and `Write`/`Bash`) so the `profile/` write guard binds to
the real workspace rather than a path-string heuristic. `SendFile` is part of this
default set and uses the supplied `BlobStore` to stage channel attachments.
Stubs exist so downstream can register them once their backing subsystem is
ready without having to invent the tool name/schema at that point.

## Design Decisions

### Unified abstraction across execution surfaces

`ToolRegistry` exposes a single `Tool` interface: Rust tools implement it directly. This keeps `AgentLoop` independent of execution shape.

### Secret access

User-managed secrets (env-var-style tokens) reach tools through
`ToolContext::secrets: Option<Arc<dyn SecretAccess>>`, bound by the agent layer
like `ToolContext::llm` (gateway/runtime binds `Some`, argv-mode leaves `None`;
consumers fail closed). The concrete impl is `SecurityGateway`, so `resolve_env`
and `redact` reuse the same deterministic mint + vault pipeline as input
sanitization, while `add`/`list`/`exists` delegate to
`aura_security::UserSecretManager` (the `user_env.<NAME>` namespace). Tools see
only the trait.

- **`SecretAdd` / `SecretList` / `SecretCheck`** (`builtin/secret.rs`) — add a
  secret (the value is resolved from a placeholder at the reveal boundary, so
  the agent never holds plaintext), list names, and check existence. Value-blind:
  no tool ever returns secret material. There is **no** delete tool — deletion is
  CLI-only.
- **`Bash` `secret_env`** — names listed there are resolved to plaintext and
  injected as env vars into that one child process (via `SpawnOpts::extra_env`
  through the sandbox, never the command string), then exact-redacted out of
  stdout/stderr before the output returns. The agent only ever passes names;
  injected names (never values) are recorded via `tracing` for audit (no approval
  prompt — the user already chose to store the secret).

Full design + rationale: [`../secret-management.md`](../secret-management.md).

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

### Embedded MCP servers

Beyond user-configured `.mcp.json` servers, the binary ships its own
always-on MCP servers (`EmbeddedMcpServer`, `mcp/embedded.rs`). A
tool-domain family builds an `EmbeddedMcpProfile` via a
`*_mcp_profile()` helper (`browser_mcp_profile` today); the boot path
collects whichever profiles fire and `embedded_servers` materialises
each into the `EmbeddedMcpServer` entry the reconciler consumes (all
re-exported from `mcp/mod.rs`). Embedded entries are baked into the
binary and never appear in or are editable via `.mcp.json`.

- The reconciler merges embedded servers with the user list on every
  tick; embedded names win on collision and shadow any same-named
  `.mcp.json` entry. Embedded children are probed for liveness and run
  with **restart-on-disconnect backoff** (`mcp/reconciler.rs`) so a
  crashed child surfaces as disconnect-and-reconnect.
- An **embedded server with `capabilities=[]`** yields an empty
  per-server resource list, so its tools **skip per-call approval**
  (`mcp/reconciler.rs`) — the contract being "Aura controls the spawn
  and trusts the vendor; don't gate on the transport command." Embedded
  servers that *do* declare capabilities still get the transport-derived
  approval like any stdio server.

The browser sidecar arrives as one of these embedded servers; see
[`sidecars.md`](../sidecars.md) for the CDDM wrapper, security
trade-offs, and docker mode in depth rather than duplicating it here.

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

`ToolExecutor` holds an `Arc<ApprovalGateMap>` shared with `ChannelRegistry`. At execution time it calls `gate_map.get(channel, session_id)` to resolve the right gate for the session's channel; if no gate is registered, `AutoDenyGate` (fail-closed) is returned. Matching:

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

`ApprovalGateMap` keeps two `DashMap`s — `type_level: DashMap<ChannelType, Arc<dyn ApprovalGate>>` for sidecar registrations and `session_level: DashMap<(ChannelType, SessionId), Arc<dyn ApprovalGate>>` for session-scoped clients. `get(channel, session_id)` tries the session-level entry first and falls back to the type-level gate, returning `AutoDenyGate` when neither is present. `ChannelRegistry` populates entries at `register()` time and evicts on `unregister()`; `ToolExecutor` reads per-call. Both sides hold an `Arc` to the same map, so gates registered after `ToolExecutor` construction are visible immediately. Adding a new channel with approval support requires only wiring an `Arc<dyn ApprovalGate>` into the `Channel` at construction time — no changes to `ToolExecutor` or bootstrap code.

### Auto mode: the on-failure risk judge (`SandboxMode::Auto`)

`sandbox.mode` selects how `BashTool` isolates a command: `sandboxed` (OS sandbox, no judge), `auto` (the **default** — sandbox + on-failure judge), or `none` (drops **only** the OS sandbox — the tool-layer `work/` jail and the uv python shim stay; for running on a trusted host). The mode is a shared, **hot-reloadable** `LiveSandboxMode` handle: a `sandbox` config reload swaps it live, and `BashTool::description` is rendered per-mode so the prompt the LLM sees re-skins to match (sandboxed/auto advertise the masked surface; `none` says the OS sandbox is off but keeps the work-dir/uv sections; `auto` adds the judge to its APPROVAL section).

Separately, the **`bench-bash` Cargo feature** (off by default, compiled out of every prod build — see `bench/swe` + `bench/terminal`) switches `BashTool` to a **bench profile**: raw exec with no OS sandbox, no uv shim, no work-dir jail, cwd inherited from the process, and a dedicated prompt — for running inside a disposable container where bwrap can't nest. It overrides `sandbox.mode` and disables the judge. `none` is the *config* counterpart that only drops the OS sandbox; the feature is the bench-only behavior hack.

`auto` keeps every command sandboxed but adds an LLM risk judge at two points. The judge runs through `ctx.llm` (current-user attribution; once the LLM layer grows a shared "flash" slot it should prefer that cheaper model) and emits a single flat JSON verdict parsed by `aura_llm::extract_json_object` (shared with the skill assessor). It is **fail-closed**: a missing LLM, provider error, or unparseable reply is treated as "risky" (the opposite of the skill assessor's availability-first fail-open), so a failure never produces an unprompted escape.

- **Pre-execution** (`pre_exec_gate`): for a destructive-token command (the same `rm`/`git reset --hard`/… set the legacy gate keys on), the judge decides `safe` → run sandboxed unprompted, or `risky` → cached approval prompt (so "approve always" sticks). In auto mode `accessed_resources` returns `[]` for these so the executor's pre-execute gate doesn't double-fire — the judge owns the gate. The blunt token list survives only as the cheap "should I ask the judge?" filter.
- **Post-failure** (`escalate_if_failed`): when a sandboxed command exits non-zero, one judge call returns `{sandbox_related, risk, rationale}` → `unrelated` ⇒ return the original failure unchanged; `related + safe` ⇒ re-run the command **outside** the sandbox automatically; `related + risky` ⇒ **uncached** approval prompt (an unsandboxed run is a different, elevated privilege than any prior sandboxed approval). In an unattended session (no approval handle: cron / nested subagent) a `risky` verdict has no human to ask, so it returns the original failure rather than escaping — but a `safe` verdict still self-heals anywhere. This runs on both the blocking path and a detached command that completes in-window; a command that overran and backgrounded is not retroactively escalated.

Every unsandboxed auto-run emits a `ctx.notifier` Warn notice (no-op in cron, but the `tracing::warn!` always fires) and tags the tool result with a `sandbox_escalation` field so the model knows the output came from an unsandboxed run. Secret values injected via `secret_env` are redacted out of the stdout/stderr tails before they reach the judge LLM.

### LLM visibility boundary

`tool_definitions()` exposes only `name`, `description`, and `parameters_schema` to the model. Capabilities and trust level are never exposed.

### Per-tool timeout ceiling

Each `Tool` impl declares its own outer wall-clock cap via `fn max_timeout(&self) -> Duration` (default 30 s). `ToolExecutor` reads it per call, writes the result into `ToolContext::timeout`, and uses it to size the outer cancel deadline (`+ APPROVAL_HEADROOM`). There is no `aura.json` knob — the cap lives in code, where the tool author already knows the right ceiling for the workload they own.

Current overrides:

- `BashTool` → 600 s — builds, test suites, and migrations regularly run for minutes; the per-call `timeout_ms` parameter still tightens further inside the tool.
- `WebFetchTool` → 120 s — slow upstreams need headroom, but a stuck host shouldn't pin a turn forever; `connect_timeout` independently caps the connect phase at 10 s.
- `SkillInstallTool` → 120 s — risk-assessor LLM call + recursive directory copy + registry hot-reload, against bundles that may carry templates + scripts + reference docs.
- `GlobTool`, `GrepTool`, `SendFileTool`, `SkillTool`, `McpTool` → 60 s — recursive walks against large monorepos (`Glob`, `Grep`), 100-MiB file streams into the blob store (`SendFile`), the per-call risk-assessor LLM round-trip (`Skill`), and arbitrary upstream MCP servers (`McpTool`) all routinely overflow the 30 s default.

All other builtins (`Read`, `Write`, `Edit`, `Echo`, `Now`, `CronCreate`, `CronDelete`, `CronList`, `SkillUninstall`, every `todo` stub) use the default — they're either pure data ops or fail fast.

### Per-tool concurrency

The agent loop dispatches every tool call in one LLM response together. Each `Tool` declares whether it is safe to overlap its siblings via `fn concurrency(&self) -> ToolConcurrency` (default `Exclusive`). `ToolRegistry::concurrency(name)` exposes the lookup (unknown tools fail safe to `Exclusive`).

- **`ToolConcurrency::Concurrent`** — safe to run alongside other concurrent calls; declared by the read-only builtins: `Read`, `Glob`, `Grep`, `WebFetch`, `Now`, `Skill`, `CronList`, `TaskGet`, `TaskList`, `SecretList`, `SecretCheck`. They read (filesystem, network, or a store) and mutate no shared state, so parallel calls within a turn cannot race. `SendFile` stays exclusive — staging a blob to send is an outward-facing write, kept on the clean "concurrent = pure read" side of the line.
- **`ToolConcurrency::Exclusive`** (the default) — runs alone among pool calls. Every mutating builtin (`Write`, `Edit`, `Bash`, `SecretAdd`, `Cron{Create,Delete}`, `Skill{Install,Uninstall}`, `Task{Create,Update}`), every MCP/dynamic tool, and any tool the registry can't classify falls here. A tool with side effects must never overlap a reader (read-while-write race) or another writer.
- **`ToolConcurrency::Independent`** — opts out of the pool: acquires no permit, so it neither waits for one nor blocks others (it can overlap even an `Exclusive` call) and is **not** counted against the cap. For tools that bound their own concurrency out-of-band. Today only `spawn_subagent`, capped per-root by its `SubagentDispatchLimiter` (default 8): a foreground spawn blocks on its child for the child's whole lifetime, so holding a shared permit would serialize the parent's fan-out — fan-out is meant to run in parallel, so it stays off the pool.

The loop enforces this with a per-response `tokio::sync::Semaphore` sized to `MAX_CONCURRENT_TOOL_CALLS` (10, a code const like the timeout ceiling — no `aura.json` knob). It is used as a read/write lock: a `Concurrent` call acquires **one** permit (so at most 10 run at once), while an `Exclusive` call acquires **all** permits, so it waits for in-flight pool calls to drain and then runs alone, blocking every other pool call until it returns. An `Independent` call acquires **no** permit, so it runs immediately and overlaps anything — including an `Exclusive` call — leaving its own out-of-band limiter (the subagent fan-out cap) as the sole bound. The semaphore is fair (FIFO), so an exclusive call is never starved by a stream of reads. The limiter is scoped to a single response because that is the only place tool calls overlap — `ToolExecutor` is process-global and shared across sessions, so a limiter there would wrongly couple unrelated sessions. The post-execution pass that appends tool results stays sequential in `tool_calls` order regardless, keeping the next turn's context byte-stable.

### Output control

Tool output should prefer structured `Json`, use `LargeText` for long text with truncation, and be sanitized before entering Job or Trace.

### Per-tool events

`ToolContext` carries an `Arc<dyn ToolEventSink>` (`events`) that tools call to emit structured observations under the running tool span. Sinks are sync and fire-and-forget — tools must not `await` emission.

Two emission styles share the same sink:

- **Phase timers** — use `start_timer(&ctx.events, "phase_name")` for the common scoped-duration case. The returned RAII guard emits a `ToolEventPayload::Phase { duration_ms }` on `Drop`, so timing a scope is one line.
- **Rich payloads** — call `events.emit(action, ToolEventPayload::HttpFetch { … })` (or `LlmCall { … }`) directly when the observation carries content. Producers MUST truncate large string fields before emitting; the executor enforces a second-layer per-call byte cap.

The agent's `ToolExecutor` builds a per-call `SpanEventRecorder` (`crates/agent/src/tool_executor.rs`), threads it through `ctx.events`, and drains the buffered `(action, payload)` entries into `SpanEventKind::ToolEvent` span events after the tool returns — regardless of success, failure, or timeout. The drain runs every text field in the payload through `SecurityGateway::sanitize_stream_fragment` so any leaked secrets are minted into placeholders before they hit the trace store. There is no entry-count cap; the only guard is a 64 MiB ceiling on the total text bytes carried by payloads from a single tool call. Entries that would push the running total past that ceiling are dropped silently — the trace is best-effort, not load-bearing. The trace view (`web/src/pages/TraceSessionPage.tsx`) renders each event under the tool span's Events tab.

`WebFetchTool` is the reference consumer:

- Phase timers: `http_request`, `read_body`, `html_to_markdown` (HTML only), `llm_summary` (prompt + side-LLM only), `read_error_body` (non-2xx).
- `HttpFetch` payload on `http_response` (both success and failure paths): status, byte count, content-type, and a UTF-8-bounded rendered-body preview.
- `LlmCall` payload on `llm_summary` (success path): model id, the full assembled user message handed to the side LLM, and the model's response (each producer-truncated to `MAX_SUMMARY_INPUT_BYTES` / `MAX_OUTPUT_BYTES`).

## Constraints

- Depends on `aura-llm`, `aura-model`, `aura-security`, `aura-storage`, `aura-workspace`, plus `rmcp` + `oauth2` + `axum` (callback listener) for the MCP client
- Does not install third-party artifacts
- Defines the `ApprovalGate` trait but never implements the user-facing UX — the per-connection gate is built by the gateway's WS sidecar (`ChannelApprovalGate` backed by an `ApprovalQueue`), and the TUI renders the resulting prompts inline in its scrollback
- `artifact_hash` must be recorded in `trace::ExecutionProvenance`

## Collaboration

| Module     | Role                                                                                               |
| ---------- | -------------------------------------------------------------------------------------------------- |
| `agent`    | `ToolExecutor` validates trust/capability, executes tools, records observability                   |
| `security` | Upper layers inject secrets and network policy (no direct dependency)                              |
| `trace`    | Records tool parameters, results, artifact hash, and source                                        |
| `llm`      | Consumes tool definitions for function calling                                                     |
| `rmcp`     | External SDK for MCP client transports                                                             |
