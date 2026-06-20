# SWE-bench baseline — mini-swe-agent

A reference baseline for `bench/swe`: runs the SWE-bench team's official minimal
agent [`mini-swe-agent`](https://github.com/SWE-agent/mini-swe-agent) with the
**same model** as the aura arm (deepseek-v4-flash) on **SWE-bench_Lite**, grades
with the **same official `swebench` harness**, and emits a results JSON in
`bench/swe`'s schema — so the resolve-rate is directly comparable to aura.

```bash
# validation (first 5 instances)
bench/swe-baseline/run.sh
# full 300 + side-by-side vs an aura run
LIMIT=300 AGENT_RESULTS=../swe/results/latest-agent.json bench/swe-baseline/run.sh
```

Knobs (env): `LIMIT` (first N) or `INSTANCE_IDS` (space-separated → regex),
`WORKERS`, `MODEL` (default `$AURA_MODEL`), `STEP_LIMIT` (mini's per-instance
budget, default 250), `AGENT_RESULTS` (aura results JSON to compare against).

- Model key reused from `bench/swe/.env` (`AURA_API_KEY`); litellm reads it via
  `DEEPSEEK_API_KEY`/`OPENAI_API_KEY` (exported by `run.sh`).
- Reuses the cached `swebench/sweb.eval.x86_64.*:latest` eval images (mini picks
  them by the standard name — no re-pull).
- `mini` runs inside those images at `/testbed` (conda `testbed` env), produces
  `runs/<run_id>/preds.json`; the harness grades it; `shape_results.py` writes
  `results/results-baseline-<run_id>.json` (+ a `latest-baseline.json` link) and
  prints the aura-vs-baseline comparison.

Scratch (`runs/`, `results/`, `mini-config/`, `.venv/`) is gitignored; the uv
lockfile + `pyproject.toml` + `.python-version` are tracked.
