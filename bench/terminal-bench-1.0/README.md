# baybo ↔ Terminal-Bench installed-agent adapter

Run the **real `baybo` agent inside the official [Terminal-Bench](https://www.tbench.ai)
harness** to get **leaderboard-comparable** scores. The adapter drops baybo
**into each task's Docker container**, drives it over tmux, and grades with the
task's own pytest — exactly how the leaderboard agents (Claude Code, Codex, …)
are run.

`BayboAgent` (in [`tb_adapter/`](tb_adapter/)) is a thin subclass of the harness's
`AbstractInstalledAgent`, modeled on the upstream Codex adapter. It installs baybo
into the container and runs one `baybo prompt` per task. There is no fork of
terminal-bench — it's used via `--agent-import-path`.

> ⚠️ **Branch-only.** This adapter (an in-container agent runs baybo built
> `--features bench-bash` — the analog of Codex's `--sandbox danger-full-access`)
> lives only on the `tb-installed-agent` branch and never ships to master.

## Why `--features bench-bash`

baybo normally wraps every shell command in an OS sandbox (bwrap/docker). Inside a
TB task container there is no bwrap, and the container already *is* the isolation
boundary. So the binary is built `--features bench-bash` (the off-by-default
bench profile: Bash runs raw — no OS sandbox, no work-dir jail, no uv shim, cwd
inherited from the container's WORKDIR) and the rendered `baybo.json` also sets
`permission = free`. The feature is what lifts the uv shim + work jail;
`free` alone would keep them.

## Build the binary

A **static musl** binary runs in any linux task container regardless of glibc:

```bash
rustup target add x86_64-unknown-linux-musl     # one-time (+ musl-tools/musl-gcc)
cargo build --release --target x86_64-unknown-linux-musl --features bench-bash -p baybo
# -> target/x86_64-unknown-linux-musl/release/baybo
```

If the musl build is awkward on your host (e.g. the bundled SQLite in `sqlite`),
a glibc release binary also works when the
task container's glibc is compatible — musl is just the safe default.

## Run

The Python env is uv-managed (`pyproject.toml` + `uv.lock` here pin the harness
and a compatible Python — 3.12/3.13, since terminal-bench needs ≥3.12 and 3.14
isn't supported yet). One command — [`run.sh`](run.sh) builds the binary if it's
missing, loads `.env`, and runs the harness:

```bash
cd bench/terminal-bench-1.0
cp .env.example .env && $EDITOR .env   # set the API key (+ optional model/base_url)

./run.sh                               # defaults: gemini-2.5-flash, core 0.1.1, all tasks
./run.sh -t fix-permissions            # one task — extra args pass through to `tb run`
./run.sh --n-tasks 3 -m openai/gpt-4o  # override model / task count / dataset / …
```

`run.sh` builds the musl binary on first use
(`BAYBO_REBUILD=1` forces a rebuild), then runs the equivalent of — no global
install, no `PYTHONPATH`, env from the lockfile:

```bash
uv run tb run --agent-import-path tb_adapter.baybo_agent:BayboAgent \
  --model "$BAYBO_MODEL" -d terminal-bench-core==0.1.1 …
```

Adapter configuration (all via env — see [`.env.example`](.env.example)):

- **API key** (the one required value) — `BAYBO_API_KEY` (provider-agnostic) or
  the provider's own env var (`DEEPSEEK_API_KEY` / `OPENAI_API_KEY` /
  `ANTHROPIC_API_KEY` / `GEMINI_API_KEY` / `OPENROUTER_API_KEY`).
- **Model** — `--model <provider>/<model>` (CLI flag, wins; also used by the
  harness for run naming) or `BAYBO_MODEL`. Known providers: `deepseek`,
  `openai`, `anthropic`, `gemini`/`google`, `openrouter`.
- **Base URL** — `BAYBO_BASE_URL` (optional): a custom / OpenAI-compatible
  endpoint, proxy, or gateway; empty = the provider's built-in URL.
- Pick a capable model — weak tool-callers (e.g. `deepseek-chat`) intermittently
  claim completion without invoking tools, which tanks scores independent of the
  plumbing.
- The harness reports `resolved` / accuracy / pass@k and records each run's
  asciinema cast — the comparable number. `run.sh` surfaces the run's
  `results.json` into `results/results-<ts>.json` (the full tb output — casts,
  logs — stays under `runs/<ts>/`), matching swe/memory's `results/`.
- Each task also writes baybo's **verbatim transcript + call-tree trace** to
  `trace/<run>/<task>/…/{messages,trace}.json` (mirroring `runs/`), so you can see
  what the agent did beyond the cast. Default-on; `NO_TRACE=1` disables it.

**Sanity first:** run the harness's own `--agent oracle` on a couple of tasks to
confirm Docker + the harness work before pointing it at baybo.

## How it works (per task)

1. The harness builds/starts the task's container.
2. `BayboAgent.perform_task` copies the `baybo` binary **and** a rendered
   `baybo.json` (`permission = free`, a self-contained state dir under
   `/installed-agent/baybo-home`, the provider/model under test) into
   `/installed-agent`, then the base class sources the provider key and runs
   `baybo-setup.sh` — which installs the binary, mints a vault key, and (only when
   absent) installs `git` + `ca-certificates`, both of which baybo needs at
   startup. It then sends the run command over tmux:
   `BAYBO_CONFIG_PATH=/installed-agent/baybo.json baybo prompt --json -y --timeout 0 -- <instruction>`.
3. baybo runs its full agent loop in-process against the container's working dir.
4. The harness runs the task's `tests/` (pytest) and scores `resolved`.

## Caveats

- **Network-restricted tasks** (the agent phase normally has bridge networking)
  fail for any API-based agent, not just baybo.
- **Background bash** (timeout-to-background) is unsupported in `none` mode — TB
  turns are foreground and the harness enforces the agent timeout, so this
  doesn't bite in practice.
- The provider key is exported into the container's shell for the run; it is
  never written to `baybo.json` (only the env-var *name* is).
