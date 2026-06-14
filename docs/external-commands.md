# External command dependencies

Aura shells out to a small set of external binaries at runtime. This catalogs
them — required vs optional, where they're invoked, and what each is for — then
covers how the benchmark harnesses (which run the real agent *inside* disposable
task containers) provide or skip each one.

## Inventory

| Command | Status | Invoked by | Purpose |
|---------|--------|-----------|---------|
| `git` | **Required at startup** | `aura-workspace` (`manager.rs`) | `git init` of the workspace identity repos (`skills/`, `agents/`). Aura surfaces a startup error if it's genuinely missing. |
| `sh` | **Required** | `aura-tools` Bash (`bash.rs`) | Every Bash-tool command runs as `sh -c "…"`. |
| `rg` (ripgrep) | Required **for the `Grep` tool** | `aura-tools` Grep (`grep.rs`) | The agent's regex content-search tool. Absent → the tool returns *"ripgrep not found; install it"* and the agent must fall back to Bash `grep`/`python`. Aura itself still runs. |
| one of `bwrap` / `sandbox-exec` / `docker` | **Required unless `sandbox.mode = none`** | `aura-sandbox` | The OS sandbox backend for `sandbox.mode = auto` (default) / `sandboxed`: `bwrap` (Linux), `sandbox-exec` (macOS, ships with the OS), or `docker`. With none present and a sandboxing mode selected, aura errors at startup (*"no sandbox backend available…"*). |
| `systemd-run` | Optional | `aura-sandbox` (`bwrap.rs`) | cgroup resource limits for `bwrap` when available. |
| `uv` | Optional | `aura-tools` (`bash.rs` prewarm) | Python tool prewarm / the uv shim. Non-fatal if absent (a startup `WARN`). |
| `bun` | Required **for channel sidecars** | `aura-setup` | Runs the bundled JS channel sidecars (Telegram/Discord/…). Only when those channels are configured. |
| `claude` / `codex` | Optional | `aura-agent` external-agent delegation | Only when an external agent is configured. |
| *(per-skill binaries)* | Per-skill | `aura-skills` (`registry.rs`) | A skill manifest may declare any required binary; checked at load. |

Build/test-only (not runtime): `cargo`, `pnpm` (web build), `musl-gcc` (bench
musl build), `tmux` (`aura-term-harness` tests).

## Bench environment (`bench-bash` + `sandbox.mode = none`)

The in-container benches (`bench/swe`, `bench/terminal-bench-1.0`) run the real agent
*inside* each task's disposable container. Two mechanisms adapt aura to that:

- **Build with `--features bench-bash`** — compiles the bench-only agent prompt
  framing (raw host exec, no work-dir jail, no uv shim, no approval gate). The
  normal-build prompt sections are absent then.
- **`sandbox.mode = none`** in the in-container `aura.json` — disables the OS
  sandbox (a container can't nest `bwrap`, and the container *is* the isolation
  boundary). `none` is an ungated runtime option; it is deliberately
  "dangerous" (full host-user reach) and meaningful only inside a throwaway env.

How each runtime dependency is handled there:

| Command | Bench handling |
|---------|----------------|
| `git` | **Not needed.** A `bench-bash` build skips the workspace identity-repo `git init` (`aura-workspace`), and the install script no longer apt-installs git. (Previously the base image's missing git forced a per-task `apt-get install git` that on a slow network took ~11 min, blowing the agent budget — the motivation for the fix.) |
| `sh` | Present in every task container; nothing to do. |
| `rg` | **Bundled (`bench/swe`).** The agent driver `docker cp`s a static-musl `rg` to `/usr/local/bin/rg` (on the image PATH) alongside the `aura` binary, so the `Grep` tool works. `run.sh` fetches the pinned ripgrep musl release into `bench-out/rg` (cached, gitignored) or honours `AURA_RG_BIN`. A glibc host `rg` won't load in the older-glibc images — hence static-musl, same as `aura`. The `terminal-bench` installed-agent path doesn't bundle it yet. |
| sandbox backend | **Not needed** — `sandbox.mode = none` disables OS sandboxing. |
| `uv` | **Not needed** — the `bench-bash` prompt tells the agent that `python`/`pip` are the host interpreters (no uv shim). |
| `bun`, external agents | N/A in bench. |

### Status

1. **`git` apt tax — fixed.** A `bench-bash` build skips the workspace
   identity-repo `git init`, so aura boots without `git`, and the install script
   no longer apt-installs it (it only touches `ca-certificates`, and only when a
   task image genuinely lacks them — the t-bench base ships them, so the common
   case does **no `apt` at all**). This removes the per-task ~11-min
   `apt-get update` that was blowing agent budgets.
2. **`rg` — bundled in `bench/swe`.** The agent driver copies a static-musl
   ripgrep onto the container PATH (chosen over `apt-get install ripgrep`, which
   would re-add the per-task `apt-get update` tax). `run.sh` downloads the pinned
   release once into `bench-out/` and reuses it. If a `bench/swe` run still shows
   the *"ripgrep not found"* tool error, the container didn't get the binary —
   check `--rg-bin` / `AURA_RG_BIN`. The `terminal-bench` installed-agent path
   doesn't bundle `rg` yet, so the Grep tool still falls back to Bash `grep` there.
