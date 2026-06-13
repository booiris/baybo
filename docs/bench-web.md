# bench-web — bench results & trace viewer

`bench-web` (`bench/bench-web`, binary `bench-web`) is a standalone,
read-only web UI for the artifacts the bench harnesses leave on disk
under `bench/<name>/{results,trace,runs}/`. It scans those files fresh on
every request, normalizes each bench's heterogeneous `results-*.json`
into one shared model, and serves both a small JSON API and an embedded
React dashboard. No database, no auth — it reads files a developer
already has locally. It is intentionally decoupled from the production
`aura-gateway` daemon.

## Run it

One-click launcher (`bench/bench-web/run.sh`):

```bash
bench/bench-web/run.sh                 # build embedded UI + serve on :7000
bench/bench-web/run.sh --port 8080     # pick a port
bench/bench-web/run.sh --dev           # Vite dev server (HMR) + API, on :5173
bench/bench-web/run.sh --release       # optimized backend
# headless: ssh -L 7000:localhost:7000 <user>@<host> → http://localhost:7000
```

Or directly:

```bash
cargo run -p aura-bench-web -- --root bench --port 7000   # embedded UI
```

Backend flags: `--root` (default `bench`), `--host` (default `127.0.0.1`),
`--port` (default `7000`). Logging via `RUST_LOG=aura_bench_web=debug`.

## Layout

```
bench/bench-web/
  build.rs              pnpm build + zstd-embed web/dist (gateway pattern, webui-only)
  src/
    main.rs             clap CLI + axum serve
    api.rs              /api/* routes + state; webui fallback
    model.rs            the spine: BenchInfo / RunSummary / Item / BenchExtra … (#[derive(TS)] under ts-export)
    input.rs            Deserialize mirrors of each bench's results JSON
    adapters/           swe.rs / tb.rs / memory.rs + mod.rs (scan, standing, search, path helpers)
    trace.rs            trace reshape + raw-file endpoint + safe_join path guard
    webui.rs            serve the embedded bundle
    error.rs            ApiError → HTTP
  web/                  fresh Vite+React+TS+Tailwind-v4 app (pnpm pkg aura-bench-web)
    src/generated/      ts-rs output (COMMITTED; gated by scripts/check-ts-bindings.sh)
    src/types/trace.ts, components/trace/{MessageList,SanitizeChip}.tsx  (ported from web/)
    components/trace/FlowRail.tsx  the item view's default: a dense step rail —
                        one ~30px row per action (user/llm/tool/aux: glyph + label +
                        args preview + duration micro-bar + outcome + token delta),
                        a glance header (sequence strip with failures + token-flow
                        bar), failures/tools/llm filter chips, aggregate footer, and
                        per-row inline drill-down (params/result/thinking/inputs).
                        Errors auto-expand. MessageList.tsx (linear transcript) +
                        Timeline.tsx (raw span tree) sit behind a "raw detail" expander.
```

## Data model — common spine + per-bench extension

Each bench has an adapter that reads its result files into one shared
`RunSummary` → `Item` spine; bench-specific detail rides in the tagged
`BenchExtra` enum (`swe` / `tb` / `memory`), which the UI renders in a
per-item detail panel. The adapters read the **`arm` / `run_id` / `id`
fields inside each JSON**, never the filename (id formats are
inconsistent — `2026-06-13__15-40-24` vs `swe-20260611-005135`).

Two lenses per bench:

- **Run history** — every individual `results-*.json`, time-sorted.
- **Standing** — one row per arm, sourced from the arm's `merged-*`
  consolidation if present, else its newest individual run. `latest-*`
  symlinks are skipped during the scan (deduped against their targets);
  "latest" is computed from the individual runs.

## API

All read-only, all re-scan the filesystem per call:

| route | returns |
|---|---|
| `GET /api/benches` | `BenchInfo[]` — home cards |
| `GET /api/benches/:bench` | `BenchDetail` — info + runs + standing |
| `GET /api/benches/:bench/runs/:run_key` | `RunDetail` — summary + items (`run_key` = results filename stem) |
| `GET /api/benches/:bench/trace?trace=&messages=` | `{session_id, session_messages, jobs}` (reshaped) |
| `GET /api/benches/:bench/file?path=` | raw artifact; `path` may be `<file>#<instance>` to extract a SWE patch from a predictions `.jsonl` |
| `GET /api/search?q=` | `SearchHit[]` across all runs (capped, logged) |

Trace files are treated as **opaque JSON** (the gateway's stance): the
backend only reshapes the envelope (`{session, jobs}` + `{messages}` →
`{session_id, session_messages, jobs}`) so the ported viewer types line
up — `jobs[i].steps` is already the viewer's `ReplayStep[]`.

`trace`/`file` paths are client-supplied but constrained by `safe_join`:
the relative path is rejected if it is absolute or contains `..`, and
after canonicalization must still live under the bench dir (defeats
symlink escape). The first path segment (`:bench`) is validated against
the static bench registry before any file access.

## ts-rs bindings

`model.rs` derives `ts_rs::TS` under the `ts-export` feature and exports
to `web/src/generated/` (committed, like `sdks/channel-ts/src/generated`).
`scripts/check-ts-bindings.sh` regenerates and diffs against HEAD — a
spine change must land with the regenerated `.ts`. 64-bit / `usize`
fields carry `#[ts(type = "number")]` so the TS side is JSON-safe
`number`, not `bigint`. The trace types stay a hand-written mirror
(`web/src/types/trace.ts`, ported from the gateway dashboard).

## Per-bench notes / gotchas

- **swe / memory** carry an `arm` dimension (noop / oracle / agent;
  noop / mem0 / openviking / oracle) — the comparison axis. Arms do not
  share a run timestamp.
- The **`merged-*` consolidations write `mean_latency_ms` as a fractional
  mean** (e.g. `182740.5`) while individual runs write an integer — the
  input DTO types it `f64` and rounds. (A `u64` there silently dropped
  every merged file before this was caught.)
- **terminal-bench** records no per-task cost (so `cost_micro_usd` shows
  `—`). Token counts (input/output **and cached**) aren't in the harness
  results either — they're recovered from each task's agent trace by the
  `run.sh` sync (TB-2.0 also pulls trial timing from Harbor's
  `result.json`). TB-1.0 run-level model / wall-clock comes from
  `runs/<ts>/run_metadata.json`.
- **Cache tokens**: every bench surfaces `cached_input_tokens` (the
  prompt-cache-hit share of input — often 88–99%). swe/memory read it
  from `aura cost show`'s summary; TB sums it from the trace. The UI shows
  it inline (`in / out · N cached (RATE%)`) plus an `input cache rate`
  chip on the item page. Note `cost_micro_usd` already prices cache hits
  at the cheaper rate, so this is a visibility add, not a cost fix.
- **Trace path resolution** differs per bench: swe
  `trace/<run_id>/agent/<id>.{trace,messages}.json`; tb via the item's
  `trace_path` field (TB-2.0 individual + both merged) or derived from
  the `trial_name` timestamp (TB-1.0 individual); memory
  `trace/<run_id>/<arm>/<session_id>.{trace,messages}.json`.
- **Artifacts**: swe → the instance patch (extracted from the predictions
  `.jsonl`); TB-2.0 → `verifier/{test-stdout.txt,ctrf.json}`; TB-1.0 →
  the asciinema `agent.cast` (link only — no in-browser player yet).
- **memory** has no runs on disk yet; its adapter is written against
  `bench/memory/src/report.rs` and will light up once a run lands.
- **Completed runs only.** An in-progress run (no `results-*.json` yet)
  simply doesn't appear until it finishes; the next page load picks it up
  (per-request re-scan, no restart).
