# SWE-bench baseline — mini-swe-agent

A reference baseline for `bench/swe`: runs the SWE-bench team's official minimal
agent [`mini-swe-agent`](https://github.com/SWE-agent/mini-swe-agent) with the
**same model** as the baybo arm (deepseek-v4-flash) on **SWE-bench_Lite**, grades
with the **same official `swebench` harness**, and emits a results JSON in
`bench/swe`'s schema — so the resolve-rate is directly comparable to baybo.

```bash
# validation (first 5 instances)
bench/swe-baseline/run.sh
# full 300 + side-by-side vs an baybo run
LIMIT=300 AGENT_RESULTS=../swe/results/latest-agent.json bench/swe-baseline/run.sh
```

Knobs (env): `LIMIT` (first N) or `INSTANCE_IDS` (space-separated → regex),
`WORKERS`, `MODEL` (default `$BAYBO_MODEL`), `STEP_LIMIT` (mini's per-instance
budget, default 250), `AGENT_RESULTS` (baybo results JSON to compare against).

- Model key reused from `bench/swe/.env` (`BAYBO_API_KEY`); litellm reads it via
  `DEEPSEEK_API_KEY`/`OPENAI_API_KEY` (exported by `run.sh`).
- Reuses the cached `swebench/sweb.eval.x86_64.*:latest` eval images (mini picks
  them by the standard name — no re-pull).
- `mini` runs inside those images at `/testbed` (conda `testbed` env), produces
  `runs/<run_id>/preds.json`; the harness grades it; `shape_results.py` writes
  `results/results-baseline-<run_id>.json` (+ a `latest-baseline.json` link) and
  prints the baybo-vs-baseline comparison.

Scratch (`runs/`, `results/`, `mini-config/`, `.venv/`) is gitignored; the uv
lockfile + `pyproject.toml` + `.python-version` are tracked.

## Reading the conversations + comparing arms

Three stdlib-only helpers turn a graded run's raw transcripts into readable
Markdown and a side-by-side HTML page. **One-click:**

```bash
bench/swe-baseline/view_compare.sh          # latest baybo + mini runs → serve on :8091
PORT=9000 bench/swe-baseline/view_compare.sh   # different port
SERVE=0   bench/swe-baseline/view_compare.sh   # just regenerate, don't serve
BAYBO_RID=full300-… MINI_RID=baseline-full300-… bench/swe-baseline/view_compare.sh
```

It auto-derives both run ids from the `latest-*.json` result symlinks, runs the
three steps below, and serves the result (refresh in place if a server is up):

```
view it:  ssh -L 8091:localhost:8091 <host>   then open  http://localhost:8091/compare.html
stop:     pkill -f "http.server 8091"
```

The page shows the **36 divergent cases** (one arm solved, the other didn't) in
two scrollable columns with **module-aware snap scrolling** (Step N lines up
with Step N), filter buttons, and a `Sync scroll` toggle.

The steps it runs, if you want them individually (run ids come from the
`results-<arm>-<run_id>.json` filenames):

```bash
# baybo: trace .messages.json -> trace/<RID>/_export/<id>.md + index.md
python ../swe/export_baybo_trajs.py --run-id <BAYBO_RID>
# mini: runs/<RID>/<id>/<id>.traj.json -> runs/<RID>/_export/<id>.md + index.md
python export_trajs.py --run-id <MINI_RID>
# both -> _compare/compare.html (divergent cases, side by side)
python compare_arms.py \
  --baybo-export ../swe/trace/<BAYBO_RID>/_export \
  --mini-export runs/<MINI_RID>/_export \
  --baybo-results ../swe/results/results-agent-<BAYBO_RID>.json \
  --mini-results results/results-baseline-<MINI_RID>.json \
  --out _compare/compare.html
```

Each `.md` is `Task → Trajectory (per-step thinking + command + output) → Final
diff`. Exporters take `--only id,id`, `--max-output-chars`, `--max-task-chars`.
The `_export/` and `_compare/` outputs are gitignored scratch.
