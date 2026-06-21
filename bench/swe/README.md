# aura-bench-swe — SWE-bench benchmark

Measures whether **the real Aura agent** can resolve real GitHub issues, the
[SWE-bench](https://www.swebench.com) way: given an issue and a repository
checked out at a base commit, produce a patch; the patch is *resolved* iff,
after it's applied, the hidden `FAIL_TO_PASS` tests pass and the `PASS_TO_PASS`
tests still pass.

This is the faithful, leaderboard-comparable setup:

- **The agent runs INSIDE each instance's official SWE-bench Docker image** —
  the repo at `base_commit` at `/testbed`, all dependencies installed. So the
  agent can read, edit, *and run the project's own tests* to check its work.
- **Grading uses the official `swebench` harness** (`python -m
  swebench.harness.run_evaluation`) — canonical, leaderboard-parity, with the
  oracle built in via `--predictions_path gold`.

> ⚠️ **Not a CI / `cargo test` target.** The `run` bin starts Docker containers,
> drives a real `aura` against a model API (spends money), and shells out to the
> Python harness. Run it by hand. The Rust *library* (IR + helpers + report) is
> unit-tested and has no Docker/Python/Aura dependency.

## Arms

| arm | what it grades | needs aura? | needs a key? |
| --- | --- | --- | --- |
| `noop` | empty patches → the **floor** (~0%) | no | no |
| `oracle` | the gold patches (`--predictions_path gold`) → the **ceiling** (~100%) | no | no |
| `agent` | aura's patch, produced inside each eval image → **the measurement** | yes | yes |

`noop`/`oracle` need no aura and no API key, so they validate the entire
Docker + grader pipeline **offline and unpriced** (oracle ≈100%, noop 0%) before
the agent arm costs anything. Run them first.

## Build with `--features bench-bash` + `sandbox.mode = none`

The agent runs inside the eval container, where aura's normal OS sandbox (bwrap)
can't nest. Two knobs make Bash run raw there: the harness writes a
`sandbox.mode = none` config (drops the OS sandbox) **and** the binary is built
`--features bench-bash` (the bench profile — also no uv shim, no work-dir jail,
cwd inherited from `/testbed`). `bench-bash` is off-by-default and compiled out
of every prod build; `none` on its own (no feature) keeps uv + the work jail, so
the bench needs both.

The binary is copied into Ubuntu-based eval images, so it must be **static
musl** (a glibc build from a newer host won't run there). `run.sh` builds
it for you:

```bash
cargo build --release --target x86_64-unknown-linux-musl --features bench-bash
```

That needs the musl target (`rustup target add x86_64-unknown-linux-musl`) and a
musl C compiler (`x86_64-linux-musl-gcc`; on Arch: `sudo pacman -S musl`), which
libsql's bundled SQLite requires. Point the bench at a prebuilt binary with
`AURA_BIN=…` to skip the build.

## Prerequisites

- A running **Docker** daemon.
- **[`uv`](https://docs.astral.sh/uv/)** — manages the Python side. The `swebench`
  stack (used to export the dataset + image keys via `swe_export.py`, build
  images via `prepare_images`, and grade via `run_evaluation`) lives in a
  **persistent uv venv** declared by `bench/swe/pyproject.toml`. `uv sync`
  provisions a pinned CPython + deps into `bench/swe/.venv` (built once, reused);
  `run.sh` does it for you. Never `pip install` into the system Python —
  in fact the system interpreter may be too new for the `swebench` stack, which
  is exactly why uv pins one. (Set `PYTHON=…` to use your own interpreter instead.)
- For the agent arm only: the musl toolchain above + a model API key.

## How it works

1. **Export** — `swe_export.py` reads the HF dataset and writes
   `instances.json`: each instance's `problem_statement` / `repo` /
   `base_commit` plus the **exact Docker image key** the grader uses (from
   `swebench`'s `make_test_spec`), so the agent runs in the same image grading
   will. (The `gold`/oracle arm never touches this file.)
2. **Images** (agent arm) — by default (`--namespace swebench`) the prebuilt
   eval images are **pulled** from Docker Hub on demand (fast). Pass
   `--namespace none` to build them locally instead (`python -m
   swebench.harness.prepare_images`). The same `--namespace` goes to
   `swe_export.py` (so the image keys line up) and to the grader.
3. **Agent** (agent arm) — per instance: `docker run` the image, copy in the
   musl `aura` + a `sandbox.mode = none` config (workspace `/aura-home`, *outside*
   `/testbed` so aura's own state never pollutes the diff), run `aura prompt
   --json -y` with cwd `/testbed`, then capture `git diff <base_commit>` as the
   prediction and read the turn's cost from aura's ledger. Containers run
   bounded-concurrently and are always torn down.
4. **Grade** — write `predictions.jsonl` (or use `gold`), then
   `run_evaluation` builds/reuses the images, applies the test patch + the
   prediction, runs the tests, and writes a `<model>.<run_id>.json` report. We
   parse `resolved_ids` and join it with each instance's run metrics.

The agent phase uses **our** container (network on, so aura reaches the LLM);
grading uses the harness's own hermetic containers. Two separate lifecycles.

## Usage

Credentials + overrides live in `bench/swe/.env` (gitignored — copy the template):

```bash
cp bench/swe/.env.example bench/swe/.env   # then fill AURA_API_KEY for the agent arm
```

Easiest — `run.sh` exports the dataset, (for the agent arm) builds the
musl `aura` and pulls the prebuilt images, runs each arm, and prints a
`noop → agent → oracle` table. It defaults to the first SWE-bench_Lite instance
for a cheap smoke:

```bash
# offline floor vs ceiling (no key, no aura) — validates Docker + grader:
bench/swe/run.sh

# all three arms on specific instances (agent needs AURA_API_KEY):
ARMS="noop oracle agent" INSTANCE_IDS="sympy__sympy-20590" bench/swe/run.sh

# preview the plan (resolves image keys via swebench; no Docker/spend):
DRY_RUN=1 ARMS="noop oracle agent" bench/swe/run.sh
```

Or drive the pieces directly. The Python steps go through the uv venv — `uv sync`
once, then run via `bench/swe/.venv/bin/python` (or `uv run`); pass that same
interpreter to the grader with `--python-bin`:

```bash
uv sync --project bench/swe            # build the persistent .venv once

# 1. export instances + image keys
uv run --project bench/swe python bench/swe/swe_export.py \
    --dataset-name princeton-nlp/SWE-bench_Lite \
    --split test --limit 3 --out bench/swe/bench-out/instances.json

# 2. floor / ceiling — no aura, no key (grader runs via the uv venv):
cargo run -p aura-bench-swe --bin run -- --arm oracle \
    --instances-json bench/swe/bench-out/instances.json --results-dir bench/swe/results \
    --python-bin bench/swe/.venv/bin/python
cargo run -p aura-bench-swe --bin run -- --arm noop \
    --instances-json bench/swe/bench-out/instances.json --results-dir bench/swe/results \
    --python-bin bench/swe/.venv/bin/python

# 3. the agent (after prepare_images + a musl aura build):
cargo run -p aura-bench-swe --bin run -- --arm agent \
    --instances-json bench/swe/bench-out/instances.json --results-dir bench/swe/results \
    --python-bin bench/swe/.venv/bin/python \
    --aura-bin target/x86_64-unknown-linux-musl/release/aura \
    --model deepseek/deepseek-v4-flash   # <provider>/<model>; key read from $AURA_API_KEY
    # Other providers: --model openai/gpt-4o, --model anthropic/claude-3-5-sonnet, …
    # Override the endpoint with --base-url <URL>.

# plan only (no Docker, no keys, no Python):
cargo run -p aura-bench-swe --bin run -- --arm agent \
    --instances-json bench/swe/bench-out/instances.json --dry-run
```

Each `run` writes the report to `results/results-<arm>-<run_id>.json` (overall +
per-repo resolved rate, and every instance's resolved/empty/errored flags, patch
size, latency, tokens, and cost). Its working artifacts — `predictions-*.jsonl`,
the swebench harness report (`<model>.<run_id>.json`), and the harness `logs/` —
go to `runs/`, keeping `results/` to the report alone. `bench-out/`, `results/`,
and `runs/` are **gitignored** (regenerated; results embed full patches).

Every agent run also writes its **verbatim transcript + call-tree trace** to
`trace/<run_id>/agent/<instance>.{messages,trace}.json` (`aura session history` +
`session export`, captured in-container before teardown) — read these to see
exactly what the agent did and why it stopped. Default-on; `NO_TRACE=1` (or
`--no-trace`) disables it; `trace/` is gitignored.

For a readable rendering, `export_aura_trajs.py --run-id <run_id>` turns those
traces into per-instance Markdown (`Task → per-step thinking/command/output →
final diff`) under `trace/<run_id>/_export/`. To compare against the
mini-swe-agent baseline side by side, see `bench/swe-baseline/view_compare.sh`
(one-click export + diff page).

## Caveats

- **musl build is effectively mandatory for the agent arm.** A glibc `aura`
  built on a newer host won't load in the older-glibc eval images. `run.sh`
  fails the agent arm with instructions if the musl toolchain is missing;
  `noop`/`oracle` never need it.
- **Cost & time scale with the instance set.** Full splits build/pull tens of GB
  of images and take hours. Default to a small `INSTANCE_IDS`/`LIMIT`; opt into
  Lite/Verified/full explicitly.
- **Agent latency** is the `aura prompt` turn time only (not image build or
  grading). **Cost** is the whole-turn answer-side spend from aura's ledger; a
  cost-read failure degrades to zeros rather than failing the instance.
- **The agent is told not to edit tests.** Test files are withheld at agent time
  (the harness supplies them at grade time) and any stray test edits in the
  prediction are reset by the grader, matching upstream.
- **`--predictions_path gold` is the oracle.** If oracle isn't ~100% on your
  instances, the grader/images are misconfigured — fix that before trusting the
  agent number.
