# SWE-bench claude arm — Claude Code CLI in-container

A third agent arm for `bench/swe`, holding the **model constant** so it compares
*scaffolds*: the real **Claude Code CLI** (native standalone binary, no Node) run
**inside each official eval image** — the same in-container model as the aura arm
— driving **deepseek-v4-flash** via a litellm proxy, graded by the **same official
`swebench` harness**. Directly comparable to the aura + mini-swe-agent arms.

```bash
# validation (first 3 instances)
bench/swe-claude/run.sh
# full run + compare to an aura run
LIMIT=500 AGENT_RESULTS=../swe/results/latest-agent.json bench/swe-claude/run.sh
# a specific set, on Lite
DATASET_NAME=princeton-nlp/SWE-bench_Lite INSTANCE_IDS="django__django-12983 ..." bench/swe-claude/run.sh
```

## How it works

- **Native binary, no Node.** The host's standalone Claude Code build
  (`~/.local/bin/claude` → a single ~220 MB ELF needing only glibc ≥ 2.17, well
  under the eval images' 2.35) is **mounted read-only** into each container at
  `/installed-agent/claude` — no per-container install. Override with `CLAUDE_BIN`.
- **deepseek via a litellm `/v1/messages` proxy.** Claude Code speaks the
  Anthropic Messages API; `run.sh` starts a host litellm proxy that translates
  `/v1/messages` → deepseek-v4-flash. `ANTHROPIC_BASE_URL`/`ANTHROPIC_MODEL` point
  Claude Code at it. (Claude Code's reported `cost_usd` uses *Claude* pricing and
  is wrong; token counts are correct.)
- **`--network=host`.** The container reaches the host proxy at `127.0.0.1:$PROXY_PORT`.
  The docker-bridge / `host.docker.internal` routes are blocked by the host
  firewall here, so host networking is required.
- **Default Claude Code.** The user prompt is just the raw issue
  (`problem_statement`); Claude Code's **default system prompt** drives the agent
  (no custom framing / no `--system-prompt` override) — a faithful out-of-the-box
  baseline. Headless: `claude -p … --output-format stream-json --verbose
  --dangerously-skip-permissions --max-turns N`, in `/testbed` (conda `testbed`).
- **Prediction** = `git diff HEAD` minus any pre-existing dirty files (the sphinx
  setup.py/tox.ini contamination guard), graded by `swebench.harness.run_evaluation`.
- **Captured per instance:** Claude Code's own token usage (input + cache_read +
  output) → preds → results; and a transcript — raw `trace/<run>/agent/<id>.stream.jsonl`
  plus an aura-format `<id>.messages.json` the `export_aura_trajs.py` / `compare_arms.py`
  tooling can render alongside aura/mini.

Knobs (env): `LIMIT` / `INSTANCE_IDS` / `SLICE`, `WORKERS`, `MAX_WORKERS` (grader),
`MODEL`, `MAX_TURNS`, `PROMPT_TIMEOUT`, `PROXY_PORT`, `CLAUDE_BIN`, `AGENT_RESULTS`.
`shape_results.py` reads tokens from preds and reports cost if `PRICE_IN_PER_MTOK`
/ `PRICE_OUT_PER_MTOK` (USD per 1M tokens) are set. Scratch (`runs/`, `results/`,
`trace/`, `proxy/`, `.venv/`) is gitignored; `uv.lock` + `pyproject.toml` +
`.python-version` are tracked.
