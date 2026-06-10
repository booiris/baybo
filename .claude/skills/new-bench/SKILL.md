---
name: new-bench
description: Scaffold a new benchmark under bench/ for measuring the real Aura agent, following the shared conventions of bench/swe, bench/memory, and bench/terminal. Use when the user wants to add a benchmark, eval, or leaderboard harness for aura.
argument-hint: "[what the bench measures, e.g. 'GAIA web-agent tasks']"
---

# Add a new Aura benchmark

You are scaffolding a new benchmark under `bench/<name>/`. Aura already has three,
and they share a deliberate skeleton — **reuse it; do not reinvent it.** This skill
captures the cross-cutting decisions and invariants so a new bench is faithful,
safe-to-merge, and consistent with the others on day one.

**The three living templates — read the closest one before writing anything:**

| bench | shape | how aura is driven | grader | build feature |
| --- | --- | --- | --- | --- |
| `bench/swe` | self-hosted Rust harness | **`docker run` per item**, `aura prompt` inside each official eval image | official `swebench` Python harness (shelled out) | `bench-passthrough` (musl) |
| `bench/memory` | self-hosted Rust harness | **`aura gateway` once per arm**, concurrent `aura prompt` over it | LLM-as-judge (`deepseek`) + deterministic F1 | `bench-readonly-memory` |
| `bench/terminal` | **adapter into an external official harness** (no Rust crate, no fork) | `aura prompt` inside the harness's task container, over tmux | the external harness's own pytest | `bench-passthrough` (musl) |

`reference.md` (next to this file) holds the copy-paste boilerplate: the
`run-bench.sh` skeleton, the self-contained `aura.json` shapes, the exact `aura`
CLI contract for black-box driving, and the small tracked files
(`.gitignore` / `.env.example` / `pyproject.toml` / `Cargo.toml` header).

---

## Step 0 — Resolve the shape (infer it; decide, don't ask)

These five axes decide which template you clone. **Infer them from the dataset /
task the user named and the defaults below — pick, state your choice in one line,
and proceed.** Don't turn this into a questionnaire. Only reach for AskUserQuestion
when one is *genuinely* ambiguous after you've looked — e.g. no official harness
exists **and** the grading approach is unclear, the dataset supports two equally
valid framings, or there's a cost/scope tradeoff only the user can weigh.

1. **What the real arm measures** — read it straight off the dataset/task (a
   patch? an NL answer? a terminal task?). Fixes the per-item IR and the metric.
2. **Harness shape** — **default: if a credible official harness exists, use the
   adapter shape** (clone `terminal`: thin adapter, no crate, no fork —
   leaderboard-comparable, far less to maintain). Own the loop (clone `swe` /
   `memory`: a Rust crate + `run` bin) only when there's no official harness.
3. **Build feature** — runs **inside a disposable container**? → `bench-passthrough`
   (+ static musl). The agent's own loop **writes state that contaminates the
   measurement** (e.g. memory)? → a `bench-readonly-*` feature. Neither otherwise.
   Both fail-closed (Invariant 2).
4. **Driving shape** — **default one-shot** (`aura prompt` per item; `swe` /
   `terminal`). Use **gateway-once then prompt-per-item** (`memory`) only when many
   items share one long-lived aura process/workspace.
5. **Grader** — **default: shell out to the official grader** if one exists (most
   faithful). Else **LLM-as-judge** for open-ended NL answers (clone
   `memory/judge.rs` + `llm.rs`); else a **deterministic** check. Oracle must hit
   ≈100% under it (Invariant 1).

---

## Invariants (the extracted commonality — keep all of them)

These hold across all three benches. A new bench that drops one is wrong.

1. **Arms = `noop` (floor) + `oracle` (ceiling) + the real arm(s).** `noop` and
   `oracle` need **no aura build and no API key**, so they validate the entire
   Docker/grader/judge pipeline **offline and unpriced** before the real arm
   spends a cent. Run them first; if `oracle` isn't ≈100% your grader is
   misconfigured — fix that before trusting any real number. `noop` ≈ floor
   (empty patch / memory-off); `oracle` = ceiling (gold patch / whole-context-in-prompt).

2. **The bench build feature is off-by-default and fails-closed.** A new
   capability that weakens isolation or alters agent behavior for the bench must
   be a Cargo feature that, **when absent, makes the dangerous config a hard
   startup error — never a silent downgrade** (see `crates/config/src/sandbox.rs`:
   `Passthrough` refuses to start without `bench-passthrough`). Declare it in the
   root `Cargo.toml` `[features]`, forward to the owning crate(s), and gate the
   code with `#[cfg(feature = "…")]`. This off-by-default lock is exactly what
   makes the bench safe to merge to `master` while the dangerous path stays
   uncompiled in every real build.

3. **Drive aura as a black box.** Exec the `aura` *binary* (`prompt` / `gateway` /
   `cost`); do **not** link aura's agent/context/session/tool stack. The single
   permitted exception is one concrete backend crate when the bench must populate
   state directly (`bench/memory` depends on `aura-memory` solely for its `ingest`
   bin). Everything else is the CLI contract in `reference.md`.

4. **Self-contained config + minted key + lifted rate limit.** The bench writes
   its own `aura.json` into a fresh isolated workspace and mints a 32-byte
   `/dev/urandom` encryption key — so nothing of the user's `~/.aura` is touched
   and reruns never collide. Always set `cost.rate_limit.max_requests` to
   `1_000_000`: the bench fires every item under **one** `user_id` and aura's
   default 30 req/60s would wedge the run at item 30. (Optionally also support a
   `--aura-config` derive-from-yours mode that overwrites only the one section
   under test — see `bench/memory/src/agent.rs::prepare_arm_config`.)

5. **Money is integer micro-USD (`i64`).** Never `f32/f64` for cost. Read it from
   aura's ledger via `aura cost show --session <id> --json` (with `RUST_LOG=off`
   so stdout is pure JSON); poll briefly for the row (it's written on a detached
   task that races the answer), and key "row landed" on `summary.calls > 0`, not
   on cost (a zero-priced model still writes a row). Render to dollars only at
   print time (`/ 1_000_000.0`). Surface latency + in/out tokens + cost per item.

6. **Isolation scope keys.** Namespace every per-item scope as
   `{bench}-{run_id}-{arm}-conv{N}` so reruns, arms, and co-tenant benches never
   silently contaminate each other.

7. **Secret hygiene.** Pass the API key **by name** — `aura.json` carries only
   `api_key_env`; the value rides in the process/container env (`docker run -e
   NAME`, the adapter's `_env`), never in argv (`ps`) and never written to disk.

8. **Not a CI / `cargo test` target — but the library half is unit-tested.** The
   `run` bin spends money / needs Docker / hits external servers → run by hand,
   and say so in the `Cargo.toml` header comment. The **pure** half (the instance
   IR, prompt framing, prediction/line shaping, scope keys, F1, report
   aggregation, all parsers) has **no aura/Docker/Python dep** and carries real
   `#[cfg(test)]` unit tests. Keep that split: heavy I/O in `agent.rs`/`grader.rs`,
   pure logic in `lib.rs`/`report.rs`.

9. **Conventions:** `[lib] doctest = false` + `publish = false` in the crate
   `Cargo.toml`; all deps `{ workspace = true }` (add the crate to the root
   `[workspace.dependencies]` + `members`); Python tooling is **uv-managed** (own
   `pyproject.toml` + `.python-version`, commit `uv.lock`, gitignore `.venv/` —
   never `pip install` into the system interpreter); gitignore
   `bench-out/` + `results/` + `trace/` + per-run workspaces + `.env`. Follow the
   repo's `CLAUDE.md` style throughout (raw strings for prompts/JSON-ish text, named
   `const`s not magic numbers, `parking_lot` locks, no `.unwrap()` in non-test code).

10. **Trace export — every run dumps its verbatim transcript + call tree.**
    Default-on (opt out with `NO_TRACE=1`) so a run is always debuggable after the
    fact. Per item, after the turn — and for an **in-container** bench *before*
    teardown (the session DB dies with the container) — dump both artifacts to
    `bench/<name>/trace/<run_id>/<arm>/<item>.{messages,trace}.json` (gitignored,
    mirrors `results/`): the verbatim conversation via `aura session history <id>
    --include-superseded --json` (incl. compaction-dropped rows) and the
    jobs/steps/spans call tree via `aura session export <id> --json`. Capture
    stdout with `RUST_LOG=off`; **best-effort** — a trace failure warns and is
    dropped, never failing the graded item. See `reference.md` §"Trace export".

---

## Layout (self-hosted Rust shape — clone `swe` or `memory`)

```
bench/<name>/
  Cargo.toml          # publish=false, [lib] doctest=false, workspace deps; header: "NOT a CI target"
  README.md           # measures-what + faithfulness; arms table; the build feature + why; usage; caveats
  run-bench.sh        # one-command driver: .env autoload → config block → preflight → dry-run → build → arms loop → jq table
  .env.example        # the API key (AURA_API_KEY) + optional AURA_MODEL/AURA_BASE_URL overrides
  .gitignore          # /bench-out/ /results/ /trace/ /.env  (+ per-run workspaces, __pycache__/, /.venv/)
  src/
    lib.rs            # PURE IR + helpers + unit tests, no heavy deps:
                      #   the per-item struct, frame_instruction(), scope keys, run-id, parse_model(), metric
    agent.rs          # drive aura as a black box: render config, mint key, run prompt, parse response + cost + trace export
    report.rs         # InstanceResult + aggregate() → RunReport (overall + per-bucket + means) + print_table() + JSON
    grader.rs         # (if official grader) shell out + parse its report   — OR —
    judge.rs + llm.rs # (if LLM-judge) judge prompt + a minimal OpenAI-compatible ChatClient
    bin/run.rs        # one arm per invocation: select items → run real arm → grade → join → aggregate → write results JSON
    bin/ingest.rs     # (only if a populate-then-query bench, like memory) couples to run.rs via a manifest
  # if it needs Python (dataset export or an official grader):
  pyproject.toml  .python-version  uv.lock  <export>.py
```

**Adapter shape (clone `terminal`)** — no Rust crate, not a workspace member:
`tb_adapter/<agent>.py` (subclass the harness's installed-agent base, copy in the
musl binary + a rendered `aura.json`, run `aura prompt --json -y`),
`<setup>.sh.j2` (install binary, mint key, ensure `git`+`ca-certificates`),
`run.sh` (build the musl binary if missing, load `.env`, exec the harness's
`run --agent-import-path …`), `pyproject.toml` (uv, pin to the harness's Python
range). See `bench/terminal/`.

---

## Build order

1. **Scaffold the dir** + add `bench/<name>` to root `Cargo.toml` `[workspace]
   members` and (self-hosted) `[workspace.dependencies]`.
2. **`lib.rs` first** — the pure IR, framing, scope keys, metric, *with unit
   tests*. `cargo test -p aura-bench-<name>` must pass with zero heavy deps.
3. **The build feature** (if any) — root `[features]` → forward to owning crate →
   `#[cfg(feature)]` gate → **hard error when absent** (Invariant 2). Confirm a
   normal `cargo build` still rejects the dangerous config.
4. **`agent.rs`** — clone the driving module from the closest bench; change only
   the config section and the framing. Keep the response/cost parsers verbatim
   (they're stable and unit-tested).
5. **Grader** — shell out to the official harness (`grader.rs`, clone `swe`) or
   wire the judge (`judge.rs`+`llm.rs`, clone `memory`).
6. **`report.rs`** — clone; rename the per-bucket axis (per-repo / per-category)
   and the metric column. Keep micro-USD + the latency/token/cost columns.
7. **`bin/run.rs`** — clap `Arm` enum (`Noop`/`Oracle`/`<Real>`), `--dry-run` that
   prints the plan with no spend, the noop/oracle branches that skip aura
   entirely, bounded-concurrency real arm (`futures::stream::buffer_unordered`).
8. **`run-bench.sh` + `.env.example` + `.gitignore` + `README.md`** — clone the
   skeleton from `reference.md`; wire the arms list and the build command.
9. **Validate offline first:** `DRY_RUN=1 … run-bench.sh`, then `noop`+`oracle`
   (no key) → oracle ≈100%. Only then point the real arm at a model.
10. **`cargo fmt` + `cargo clippy --all --tests` (zero warnings) + `cargo test`.**

---

## Final checklist

- [ ] `noop` + `oracle` + real arm; oracle ≈100% offline, no key needed for either floor/ceiling.
- [ ] Dangerous bench capability is a fail-closed, off-by-default Cargo feature; normal build rejects it.
- [ ] aura driven only via its binary (+ at most one concrete backend crate); no agent-stack linkage.
- [ ] Self-contained `aura.json` + minted key + `rate_limit.max_requests = 1_000_000`.
- [ ] Cost = `i64` micro-USD from the ledger (`RUST_LOG=off`, poll on `calls`); latency/tokens/cost surfaced per item.
- [ ] Trace export on by default (`NO_TRACE=1` opts out): per item → `trace/<run_id>/<arm>/<item>.{messages,trace}.json` via `aura session history/export`; in-container benches export before teardown.
- [ ] Scope keys are `{bench}-{run_id}-{arm}-conv{N}`.
- [ ] API key by name only — never in argv or written to disk.
- [ ] `Cargo.toml`: `publish=false`, `[lib] doctest=false`, workspace deps, "NOT a CI target" header; `lib.rs` has unit tests and no heavy deps.
- [ ] gitignore `bench-out/ results/ trace/ .env` (+ workspaces/.venv); Python (if any) is uv-managed with committed `uv.lock`.
- [ ] `run-bench.sh` (.env autoload, dry-run, jq floor→real→ceiling table) + `README.md` (arms table, the feature + why, caveats).
- [ ] `cargo fmt`/`clippy --all --tests` clean, `cargo test` green, repo `CLAUDE.md` style followed.
