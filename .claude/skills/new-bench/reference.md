# new-bench reference — boilerplate & the aura CLI contract

Copy-paste scaffolding for `SKILL.md`. Everything here is distilled from the
three live benches; where a chunk is large and already unit-tested, this file
points at the real file to clone verbatim rather than duplicating it (so it can't
drift).

---

## 1. The aura black-box CLI contract

The bench only ever execs the `aura` binary. Config path is selected with the
`AURA_CONFIG_PATH` env var. Three commands matter:

**Run one agent turn (one-shot, in-container or in-process):**
```
AURA_CONFIG_PATH=<cfg> aura prompt --json -y --session <id> --timeout <secs> -- <instruction>
```
- `--json` → the last stdout line that parses as a JSON object is the result:
  `{"session_id":…,"response":"…"}` on success, `{"session_id":…,"error":"…"}` if
  the runtime rejected the turn. Parse from the **last** `{`-line backwards;
  tolerate leading log lines.
- `-y` auto-approves writes outside the workspace `work/` dir (needed so the agent
  can edit the target tree). `--timeout 0` = no aura-side limit (let an external
  harness enforce its own); a positive value bounds the turn — still wrap the
  whole `docker exec` in a hard `tokio::time::timeout` above it as a safety net.
- `--` so an instruction starting with `-` isn't parsed as a flag. Pass the
  (possibly multi-line) instruction as a single trailing argv element.

**Gateway mode (many turns share one aura process):**
```
AURA_CONFIG_PATH=<cfg> aura gateway start            # spawn detached; killed on Drop
# poll TcpStream::connect(gateway addr) until it accepts (catch early exit via try_wait)
AURA_CONFIG_PATH=<cfg> USER=<scope> USERNAME=<scope> aura prompt --json --session <fresh> -- <q>
```
- `USER`/`USERNAME` → `cli_user()` → message sender → `session.user.id` → the
  recall/isolation scope. Set it per item to the scope you populated.
- All `aura prompt`s against the same config/workspace route over the running
  gateway (it holds the workspace lock) instead of going in-process.

**Read the turn's spend from the ledger (after each turn):**
```
AURA_CONFIG_PATH=<cfg> RUST_LOG=off aura cost show --session <id> --json
```
- `RUST_LOG=off` keeps stdout pure JSON. Parse from the first `{`. Shape:
  `{"scope":…,"summary":{"cost_micro_usd":<i64>,"calls":<u64>,"input_tokens":<u64>,
  "output_tokens":<u64>, …}}`. Use the integer `cost_micro_usd`, not the string
  `cost_usd`. The row is written on a detached task that races `prompt`'s return —
  **poll** (≈8× / 250ms) and treat `calls > 0` as "landed"; degrade to zeros, not
  an error, if it never does.

**Export the session's verbatim transcript + call tree (after each turn):**
```
AURA_CONFIG_PATH=<cfg> RUST_LOG=off aura session history <id> --include-superseded --json
AURA_CONFIG_PATH=<cfg> RUST_LOG=off aura session export  <id> --json
```
- `session history --include-superseded --json` → `{"session":…,"messages":[…]}`,
  every message **incl. ones compaction later dropped** (truly verbatim). Without
  `--json` you get only a summary; without `--include-superseded`, only what the
  next turn would see.
- `session export --json` → `{"session":…,"jobs":[{"steps":[{"spans":…}]}]}`, the
  full LLM/tool call tree (it also takes `--out <path>`, but capture stdout for
  uniformity with the in-container case). It keys on the session id, so pass an
  explicit `--session <id>` to the `prompt` that produced it.
- Both read the persisted session DB (concurrent SQLite reads are fine alongside a
  running gateway). Best-effort: a non-zero exit / missing session → skip that file.

**Trace export — per-shape wiring** (default-on; `NO_TRACE=1` opts out). Write both
files to `bench/<name>/trace/<run_id>/<arm>/<item>.{messages,trace}.json`:
- **Gateway (memory):** after each turn's cost read, a `GatewayHandle` method runs
  the two `session` commands against the workspace config and writes the files —
  clone `bench/memory/src/agent.rs::{export_trace, write_session_dump}`.
- **In-container (swe):** run them **inside the container before reap** (the session
  DB dies with it) — clone `bench/swe/src/agent.rs::{export_trace, write_session_dump}`;
  the `run` bin threads `--trace-dir` (+ `--no-trace`) into `RunOpts.trace_dir`.
- **External-harness adapter (terminal), two flavors.** *Terminal-Bench 1.0* runs
  on the legacy `tb` CLI: `bench/terminal/tb_adapter` (`AbstractInstalledAgent`) —
  in `perform_task`, after the prompt, use the container handle directly
  (`session.container.exec_run([...])`, no copy-out) and mirror the harness's
  per-task `runs/` path into `trace/`; see `…::_export_trace`. *Terminal-Bench 2.0*
  runs on the **Harbor** framework (`harbor run -d terminal-bench/terminal-bench-2`):
  `bench/terminal-harbor/harbor_adapter` is a `BaseInstalledAgent` — `install()`
  `environment.upload_file`s the musl binary + a rendered `aura.json` + mints the
  key (→ chown to the agent user), `run()` runs `aura prompt` and exports the trace
  into Harbor's per-trial `/logs/agent` dir (which Harbor mounts to
  `runs/<ts>/<trial>/agent/`, sibling to `verifier/`). Same `aura.json`
  (`sandbox.mode=none`) + provider/key handling as 1.0; only the harness API differs.

**Clone these parsers verbatim** (stable, unit-tested) instead of rewriting:
`bench/swe/src/agent.rs` → `prompt_output_error`, `parse_cost_summary`, the
`docker*` process helpers; `bench/memory/src/agent.rs` → `parse_response`,
`parse_cost_summary`, `GatewayHandle`.

---

## 2. Self-contained `aura.json` (mint it; don't touch ~/.aura)

Create a fresh workspace dir, mint a 32-byte hex key from `/dev/urandom` into it
(reuse the key file if it already exists — re-minting orphans an existing vault),
and write one of these. Key is referenced **by file**, API key **by env-var name**.

**Passthrough (aura runs inside a disposable container):**
```json
{
  "llm": [{ "name": "bench", "provider": "<p>", "model": "<m>", "api_key_env": "AURA_API_KEY" }],
  "default-llm": "bench",
  "channels": { "cli": { "enabled": true } },
  "security": { "encryption_key_file": "<dir>/encryption.key", "leak_detection_enabled": true },
  "workspace": { "path": "/aura-home" },
  "sandbox": { "mode": "none" },
  "cost": { "rate_limit": { "max_requests": 1000000 } }
}
```
- `sandbox.mode = none` drops aura's OS sandbox (Bash runs via `sh -c` directly)
  but keeps the uv shim + the `work/` jail. For an **in-container** bench (bwrap
  can't nest) you also build `--features bench-bash` — the off-by-default bench
  profile that additionally lifts uv + the jail and inherits the container's cwd.
  Both together = raw exec. Omit the sandbox key (default `auto`) for a host bench
  with real isolation.
- `workspace.path` MUST be **outside** the tree you grade (e.g. `/aura-home` while
  the repo is `/testbed`), or aura's vault/sessions/logs pollute the captured diff.
- Add `"base_url"` to the llm entry only when set (omit → provider default).
- An in-process-only run (no gateway) that still fails config validation for a
  missing `gateway` block: add a dummy `"gateway": {"bind_address":"127.0.0.1","port":<any>}`
  (see `bench/terminal`); it never binds.

**Gateway (many prompts over one process):** same, but replace `sandbox` with
```json
  "gateway": { "bind_address": "127.0.0.1", "port": <port-or-0-for-auto> },
  "<section-under-test>": <arm config>   // e.g. "memory": {...}
```
Parse the gateway connect addr back out (`0.0.0.0` → poll `127.0.0.1`). See
`bench/memory/src/agent.rs::{generate_config, prepare_arm_config}`.

---

## 3. `run-bench.sh` skeleton

One-command driver. Every UPPERCASE var is env-overridable; `.env` is auto-loaded
and exported. Adapt the arms, the build line, and the table columns.

```bash
#!/usr/bin/env bash
# Run the aura <NAME> benchmark for one or more arms and print a comparison table.
#   noop = floor, oracle = ceiling (neither needs aura or a key — run first),
#   <real> = the measurement (needs the bench-feature aura + a model key).
# See bench/<name>/README.md.
set -euo pipefail
# shellcheck disable=SC2086  # *_ARG vars are intentionally word-split.

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git -C "$BENCH_DIR" rev-parse --show-toplevel)"
if [ -f "$BENCH_DIR/.env" ]; then set -a; . "$BENCH_DIR/.env"; set +a; fi

# ---- config (override via env) --------------------------------------------
: "${ARMS:=noop oracle}"                 # space-separated: noop oracle <real>
: "${CONCURRENCY:=4}"
: "${AURA_MODEL:=deepseek/deepseek-v4-flash}"  # real arm LLM as <provider>/<model>
: "${AURA_BASE_URL:=}"                          # empty => provider default endpoint
: "${AURA_BIN:=}"                        # empty => build the bench-feature aura here
: "${OUTDIR:=$BENCH_DIR/bench-out}"      # scratch (gitignored)
: "${RESULTS_DIR:=$BENCH_DIR/results}"   # per-run result JSONs (gitignored)
: "${RUN_ID:=<name>-$(date +%Y%m%d-%H%M%S)}"
: "${DRY_RUN:=0}"
: "${RUST_LOG:=aura_bench_<name>=info}"; export RUST_LOG
PKG="aura-bench-<name>"
# ---------------------------------------------------------------------------
mkdir -p "$OUTDIR" "$RESULTS_DIR"

# ---- preflight: validate arms; require a key only for arms that spend -------
for arm in $ARMS; do case "$arm" in
  noop|oracle|<real>) ;;
  *) echo "unknown arm '$arm'" >&2; exit 1 ;;
esac; done
want_real=0; for a in $ARMS; do [ "$a" != noop ] && [ "$a" != oracle ] && want_real=1; done
if [ "$DRY_RUN" != 1 ] && [ "$want_real" = 1 ]; then
  : "${AURA_API_KEY:?the real arm needs the model key in \$AURA_API_KEY (set it in .env)}"
fi

# ---- dry run: print each arm's plan, no spend ------------------------------
if [ "$DRY_RUN" = 1 ]; then
  for arm in $ARMS; do
    echo ">> [dry-run] $arm"
    cargo run -q -p "$PKG" --bin run -- --arm "$arm" --dry-run   # ...item-selection flags
  done
  exit 0
fi

# ---- build the bench bins (+ the feature-gated aura for the real arm) -------
echo ">> building $PKG bins"; cargo build -p "$PKG" --bins
if [ "$want_real" = 1 ] && [ -z "$AURA_BIN" ]; then
  # In-container bench → static musl (`--target x86_64-unknown-linux-musl`, needs
  # musl-gcc for libsql's bundled SQLite). In-process bench → a normal release build.
  echo ">> building aura --features <bench-feature>"
  cargo build --release -p aura --features <bench-feature>
  AURA_BIN="$REPO_ROOT/target/release/aura"
fi

# ---- run each arm ----------------------------------------------------------
run_arm() {
  local arm="$1" results="$RESULTS_DIR/results-$arm-$RUN_ID.json"
  echo ">> [$arm] -> $results"
  local common=( --arm "$arm" --run-id "$RUN_ID" --out "$results" --concurrency "$CONCURRENCY" )
  if [ "$arm" != noop ] && [ "$arm" != oracle ]; then
    local real=( --aura-bin "$AURA_BIN" --model "$AURA_MODEL" )
    [ -n "$AURA_BASE_URL" ] && real+=( --base-url "$AURA_BASE_URL" )
    cargo run -q -p "$PKG" --bin run -- "${common[@]}" "${real[@]}"
  else
    cargo run -q -p "$PKG" --bin run -- "${common[@]}"
  fi
}
for arm in $ARMS; do run_arm "$arm"; done

# ---- comparison table (floor -> real -> ceiling) ---------------------------
echo; echo "=== summary (run_id=$RUN_ID) ==="
if command -v jq >/dev/null 2>&1; then
  printf '%-10s %5s %8s %9s %11s\n' arm n score lat_ms cost$
  for arm in $ARMS; do
    f="$RESULTS_DIR/results-$arm-$RUN_ID.json"; [ -f "$f" ] || continue
    jq -r '[.arm,.total,(.score*100),.mean_latency_ms,(.total_cost_micro_usd/1000000)]|@tsv' "$f" \
      | awk -F'\t' '{printf "%-10s %5d %7.1f%% %9.0f %11.6f\n",$1,$2,$3,$4,$5}'
  done
else echo "(install jq for the table; raw JSON in $RESULTS_DIR/results-*-$RUN_ID.json)"; fi
```

A populate-then-query bench (clone `bench/memory/run-bench.sh`) inserts an
`ingest` step before the `run` step inside `run_arm`, coupling them via a
`--manifest` file keyed on `RUN_ID` (so one ingest backs many QA reruns).

---

## 4. Small tracked files

**`Cargo.toml` (self-hosted Rust bench):**
```toml
# <NAME> benchmark. NOT shipped and NOT on the CI/`cargo test` path: the `run`
# bin drives a real `aura` against a model API (spends money) [/ starts Docker /
# hits external servers]. Run it by hand. The library half (IR + pure helpers +
# report) is unit-tested and has no Aura/Docker/Python dependency.
[package]
name = "aura-bench-<name>"
version.workspace = true
edition.workspace = true
publish = false

[lib]
doctest = false

[[bin]]
name = "run"
path = "src/bin/run.rs"

[dependencies]
anyhow = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tokio = { workspace = true }
clap = { workspace = true }
chrono = { workspace = true }
uuid = { workspace = true }
futures = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
# Only if the bench must populate aura state directly (like memory's ingest):
# aura-<backend> = { workspace = true }
```
Also: add `"bench/<name>"` to the root `Cargo.toml` `[workspace] members`, and (if
other crates reference it) `aura-bench-<name> = { path = "bench/<name>" }` to
`[workspace.dependencies]`.

**Root `[features]` (the bench build feature, fail-closed):**
```toml
# Build a benchmark-only `aura` that <does the dangerous thing>. Without this
# feature the dangerous config is a hard startup error (never a silent
# downgrade). NOT for production.
bench-<cap> = ["aura-<crate>/bench-<cap>"]
```
Then `bench-<cap> = []` in each owning crate's `[features]`, and gate the code
with `#[cfg(feature = "bench-<cap>")]` so it isn't compiled otherwise.

**`.gitignore`:**
```gitignore
/bench-out/     # prepared inputs: exported dataset / manifests (regenerated)
/results/       # the final result JSON report ONLY (regenerated; may embed full patches)
/runs/          # run env + working artifacts: workspaces / grader intermediates / harness logs
/trace/         # per-run transcripts + call-tree traces (regenerated; full convos)
/.env           # local secrets
__pycache__/    # if there's Python tooling
/.venv/         # uv-managed venv (uv.lock / pyproject.toml / .python-version ARE tracked)
```

**`.env.example` — canonical model-under-test vars.** Every bench's real arm needs
the same three things. **Default to these flat names** so benches stay consistent;
only the real arm reads them (noop/oracle need none):

| role | canonical var | notes |
| --- | --- | --- |
| the key | `AURA_API_KEY` | the one required value |
| the model | `AURA_MODEL` | `<provider>/<model>`, e.g. `deepseek/deepseek-v4-flash` |
| custom endpoint | `AURA_BASE_URL` | empty => provider default |

**The `AURA_` prefix namespaces the bench's vars** so a bare `API_KEY` / `MODEL`
already in the environment can't clash with them. **No `*_API_KEY_ENV` knob:** the
bench generates the config, so it fixes `api_key_env: "AURA_API_KEY"` in the
`aura.json` it writes and injects the value under `AURA_API_KEY` — the env-var name
is an internal constant, not a user choice. The user only supplies the *value*; to
reuse a key already in another var, set `AURA_API_KEY=$DEEPSEEK_API_KEY` in `.env`
(it's shell-sourced, so it expands). This holds even for an external-harness adapter:
`bench/terminal` drives Terminal-Bench yet still uses the bare `AURA_API_KEY` /
`AURA_MODEL` / `AURA_BASE_URL` (+ `AURA_BIN` for the binary) — no sub-namespace,
since nothing else owns `AURA_*`.

Extend the set only when a bench genuinely has **more than one LLM role** — keep the
`AURA_` prefix and give each role its own descriptive vars, so one key can serve both
(the one case where naming the key's env-var earns its place). `bench/memory`
benchmarks memory backends with a fixed *answer* model **and** an LLM judge: the
answerer gets `AURA_ANSWER_PROVIDER` / `AURA_ANSWER_MODEL` / `AURA_ANSWER_API_KEY_ENV`
/ `AURA_ANSWER_BASE_URL` (the env knob defaulting to the judge's `DEEPSEEK_API_KEY` so
one key covers both), while the judge stays on `DEEPSEEK_API_KEY` and the backends on
`OPENVIKING_*` / `MEM0_*` — provider/infra config has no canonical form.

Bench-specific backend/server config (endpoints, dims, auth tokens — e.g. memory's
`OPENVIKING_*` / `MEM0_*`) is added on top; it has no canonical form.

```bash
# Environment for the <NAME> bench real arm. Auto-loaded + exported by run-bench.sh.
# Only the real arm needs a key; noop/oracle run with none.
AURA_API_KEY=
# Optional (empty => the default in the comment):
AURA_MODEL=     # deepseek/deepseek-v4-flash  (<provider>/<model>)
AURA_BASE_URL=  # provider's built-in endpoint  (set for a proxy / gateway)
```

**`pyproject.toml` + `.python-version`** (only if the bench needs Python — a
dataset export or an official grader). Deps-only env:
```toml
[project]
name = "aura-bench-<name>-pytools"
version = "0.0.0"
description = "Python tooling for the aura <NAME> bench."
requires-python = ">=3.11,<3.13"   # pin to what the external stack supports
dependencies = [ "<the-official-harness-or-dataset-lib>" ]

[tool.uv]
package = false
```
`.python-version` → e.g. `3.12`. Run via `uv run --project bench/<name> python …`
or `bench/<name>/.venv/bin/python`; `uv sync` builds the reused `.venv` once.

---

## 5. README outline

Mirror the existing READMEs (`bench/swe/README.md` is the fullest):

1. **One-paragraph "measures whether the REAL Aura agent …"** + the faithfulness
   claim (runs inside the official image / drives the real loop / graded by the
   official harness).
2. **Arms table** — arm · what it grades · needs aura? · needs a key? — and the
   "run noop/oracle first, offline & unpriced" note.
3. **"You must build aura with `--features <bench-feature>`"** — what the feature
   does and why a normal build refuses the config (the fail-closed lock); the
   musl note if in-container.
4. **Prerequisites** (Docker / uv / external servers).
5. **How it works** — numbered, the two lifecycles (agent run vs grading) if they
   differ.
6. **Usage** — `run-bench.sh` quick start (offline floor/ceiling first, then the
   real arm) + driving the bins directly.
7. **Caveats** — the real gotchas (musl-mandatory, cost/time scale, oracle-must-be-100%,
   any contamination guard, cost-read race).
