# setup — Onboarding Wizard

## Problem

A fresh Aura install has *nothing* on disk: no workspace dir, no
master encryption key, no `aura.json`, no libsql storage, no
configured LLM. The existing argv commands (`aura llm add`,
`aura channel add`, …) all assume the encryption key + vault are
already up; the gateway boot path assumes a valid `aura.json`. Without
a wizard, an operator's first run goes:

1. Generate a 32-byte hex key by hand and write it under a path of
   their choice.
2. Hand-author `aura.json` with `security.encryption_key_file` set to
   that path, plus enough other sections to validate.
3. Start the gateway once so the vault opens, *then* run
   `aura llm add` to register a provider.
4. Restart the gateway so the new entry takes effect.

`aura setup` collapses that down to one interactive command that

- bootstraps the workspace skeleton (`config/`, `profile/`, `skills/`,
  `.key/`, `state/`, `work/`, `logs/`),
- mints the master encryption key at `<root>/.key/encryption.key`
  with mode 0600,
- writes a default `aura.json` pinned to that key,
- opens libsql storage and the secret vault,
- walks an LLM-provider step (Quick + Full both run it), an optional
  channel-bot step, and (Full only) an optional browser-tool step,
- writes the final `aura.json` once at the end (never partway through),
- and offers to spawn the gateway and open the dashboard in a
  browser, or print a hint and exit.

The command is also idempotent: running it on a workspace that
already exists reuses the key and the existing `aura.json`, and
re-prompts only the steps the operator chooses (LLM step's
`Add another / Skip`, channel step's `Add another / Skip`).

## Design

### Layering

```text
aura-cli (top)            aura-cli::commands::{llm,channel}::add — thin wrappers
    │
    └─► aura-setup ◄──── public API: Prompter, flow::*, run_*, end picker
            │
            ├─ aura-config / aura-security (vault + encryption key)
            ├─ aura-llm (provider catalog + OAuth)
            ├─ aura-channels (sidecar registration protocol)
            ├─ aura-storage (libsql open + SecretStore + ChannelBotStore)
            ├─ aura-workspace (paths, ensure_layout)
            └─ aura-gateway (SidecarRuntime, BUN_BINARY_ENV)
```

The wizard's per-step interactive primitives live in
`aura_setup::flow::*`. The CLI's `aura llm add` / `aura channel add`
are now ~10-line wrappers that build a `TtyPrompter`, call the
matching `flow::configure_*_step(allow_skip = false)`, and write
`aura.json`. So "the wizard's LLM step is the same as `aura llm add`"
is *structurally* the same code, not a soft promise.

### Step 0 — workspace bootstrap

Before showing any picker, `bootstrap_workspace_if_needed`:

1. Calls `WorkspaceManager::ensure_layout`, which creates every
   workspace subdir and runs `git init` inside `config/`,
   `profile/`, and `skills/` (per-dir repos, no top-level repo).
2. If `<root>/.key/encryption.key` is missing: mint 32 random bytes
   via `rand::rng().fill`, hex-encode, and write to that path with
   `O_CREAT | O_EXCL` and mode 0600. Exists → reused as-is.
3. Resolves the config path (`AURA_CONFIG_PATH` env override else
   `<root>/config/aura.json`). Missing → write
   `AuraConfig::default()` with `security.encryption_key_file`
   pointing at the freshly-minted key. Existing → load and reuse;
   patch in the key file pointer only if the existing config
   left `encryption_key_file` unset.
4. Validates the in-memory config.
5. Opens libsql at `<root>/state/storage.db` and builds the
   `SecretVault`.

The result is a `SetupContext { config_path, config, vault, stores }`
handed to every flow primitive. `WorkspacePaths` is a local inside
`bootstrap_workspace_if_needed` — flow primitives derive paths from
`config_path` or build their own as needed. Steps mutate `config` in
memory; the runner commits exactly once at the end.

### β2 — single `aura.json` write at the end

Each step performs its own external side effects as it runs:

- vault writes (api keys, OAuth bundles, channel tokens),
- libsql rows (channel bot metadata),
- platform-side sidecar registrations (Telegram BotFather, etc.).

The **only** deferred write is `aura.json`. The wizard accumulates
the desired config in memory, validates the whole thing once at the
very end, and atomic-writes via `AuraConfig::write_to_file`.

A Ctrl-C before the final write leaves `aura.json` untouched. Vault
and libsql side effects persist; a re-run picks them up via the
"Add another / Skip" pickers and the new `aura.json` write at the
end becomes authoritative.

### OAuth stranding semantics

For `openai-subscription` providers the OAuth flow itself stores the
token bundle at the single vault key
`llm.openai-subscription.tokens`. That bundle is only refreshed when
an `OpenAiSubscriptionCompletionModel` is instantiated by a runtime
(gateway / TUI), so a stranded bundle from a cancelled wizard sits
quietly until it expires; on next sign-in the same key is overwritten
because the bundle is single-key, single-profile.

In other words: a Ctrl-C right after the OAuth login does *not*
cause a useless background refresh loop to keep the token alive — no
runtime is built, no loop exists. The bundle is harmlessly stranded
and gets cleanly replaced on retry.

### Browser step (Full only)

Three tiers, chosen so the 90%-case operator answers one yes/no and
power users still reach the docker / sandbox knobs without hand-
editing JSON:

1. `Enable agent web browsing? [y/N]` — flips `browser.enable`.
   On `N` the rest of the step is skipped; `BrowserConfig` stays at
   `Default::default()`.
2. **Linux only**: `Run Chrome inside Docker? [y/N]`. Auto-skipped
   when the `docker` binary isn't on `PATH` or `docker info` fails.
   On macOS the prompt is unconditionally skipped — Docker Desktop
   on macOS runs Linux containers in a hidden VM, defeating the
   anti-fingerprint point of the switch (matches the upstream
   `BrowserDockerConfig` behaviour).
3. **If docker mode is OFF**: `Enable Chrome renderer sandbox?
   [y/N]`. Docker mode upstream ignores `browser.sandbox`, so the
   prompt is hidden when docker is on.

Other knobs (viewport, profile_dir, chrome_path, cdp_url) stay at
defaults — operators wanting custom values edit `aura.json`
directly.

### Exit hint

Setup never starts the gateway itself. Once the config is committed it
prints a hint with the `aura gateway start` / `aura tui` commands and
returns cleanly — the operator starts the daemon themselves. `aura
gateway start` then prints the dashboard URL followed by the admin
token (each on its own line; the token is deliberately not embedded in
a `?token=` URL, which would leak it into the access log).

### Re-run idempotency (the `(B)` policy)

- Step 0 (`bootstrap_workspace_if_needed`) is a strict no-op when
  the workspace already exists.
- The LLM step is mandatory on a fresh install (`config.llm` is
  empty) and skippable on re-runs (operator is offered
  `Add another / Skip`).
- The channel step always offers `Add (another) / Skip` when called
  with `allow_skip = true` (Quick + Full).

There is no `setup_state.json` or partial-progress file: a Ctrl-C
mid-wizard leaves `aura.json` unchanged, and the next run just
prompts again.

### TTY-only / `--json` refusal

`aura setup --json` errors out with a clear message: an interactive
wizard has no scripted-mode contract, and operators wanting argv
automation should chain `aura llm add` / `aura channel add` directly
(both reach the same `flow::*` primitives). The slash dispatcher
returns `AgentSendForbiddenInSlash("setup")` because interactive
prompts don't fit the slash-command shape.

## Public API

```rust
pub trait Prompter: Send {
    fn select(&mut self, label: &str, options: &[&str]) -> Result<usize>;
    fn multi_select(&mut self, label: &str, options: &[&str], initial: &[bool]) -> Result<Vec<usize>>;
    fn text(&mut self, label: &str, default: &str) -> Result<String>;
    fn confirm(&mut self, label: &str, default: bool) -> Result<bool>;
    fn password(&mut self, label: &str) -> Result<String>;
}

pub struct TtyPrompter { /* … */ }   // real-terminal Prompter

pub mod flow {
    pub fn configure_llm_step(...) -> Result<LlmStepOutcome>;
    pub fn configure_channel_step(...) -> Result<ChannelStepOutcome>;
    pub fn configure_browser_step(...) -> Result<BrowserStepOutcome>;
    pub fn run_registration(...) -> Result<RegistrationResult>;  // sidecar driver
}

pub struct SetupContext { /* config_path, config, vault, stores */ }
pub async fn bootstrap_workspace_if_needed(workspace_root) -> Result<SetupContext>;

pub async fn run(prompter, ctx) -> Result<SetupOutcome>;          // mode picker → quick/full
pub async fn run_quick(prompter, ctx) -> Result<SetupOutcome>;
pub async fn run_full(prompter, ctx) -> Result<SetupOutcome>;
pub fn pick_end_action(prompter) -> Result<EndAction>;
pub async fn launch_gateway_and_open_browser() -> Result<()>;
pub fn print_exit_hint(outcome, config_path);

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    pub struct MockPrompter { /* scripted answer queues per method */ }
}
```

## Constraints

- **TTY required**: every flow primitive bails with
  `SetupError::NotATerminal` when stdin / stderr aren't both ttys.
- **Unix-only**: the master-key file is opened with
  `OpenOptionsExt::mode(0o600)` and the masked-secret reader uses
  termios `ECHO`/`ICANON` toggles. Matches the project-wide Unix
  posture (`docs/modules/cli.md`).
- **No `aura.json` write before commit**: any flow primitive that
  needs to write the file is by definition wrong — they must mutate
  `config` in place. The runner is the one allowed writer.
- **OAuth bundle is single-key**: `aura llm add` and the wizard's
  LLM step both write `llm.openai-subscription.tokens` as one
  bundle; running setup twice with `openai-subscription` rotates
  the refresh token (each login overwrites).
- **`.key/` must never be committed**: `WorkspaceManager::ensure_layout`
  deliberately does *not* `git init` it; documented in
  `docs/modules/workspace.md`.

## Collaboration

| Crate                | What setup uses                                                         |
| -------------------- | ----------------------------------------------------------------------- |
| `aura-config`        | `AuraConfig` (load/validate/write), `LlmEntry`, `BrowserConfig`         |
| `aura-security`      | `EncryptionKey::new`, `SecretVault::new`/`store_secret`                 |
| `aura-llm`           | `LlmProviderRegistry`, `BUILTIN_PROVIDERS`, `default_base_url_for_*`, OAuth (`pkce_login` / `device_code_login`, `VaultTokenStore`) |
| `aura-channels`      | `register_wire::*`, `registration::Prompter` + `RegistrationResult`     |
| `aura-storage`       | `Store::open`, `ChannelBotStore`, `retry_on_busy`                       |
| `aura-workspace`     | `WorkspacePaths`, `WorkspaceManager::ensure_layout`, `default_workspace_root` |
| `aura-gateway`       | `SidecarRuntime`, `BUN_BINARY_ENV`, `SIDECAR_ENV_ALLOWLIST`             |
| `aura-cli`           | Wrappers: `commands::llm::add`, `commands::channel::add_bot`            |
| `src/setup_cmd.rs`   | Binary entry point that owns the wizard process; ahead of `boot::load_config` in `main.rs` |
