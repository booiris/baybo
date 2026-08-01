# tools - Tool System

## Overview

The `tools` crate provides Baybo's tool abstraction, registration, and runtime routing. It exposes a uniform `Tool` interface so Agent does not care how a particular tool is implemented.

Core responsibilities:

- Define `Tool` trait and `ToolOutput`
- Manage `ToolRegistry` — both built-in tools (registered at startup) and **dynamic** tools sourced from external providers like MCP servers (registered/unregistered at runtime via `register_dynamic` / `unregister_for_source`)
- Generate tool definitions for the LLM (name, description, parameters schema only — no secrets or governance details)
- Carry source, capability, trust, and runtime metadata in `ToolManifest`

### Builtin tool set

Modeled after Claude Code's
[tools reference](https://code.claude.com/docs/en/tools-reference). Tool
names match the strings the LLM uses in function calls and operators use in
permission rules. The table covers the tools the LLM actually sees — the
`baybo-tools::builtin` set plus the Cron/Skill/Task/Deck/subagent/memory families
registered at runtime — not just `baybo-tools::builtin`.

| Tool                                                                                                                                                                                                                                                                  | Status      | Notes                                                                                                       |
| --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------- | ----------------------------------------------------------------------------------------------------------- |
| `Read`, `Write`, `Edit`                                                                                                                                                                                                                                               | implemented | file I/O on absolute paths. Writes under `<workspace>/personas/` are **audited instead of approved** (`builtin/managed_repo.rs`): the approval gate is bypassed, the file is capped at 1 MiB, and the change is staged and committed to `personas/`'s standalone git repo with a fixed `Baybo <baybo@local>` author and `--no-verify` (audit history, not a hand-curated repo). Three tiers qualify — the shared **`personas/USER.md`** (writable by every agent: what one learns about the person is worth the others knowing), and, restricted to the *calling agent's own* directory, **identity files** (`<agent>/{SOUL,IDENTITY,USER}.md`; `Edit` only, closed filename allowlist) and **memory files** (`<agent>/memory/**`; `Edit` and `Write`, no filename allowlist — see [`memory-builtin.md`](memory-builtin.md)). A path containing `..` or a `.git` component is refused outright, and a symlink anywhere below `personas/` is refused too (`starts_with` is lexical, so all three would otherwise carry the bypass elsewhere). Detached HEAD or commit failure leaves the write in place and surfaces a `commit_warning`; a byte-identical `Write` is a no-op, not a warning. Such an edit does **not** change the running session's system prompt mid-turn; `ContextManager` re-resolves it on the next compaction (see [`agent.md`](agent.md)). |
| `MemoryDelete`                                                                                                                                                                                                                                                        | implemented | removes one file from the calling agent's own memory tree, audit-committing the removal; any other path is refused before any I/O, so the tool declares no gated resource at all — a prompt could only sanction a deletion that cannot happen. If the tree's `MEMORY.md` still names the deleted file, the tool output says so rather than rewriting the model's markdown. Exists because Bash `rm` gating is permission-mode-dependent (an `auto`-mode risk judge can wave a scoped `rm` through unprompted; `manual` routes to an approval an unattended dream pass never gets answered; `free` skips the check), so forgetting needs to be deterministic and audited regardless. See [`memory-builtin.md`](memory-builtin.md). |
| `Bash`                                                                                                                                                                                                                                                                | implemented | `sh -c` under the configured `permission` policy. `auto` and `manual` use the OS sandbox in **permissive filesystem** mode when an inner sandbox runner is available; if Baybo detects an outer container/sandbox, Bash silently skips the inner sandbox, and if no backend is available on a non-container host, Bash warns and runs without it. `free` runs directly while keeping the tool-layer work-dir jail and uv shim. Network enabled. No env/cwd persistence across calls. See `docs/modules/sandbox.md#filesystem-policy-workspace-vs-permissive`. |
| `Glob`, `Grep`                                                                                                                                                                                                                                                        | implemented | shell out to the `rg` (ripgrep) binary (`Glob` = `rg --files --glob`, mtime-sorted; `Grep` parses `--json`/`--null` output). Sensitive paths are filtered from `Grep` results via `baybo_security::is_sensitive_path`. See [`external-commands.md`](../external-commands.md) for the `rg` dependency. |
| `WebFetch`                                                                                                                                                                                                                                                            | implemented | renders the response as Markdown; when `prompt` is supplied, the agent layer has bound a side LLM into `ToolContext::lite_llm` (gateway/runtime path binds `Some`, argv-mode leaves `None`), AND the rendered content is at least `SUMMARY_MIN_CHARS` (2048 chars), runs a fixed-system extraction pass and returns the model's reply instead of the raw body. Shorter pages and LLM-less builds fall through to raw markdown — the prompt is silently ignored. |
| `AttachFile`                                                                                                                                                                                                                                                            | implemented | streams a local file into `BlobStore`; the loop attaches it to the turn's final reply                                      |
| `PutBlob`                                                                                                                                                                                                                                                                | implemented | owner-only; streams any local file into `BlobStore` and returns structured `blob_id` / MIME / size metadata without attaching it; accepts an optional use-case-specific cap up to 100 MiB |
| `Now`                                                                                                                                                                                                                                                                 | implemented | returns the current UTC + host-local time so the LLM can anchor relative-time reasoning; no parameters, no capabilities |
| `SecretAdd`, `SecretList`, `SecretCheck`                                                                                                                                                                                                                              | implemented | value-blind user-secret management (`builtin/secret.rs`); no delete tool — deletion is CLI-only. See [Secret access](#secret-access) |
| `JobList`, `JobStop`                                                                                                                                                                                                                                                  | implemented | view and kill the conversation's in-flight background jobs (detached subagents and `Bash` commands) via `BackgroundJobControl`; outside a user-facing session they report nothing in flight |
| `Echo`                                                                                                                                                                                                                                                                | debug-only  | returns params verbatim; registered only under `debug_assertions` for round-trip smoke-testing              |
| `CronCreate`, `CronUpdate`, `CronDelete`, `CronPause`, `CronResume`, `CronList`                                                                                                                                                                                                     | implemented | live in `baybo-cron::tools` (not `baybo-tools::builtin`) because they hold `Arc<CronScheduler>`; registered from `crates/baybo/src/runtime.rs` after the scheduler is constructed. `CronUpdate` edits a job in place (a partial patch of `title` / `prompt` / `schedule` / `timezone`) and is the tool to reach for whenever a job changes — it keeps the job's id, and with it its past runs and the conversations they opened, which delete + create throws away. `CronPause`/`CronResume` take a job out of the firing schedule and back in (resume recomputes the next fire from now); `CronDelete` moves it to the recycle bin, where only the human surfaces (web page, admin API) can see or restore it — `CronList` returns live jobs only. See [`cron.md`](cron.md) |
| `Skill`                                                                                                                                                                                                                                                               | implemented | lives in `baybo-skills::tools` (parallel to `baybo-cron::tools`) because it holds `Arc<SkillRegistry>` + `Arc<dyn SkillRiskCheck>`; registered from `crates/baybo/src/runtime.rs` after the assessor is constructed. Mode 1 (no `file_path`) returns the SKILL.md body plus a categorized inventory of helper files (`references/`, `templates/`, `scripts/`, `other`). Mode 2 (`file_path` set) returns a sub-file's contents with path-traversal protections. Risk assessor and `required_env` approval gate fire on every call. |
| `SkillInstall`                                                                                                                                                                                                                                                        | implemented | lives in `baybo-skills::tools` alongside `Skill`. Validates a source directory (must contain a parseable SKILL.md, must be outside the workspace skills dir, must not collide with an existing install), runs the risk assessor (`Dangerous` aborts with `ToolError::Denied`), copies the tree to `<workspace>/skills/<name>/` via a temp-dir-and-rename for atomicity, then triggers `SkillRegistry::reload()` so the new skill is available next turn. Declares `WriteFile` capability scoped to the skills dir. |
| `SkillUninstall`                                                                                                                                                                                                                                                      | implemented | symmetric counterpart to `SkillInstall`. Looks up the skill by name, refuses if it has no on-disk source or its canonicalized `source_path` doesn't sit under the workspace skills dir (so registry-only or third-party-mounted skills aren't deletable), removes the directory recursively, then triggers `SkillRegistry::reload()`. Same `WriteFile` capability scoping. |
| `TaskCreate`, `TaskGet`, `TaskList`, `TaskUpdate`                                                                                                                                                                                                                      | implemented | the session planning checklist — live in `baybo-task::tools` (hold `Arc<dyn TaskStore>`); registered from `crates/baybo/src/runtime.rs`. The agent loop re-injects a throttled model-facing reminder (a nudge after ~10 turns without task management) and emits the live list to the web checklist unthrottled. See [`task.md`](task.md). |
| `DeckCardList`, `DeckCardGet`, `DeckCardCreate`, `DeckCardUpdate`                                                                                                                                                                                                                                     | implemented | live in `baybo-deck::tools` (hold `Arc<DeckManager>`); registered from `crates/baybo/src/runtime.rs` after the deck manager is constructed. **Authoring pair** (`Create`/`Update`): install / replace a card from a staged bundle directory; both run the dry-run gate (static validation → real host boot → one refresh-op invocation → schema-checked first snapshot) before anything goes live, and failures return in the tool result (with the service's stderr) so the agent iterates in the same turn; `Trusted` + `ReadFile` (they read the agent's staged dir). **Discovery pair** (`List`/`Get`): `List` returns every live card's id/title/size/enabled/spec_hash; `Get(card_id)` returns the card's four source files inline — together they let the agent update a card it didn't author in this conversation (resolve description → uuid, edit from real source) since its file tools can't reach the deck root; `Trusted`, no filesystem capability (they read through the manager). All four are `channels: [owner]`. See [`deck.md`](deck.md) |
| `spawn_subagent`                                                                                                                                                                                                                                                      | implemented | lives in `baybo-subagent::tool` (holds the spawner, subagent registry, session manager, and the shared `SubagentDispatchLimiter`); registered from `crates/baybo/src/runtime.rs`. See the concurrency section below for why it is `Independent`. |
| `viking_recall`, `viking_store`, `viking_forget`, `viking_archive_expand`                                                                                                                                                                                             | implemented | the memory backend's own tools (`crates/memory/src/backends/openviking.rs`); registered from `crates/baybo/src/runtime.rs` as builtins whenever memory is enabled — whatever `Memory::tools()` yields. See [`memory.md`](memory.md). |
| `Agent`, `AskUserQuestion`, `SendMessage`, `EnterPlanMode`/`ExitPlanMode`, `EnterWorktree`/`ExitWorktree`, `LSP`, `Monitor`, `NotebookEdit`, `TaskStop`/`TaskOutput`, `ToolSearch`, `WebSearch`, `Team*`                                                               | TODO stub   | lives in `builtin::todo`; not auto-registered — each depends on a backing subsystem that has not yet landed (`TaskStop`/`TaskOutput` need the background-task runtime) |

`ToolRegistry::with_defaults(blob_store, workspace_paths, proxy, permission)`
registers the implemented set with `TrustLevel::Trusted` manifests declaring their
capabilities (`ReadFile`, `WriteFile`, `Http`, `ExecCommand`). No LLM handle is
threaded through the constructor or the
`default_tools(blob_store, workspace_paths, proxy, permission)`
factory; `WebFetch`'s prompt-driven extraction reads its LLM from the per-call
`ToolContext::lite_llm` slot the agent layer binds at tool-call time. `workspace_paths`
is forwarded to `Edit` (and `Write`/`Bash`) so the `personas/` write guard binds to
the real workspace rather than a path-string heuristic. `proxy` is the optional
egress proxy handed to `WebFetch` (which also uses the supplied `BlobStore` to
archive the fetched raw content), and `permission` is the shared
`LivePermissionMode` handle described in the Bash permission policy section.
`AttachFile` is part of this default set and uses the supplied `BlobStore` to
stage channel attachments. `PutBlob` is the owner-only reference-producing
counterpart: it returns `ToolOutput::Json` rather than
`ToolOutput::WithAttachments`, so a skill can embed the capability id in its own
protocol without creating a duplicate file card. Everything up to and including
the store write lives once in `builtin::blob_upload`: `LocalBlobFile` for
absolute/sensitive-path checks, regular-file and size validation and the
streaming write, plus `resolve_mime_type`, the shared `BLOB_TOOL_TIMEOUT`, and
the `path`-derived progress label and `ReadFile` access declaration. Only their
delivery semantics differ, so only their `execute` bodies do — the divergent
halves (probing duration/pages/dimensions and building a media `ContentBlock`
versus returning a JSON reference) stay per-tool rather than collapsing into one
`match` on a mode flag, and the two manifests stay separate because
`channels: [owner]` is the only enforced gate on minting a bearer `blob_id`.
`resolve_mime_type` is shared for a concrete reason: it rejects an empty or
newline-bearing override, which otherwise reaches `HeaderValue::from_str` in the
gateway's blob download, gets refused there, and serves the bytes with no
`Content-Type` at all. `AttachFile` skipped that check while `PutBlob` made it
— exactly the drift a shared seam exists to prevent. Both name the parameter
`mime_type`.
Stubs exist so downstream can register them once their backing subsystem is
ready without having to invent the tool name/schema at that point.

## Design Decisions

### Unified abstraction across execution surfaces

`ToolRegistry` exposes a single `Tool` interface: Rust tools implement it directly. This keeps `AgentLoop` independent of execution shape.

### Secret access

User-managed secrets (env-var-style tokens) reach tools through
`ToolContext::secrets: Option<Arc<dyn SecretAccess>>`, bound by the agent layer
like `ToolContext::lite_llm` (gateway/runtime binds `Some`, argv-mode leaves `None`;
consumers fail closed). The concrete impl is `SecurityGateway`, so `resolve_env`,
`redact` and `sanitize` reuse the same deterministic mint + vault pipeline as
input sanitization, while `add`/`list`/`exists` delegate to
`baybo_security::UserSecretManager` (the `user_env.<NAME>` namespace). Tools see
only the trait.

- **`SecretAdd` / `SecretList` / `SecretCheck`** (`builtin/secret.rs`) — add a
  secret (the value is resolved from a placeholder at the reveal boundary, so
  the agent never holds plaintext), list names, and check existence. Value-blind:
  no tool ever returns secret material. There is **no** delete tool — deletion is
  CLI-only.
- **`Bash` `secret_env`** — names listed there are resolved to plaintext and
  injected as env vars into that one child process (via `SpawnOpts::extra_env`
  through the sandbox, never the command string), then exact-redacted out of
  stdout/stderr before blocking or foreground-completed detached output returns.
  If the command overruns and moves to the background, stdout/stderr files and
  completion tails are stored raw and may contain echoed secret values; that risk
  is recorded in the background handoff warning. The agent only ever passes
  names; injected names (never values) are recorded via `tracing` for audit (no
  approval prompt — the user already chose to store the secret).

Full design + rationale: [`../secret-management.md`](../secret-management.md).

### MCP client support

The `mcp` submodule (`crates/tools/src/mcp/`) implements an MCP **client**
that surfaces every tool advertised by a configured MCP server through the
agent loop's `Tool` path. Per the workspace's "MCP scope is agent-loop only"
rule, MCP tools never bridge to slash, mention, or elicitation surfaces.

- **Configuration** lives in `<workspace>/.mcp.json` (loaded/written by
  `baybo_tools::mcp::McpFile`). Each entry carries a `name`, a transport
  (`stdio { command, args }` or `http { url }`), a `trust_level`, an
  optional `capabilities` set, and an optional `oauth { client_id,
  callback_port }` block. **Nothing secret lives in this file** — env
  bags, header bags, OAuth client secrets, and OAuth refresh/access
  tokens all live in `SecretVault` under the `mcp.<name>.…` namespace
  (`baybo_tools::mcp::vault_keys`).
- **Tool wrapping** — every server-side tool descriptor becomes an
  `baybo_tools::mcp::McpTool` named `<server>/<tool>` so MCP names cannot
  collide with builtins. Each `McpTool` carries an `Arc`-cloned
  `Peer<RoleClient>` that proxies `call_tool` over the connected
  rmcp transport.
- **Reconciler** (`McpReconciler`) re-reads `.mcp.json` every 5 seconds,
  computes a per-entry identity hash (transport + trust + capabilities +
  OAuth client_id), and connects/disconnects accordingly. Connections
  are torn down + re-established when the identity hash changes;
  `register_dynamic` / `unregister_for_source` keep the registry in
  sync. Cancelled via the shared shutdown signal.
- **OAuth** — the `oauth` submodule (`baybo_tools::mcp::oauth`) drives
  OAuth 2.1 + PKCE + Dynamic Client Registration via rmcp's
  `OAuthState`. The flow runs **inline inside `baybo mcp add`** for HTTP
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
  (`mcp/reconciler.rs`) — the contract being "Baybo controls the spawn
  and trusts the vendor; don't gate on the transport command." Embedded
  servers that *do* declare capabilities still get the transport-derived
  approval like any stdio server.

The browser sidecar arrives as one of these embedded servers; see
[`sidecars.md`](../sidecars.md) for the CDDM wrapper, security
trade-offs, and docker mode in depth rather than duplicating it here.

### Skill tool

The `Skill` builtin is the LLM's entry point for declarative skills.
Lives in `baybo-skills::tools` so it can take `Arc<SkillRegistry>`
without `baybo-tools` gaining a dep edge into `baybo-skills`.

- **Visibility:** `ContextManager::ensure_seeded` appends a skill
  reminder once per fresh session — a persisted `Role::User`
  agent-context row (`render_skill_reminder`) listing every
  `agent_invocable && trust_level != Untrusted` skill. The `Skill`
  tool itself is always registered; when the registry is empty the
  reminder is skipped, so the LLM never sees a usable list and won't
  call.
- **Slash expansion:** `/<cmd> [args]` is expanded before the first
  LLM call by `ContextManager::expand_slash_command`: the skill's body
  (via `baybo_skills::render_skill_for_slash`, `{{session_id}}`
  substituted) is appended as a hidden agent-context row. This
  deliberately skips the risk assessor — an explicit user slash command
  is treated as authorized. Linked sub-files still carry an inventory +
  hint so the model pulls them with a follow-up `Skill` tool call, which
  does run the full gate (risk assessor, env-var approval).
- **Manifest:** `TrustLevel::Trusted`, no capabilities — the tool
  itself only renders metadata and reads files inside the
  operator-controlled skill directory; outbound side effects all
  happen through whatever tools the skill body subsequently prompts.
- **Output:** `ToolOutput::Json` with `name`, `description`,
  `content`, `path`, `skill_dir`, `linked_files{references,templates,scripts,other}`,
  optional `args`, optional `risk_warning`, and a `usage_hint`. Mode 2
  (`file_path` set) collapses to `{name, file, content, file_type}`.
- **Risk:** verdict from `Arc<dyn SkillRiskCheck>` (impl in
  `baybo-skills-assessor`). `Dangerous` → `ToolError::Denied`;
  `Suspicious` → response carries `risk_warning` and a
  `SessionNotifier` warn (when wired); `Safe` runs silently.
- **Env-var gate:** `SkillRequirements::required_env` is checked
  *before* prompting — any missing var fails with
  `ToolError::Execution`. If everything is set, an approval prompt
  fires using the new `ResourceAccess::Env { vars }` variant. Env
  *values* are never templated into the response; the skill body is
  expected to instruct downstream tool calls on how to read them.

See [`skills.md`](./skills.md#selection-pipeline) for how the skill
list is rendered and reused for slash-command matching.

### Capability-driven governance

`ToolManifest` carries coarse capability ceilings (`ToolCapability`): `ReadFile`, `WriteFile`, `Http`, `ExecCommand`. The manifest answers "what *kind* of thing may this tool do"; the concrete resource per call comes from `Tool::accessed_resources(params)` as [`ResourceAccess`] and is what the approval gate routes on. Trust level is a separate axis enforced before execution.

The manifest also carries `channels: Vec<ChannelType>` (empty = every channel, the norm): a channel-restricted tool is enforced twice — the agent loop assembles the LLM's tool list via `ToolRegistry::tool_definitions_for_channel(&session.channel)` so other sessions never see it (the channel is session-stable, so the list stays byte-identical across calls and the prompt-cache sort guarantee holds), and the executor refuses a call that names it anyway (omission is not a gate against a hallucinated or skill-body-prompted name). The deck tools (`DeckCardList`/`DeckCardGet`/`DeckCardCreate`/`DeckCardUpdate`) are the first restricted tools (`channels: [owner]` — the deck is the owner's surface).

Typical rules:

- `Untrusted` tools may not auto-execute
- `Installed` tools may not declare `WriteFile` or `ExecCommand` (requires `Trusted`)
- Concrete paths/hosts/commands are gated by user approval, not by manifest
- A `channels`-restricted tool is invisible and refused outside its channels

### User-approval gate

`ToolExecutor` holds an `Arc<ApprovalGateMap>` shared with `ChannelRegistry`. At execution time it calls `gate_map.get(channel, session_id)` to resolve the right gate for the session's channel; if no gate is registered, `AutoDenyGate` (fail-closed) is returned. Matching:

- `ReadFile` / `WriteFile` — component-aware path prefix (`Path::starts_with`). Approving `/tmp/a` covers `/tmp/a/b` but not `/tmp/ab`. Read and write are independent (an approved read does not cover a write). `ReadFile` is unconditionally bypassed by `ToolExecutor` (read is non-destructive; per-path prompting is friction without a safety win), but the matching rule is still defined for tools that gate writes via this mechanism.
- `Http` — `HostPattern::Exact` is case-insensitive equality; `HostPattern::Wildcard("foo.com")` covers `foo.com` and any subdomain but not `barfoo.com`. `ResourceAccess::to_approved()` produces `Exact` only — wildcards are operator-authored.
- `ExecCommand` — exact full-command string match (no shell tokenization). `BashTool` declares this `ResourceAccess` for every executable command when `permission=manual`; FileToolRedirect commands (`cat foo`, `sed -i …`) are rejected before any approval prompt. `auto` owns the destructive-command gate internally through the risk judge, and `free` disables Bash pre-execution approval by running directly outside the OS sandbox.

`ApprovalDecision::ApproveAlways` promotes every `ResourceAccess` the call touched into `ApprovedResource` entries that the executor pushes directly into the shared `Mutex<Vec<ApprovedResource>>` provided by `AgentLoop`. After all tool calls in a turn complete, `AgentLoop` flushes the contents back into `SessionState::approved_resources` so they survive session replay.

#### What each builtin actually declares

Tools decide what `ResourceAccess` to declare from their parameters; the matching rules above only describe coverage *given* a declaration. Two builtins deliberately suppress declarations to skip prompts that wouldn't add safety:

- **`Read`** declares `ReadFile`, but `ToolExecutor` unconditionally drops it from the uncovered set. The tool still runs through `baybo_security::is_sensitive_path` for the actual access decision.
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

### Bash permission policy

`permission`: `auto` (default), `manual`, or `free`. `auto` judges destructive commands before the active execution route and judges sandbox failures before automatic unsandboxed retry; if the judge does not approve automatic escape, Baybo asks for approval.

The post-failure judge is a **prompt-injection surface**, because a `safe`
verdict re-runs the command on the host with no approval gate and its input is
whatever the failed command printed — a dependency's build script, a test
fixture, a downloaded file. Its `stdout`/`stderr` tails therefore get the same
framing as tool output entering the main transcript: `<tool_output>` envelope
(`baybo_model::wrap_tool_output`), forged-delimiter escaping, and a banner when
`InjectionDetector` fires; the system prompt states that envelope contents are
data, that text claiming to be a policy update or a prior verdict is itself
grounds for `risky`, and that a command whose behaviour is decided by content
the judge cannot see is not safe. Both tails also run through
`SecretAccess::sanitize` unconditionally — `redact` alone only covers values
this run injected as env vars, which would leave an ordinary command's capture
unredacted. `manual` asks before every Bash command, then runs in the sandbox when one is available; if the sandboxed run fails, it asks again before retrying unsandboxed. If Baybo detects an outer container/sandbox, Bash silently skips the inner OS sandbox under the same approval policy; if no sandbox backend is available on a non-container host, Bash emits a notice before running without it. `free` runs directly without Bash approval or OS sandboxing. Legacy values `open` and `none` are accepted as aliases for `free`.

The permission is a shared, **hot-reloadable** `LivePermissionMode` handle: a `permission` config reload swaps it live, and `BashTool::description` is rendered per permission so the prompt the LLM sees matches the active isolation and approval policy.

In user-facing sessions, `on_timeout=background` detaches over-budget Bash commands for both execution routes: sandboxed commands hand the sandbox backend's detached child to the background-job sink, while `permission=free` / self-CLI commands spawn an unsandboxed child in its own process group so `/stop` can cancel the whole tree. Commands with `secret_env` can background too, but the output files and completion notification tails are raw; the handoff logs and returns an explicit warning when secret env vars were injected.

Separately, the **`bench-bash` Cargo feature** (off by default, compiled out of every prod build — see `bench/swe` + `bench/terminal-bench-1.0`) switches `BashTool` to a **bench profile**: raw exec with no OS sandbox, no uv shim, no work-dir jail, cwd inherited from the process, and a dedicated prompt — for running inside a disposable container where bwrap can't nest. It overrides `permission` and disables the judge. `permission=free` is the config counterpart that only drops the OS sandbox; the feature is the bench-only behavior hack.

The detailed design is in [`../permission.md`](../permission.md).

### LLM visibility boundary

`tool_definitions()` exposes only `name`, `description`, and `parameters_schema` to the model. Capabilities and trust level are never exposed.

### Per-tool timeout ceiling

Each `Tool` impl declares its own outer wall-clock cap via `fn max_timeout(&self) -> Duration` (default 30 s). `ToolExecutor` reads it per call, writes the result into `ToolContext::timeout`, and uses it to size the outer cancel deadline (`+ APPROVAL_HEADROOM`). There is no `baybo.json` knob — the cap lives in code, where the tool author already knows the right ceiling for the workload they own.

Current overrides:

- `BashTool` → 600 s — builds, test suites, and migrations regularly run for minutes; the per-call `timeout_ms` parameter still tightens further inside the tool.
- `WebFetchTool` → 120 s — slow upstreams need headroom, but a stuck host shouldn't pin a turn forever; `connect_timeout` independently caps the connect phase at 10 s.
- `SkillInstallTool` → 120 s — risk-assessor LLM call + recursive directory copy + registry hot-reload, against bundles that may carry templates + scripts + reference docs.
- `GlobTool`, `GrepTool`, `AttachFileTool`, `PutBlobTool`, `SkillTool`, `McpTool` → 60 s — recursive walks against large monorepos (`Glob`, `Grep`), 100-MiB file streams into the blob store (`AttachFile`, `PutBlob`), the per-call risk-assessor LLM round-trip (`Skill`), and arbitrary upstream MCP servers (`McpTool`) all routinely overflow the 30 s default.
- `SpawnSubagentTool` → 30 days (`TOOL_WAIT_BACKSTOP`) — a foreground spawn blocks on its child's whole lifetime, so the outer deadline is a backstop, not a budget.
- `viking_store` → config-driven (`OpenViking` `timeouts.store_max`).

All other tools (`Read`, `Write`, `Edit`, `Echo`, `Now`, `Secret*`, `JobList`, `JobStop`, `Cron*`, `DeckCard*`, `SkillUninstall`, `Task*`, the other `viking_*` tools, every `todo` stub) use the default — they're either pure data ops or fail fast.

### Per-tool concurrency

The agent loop dispatches every tool call in one LLM response together. Each `Tool` declares whether it is safe to overlap its siblings via `fn concurrency(&self) -> ToolConcurrency` (default `Exclusive`). `ToolRegistry::concurrency(name)` exposes the lookup (unknown tools fail safe to `Exclusive`).

- **`ToolConcurrency::Concurrent`** — safe to run alongside other concurrent calls; declared by the read-only builtins: `Read`, `Glob`, `Grep`, `WebFetch`, `Now`, `Skill`, `CronList`, `TaskGet`, `TaskList`, `SecretList`, `SecretCheck`. They read (filesystem, network, or a store) and mutate no shared state, so parallel calls within a turn cannot race. `AttachFile` and `PutBlob` stay exclusive — staging a blob to send or reference is an outward-facing write, kept on the clean "concurrent = pure read" side of the line.
- **`ToolConcurrency::Exclusive`** (the default) — runs alone among pool calls. Every mutating builtin (`Write`, `Edit`, `MemoryDelete`, `Bash`, `SecretAdd`, `Cron{Create,Update,Delete,Pause,Resume}`, `DeckCard{Create,Update}`, `Skill{Install,Uninstall}`, `Task{Create,Update}`), every MCP/dynamic tool, and any tool the registry can't classify falls here. A tool with side effects must never overlap a reader (read-while-write race) or another writer.
- **`ToolConcurrency::Independent`** — opts out of the pool: acquires no permit, so it neither waits for one nor blocks others (it can overlap even an `Exclusive` call) and is **not** counted against the cap. For tools that bound their own concurrency out-of-band. Today only `spawn_subagent`, capped per-root by its `SubagentDispatchLimiter` (default 8): a foreground spawn blocks on its child for the child's whole lifetime, so holding a shared permit would serialize the parent's fan-out — fan-out is meant to run in parallel, so it stays off the pool.

The loop enforces this with a per-response `tokio::sync::Semaphore` sized to `MAX_CONCURRENT_TOOL_CALLS` (10, a code const like the timeout ceiling — no `baybo.json` knob). It is used as a read/write lock: a `Concurrent` call acquires **one** permit (so at most 10 run at once), while an `Exclusive` call acquires **all** permits, so it waits for in-flight pool calls to drain and then runs alone, blocking every other pool call until it returns. An `Independent` call acquires **no** permit, so it runs immediately and overlaps anything — including an `Exclusive` call — leaving its own out-of-band limiter (the subagent fan-out cap) as the sole bound. The semaphore is fair (FIFO), so an exclusive call is never starved by a stream of reads. The limiter is scoped to a single response because that is the only place tool calls overlap — `ToolExecutor` is process-global and shared across sessions, so a limiter there would wrongly couple unrelated sessions. The post-execution pass that appends tool results stays sequential in `tool_calls` order regardless, keeping the next turn's context byte-stable.

### Output control

Tool output should prefer structured `Json`, use `LargeText` for long text with truncation, and be sanitized before entering Turn or Trace.

### Per-tool events

`ToolContext` carries an `Arc<dyn ToolEventSink>` (`events`) that tools call to emit structured observations under the running tool span. Sinks are sync and fire-and-forget — tools must not `await` emission.

Two emission styles share the same sink:

- **Phase timers** — use `start_timer(&ctx.events, "phase_name")` for the common scoped-duration case. The returned RAII guard emits a `ToolEventPayload::Phase { duration_ms }` on `Drop`, so timing a scope is one line.
- **Rich payloads** — call `events.emit(action, ToolEventPayload::HttpFetch { … })` (or `LlmCall { … }`) directly when the observation carries content. These fields are a *preview*, not an archive: the sink truncates every text field to `SPAN_EVENT_TEXT_MAX_BYTES` (4 KiB, in `baybo-trace`), so a producer cannot bloat the trace by forgetting to. Emit the smallest thing that identifies what happened, and put anything larger in the blob store.

The agent's `ToolExecutor` builds a per-call `SpanEventRecorder` (`crates/agent/src/runtime/tool_executor.rs`), threads it through `ctx.events`, and drains the buffered `(action, payload)` entries into `SpanEventKind::ToolEvent` span events after the tool returns — regardless of success, failure, or timeout. The drain runs every text field in the payload through `SecurityGateway::sanitize_stream_fragment` so any leaked secrets are minted into placeholders before they hit the trace store. Two bounds apply, in this order: `emit` first truncates each text field to `SPAN_EVENT_TEXT_MAX_BYTES`, then a 64 MiB ceiling on the aggregate text bytes of one call's payloads acts as a backstop against a tool emitting a very large *number* of events. Entries past the ceiling are dropped whole and silently (the trace is best-effort, not load-bearing) — which is why the per-field bound has to come first, or one fat field would evict later events, including the `ParseFailure` audit record. The trace view (`app/web/src/pages/TraceSessionPage.tsx`) renders each event under the tool span's Events tab.

`WebFetchTool` is the reference consumer:

- Phase timers: `http_request`, `read_body`, `extract_article` (HTML readability extraction + markdown render), `archive_raw_blob` (raw-content archival into `BlobStore`), `llm_summary` (prompt + side-LLM only), `read_error_body` (non-2xx).
- `HttpFetch` payload on `http_response` (both success and failure paths): status, byte count, content-type, and a UTF-8-bounded rendered-body preview.
- `LlmCall` payload on `llm_summary` (success path): model id, a preview of the user message handed to the side LLM, and the model's response.

Both previews are bounded by `SPAN_EVENT_TEXT_MAX_BYTES`. They used to carry the fetched page *twice* per call — the whole 96 KiB summariser prompt plus a 32 KiB body preview of the same text — which made `span_events` 93% page text. The full rendered page is archived in the blob store and reachable via `raw_content_file`; the events only need enough to recognise the call.

## Constraints

- Depends on `baybo-llm`, `baybo-model`, `baybo-security`, `baybo-storage`, `baybo-workspace`, plus `rmcp` + `oauth2` + `axum` (callback listener) for the MCP client
- Does not install third-party artifacts
- Defines the `ApprovalGate` trait but never implements the user-facing UX — the per-connection gate is built by the gateway's WS sidecar (`ChannelApprovalGate` backed by an `ApprovalQueue`), and the TUI renders the resulting prompts inline in its scrollback
- `artifact_hash` must be recorded as `tool_artifact_hash` on the `ToolCall` span's `trace::ToolCallBegin` — today `ToolManifest` carries no artifact hash, so the executor writes an empty string

## Collaboration

| Module     | Role                                                                                               |
| ---------- | -------------------------------------------------------------------------------------------------- |
| `agent`    | `ToolExecutor` validates trust/capability, executes tools, records observability                   |
| `security` | Upper layers inject secrets and network policy (no direct dependency)                              |
| `trace`    | Records tool parameters, results, artifact hash, and source                                        |
| `llm`      | Consumes tool definitions for function calling                                                     |
| `rmcp`     | External SDK for MCP client transports                                                             |
