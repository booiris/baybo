# External command dependencies

Baybo shells out to a small set of external binaries at runtime. This catalogs
them — required vs optional, where they're invoked, and what each is for — then
covers how the benchmark harnesses (which run the real agent *inside* disposable
task containers) provide or skip each one.

## Inventory

| Command | Status | Invoked by | Purpose |
|---------|--------|-----------|---------|
| `git` | **Required at startup** | `baybo-workspace` (`manager.rs`) | `git init` of the workspace identity repos (`config/`, `personas/`, `agents/`). Baybo surfaces a startup error if it's genuinely missing. Also used best-effort by `baybo-workspace` to commit a persona when it is materialised, and by the Edit tool to auto-commit `personas/` identity-file edits (failure degrades to a `commit_warning` in the tool output), and by `baybo-deck` (`repo.rs`) to auto-commit each card-bundle mutation (install/update/purge) into a `workspace/deck/` repo for version history (failure degrades to a `deck::provenance` warn; the deck operation still succeeds). |
| `sh` | **Required** | `baybo-tools` Bash (`bash/mod.rs`) + `baybo-deck` (`host.rs`) | Every Bash-tool command runs as `sh -c "…"`; a deck card's `ctx.exec` runs `/bin/sh -c` directly on the host (unsandboxed, 10s + output caps). |
| `rg` (ripgrep) | Required **for the `Grep` and `Glob` tools** | `baybo-tools` (`builtin/rg.rs`, shared by `grep.rs` / `glob_tool.rs`) | The agent's regex content-search and file-glob tools both shell out to `rg`. Absent → each returns *"ripgrep (`rg`) not found on PATH; install it"* and the agent must fall back to Bash `grep`/`find`/`python`. Baybo itself still runs. |
| one of `bwrap` / `sandbox-exec` / `docker` | Optional but recommended for `permission = auto` / `manual` | `baybo-sandbox` | The inner OS sandbox backend: `bwrap` (Linux), `sandbox-exec` (macOS, ships with the OS), or `docker`. With none present on a non-container host, Bash emits a notice and runs without the inner OS sandbox under the configured approval policy; inside a detected outer container/sandbox, it skips the inner sandbox silently. (Deck card services do not use this — they run on the host; see the `bun` row.) |
| `systemd-run` | Optional | `baybo-sandbox` (`bwrap.rs`) | cgroup resource limits for `bwrap` when available. |
| `systemctl` / `launchctl` | Optional (only for `baybo gateway install`-family subcommands) | `baybo-gateway` (`installer/systemd.rs`, `installer/launchd.rs`) | Install/manage the gateway as a systemd unit (Linux) or launchd service (macOS). |
| `uv` | Optional | `baybo-tools` (`bash/mod.rs` prewarm) | Python tool prewarm / the uv shim. Non-fatal if absent (a startup `WARN`). |
| `bun` | Required **for channel sidecars and deck card services** | `baybo-gateway` (sidecar supervisor) + `baybo-setup` (channel registration flow) + `baybo-deck` (`service.rs`) — all honoring the one `BAYBO_BUN_BIN` override | Runs the bundled JS channel sidecars (Telegram/Discord/…) and every enabled deck card's resident service (one bun child per card, run directly on the host — like the sidecars, inheriting the login `PATH` — spawned at gateway boot and per install dry-run). Only when those channels are configured / any deck card is installed. |
| `node` | Required **for embedded MCP / tool sidecars** (e.g. the browser sidecar) | `baybo-gateway` (`sidecar/embedded_mcp.rs`) | Runs embedded MCP-server sidecar bundles (`node <bundle.mjs>`); resolved from `PATH`, overridable via `BAYBO_NODE_BIN`. Only when a tool-sidecar domain (e.g. `browser.enable`) is configured. |
| `claude` / `codex` / `gemini` | Optional | `baybo-agent` external-agent delegation | Only when an external agent is configured. |
| *(per-skill binaries)* | Per-skill | `baybo-skills` (`registry.rs`) | A skill manifest may declare any required binary; checked at load. |

Build/test-only (not runtime): `cargo`, `pnpm` (web build), `musl-gcc` (bench
musl build), `tmux` (`baybo-term-harness` tests).

`pnpm` is **required to build `baybo-gateway`**: the dashboard is compiled into
the binary, and `build.rs` hard-fails when it can't be built. Set
`BAYBO_SKIP_WEBUI=1` to embed the placeholder page and build the backend without
it. The sidecar half of that build script stays lenient (empty assets +
`cargo:warning`) unless `BAYBO_REQUIRE_SIDECARS=1`.

## Bench environment (`bench-bash` + `permission = free`)

The in-container benches (`bench/swe`, `bench/terminal-bench-1.0`,
`bench/terminal-bench-2.0`) run the real agent *inside* each task's disposable
container. Two mechanisms adapt baybo to that:

- **Build with `--features bench-bash`** — compiles the bench-only agent prompt
  framing (raw host exec, no work-dir jail, no uv shim, no approval gate). The
  normal-build prompt sections are absent then.
- **`permission = free`** in the in-container `baybo.json` — disables the OS
  sandbox (a container can't nest `bwrap`, and the container *is* the isolation
  boundary). `free` is an ungated runtime option; it is deliberately
  "dangerous" (full host-user reach) and meaningful only inside a throwaway env.

How each runtime dependency is handled there:

| Command | Bench handling |
|---------|----------------|
| `git` | **Not needed.** A `bench-bash` build skips the workspace identity-repo `git init` (`baybo-workspace`), and the install script no longer apt-installs git. (Previously the base image's missing git forced a per-task `apt-get install git` that on a slow network took ~11 min, blowing the agent budget — the motivation for the fix.) |
| `sh` | Present in every task container; nothing to do. |
| `rg` | **Bundled (`bench/swe`).** The agent driver `docker cp`s a static-musl `rg` to `/usr/local/bin/rg` (on the image PATH) alongside the `baybo` binary, so the `Grep` tool works. `run.sh` fetches the pinned ripgrep musl release into `bench-out/rg` (cached, gitignored) or honours `BAYBO_RG_BIN`. A glibc host `rg` won't load in the older-glibc images — hence static-musl, same as `baybo`. The `terminal-bench` installed-agent path doesn't bundle it yet. |
| sandbox backend | **Not needed** — `permission = free` disables OS sandboxing. |
| `uv` | **Not needed** — the `bench-bash` prompt tells the agent that `python`/`pip` are the host interpreters (no uv shim). |
| `bun`, external agents | N/A in bench. |

### Status

1. **`git` apt tax — fixed.** A `bench-bash` build skips the workspace
   identity-repo `git init`, so baybo boots without `git`, and the install script
   no longer apt-installs it (it only touches `ca-certificates`, and only when a
   task image genuinely lacks them — the t-bench base ships them, so the common
   case does **no `apt` at all**). This removes the per-task ~11-min
   `apt-get update` that was blowing agent budgets.
2. **`rg` — bundled in `bench/swe`.** The agent driver copies a static-musl
   ripgrep onto the container PATH (chosen over `apt-get install ripgrep`, which
   would re-add the per-task `apt-get update` tax). `run.sh` downloads the pinned
   release once into `bench-out/` and reuses it. If a `bench/swe` run still shows
   the *"ripgrep not found"* tool error, the container didn't get the binary —
   check `--rg-bin` / `BAYBO_RG_BIN`. The `terminal-bench` installed-agent path
   doesn't bundle `rg` yet, so the Grep tool still falls back to Bash `grep` there.
