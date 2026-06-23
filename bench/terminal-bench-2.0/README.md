# baybo ↔ Terminal-Bench 2.0 (Harbor) installed-agent adapter

Run the **real `baybo` agent inside the official [Terminal-Bench 2.0](https://www.tbench.ai)
tasks** for **leaderboard-comparable** scores. The adapter installs baybo **into
each task's Docker container**, runs one `baybo prompt`, and lets the task's own
verifier (pytest) grade it — exactly how the leaderboard agents (Claude Code,
Codex, …) are run.

## Terminal-Bench 2.0 runs on **Harbor**, not the legacy `tb` CLI

TB 2.0 moved off the `terminal-bench` PyPI package onto the
[Harbor](https://www.harborframework.com) framework. There is no
`terminal-bench-core` 2.0 dataset for the old CLI — the 2.0 task set lives in the
Harbor dataset **`terminal-bench/terminal-bench-2`** (89 tasks). The legacy `tb`
adapter is the sibling [`../terminal-bench-1.0/`](../terminal-bench-1.0/).

[`harbor_adapter/baybo_agent.py`](harbor_adapter/baybo_agent.py) is a thin subclass
of Harbor's `harbor.agents.installed.base.BaseInstalledAgent`, modeled on the
bundled `codex.py`. No fork of Harbor — it's used via `--agent-import-path`.

## Why a static **musl** binary + `--features bench-bash`

Same rationale as 1.0. baybo normally wraps every shell command in an OS sandbox
(bwrap/docker); inside a task container there is no bwrap and the container already
*is* the isolation boundary, so the binary is built `--features bench-bash` (Bash
runs raw — no OS sandbox, no work-dir jail, no uv shim) and the rendered
`baybo.json` sets `sandbox.mode = none`. A **static musl** build runs in any task
container regardless of glibc:

```bash
rustup target add x86_64-unknown-linux-musl     # one-time (+ musl-tools)
cargo build --release --target x86_64-unknown-linux-musl --features bench-bash -p baybo
# -> target/x86_64-unknown-linux-musl/release/baybo
```

## Run

The Python env is uv-managed (`pyproject.toml` + `uv.lock` pin Harbor + Python
≥3.12). [`run.sh`](run.sh) builds the binary if missing, loads `.env`, runs Harbor,
and mirrors each trial's outcome into `trace/` + `results/`:

```bash
cd bench/terminal-bench-2.0
cp .env.example .env && $EDITOR .env       # set the API key (+ optional model/base_url)

./run.sh                                    # all 89 tasks (deepseek/deepseek-v4-flash)
./run.sh -i terminal-bench/fix-git -l 1     # ONE task — see the task-name note below
./run.sh -n 4                               # 4 concurrent trials (RAM: ~2G/task)
BAYBO_MODEL=openai/gpt-4o ./run.sh           # override model
```

> ⚠️ **Single-task filtering uses the org-prefixed name with `-i`.** Harbor's
> `--include-task-name`/`-i` filter wants the full dataset name
> `terminal-bench/<task>` (e.g. `-i terminal-bench/fix-git`), **not** a bare
> `-t fix-git` (errors "Package name must be in org/name format"). `-i` is
> repeatable and ORs together — but pass each as a **separate literal flag**
> (`-i terminal-bench/a -i terminal-bench/b`); a shell array that doesn't
> word-split collapses into one filter that matches nothing.

Config (all via env — see [`.env.example`](.env.example)): **API key** =
`BAYBO_API_KEY` (provider-agnostic) or the provider's own var
(`DEEPSEEK_API_KEY`/`OPENAI_API_KEY`/…); **model** = `--model <provider>/<model>`
or `BAYBO_MODEL`; **base URL** = `BAYBO_BASE_URL` (optional).

Harbor writes each run to `runs/<ts>/<trial>/{agent/,verifier/}`; `run.sh` mirrors
that into the bench-wide convention — `trace/<ts>/<task>/<trial>/agent-logs/` +
per-task `result.json`, and an aggregate `results/results-<ts>.json` — with
`trace/latest` · `results/latest.json` · `runs/latest` pointers, live as trials
grade.

## Consolidate a multi-invocation run

[`consolidate.sh`](consolidate.sh) merges **every** timestamped run under `runs/`
into one scoreboard, keeping the **best attempt per task** (resolved > graded >
infra-failed, tie-broken by recency) — so a dataset that took several invocations
(e.g. retrying infra-flaky tasks) reads as a single run. It also recovers tasks
that *never* graded in any run by scanning `runs/<ts>/<trial>/exception.txt`, so
the denominator is the true attempted-task count, not just the graded ones.

```bash
./consolidate.sh        # -> results/merged.json  (+ trace/merged/<task> hardlinks)
```

## How it works (per task)

1. Harbor builds/starts the task's container (the `docker_image` in its
   `task.toml`).
2. `BayboAgent.install()` `environment.upload_file`s the musl binary + a rendered
   `baybo.json` (`none` sandbox, self-contained state dir, the provider/model under
   test), installs it to `/usr/local/bin/baybo`, mints a vault key, and ensures
   ca-certificates.
3. `BayboAgent.run()` runs `baybo prompt --json -y` to completion (a non-zero exit
   is logged, not raised, so the verifier still grades), then exports the
   transcript + call-tree to Harbor's per-trial `/logs/agent` dir.
4. Harbor runs the task's verifier (pytest) and writes `verifier/reward.txt`.

## Results — `deepseek/deepseek-v4-flash`, full 89-task run (2026-06-13)

**47 / 89 = 52.8%** resolved (harbor `eval mean`), or **58.0%** excluding the 8
infra-limited tasks below. Best-attempt scoreboard in
[`results/merged.json`](results/) (`consolidate.sh`): 74 tasks from the main run +
15 from a recovery re-run. Verified genuine — real agent transcripts, official
pytest, verifier correctly fails botched attempts. Far above a naive expectation
for a fast/cheap model; the hard tail (cryptanalysis, ML training, VM tasks) is
where the genuine failures sit.

## Known limitations — the 8 infra-limited tasks

These never graded for **environment**, not agent, reasons, so they're excluded
from the 58.0% "faithful" rate. Two root causes:

**(a) Giant ML images won't pull on a slow link.** The task images bundle the full
torch/CUDA/transformers stack + model weights (5–20 GB each). On a ~2 MB/s link to
the registry mirrors a single image's content layer doesn't finish before the pull
drops, so the env never starts (mostly `EnvironmentStartTimeoutError`):

| task | difficulty | what it tests |
|---|---|---|
| `hf-model-inference` | medium | download a HF DistilBERT sentiment model, serve it behind a Flask API |
| `mteb-leaderboard` | medium | name the best embedding model on the Scandinavian MTEB leaderboard |
| `mteb-retrieve` | medium | retrieve the 5th-most-similar doc via the `bge-small-zh-v1.5` model |
| `pytorch-model-recovery` | medium | reconstruct a torch model from a `state_dict`, tune `output_layer` under an MSE target |
| `torch-tensor-parallelism` | hard | implement `Column`/`RowParallelLinear` tensor parallelism |

**(b) The bench container can't provide the runtime.** Image present, but the task
needs hardware/virtualization the container doesn't expose:

| task | difficulty | what it tests | why it can't run |
|---|---|---|---|
| `qemu-alpine-ssh` | medium | boot `alpine.iso` in QEMU, expose sshd on port 2222 | no `/dev/kvm` (nested virt); pure-TCG emulation too slow |
| `qemu-startup` | medium | boot `alpine.iso` in QEMU reachable via telnet | same QEMU/virt limit |
| `torch-pipeline-parallelism` | hard | pipeline-parallel LLaMA training (`train_step_pipeline_afab`) | OOMs under the 2 GB/container cap |

To cover (a), pull the giant images somewhere with real bandwidth and
`docker save`/`load` them over, or run that subset on a better-connected host —
this link isn't viable for them.

## Caveats

- **Image-pull is the dominant cost** on a constrained network (~2 MB/s mirrors).
  Pre-pull the task images before a full run so a slow pull doesn't eat each task's
  `build_timeout`; the bench's verifier bootstrap (curl/uv/pytest) fits the
  generous TB 2.0 verifier timeout, so unlike 1.0 no base-image pre-bake is needed.
- Pick a capable model — weak tool-callers tank scores independent of the plumbing.
- The provider key is exported into the container's env for the run; it is never
  written to `baybo.json` (only the env-var *name* is).
