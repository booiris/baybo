# Aura for macOS — native app

A native macOS desktop app that puts a single user in a one-on-one chat with their Aura agent. The aura runtime runs **in-process** inside the app's Rust backend — no shipped CLI binary, no child process, no external daemon — and the webview talks to it over the same loopback `GatewayServer` HTTP/WebSocket surface the web dashboard uses. The frontend is a fresh React build with a warm neo-brutalist look modelled on [raft.build](https://raft.build); it does **not** reuse `web/`'s components, pages, or design tokens — only its connection code (`client.ts`, the `ChatWs` class, the `Frame` union, the openapi-typescript codegen).

This document is the source-of-truth spec for the app. It assumes the runtime-boot, gateway HTTP/WS, chat-protocol, setup-primitive, config/packaging, and web-reuse facts established during scoping; every cited `path:line` below points at code that exists today, except where a section is explicitly marked **new work**.

## 1. Overview & goals

### What it is

Aura for macOS is a **single-user AI-assistant chat client**, not a multi-channel team-chat product. The visual and interaction reference is Raft's warm neo-brutalist team-chat UI — cream canvas, a gold/yellow rail, coral selected rows, bold black borders with hard offset shadows, monospace timestamps and code badges — but the *content model* is Aura's: one operator, many **sessions** (conversations), one agent with tools/skills/approvals behind it. Where Raft has channels and DMs, Aura has sessions; where Raft has a member roster, Aura has nothing — there is exactly one human.

The app embeds the real `aura-gateway` `GatewayServer` (`crates/gateway/src/server.rs:243`) inside the Tauri Rust process, bound to `127.0.0.1` on an ephemeral port. The webview is a thin client over that gateway's authenticated `/v1/*` REST + `/v1/channel-ws` WebSocket surface — the exact contract the web dashboard already speaks (the `http` channel). Nothing about the agent, tools, sandbox, or storage is reimplemented; the app is a new *shell* and a new *frontend* around the existing runtime.

### Goals

- **Native, self-contained desktop experience.** Launch the `.app`, complete a one-time setup wizard, chat. No terminal, no separate `aura` install, no config file editing.
- **Full agent power.** The agent keeps full filesystem and process access (no App Sandbox entitlement); security is enforced at Aura's existing per-tool `sandbox-exec` boundary and the interactive approval gate, surfaced natively in the chat.
- **Single source of truth for the runtime.** Boot/manager-graph/router wiring is extracted into a reusable `aura-runtime` library consumed by both the existing `aura` CLI and this app, so the two can never drift.
- **App-owned data.** Aura's workspace lives under the app's own data directory (`~/Library/Application Support/<bundle-id>/`), not `~/.aura`, so the app and a developer's CLI Aura never share or clobber state.

### The Raft style reference

Raft.build is a chat UI built around warmth and density: a left **icon rail**, a **sessions/channels sidebar**, a **main thread**, a **composer**, monospace metadata, and a confident hard-edged border-and-shadow treatment. We borrow its *layout grammar* and *aesthetic* (see §7) and reskin it for an assistant: the channel list becomes a session list, an inline "Tasks" affordance maps to Aura's `task_list` frames, and an "Activity" surface maps to cron fires + session-activity pulses.

### Non-goals (explicit)

- **No team-chat semantics.** No channels-as-rooms, no DMs, no members, no presence, no multi-user anything. One operator.
- **No admin / management dashboards.** No analytics, traces, cron editors, log viewers, or config viewers in the UI. (The gateway *exposes* those routes; the app simply does not surface them.) Post-setup configuration is a single minimal Settings sheet (§9).
- **Not App-Store-sandboxed.** No macOS App Sandbox entitlement (§11). Distributed via Developer ID + notarization, not the Mac App Store.
- **No reuse of `web/` UI.** No `web/` components, pages, routes, or tokens. Connection code only.
- **No shipped CLI / child process / daemon.** The runtime is embedded in-process.

## 2. Architecture

### In-process embedding

The Tauri Rust backend *is* the Aura host process. On launch (after setup), it builds the full manager graph and starts the real `GatewayServer` on loopback (the separate `ChannelServer` listener is optional and skipped by default — the admin bind co-hosts the channel-ws subrouter; see *Ephemeral port + bind*), then hands the webview a base URL + admin token. The webview is an ordinary HTTP/WS client of that local gateway.

```
┌───────────────────────────────────────────────────────────────┐
│  Aura.app  (single OS process)                                 │
│                                                                │
│  ┌─────────────────────────┐    Tauri IPC      ┌────────────┐  │
│  │  WebView (WKWebView)     │ ◄──invoke()────►  │ Tauri Rust │  │
│  │  React 19 + Vite bundle  │  get_connection   │  (main)    │  │
│  │  (tauri://localhost)     │  setup_* cmds     │            │  │
│  └───────────┬─────────────┘                    └─────┬──────┘  │
│              │ HTTP /v1/*  (Bearer admin token)        │        │
│              │ WS  /v1/channel-ws?token=<channel tok>  │ embeds │
│              ▼                                          ▼        │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │  aura-runtime (lib)  → ManagerGraph + Router             │  │
│  │  aura-gateway GatewayServer  bind 127.0.0.1:<ephemeral>  │  │
│  │  (ChannelServer listener optional — off by default)     │  │
│  │  agent · tools · sandbox-exec · libsql store · vault     │  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                │
│  workspace root: ~/Library/Application Support/<bundle-id>/    │
└───────────────────────────────────────────────────────────────┘
```

### Process & threading model

Tauri v2 owns the process and the main (UI) thread. The Aura runtime is async-tokio throughout. The app builds a multi-threaded tokio runtime (or reuses the one Tauri sets up via its async runtime) and spawns the gateway servers as long-lived tasks; the manager graph, actors, cron tick loop, and janitors live on that runtime exactly as they do under `gateway_cmd::start` today (`src/gateway_cmd.rs:222`). The UI thread never blocks on the runtime — all calls cross either Tauri IPC (commands) or the loopback socket.

Because Tauri installs its own logging/tracing and owns signal handling, the embedded gateway start path must **not** install the CLI's process-global concerns: no SIGINT/SIGTERM handler (`runtime::install_signal_handler`, `src/runtime.rs:994`), no `std::process::exit` watchdog (`runtime::force_exit_watchdog`, `src/runtime.rs:1018`), no stdout banner, and no global tracing-subscriber install (`tracing_init::init_tracing`, single-init at `src/tracing_init.rs:89-94`). These are made opt-in in the extraction (§3). The runtime still needs a `LogBuffer` for `GatewayDeps.log_buffer`. A `LogBuffer` is **not** tied to file logging: `init_tracing` constructs `LogBuffer::new(LOG_BUFFER_CAPACITY)` **unconditionally** (`src/tracing_init.rs:116`) and returns it for *every* `TracingMode` (Stdout/Stderr/File/Tui — `:145/:174/:217/:231`). The only real constraint is that `init_tracing` installs a **global subscriber**. So the embed either (a) calls `init_tracing` once in a chosen mode (e.g. `File` rooted under the app workspace `logs/`) and takes the returned buffer, or (b) skips the global install and constructs `LogBuffer::new(capacity)` directly (`LogBuffer` is public, re-exported from `aura-gateway`). **We default to (b)** so Tauri keeps ownership of tracing; §3 makes the install opt-in.

### Ephemeral port + bind

`GatewayConfig::default()` ships `enabled = false`, `port = 8888`, and an empty `cors_allowed_origins` (`crates/config/src/gateway.rs` Default), and `bootstrap_workspace_if_needed` writes a default `aura.json` — so the embed **must override the gateway section in memory after load and before boot, every launch**: set `gateway.bind_address = "127.0.0.1"`, **`gateway.port = 0`** (OS-assigned ephemeral), and `gateway.cors_allowed_origins = [<webview origins>]` (see CORS below). These overrides are applied in memory and are **not** persisted back to `aura.json` — the app owns the gateway config, not the user (persisting `8888` would be wrong; persisting `0` would be merely pointless). The admin bind then comes from `RuntimeGatewayConfig::from_config(&config.gateway)`, parsing `format!("{bind_address}:{port}")` (`crates/gateway/src/config.rs:18-32`).

**Reading back the OS-assigned port (committed mechanism — new work).** `GatewayServer::bind()` (`crates/gateway/src/server.rs:262`) returns the *configured* `SocketAddr` (port 0), and `GatewayServer::run` binds the `TcpListener` internally and consumes `self`, returning only `Result<()>` (`server.rs:273-274`) — so there is no way today to recover the real port. We add a split-bind API to `aura-gateway`:

```rust
// new on GatewayServer
pub async fn bind_listener(self) -> Result<BoundGateway>;   // binds 127.0.0.1:0 eagerly
pub struct BoundGateway { pub local_addr: SocketAddr, /* server + listener */ }
impl BoundGateway { pub async fn serve(self) -> Result<()>; } // the former run(self) body
```

The host calls `bind_listener()`, reads `local_addr` (the real ephemeral port), feeds it to `get_connection`, then spawns `serve()` as a long-lived task. This mirrors `ChannelServer`, which already binds `127.0.0.1:0`, reads `self.port()`, and writes a port file (`crates/gateway/src/channel_listener.rs:65-102`). The webview reaches `/v1/channel-ws` on the **admin** bind (which co-hosts the channel subrouter, `crates/gateway/src/server.rs:311-346`), so it never needs the channel-listener port — and the embed can **skip starting `ChannelServer` altogether** (it would otherwise bind a second unused loopback port and write `state/channel.port` for no consumer; see §13).

### Token handoff

The admin token is the vault secret `gateway.admin_token`, minted by `AdminToken::mint_if_absent` (`crates/gateway/src/auth/admin.rs:50`). The Rust side mints/reads it from the vault after boot and exposes it to the webview via a Tauri command (e.g. `get_connection` returning `{ base_url, admin_token }`). **There is no token-paste UI** — the web dashboard's `LoginScreen` and `localStorage` token flow are dropped entirely. The webview then constructs the openapi-fetch admin client with `Authorization: Bearer <token>` (`web/src/api/client.ts:26-31`) and mints per-session **channel** tokens via `POST /v1/chat/sessions` (`crates/gateway/src/api/admin/chat.rs:438`) as usual.

### CORS

A Tauri webview is served from a custom scheme, so its `fetch`/XHR calls to `http://127.0.0.1:<port>/v1/*` are cross-origin and subject to browser-enforced CORS. `build_cors` uses `AllowOrigin::list` — **exact-match, no wildcard** (`crates/gateway/src/server.rs:363-378`; field at `crates/config/src/gateway.rs:20-49`). So `gateway.cors_allowed_origins` must carry the **exact** webview origin(s), and there are **two**, one per build profile:

- **Production** (`tauri build`): the WKWebView custom scheme — `tauri://localhost` on macOS.
- **Dev** (`tauri dev`): the Vite dev-server origin Tauri loads the webview from, e.g. `http://localhost:1420`.

Both are added to the allowlist (the app can include both unconditionally, or select by build profile). Blob upload additionally needs the channel-token request header allowed — `x-aura-channel-token` (`CHANNEL_TOKEN_HEADER`) must be covered by `Access-Control-Allow-Headers` (§8); verify `build_cors`'s allowed-headers set covers it and widen if not. WebSocket upgrades are not CORS-preflighted, and the channel token rides as `?token=` because the browser `WebSocket` API can't set headers (`crates/gateway/src/auth/channel.rs:201-206`); the REST calls that mint that token *are* CORS-gated, hence the origin allowlist. Verification is deterministic: on a 403, log the server-side `Origin` header and compare against the allowlist (§13).

### Single instance & workspace lock

`gateway_cmd::start` acquires a per-workspace singleton lock before boot (`singleton::acquire(&Path) -> anyhow::Result<WorkspaceLock>`, `src/gateway_cmd.rs:231`; impl `src/singleton.rs:27`, writes a pidfile under `WorkspacePaths::new(root).singleton_lock()` = `state/aura.lock`). Two distinct mechanisms cooperate in the app:

- **Tauri single-instance plugin** — a second *app* launch focuses the existing window instead of re-booting (same-app double-launch).
- **`flock` workspace lock** — guards cross-process collision: if a CLI Aura is pointed at the *same* app-owned root, `acquire` fails with the holder pid (`src/singleton.rs:27` bails "another aura instance is running … (held by pid N)"). The host surfaces this as a **native error dialog** naming the pid, then quits — it does not retry or boot.

Because the app uses its own data directory, a CLI gateway on `~/.aura` never collides in the first place; the `flock` is the backstop for a deliberate same-root clash.

### Shutdown (Tauri-owned)

Signal handlers are left off in the embed (§3), so the host drives shutdown explicitly. The reusable start function (§3) returns a `ShutdownHandle` wrapping the runtime's shared `ShutdownSignal` (the one `build_managers` threads through actors, the cron tick loop, and janitors). On Tauri's window-close / app-quit, the host: (1) cancels the `ShutdownSignal` (stopping the cron loop, janitors, and the actor supervisor); (2) lets the gateway `serve()` task observe cancellation and drain within `gateway.shutdown_grace_secs`; (3) drops the held `WorkspaceLock`, releasing `state/aura.lock`. Session rows, transcripts, summaries, and the vault stay durable on disk throughout — **nothing is deleted on shutdown** (the project's session-data-is-core invariant; the embed has no expiry/cleanup path).

## 3. Runtime extraction — the `aura-runtime` library

### Why

Everything needed to stand up a gateway lives in the **binary crate** today (`src/boot.rs`, `src/runtime.rs`, `src/reload.rs`, `src/singleton.rs`, plus the orchestration body of `src/gateway_cmd.rs::start`). None of it is a library (`src/main.rs:1-11` declares them as private `mod`s). The Tauri app cannot depend on a binary crate, and we refuse to ship/spawn a CLI. So the reusable core is extracted into a **new crate `crates/runtime` (`aura-runtime`)**, consumed by both the existing `aura` CLI and the Tauri app — single source of truth.

### What moves

| Item | From | Notes |
|---|---|---|
| `boot::{load_config, resolve_config_path, build_llm_client*, load_encryption_key, storage_db_path, build_leak_detector, …}` | `src/boot.rs` | Pure mapping layer; the module already declares "no Arc wiring, no channels, no actors" (`src/boot.rs:5-6`). Trivially movable. |
| `ManagerGraph`, `build_managers`, `wire_router`, `RouterRunHandle`, `build_leak_detector`, `build_secret_vault`, `build_bot_registry_deps`, `install_signal_handler`, `force_exit_watchdog` | `src/runtime.rs` | The heavy manager constructor (`build_managers` at `src/runtime.rs:217`) derives the root **purely** from `config.workspace.path` (`src/runtime.rs:238-240`) — the clean seam. |
| `RuntimeConfigReloader` and pool/pricing helpers | `src/reload.rs` | The struct is already `pub` (`src/reload.rs:302`); the pool/pricing helpers (e.g. `build_pool_clients`, `src/reload.rs:37`) are `pub(crate)`. Its own doc says it "lives in the bin crate because rebuilding the LLM pool needs the application boot layer" (`src/reload.rs:3-5`) — i.e. it belongs in `aura-runtime`, not `aura-gateway`. |
| `singleton::{acquire, WorkspaceLock}` | `src/singleton.rs` | Pure, root-parameterised. |
| The orchestration body of `gateway_cmd::start` | `src/gateway_cmd.rs:222` | Extracted as a reusable `start_gateway(...)`-style function — steps 1–22 (singleton → mint tokens → leak detector → `build_managers` → `wire_router` → `GatewayDeps` → `ChannelServer::bind` → `GatewayServer::new` → `tokio::select!`). CLI-only concerns (banner `println!`, installer subcommands, `--config` env promotion) stay in the bin. |
| `TracingMode` / `init_tracing` | `src/tracing_init.rs` | Moves, but installation becomes **opt-in** so the Tauri host can skip the global-subscriber install. |

`aura-gateway`, `aura-workspace`, `aura-config`, and the domain crates already expose everything via explicit `GatewayDeps`/config with no hardcoded root, so nothing there moves.

### The explicit-workspace-root API

The reusable start function must accept an **explicit, already-absolute workspace root** and not consult process state. Today the implicit default flows through two `Default` impls:

- `WorkspaceConfig::Default` calls `default_workspace_root()` (`crates/config/src/workspace.rs:23`).
- `SecurityConfig::Default` derives `encryption_key_file` from it (`crates/config/src/security.rs:23`).

`default_workspace_root()` (`crates/workspace/src/paths.rs:241`) reads `current_dir()`/`$HOME` directly and has no parameter; in `debug_assertions` it resolves `<cwd>/.aura`, which is meaningless for a bundled app (cwd is often `/`). Because `#[serde(default)]` is on these structs, *any* partial `aura.json` silently injects the process-derived root.

**The contract for `aura-runtime`:** callers hand in an explicit root; the lib sets `config.workspace.path` (and, if unset, `security.encryption_key_file`) to paths under that root **after** loading config, before `validate()`. The path layer is already fully root-parameterised — `WorkspacePaths::new(root)` (`crates/workspace/src/paths.rs:339`), `WorkspaceManager::new(root)` (`crates/workspace/src/manager.rs:12`), `singleton::acquire(root)`, and `storage_db_path` all compose under the supplied root — so once `config.workspace.path` is correct, the entire manager graph and gateway are correctly rooted with no other hardcoded path (`src/runtime.rs:238-240`). Validation requires `workspace.path` to be **non-empty and absolute** (`crates/config/src/validate.rs:199-205`); the app always passes an absolute `app_data_dir()` path, so this holds.

For the Tauri embed specifically, `aura-runtime` exposes a config-path **argument** rather than reading the `AURA_CONFIG_PATH` env (`src/boot.rs:31`, set once via `unsafe set_var` in `src/main.rs:47-53` — the `set_var` at `:51`). Process-global env is unsafe for an embedded host (the parent process may already have it set), so the embed path takes config-path explicitly.

### The reusable start function

The orchestration body of `gateway_cmd::start` becomes one async entry point with an explicit options struct and a rich return value, so the Tauri host can read the bound address, hand out the token, and own shutdown:

```rust
pub struct RuntimeStartOpts {
    pub workspace_root: PathBuf,            // explicit, absolute (app_data_dir()); always wins
    pub config_path: Option<PathBuf>,       // explicit; no AURA_CONFIG_PATH env read
    pub gateway_overrides: GatewayOverrides, // bind_address, port (0), cors_allowed_origins
    pub install_signals: bool,              // CLI: true; embed: false
    pub init_tracing: Option<TracingMode>,  // CLI: Some(File/Stdout); embed: None (host owns it)
    pub print_banner: bool,                 // CLI: true; embed: false
}

pub struct RunningGateway {
    pub admin_addr: SocketAddr,             // the real OS-assigned loopback addr (§2 bind_listener)
    pub admin_token: String,                // vault gateway.admin_token (minted if absent)
    pub shutdown: ShutdownHandle,           // host cancels on app-quit (§2 Shutdown)
    _workspace_lock: WorkspaceLock,         // held for the runtime's lifetime
}

pub async fn start_gateway(opts: RuntimeStartOpts) -> anyhow::Result<RunningGateway>;
```

**Root precedence:** `workspace_root` is authoritative. `start_gateway` loads config (from `config_path` or `<root>/config/aura.json`), then **overwrites** `config.workspace.path` (and `security.encryption_key_file` when unset) to paths under `workspace_root` before `validate()`, so a stale `workspace.path` baked into an existing `aura.json` can never win, and the `Default`-impl `default_workspace_root()` is never consulted. The CLI calls `start_gateway` with `install_signals: true, init_tracing: Some(…), print_banner: true` and `workspace_root` derived from `--config` exactly as today; the embed passes `install_signals: false, init_tracing: None, print_banner: false` and `workspace_root = app_data_dir()`. `get_connection` then returns `{ base_url: format!("http://{admin_addr}"), admin_token }`.

### Rewiring the existing CLI (no behavior change)

The `aura` bin keeps `src/main.rs`, `src/gateway_cmd.rs` (subcommand dispatch + banner), `src/setup_cmd.rs`, `src/prompt_cmd.rs`, `src/tui_cmd.rs` — but those now call into `aura_runtime::*` instead of private `crate::` modules. `gateway_cmd::start` becomes a thin wrapper: resolve root from `config.workspace.path` (still derived from `--config`/`AURA_CONFIG_PATH`/`default_config_file` as today), then call the shared start function with `install_signals: true`, `init_tracing: true`, `print_banner: true`. The CLI's observable behavior is unchanged; only the *location* of the code moves. `prompt_cmd::run_in_process` (`src/prompt_cmd.rs:48`) and the token/vault helpers (`build_secret_vault`, `build_bot_registry_deps`) likewise repoint at the lib.

### Known blockers (from the facts)

1. **Two `Default` impls bake `default_workspace_root()`** (`crates/config/src/workspace.rs:23`, `crates/config/src/security.rs:23`). The app cannot inject its root by constructing `AuraConfig` alone; `aura-runtime` must set `workspace.path` + `security.encryption_key_file` explicitly after load. (`bootstrap_workspace_if_needed`, §5, already pins the key path for setup; boot must apply the same explicit root.)
2. **`wire_router` is single-shot** — it `.take()`s `graph.cron_trigger_rx` and panics on a second call (`src/runtime.rs:912-915`). A host that wants to restart the runtime must rebuild the whole `ManagerGraph`. The app builds the graph once per launch and treats "restart runtime" as "relaunch" for v1.
3. **`install_signal_handler` / `force_exit_watchdog` assume process ownership** (`src/runtime.rs:994-1027`) — must be opt-in; the embed leaves them off and lets Tauri own shutdown.
4. **Global tracing subscriber** (`src/tracing_init.rs:89-94`) — install must be skippable. The `LogBuffer` that `GatewayDeps` needs is produced by `init_tracing` in *any* mode (`LogBuffer::new` is unconditional, `src/tracing_init.rs:116`), **not** only `TracingMode::File`; the blocker is the global-subscriber install, not the buffer. The embed skips the install and constructs `LogBuffer::new(capacity)` directly (§2).
5. **`AURA_CONFIG_PATH` is process-global** — the embed takes config-path as an argument (above).
6. **Stdout banner + pidfile** in `gateway_cmd::start` (`src/gateway_cmd.rs:553-554`) — the reusable core returns bind/token to the caller instead of printing.

## 4. Workspace & data location

### App-owned root

Aura's workspace lives in the Mac app's **own** data directory, resolved via Tauri's `app_data_dir()` → `~/Library/Application Support/<bundle-id>/` (e.g. `~/Library/Application Support/build.aura.mac/`). It is **not** `~/.aura`, and the `default_workspace_root()` debug/release logic (`crates/workspace/src/paths.rs:241`) is never used to pick the app's root. The app computes `app_data_dir()`, treats it as the workspace root, and threads it explicitly through both setup and boot.

### What the workspace holds

`WorkspaceManager::ensure_layout` (`crates/workspace/src/manager.rs:21`) creates `config/`, `profile/`, `skills/`, `agents/`, `.key/`, `state/`, `state/sessions/`, `work/`, `logs/` under the root and runs `git init` in the identity subdirs. Key paths (all via `WorkspacePaths::new(root)`): `config/aura.json` (`paths.rs:401`), `.key/encryption.key` mode 0600 (`paths.rs:417`), `state/storage.db` libsql store (`paths.rs:423`), `state/channel.port`, `state/aura.lock` (the singleton pidfile — `SINGLETON_LOCK_FILE = "aura.lock"`, `crates/workspace/src/paths.rs:100`), identity files `SOUL.md`/`USER.md`/`IDENTITY.md` under `profile/`.

### Isolation from `~/.aura`

Because the app root is the app-owned directory, a developer running CLI `aura` against `~/.aura` and the app run completely separate stores, vaults, keys, and locks — no shared state, no clobbering. The single-instance lock (§2) still protects against two starters on the *same* root.

### Threading the explicit root

The same absolute root string is used by **both** the setup wizard (passed to `bootstrap_workspace_if_needed(root)`, `crates/setup/src/bootstrap.rs:23`) **and** the embedded boot (set into `config.workspace.path` before `build_managers`). This guarantees setup and the running gateway resolve to the identical libsql store, vault, and config file. Both also rely on `security.encryption_key_file` being absolute; bootstrap mints the key under the root and pins the path (`crates/setup/src/bootstrap.rs:38-58`), satisfying validation (`crates/config/src/validate.rs:375-386`).

## 5. First-run setup flow

### Lifecycle

```
launch ──► resolve app_data_dir() root
        ──► setup_status: configured?  (aura.json exists AND has an LLM entry + default-llm)
              │
              ├─ NO  ──► bespoke native wizard (screens below)
              │            └─ finish_setup ─► build runtime + start loopback gateway ─► chat
              └─ YES ──► build runtime + start loopback gateway ─► chat
```

"Configured" means `config/aura.json` exists, parses, and has at least one `LlmEntry` with a valid `default-llm` (an empty `llm` list is valid config but cannot chat, so the app treats it as unconfigured). The wizard is a **bespoke native multi-screen flow driven by Tauri commands** — we do **not** reuse the TTY `Prompter` wizard or its Quick/Full runner (`crates/setup/src/runner.rs`); we reuse only the pure non-interactive primitives below.

**Failure branches.** The lifecycle is not all happy-path; each failure surfaces a native error screen (retry/quit), never a silent hang:

- **singleton-lock held** (`acquire` fails) → dialog naming the holder pid (§2), then quit.
- **workspace bootstrap / vault-open / key-mint error** → error screen with "Open data folder" + quit.
- **gateway bind/boot error** (`start_gateway` returns `Err`) → error screen with the message + retry.
- **OAuth timeout / port 1455 busy** → the Credential screen shows the failure and offers retry or the device-code fallback.
- **`list_models` network/auth failure** → the Model screen falls back to a free-text model id (the wizard must not dead-end on discovery failure, matching `flow/llm.rs`'s manual-entry fallback).
- **`git` absent** → handled at Welcome (below).

### Screens

1. **Welcome** — branding, "Let's set up Aura." Bootstraps the workspace in the background (`bootstrap_workspace_if_needed(root)`), surfaces the **git-on-fresh-Mac** caveat if needed (below).
2. **Provider** — pick an LLM provider from the catalog.
3. **Credential** — for API-key providers, paste a key; for `openai-subscription`, run OAuth (PKCE with localhost callback, or device-code fallback).
4. **Model** — live model picker, defaulting to the latest Claude.
5. **Done** — write `aura.json`, build the runtime, enter chat.

### Reusable primitives per step

Every primitive is a plain async/sync function taking data and returning data — none need a TTY. The wizard's TTY flow (`crates/setup/src/flow/llm.rs`) is the reference implementation to mirror; the Tauri commands call the same functions.

| Step | Primitive(s) |
|---|---|
| Bootstrap (Welcome) | `aura_setup::bootstrap_workspace_if_needed(root) -> SetupContext { config_path, config, vault, stores }` (`crates/setup/src/bootstrap.rs:23`). Idempotent; mints the 0600 key, writes a default `aura.json` only if absent, opens the store + vault. |
| Provider catalog | `LlmProviderRegistry::with_default_providers().provider_names()` (`crates/llm/src/registry.rs:214,252`) — ids in registration order. Prefill base URL via `aura_llm::default_base_url_for_provider(p)` and the env hint via `default_api_key_env_for_provider(p)` (`crates/llm/src/lib.rs:72,82`). |
| API key | `vault.store_secret(&aura_llm::credentials::vault_api_key_name(&entry_name), key.as_bytes()).await` (`crates/security/src/secret_vault.rs:28`; key name `crates/llm/src/credentials.rs:11`). Leave `LlmEntry.api_key_env = None` so the per-entry secret can't be masked. |
| OAuth (`openai-subscription`) | `pkce_login(present_url, &http).await` **or** `device_code_login(present_prompt, &http).await`, then `VaultTokenStore::new(vault).save(&bundle).await` (`crates/llm/src/providers/openai_subscription/{oauth.rs:91,146, token_store.rs:17}`). `http` from `aura_security::http::client(proxy)`. |
| Model picker | Build `LlmProviderConfig` (api_key via `resolve_api_key`, `vault: Some(...)`), call `registry.list_live_models(&cfg).await` → render `LiveModelInfo` (`crates/llm/src/registry.rs:274,163-187`). Default selection to the latest Claude id; fall back to a free-text model field on error. |
| Persist | Build the `LlmEntry` literal (`crates/config/src/llm.rs:10-59`), `config.llm.push(entry)`, set `config.default_llm`, `config.validate()?` (`crates/config/src/validate.rs:19`), then `config.write_to_file(&config_path).await?` (`crates/config/src/lib.rs:133` — note it does **not** validate; the app must call `validate()` first). |

### Tauri command surface (new work)

A thin command layer wraps the primitives. **Cross-command state** — the `SetupContext` (`config_path`, in-memory `config`, `vault`, `stores`) returned by `bootstrap_workspace_if_needed`, plus the pending `LlmEntry` fields being assembled — lives in Tauri managed state behind a lock: `tauri::State<Mutex<SetupDraft>>`, created on the first `setup_status` and consumed by `finish_setup`. Every command returns `Result<T, SetupCmdError>`, where `SetupCmdError` serializes to `{ code, message }` (codes: `vault`, `network`, `auth`, `validation`, `git_missing`, `not_ready`, `io`). Payloads use `#[serde(rename_all = "camelCase")]` so the TS side consumes camelCase.

```rust
// state held across the wizard
struct SetupDraft { ctx: SetupContext, pending: Option<PendingEntry> }
struct PendingEntry { provider: String, entry_name: String, base_url: Option<String> } // api_key_env stays None

setup_status()  -> SetupStatus  { configured: bool, workspace_path: String, git_available: bool }
list_providers() -> Vec<ProviderInfo { id, default_base_url: Option<String>, default_api_key_env: Option<String> }>
submit_api_key(SubmitApiKey { provider, base_url: Option<String>, api_key }) -> EntryRef { entry_name }
start_oauth(StartOauth { mode: "pkce" | "device" }) -> OauthOutcome { email: Option<String>, plan: Option<String> }
list_models(ListModels { provider }) -> Vec<LiveModelInfo>
finish_setup(FinishSetup { provider, entry_name, base_url: Option<String>, model, reasoning_effort: Option<String> }) -> Done { ok: true }
get_connection() -> Connection { base_url, admin_token }
```

- `setup_status` bootstraps the workspace if needed (`bootstrap_workspace_if_needed`), stashes the resulting `SetupContext` into `SetupDraft`, and probes `git` on PATH.
- `submit_api_key` writes the key to the vault under `vault_api_key_name(entry_name)` and records `PendingEntry` (entry name defaulted uniquely per provider, mirroring `flow/llm.rs`); it does **not** write `aura.json`.
- `start_oauth` runs `pkce_login`/`device_code_login`; the `present_url` closure opens the system browser via the Tauri **opener** plugin, and for device mode the command returns `{ user_code, verification_url }` for the UI to display. The bundle is saved via `VaultTokenStore`.
- `finish_setup` assembles the `LlmEntry` from `PendingEntry` + `{ model, reasoning_effort }` (with `api_key_env = None`), pushes it, sets `default_llm`, calls `config.validate()` then `config.write_to_file(config_path)`, and triggers `start_gateway` (§3).
- `get_connection` is valid only **post-boot**; before boot it returns a `not_ready` error so the webview shows a boot-in-progress state (§6).

### OAuth callback (`localhost:1455`)

`pkce_login` (`crates/llm/src/providers/openai_subscription/oauth.rs:91`) **itself** binds the one-shot TCP listener on `127.0.0.1:1455` (`CALLBACK_PORT = 1455`), but the `redirect_uri` it registers with the provider is `http://localhost:1455/auth/callback` (`oauth.rs:96`) — hostname `localhost` (not the literal `127.0.0.1`), with the `/auth/callback` path. It serves the "you can close this tab" page, validates `state` (CSRF), exchanges the code, and returns the bundle — bounded by a 5-minute timeout. **The app does not run its own redirect server.** It only supplies the `present_url` closure that opens the authorize URL in the system browser. Caveat: only one PKCE login can be in flight (the port bind fails otherwise); if the app can't open localhost, the wizard offers `device_code_login`, which needs no listener and shows `DeviceCode { user_code, verification_url }` (`oauth.rs:146,387`).

### git on a fresh Mac (caveat)

`WorkspaceManager::ensure_layout` runs `git init` in the identity subdirs at boot (`crates/workspace/src/manager.rs:117-126`); `git` is required there. On a fresh Mac the Xcode Command Line Tools (which provide `git`) may not be installed, so the first `git init` would fail. `setup_status` probes for `git` on PATH and, if absent, the Welcome screen shows a clear message — "Aura needs the Xcode Command Line Tools (run `xcode-select --install`)" — and blocks finish until `git` resolves. `sh`, `sandbox-exec`, and `git`-when-present all ship with macOS / Xcode CLT.

## 6. Frontend stack & data layer

### Stack

React 19 + TypeScript + Vite + Tailwind v4, all **fresh** components and tokens. Build tooling mirrors `web/`'s baseline (Vite 6, `@vitejs/plugin-react`, TypeScript 5.7, `@tailwindcss/vite`, `tailwindcss` v4) but the design system is new (§7).

### Ported verbatim from `web/` (connection only)

- **`web/src/api/client.ts` → copy unchanged.** Pure factory over `openapi-fetch` + the generated `paths` type, no React/DOM/storage (`web/src/api/client.ts:13-31`). `createAdminClient({ baseUrl, token })` injects `Authorization: Bearer ${token}`. The only adaptation is that `baseUrl` now comes from the `get_connection` Tauri command instead of `window.location.origin` — a caller concern, the file is untouched.
- **`web/src/api/chatWs.ts` → copy verbatim.** The single most reusable file; its only import is `@msgpack/msgpack` (`chatWs.ts:11`). The `ChatWs` class (auto-connect, exponential backoff `1s→30s`, app-level ping/heartbeat, `since_ordinal` catch-up on reconnect, `onTokenRejected`/`onReset` callbacks) and the hand-mirrored `Frame` union (mirrors `crates/channels/src/wire.rs`) port unchanged. The only Tauri caveat: `buildWsUrl` derives `ws(s)://…/v1/channel-ws?token=` from `baseUrl`'s protocol (`chatWs.ts:609-618`); with no Vite dev proxy the caller must pass a fully-qualified `http://127.0.0.1:<port>` base — which `get_connection` supplies. We **port the framework-agnostic `Frame` union** as the canonical wire types.
- **`Frame`-union drift gate (new work).** The `Frame` union in `chatWs.ts` is **hand-mirrored** from `crates/channels/src/wire.rs` (`chatWs.ts:16` comment), **not** codegen'd — unlike the openapi `schema.d.ts` that `tsc` gates. A new Rust wire variant therefore won't fail any TS build and can silently drift. We close this: either (a) add a `ts-rs` export of the wire `Frame` consumed by the app + a CI gate mirroring `scripts/check-ts-bindings.sh`, or (b) keep the hand-mirror but add a Rust test asserting the variant set is exhaustively covered and assign explicit ownership for lockstep updates. Either way the doc records that the msgpack decode depends on the serde **`kind`-tagged** representation (each variant's `kind` is its tag), which `@msgpack/msgpack` `decode` must see unchanged.
- **openapi-typescript codegen → replicate the script.** `gen:api` runs `openapi-typescript ../../docs/openapi.json -o src/api/schema.d.ts`, wired into `build` (`web/package.json:8,11`). The OpenAPI source of truth is `docs/openapi.json`, emitted by `aura-gateway` (utoipa) and kept in sync by `crates/gateway/tests/openapi_spec_sync.rs` (regen: `UPDATE_OPENAPI=1 cargo test -p aura-gateway --test openapi_spec_sync`). Replicating this keeps the TS `paths` type in lockstep with the Rust DTOs — adding/renaming a route fails `tsc` until the frontend catches up.

### Rebuilt fresh (not ported)

- **The auth provider.** `web/src/api/auth.tsx` is React-context + `localStorage` token plumbing tied to a paste form. The app keeps only the single load-bearing `createAdminClient({ baseUrl, token })` call (`auth.tsx:70`) and rebuilds the provider around a `setup | booting | ready | error` bootstrap state machine: poll `setup_status` → if unconfigured, render the wizard (§5); once configured, `invoke('get_connection')` — which returns a `not_ready` error until `start_gateway` finishes, so the provider shows a **boot-in-progress** screen and retries until it gets `{ base_url, admin_token }`, then constructs the admin client and flips to `ready`. No `localStorage`, no `LoginScreen`, no `logout` (the admin token lives in the Rust vault, surfaced via command — never in webview storage).
- **All UI.** Every component, page, and style is new (§7, §8).

### Libraries reused

`@msgpack/msgpack` (wire framing, required by `ChatWs`), `openapi-fetch` (admin client), `react-markdown` + `remark-gfm` (assistant markdown answers — GFM tables/strikethrough), `openapi-typescript` (devDep, codegen). `recharts`/`react-icons` from `web/` are **not** carried — no charts in this app, and the warm theme brings its own iconography.

### How token & base URL arrive

After boot, the Rust side knows the bound loopback port and the vault admin token. `get_connection()` returns `{ base_url: "http://127.0.0.1:<port>", admin_token }`. The webview constructs the admin client, then mints channel tokens per session via `POST /v1/chat/sessions` and `POST /v1/chat/sessions/{id}/token` (`crates/gateway/src/api/admin/chat.rs:438,809`) exactly as `web/` does — the Tauri token source affects only the *admin* token, not the channel token.

## 7. UI & visual design

### Layout

A three-zone Raft-style grammar:

```
┌──────┬───────────────────────────┬──────────────────────────────────┐
│ icon │  sessions sidebar         │  main thread                      │
│ rail │  ───────────────────      │  ──────────────────────────────   │
│      │  [+ New chat]             │   assistant / user bubbles        │
│  ◆   │  ▸ Session A   12:04      │   streaming markdown              │
│  ⌗   │  ▸ Session B   11:51 •    │   ▸ reasoning (collapsed)         │
│  ⚙   │  ▸ Session C   Mon        │   ▸ Worked 6s (tool block)        │
│      │                           │   [approval card]                 │
│      │  ── activity pane ──      │                                   │
│      │  cron fire · 09:00        │  ───────────────────────────────  │
│      │  session C active         │  composer  [+]  /slash  [model ▾] │
└──────┴───────────────────────────┴──────────────────────────────────┘
```

- **Icon rail** — narrow gold/yellow vertical rail with a few destinations: Chat (sessions), an Activity glyph, and the Settings gear (§9). No admin destinations.
- **Sessions sidebar + activity pane** — a "New chat" button, the session list (newest-first, coral highlight on the selected row, monospace relative timestamp, unread dot driven by `session_activity`), and below it the **activity pane** (§8) listing cron fires and session-activity pulses.
- **Main thread + composer** — the conversation transcript and a composer with attachment button, `/`-slash autocomplete, and a per-session model picker.

### Fresh warm-brutalist tokens (Tailwind v4 `@theme`)

We reuse the *mechanism* — `@import "tailwindcss"` + a `@theme { … }` block whose namespaced CSS variables auto-generate utilities (`--color-*` → `bg-*`/`text-*`, `--shadow-*` → `shadow-*`, `--radius-*` → `rounded-*`) — and define entirely fresh values. We do **not** copy `web/`'s cool-blue/black tokens. Indicative token set:

```css
@import "tailwindcss";
@theme {
  /* surfaces */
  --color-canvas:        #FAF6EC;  /* warm cream */
  --color-surface:       #FFFDF7;  /* raised card */
  --color-ink:           #1A1714;  /* near-black text */
  --color-ink-soft:      #6B6258;  /* muted metadata */

  /* accents */
  --color-rail:          #F2C14E;  /* yellow/gold icon rail */
  --color-selected:      #F08A5D;  /* coral selected row */
  --color-accent:        #E07A3F;  /* primary action */

  /* status (chat) */
  --color-ok:            #3F8F5B;
  --color-warn:          #C98A1E;
  --color-err:           #C2452D;
  --color-info:          #3A6EA5;

  /* borders + hard offset shadows (the brutalist signature) */
  --color-border:        #1A1714;            /* bold black borders */
  --shadow-brutal:       4px 4px 0 0 #1A1714;
  --shadow-brutal-sm:    2px 2px 0 0 #1A1714;

  --radius-brutal:       8px;

  /* type */
  --font-sans:           "Inter", system-ui, sans-serif;
  --font-mono:           "Space Mono", ui-monospace, monospace;
}
```

Monospace (`--font-mono`) is used for timestamps, tool/code badges, and inline code. Cards, the composer, approval cards, and the selected row carry a bold black border + a hard offset shadow. We also port the *functional* (non-stylistic) rules from `web/src/index.css` — the `@layer base` resets, the `.chat-scroll` tinted scrollbar (retuned to the cream canvas), and the `.md-list` marker normalization for `react-markdown` output (`web/src/index.css:24-116`) — these are mechanism, not aesthetic.

### Raft affordance → Aura concept mapping

| Raft affordance | Aura concept | Backing |
|---|---|---|
| Channel list | Session list | `GET /v1/chat/sessions` (`chat.rs:471`); `session_updated`/`session_activity` frames |
| Channel row unread dot | Per-session unread | `session_activity` pulse (`wire.rs:537-546`) |
| "Tasks" tab | In-thread planning checklist | `task_list` frame snapshot (`wire.rs:382-388`) |
| Activity / what's-new | Cron fires + session activity | `GET /v1/chat/cron-messages` (`chat.rs:830`) + `session_activity` |
| Member roster / DMs | (none) | single-user |

## 8. Chat surface

The chat is the product. The webview opens **one** `ChatWs` connection (admin bind, channel-token query auth) and subscribes to sessions as they're opened; REST fills history and metadata. The non-negotiable core is: streaming markdown assistant text, collapsible reasoning, collapsed tool work-blocks ("Worked Xs"), and tool **approvals** (approve / deny / approve-always).

### Session lifecycle

```
New chat ─► POST /v1/chat/sessions  → { session_id, channel_token, channel_token_header }
        ─► ChatWs.subscribe(session_id)   (Subscribe frame; since_ordinal for catch-up)
open old ─► GET /v1/chat/sessions/{id}?before_ordinal=&limit=  → reconstructed transcript
        ─► seed WS replay cursor from oldest_ordinal/newest_ordinal (never transcript ordinals)
send     ─► ChatWs.sendMessage({ session_id, content, attachments?, clientMsgId })
hide     ─► DELETE /v1/chat/sessions/{id}   (sets hidden=true; row/transcript stay live → 204)
```

`POST /v1/chat/sessions` creates an `http`-channel session and mints a `web/<uuid>` channel token (`crates/gateway/src/api/admin/chat.rs:438-459, 907-923`). The WS handshake is `register → register_ack → subscribe`; on `subscribe` the gateway replays pending approvals + a `pending_approvals_snapshot`, optionally catches up missed messages (capped at 200, else `reset`), and hydrates the task list (`crates/gateway/src/channel/route.rs:285-585`). On reconnect, `ChatWs` re-subscribes every active session carrying `since_ordinal` so gap messages replay (`chatWs.ts:496-500`). On `reset` the client clears cursors and refetches via REST; on `register_ack{ok:false}` or two pre-ack closes it fires `onTokenRejected`, and the app re-mints via `POST /v1/chat/sessions/{id}/token` (`chat.rs:809`) and calls `replaceToken`.

History on reload comes from `GET /v1/chat/sessions/{id}` (`chat.rs:538`), which returns a **reconstructed transcript** of `message | work | notice` items, reverse-paginated via `before_ordinal`/`limit` (default 50, clamp 1..=200). The reconstruction (`reconstruct_transcript`, `chat.rs:1139`) already folds each tool-using turn's intermediate reasoning/prose/tool steps into a single collapsed `work` item before that turn's final answer — so the work-block-then-answer shape is restored on reload without the live frames. The client seeds its WS replay cursor from `oldest_ordinal`/`newest_ordinal`, never from item ordinals (control-event items carry synthetic negative ordinals).

### Frames the thread renders

| Frame `kind` | Rendering |
|---|---|
| `message` (`wire.rs:275`) | A user or final-assistant bubble; assistant content rendered as markdown (`react-markdown` + `remark-gfm`). |
| `answer_delta` (`wire.rs:299`) | Append streaming answer prose to the in-flight assistant bubble. |
| `reasoning` (`wire.rs:310`) | Append to a dim, **collapsible** reasoning block. |
| `tool_started` / `tool_completed` (`wire.rs:321,336`) | Open/close a tool line by `call_id`; collapse into a **"Worked Xs"** work block; color by `status` (`ok`/`error`/`denied`). |
| `status` (`wire.rs:350`) | Coarse turn-phase banner/spinner (`compacting`/`compacted`); spinner on start, clear on matching end. |
| `notice` (`wire.rs:361`) | `transient:true` folds into the open work block; otherwise a colored bar by `level`. |
| `attachment` (`wire.rs:285`) | A standalone media bubble (tool-produced); never folds into or closes a work block. |
| `task_list` (`wire.rs:382`) | Idempotent **snapshot** that replaces the session's checklist (the Tasks view). |
| `approval_requested` (`wire.rs:393`) | An **approval card**: tool name, `accesses` (read_file/write_file/http/exec_command/env), params preview, optional description. Buttons echo `resolve_approval`. |
| `approval_resolved` (`wire.rs:410`) | Any subscriber resolved it → drop the card locally (convergence). Not persisted/replayed. |
| `pending_approvals_snapshot` (`wire.rs:439`) | Authoritative pending list on each `subscribe`; reconcile cached cards (guard the post-subscribe race). |
| `session_updated` (`wire.rs:512`) | Sidebar structural change (create/hide/unhide); merge the sparse `SessionPatch`. |
| `session_activity` (`wire.rs:537`) | Per-turn liveness pulse → bump unread badge / `last_active` (feeds the activity pane). |

Lifecycle/transport frames (`register`/`register_ack`/`subscribe`/`unsubscribe`/`reset`/`ping`/`pong`) are handled inside `ChatWs` and never reach the renderer; TUI-only frames (`history_*`, `*_bot`, `slash_manifest`) are ignored by the web/Tauri client (the composer fetches the slash list over REST instead).

### Approvals (non-negotiable)

The approval card maps `approval_requested` → an inline card with **Approve / Deny / Approve always**, echoing `resolve_approval { call_id, decision }` (`approve`/`approve_always`/`deny`, `chatWs.ts:343-345`). Because approvals fan out to all subscribers, the card is dropped on `approval_resolved` and reconciled against `pending_approvals_snapshot` on (re)subscribe so a card resolved while disconnected disappears. This is the security UX surface for Aura's tool gate (§11).

### The four v1 extras (all in v1)

| Extra | Wiring |
|---|---|
| **`/`-slash autocomplete** | Composer fetches `GET /v1/chat/slash-manifest` (`chat.rs:876`) → `{ command, description }[]` (filters `new`, since there's a New-chat button); renders an autocomplete on `/`. |
| **Attachments / image upload** | Upload bytes out-of-band: `POST /v1/blobs` → `{ blob_id }`, authorized by the **session's channel token** in the `x-aura-channel-token` header (`CHANNEL_TOKEN_HEADER`; `Web` clients take the `Bypass` path, `crates/gateway/src/channel/blobs.rs:84-138`) — that header must be in the CORS allow-headers (§2). The 100 MiB cap is `DefaultBodyLimit::max(MAX_BLOB_BYTES)`; over-cap returns **413**, which the UI maps to a clear error (`blobs.rs:149`). Build a `WireAttachment { kind, blob_id, mime_type, size, filename? }` and include it in `sendMessage`; bytes never ride the WS — only the content-addressed id (`wire.rs:71-77`). Display via `GET /v1/blobs/{blob_id}` (`blobs.rs:232`). |
| **Per-session model picker** | Header picker reads the session's `last_llm` (from `GET /v1/chat/sessions/{id}`) and writes via `PUT /v1/chat/sessions/{id}/model` (`chat.rs:657`), body `{ llm?: name }` (`""`/absent clears the pin → follow `default-llm`); options come from `GET /v1/llm/models`. |
| **Tasks (planning checklist)** | A Tasks view per session driven by `task_list` snapshots (`wire.rs:382`); each `TaskView` is `{ id, subject, status, depends_on[] }` rendered as a checklist that replaces the prior list on every snapshot. |

## 9. Settings

Post-setup configuration is a **single minimal Settings sheet** opened from the gear in the icon rail — explicitly **not** an admin dashboard, and explicitly **not** any of the gateway's analytics/traces/cron/logs/config viewer routes. It reuses the setup screens as editors:

- **LLM providers** — add / edit / remove entries. Add reuses the Provider → Credential → Model wizard steps; remove drops the `LlmEntry` from `config.llm` and the vault key (`vault.delete_secret`).
- **Rotate keys / re-OAuth** — re-store an API key via `vault.store_secret(vault_api_key_name(name), …)`; re-run `pkce_login`/`device_code_login` for `openai-subscription` and re-save via `VaultTokenStore`.
- **Default model** — set `config.default_llm` (validated against the live pool).
- **Workspace path** — read-only display of the app-owned root (`~/Library/Application Support/<bundle-id>/`).

Every mutation goes through `config.validate()?` then `config.write_to_file(&config_path)` (`crates/config/src/lib.rs:133`); live re-pinning of the model uses the same `PUT /v1/chat/sessions/{id}/model` path for the current session. Mutations the **live LLM pool** depends on — adding the *first* provider, or rotating a key the running pool is using — can't hot-reload in v1 (`wire_router` is single-shot and the reloader is off in the embed); the app persists them and prompts a **relaunch** to apply (§13). There is no UI for cron, jobs, traces, analytics, logs, channels, or raw config editing.

## 10. Packaging & distribution

### Minimal bundle (v1)

| Concern | v1 decision |
|---|---|
| **`rg` (ripgrep)** | **Bundle** (universal binary). The only external binary not shipped by macOS that a default-config Aura needs — the `Grep` tool shells out to `rg` (`crates/tools/src/builtin/grep.rs:152`); absent, it returns "install it" and the agent falls back to Bash, but we bundle it for a working `Grep`. |
| `sandbox-exec`, `sh`, `git` | **Rely on macOS / Xcode CLT.** `sandbox-exec` (`/usr/bin/sandbox-exec`), `sh` (`/bin/sh`), `git` (`/usr/bin/git`) all ship with the OS / CLT. `git` is needed at boot for identity-repo `git init`; the wizard handles its absence (§5). |
| Channels (telegram/discord/weixin) | **OFF by default.** `channels.*=None`, so the channel-sidecar supervisor is never constructed and **`bun` is never spawned** (`src/gateway_cmd.rs:506-545`; `crates/gateway/src/sidecar/supervisor.rs:142-195`). No `bun`, no channel JS sidecars bundled. |
| Browser tool | **OFF by default.** `browser.enable=false`, so `collect_profiles` is empty and no embedded MCP/browser child is spawned (`crates/gateway/src/sidecar/embedded_mcp.rs:62-87`). No `node`, no Chrome-for-Testing. (`browser.docker` is force-disabled on macOS anyway, `crates/config/src/browser.rs:130-135`.) |
| External agents (claude/codex/gemini) | **OFF by default** (`external_agents.*.enabled=false`). |

With this default config the runtime boots clean and spawns no JS/external subprocess; the only boot-time external touch is `git init` of the identity repos, and lazy `sandbox-exec`/`sh` when a tool runs. The embedded sidecar zstd blobs compiled into `aura-gateway` ride inert (a static size cost only) when channels/browser are off.

### Build & sign progression

1. **Dev build first** — an unsigned `.app` via `cargo` + Tauri (`tauri dev` / `tauri build`). This is the v1 deliverable for iteration.
2. **Later milestone** — Developer ID signing + notarization + DMG + auto-update. **Not** the Mac App Store (no App Sandbox, §11).

### Repo layout & workspace membership

```
app/mac/
  package.json     # frontend pnpm package root (name: aura-mac); the pnpm-workspace member
  index.html
  src/             # React frontend source — fresh components + tokens
  dist/            # Vite output (frontendDist)
  src-tauri/       # Tauri Rust crate (aura-mac) — embeds aura-runtime + aura-gateway
    Cargo.toml
    tauri.conf.json
    capabilities/
```

- The frontend pnpm package root is **`app/mac/`** (`package.json` here; Vite source in `src/`). Add `app/mac` to `pnpm-workspace.yaml` `packages` (next to `web`, `bench/bench-web/web`).
- Add `app/mac/src-tauri` to the cargo workspace `members` (root `Cargo.toml:3`), alongside `crates/runtime` (the new `aura-runtime` lib, §3).
- Run from `app/mac/`, `gen:api` is `openapi-typescript ../../docs/openapi.json -o src/api/schema.d.ts` — from `app/mac/` that resolves to the repo-root `docs/openapi.json` (the same source the web app uses), keeping `schema.d.ts` in lockstep with the Rust DTOs. (The path is two levels because the package root is `app/mac/`, not `app/mac/src/`.)

### Bundle identifier

`app_data_dir()` derives the workspace path from the app's `CFBundleIdentifier`, so it must be **pinned and stable across versions** — changing it later orphans the workspace under `~/Library/Application Support/`. We pin `identifier = "build.aura.mac"` in `tauri.conf.json`; every `<bundle-id>` in this doc resolves to it (`~/Library/Application Support/build.aura.mac/`). A single Tauri-side resolver computes `app_data_dir()` once at startup (and surfaces a fatal error screen if it fails — it is fallible) and feeds the same absolute root to **both** setup and boot (§4).

### `src-tauri` crate conventions (per CLAUDE.md)

The new `app/mac/src-tauri` crate (name `aura-mac`) follows the workspace rules: dependencies via `{ workspace = true }` only — no hardcoded versions (add `tauri`, `tauri-plugin-single-instance`, `tauri-plugin-opener`, `aura-runtime`, `aura-gateway`, … to the root `[workspace.dependencies]` first) — and a `[lib]` block with `doctest = false`. It is macOS-only and is not added to any default-feature platform list.

### Build & run scaffolding

- **`tauri.conf.json`** (in `src-tauri/`): `identifier = "build.aura.mac"`; `build.beforeDevCommand = "pnpm dev"`, `build.beforeBuildCommand = "pnpm build"` (where `pnpm build` runs `gen:api` + `tsc` + `vite build`); `build.devUrl = "http://localhost:1420"` (matches the dev CORS origin, §2); `build.frontendDist = "../dist"`.
- **Capabilities (Tauri v2 ACL).** A `capabilities/default.json` must grant the webview permission to `invoke` the custom commands (`setup_status` … `get_connection`) and to use the **opener** plugin (OAuth browser-open). Without the ACL grant, `invoke` and the opener are denied at runtime. The **single-instance** plugin is registered in the Rust `Builder` (no webview permission needed).
- **Commands.** `pnpm install`; dev: `pnpm --filter aura-mac tauri dev` (or `cargo tauri dev` from `src-tauri/`); release: `pnpm --filter aura-mac tauri build` → unsigned `.app` under `src-tauri/target/release/bundle/macos/`.

## 11. Security model

- **No macOS App Sandbox entitlement.** The agent keeps full filesystem and process access; sandboxing the *app* would cripple the agent. This is a deliberate trade and the reason for Developer ID + notarization (not the App Store).
- **Security lives at Aura's existing boundaries.** Per-tool isolation is `sandbox-exec` (the macOS backend selected at `crates/sandbox/src/lib.rs:150-159`; default `sandbox.mode=auto`), and the **interactive approval gate** — every privileged tool call surfaces an `approval_requested` card the user must Approve/Deny/Approve-always (§8). These are unchanged from the CLI runtime; the app only renders the gate natively.
- **Loopback-only bind.** The admin listener binds `127.0.0.1:0` (ephemeral) and co-hosts the channel-ws subrouter; the separate `ChannelServer` loopback listener (`crates/gateway/src/channel_listener.rs:6-8`) is skipped by default (§2). Nothing listens off-host.
- **Token gates.** Every `/v1/*` admin route requires the Bearer admin token (constant-time compared, `?token=` stripped before logging, `crates/gateway/src/auth/admin.rs:97-145`); `/v1/channel-ws` + `/v1/blobs` require a minted channel token (`crates/gateway/src/auth/channel.rs:132-233`). The admin token lives in the Rust vault and reaches the webview only via the `get_connection` command — never persisted in webview storage. The CORS allowlist is narrowed to the webview origin (§2).
- **Leak detection.** `leak_detection_enabled=true` by default; the admin (and TUI) token are registered as redaction rules so they never appear in logs/traces (`runtime::build_leak_detector`, `src/runtime.rs:63`).

## 12. Milestones

| # | Milestone | Done criterion |
|---|---|---|
| 1 | **Runtime extraction** — `crates/runtime` (`aura-runtime`) with `boot`/`build_managers`/`wire_router`/`singleton` + the reusable start function (signals/tracing/banner opt-in) and explicit-root API. | The existing `aura` CLI builds and runs against `aura-runtime` with **no behavior change** (gateway/prompt/tui pass their tests); `cargo clippy --all --tests` clean. |
| 2 | **Tauri shell + connectivity** — `app/mac/{src,src-tauri}`; embed the runtime, bind `127.0.0.1:0`, read back the port, `get_connection` command, CORS for the webview origin, single-instance + workspace lock. | A hardcoded-config launch reaches `GET /v1/status` and opens `/v1/channel-ws` from the webview over loopback. |
| 3 | **Setup wizard** — bespoke native screens + the `setup_*` Tauri commands over the reusable primitives; OAuth PKCE/device; git-absence UX. | A fresh Mac (empty app-data dir) completes Welcome → Provider → Credential → Model → Done, writes a valid `aura.json`, and boots into chat. |
| 4 | **Chat core + approvals** — ported `ChatWs`/`client.ts`, fresh thread UI: streaming markdown, collapsible reasoning, "Worked Xs" work blocks, approval cards. | A full turn streams, reasoning/tool blocks collapse, and an approval card round-trips approve/deny/approve-always; reload reconstructs history. |
| 5 | **v1 extras** — slash autocomplete, attachments/blob upload, per-session model picker, Tasks view. | Each extra works against its endpoint/frame: `/`-manifest, `POST /v1/blobs` round-trip, `PUT …/model`, `task_list` snapshot rendering. |
| 6 | **Settings + polish** — the single Settings sheet (LLM CRUD, key rotation/re-OAuth, default model, workspace path); activity pane (cron-messages + session-activity). | Add/edit/remove a provider and change the default model from Settings, persisted + validated; the activity pane lists cron fires + activity pulses. |
| 7 | **Packaging** — bundle `rg`, confirm no bun/sidecars/Chrome ship; produce the unsigned dev `.app`. | An unsigned `.app` runs on a clean Mac with only system `git`/`sh`/`sandbox-exec` + bundled `rg`; signing/notarization tracked as a later milestone. |

## 13. Open questions & risks

- **Exact webview origin for CORS.** We allowlist both `tauri://localhost` (prod WKWebView) and `http://localhost:1420` (`tauri dev`) per §2, but the precise prod scheme string should be confirmed empirically on the target macOS WKWebView (it has shifted across Tauri versions). `build_cors` is exact-match, so a wrong string silently breaks the CORS-gated session-token mint; the deterministic check is logging the server-side `Origin` on a 403 (§2).
- **WS origin / handshake.** WebSocket upgrades aren't CORS-preflighted, but we must confirm the gateway doesn't reject the webview `Origin` on upgrade and that the `?token=` query auth works unchanged from a `tauri://` page.
- **Reading back the ephemeral port — resolved (committed).** `GatewayServer::bind()` reports only the configured addr (port 0) (`crates/gateway/src/server.rs:262-274`); we add the `bind_listener() -> BoundGateway { local_addr }` split-bind API (§2) and read `local_addr` before `serve()`. Tracked as new work in milestones 1–2, not an open question.
- **Single instance vs CLI on the same root.** The singleton lock protects same-root collisions, but the app-owned root makes a CLI/app clash unlikely; if a user deliberately points CLI `aura` at the app root, the second starter fails — acceptable, but the failure message should be clear.
- **git absence UX.** A fresh Mac may lack `git` (Xcode CLT); the wizard probes and blocks (§5). We must ensure no boot path runs `git init` *before* that gate (`WorkspaceManager::ensure_layout` runs it inside `bootstrap_workspace_if_needed`).
- **Activity-pane sparsity.** With channels/cron off by default, the activity pane is driven mostly by `session_activity` pulses; cron-messages (`GET /v1/chat/cron-messages`) will be empty until the user schedules cron. The pane must degrade gracefully (empty state, not error).
- **Attachment/blob path details.** The web upload flow (`POST /v1/blobs` Bypass path for session-scoped `Web` clients, `crates/gateway/src/channel/blobs.rs:84-138`) should work unchanged, but we must verify large-file streaming and MIME handling from a `tauri://` origin and the 100 MiB cap surfacing as a clear UI error.
- **Runtime-extraction blockers.** The single-shot `wire_router` (`src/runtime.rs:912-915`), the two `default_workspace_root()`-baking `Default` impls (`crates/config/src/{workspace,security}.rs:23`), opt-in signals/tracing, and the process-global `AURA_CONFIG_PATH` are all surfaced in §3; none is structurally hard, but each must be handled in milestone 1 or the embed will silently re-derive `~/.aura` or fight Tauri for the process.
- **LogBuffer — resolved.** `GatewayDeps.log_buffer` is **not** `File`-only; `init_tracing` builds `LogBuffer::new` unconditionally in every mode (`src/tracing_init.rs:116`). The embed skips the global-subscriber install and constructs `LogBuffer::new(capacity)` directly (§2; §3 blocker 4). The only choice (default-skip vs run `File` mode under `logs/`) is recorded in §2.
- **Settings changes that need a runtime rebuild.** `wire_router` is single-shot and the config reloader / `SIGHUP` path is off in the embed (§3), so a Settings mutation the running LLM pool depends on — adding the *first* provider mid-session, or rotating a key the live pool is using — cannot hot-reload in v1. The app writes the change to `aura.json` + vault immediately, then prompts the user to **relaunch** to pick it up (model *switching* among already-configured entries still works live via `PUT …/model`). Re-arming `RuntimeConfigReloader` inside `aura-runtime` for in-place reload is post-v1.
