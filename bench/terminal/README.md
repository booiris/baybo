# aura ↔ Terminal-Bench installed-agent adapter

Run the **real `aura` agent inside the official [Terminal-Bench](https://www.tbench.ai)
harness** to get **leaderboard-comparable** scores. The adapter drops aura
**into each task's Docker container**, drives it over tmux, and grades with the
task's own pytest — exactly how the leaderboard agents (Claude Code, Codex, …)
are run.

`AuraAgent` (in [`tb_adapter/`](tb_adapter/)) is a thin subclass of the harness's
`AbstractInstalledAgent`, modeled on the upstream Codex adapter. It installs aura
into the container and runs one `aura prompt` per task. There is no fork of
terminal-bench — it's used via `--agent-import-path`.

> ⚠️ **Branch-only.** This depends on aura's `bench-passthrough` build feature
> (an in-container agent must disable its own sandbox — the analog of Codex's
> `--sandbox danger-full-access`). That feature and this adapter live only on the
> `tb-installed-agent` branch and never ship to master.

## Why a special aura build

aura normally wraps every shell command in an OS sandbox (bwrap/docker). Inside a
TB task container there is no bwrap, and the container already *is* the isolation
boundary — so the adapter configures aura with `sandbox.mode = passthrough`: Bash
runs commands directly, with no work-dir jail, in the container's working dir
(where the task files live). A normal aura build **refuses to start** with that
mode; only a `bench-passthrough` build honors it.

## Build the binary

A **static musl** binary runs in any linux task container regardless of glibc:

```bash
rustup target add x86_64-unknown-linux-musl     # one-time (+ musl-tools/musl-gcc)
cargo build --release --target x86_64-unknown-linux-musl --features bench-passthrough
# -> target/x86_64-unknown-linux-musl/release/aura
```

If the musl build is awkward on your host (e.g. the bundled SQLite in `libsql`),
a glibc release binary built `--features bench-passthrough` also works when the
task container's glibc is compatible — musl is just the safe default.

## Run

The Python env is uv-managed (`pyproject.toml` + `uv.lock` here pin the harness
and a compatible Python — 3.12/3.13, since terminal-bench needs ≥3.12 and 3.14
isn't supported yet). One command — [`run.sh`](run.sh) builds the binary if it's
missing, loads `.env`, and runs the harness:

```bash
cd bench/terminal
cp .env.example .env && $EDITOR .env   # set the API key (+ optional model/base_url)

./run.sh                               # defaults: gemini-2.5-flash, core 0.1.1, all tasks
./run.sh -t fix-permissions            # one task — extra args pass through to `tb run`
./run.sh --n-tasks 3 -m openai/gpt-4o  # override model / task count / dataset / …
```

`run.sh` builds the `bench-passthrough` musl binary on first use
(`AURA_REBUILD=1` forces a rebuild), then runs the equivalent of — no global
install, no `PYTHONPATH`, env from the lockfile:

```bash
uv run tb run --agent-import-path tb_adapter.aura_agent:AuraAgent \
  --model "$AURA_MODEL" -d terminal-bench-core==0.1.1 …
```

Adapter configuration (all via env — see [`.env.example`](.env.example)):

- **API key** (the one required value) — `AURA_API_KEY` (provider-agnostic) or
  the provider's own env var (`DEEPSEEK_API_KEY` / `OPENAI_API_KEY` /
  `ANTHROPIC_API_KEY` / `GEMINI_API_KEY` / `OPENROUTER_API_KEY`).
- **Model** — `--model <provider>/<model>` (CLI flag, wins; also used by the
  harness for run naming) or `AURA_MODEL`. Known providers: `deepseek`,
  `openai`, `anthropic`, `gemini`/`google`, `openrouter`.
- **Base URL** — `AURA_BASE_URL` (optional): a custom / OpenAI-compatible
  endpoint, proxy, or gateway; empty = the provider's built-in URL.
- Pick a capable model — weak tool-callers (e.g. `deepseek-chat`) intermittently
  claim completion without invoking tools, which tanks scores independent of the
  plumbing.
- The harness reports `resolved` / accuracy / pass@k and records each run's
  asciinema cast — the comparable number.

**Sanity first:** run the harness's own `--agent oracle` on a couple of tasks to
confirm Docker + the harness work before pointing it at aura.

## How it works (per task)

1. The harness builds/starts the task's container.
2. `AuraAgent.perform_task` copies the `aura` binary **and** a rendered
   `aura.json` (passthrough sandbox, a self-contained state dir under
   `/installed-agent/aura-home`, the provider/model under test) into
   `/installed-agent`, then the base class sources the provider key and runs
   `aura-setup.sh` — which installs the binary, mints a vault key, and (only when
   absent) installs `git` + `ca-certificates`, both of which aura needs at
   startup. It then sends the run command over tmux:
   `AURA_CONFIG_PATH=/installed-agent/aura.json aura prompt --json -y --timeout 0 -- <instruction>`.
3. aura runs its full agent loop in-process against the container's working dir.
4. The harness runs the task's `tests/` (pytest) and scores `resolved`.

## Caveats

- **Network-restricted tasks** (the agent phase normally has bridge networking)
  fail for any API-based agent, not just aura.
- **Background bash** (timeout-to-background) is unsupported in passthrough — TB
  turns are foreground and the harness enforces the agent timeout, so this
  doesn't bite in practice.
- The provider key is exported into the container's shell for the run; it is
  never written to `aura.json` (only the env-var *name* is).
