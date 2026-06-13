#!/usr/bin/env bash
# Merge every SWE-bench run into ONE consolidated scoreboard PER ARM, so a dataset
# finished across several invocations (or retried instances) reads as a single run
# instead of being split across results/results-<arm>-<RUN_ID>.json files.
#
#   ./consolidate.sh
#
# Arms are kept separate (noop/oracle/agent are different measurements). Within an
# arm, per instance keep the BEST attempt across runs: resolved > unresolved >
# errored, tie-broken by the newest run. Outputs:
#   results/merged-<arm>.json     — recomputed report + per-instance provenance
#   trace/merged/<arm>/<inst>.*   — hardlinked trace of the chosen run (zero extra disk)
set -euo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - <<'PY'
import json, os, glob, shutil

RESULTS, TRACE = "results", "trace"
MERGED_TRACE = f"{TRACE}/merged"


def rank(it):
    # bigger wins: resolved > unresolved-not-errored > errored, tie-break newest run.
    tier = 2 if it["resolved"] else (0 if it.get("errored") else 1)
    return (tier, it["__run_id"])


# 1) collect every per-instance item, grouped by arm then instance_id.
arms = {}  # arm -> {instance_id -> [item(+__run_id), ...]}
meta = {}  # arm -> representative top-level fields from the newest run
for rf in sorted(glob.glob(f"{RESULTS}/results-*.json")):
    if os.path.basename(rf).startswith("merged"):
        continue
    try:
        data = json.load(open(rf))
    except Exception:
        continue
    arm, run_id = data.get("arm"), data.get("run_id")
    if not arm or not run_id:
        continue
    by_inst = arms.setdefault(arm, {})
    for it in data.get("results", []):
        iid = it.get("instance_id")
        if not iid:
            continue
        by_inst.setdefault(iid, []).append({**it, "__run_id": run_id})
    if arm not in meta or run_id > meta[arm]["run_id"]:
        meta[arm] = {k: data.get(k) for k in ("dataset", "split", "model", "run_id")}

if os.path.exists(MERGED_TRACE):
    shutil.rmtree(MERGED_TRACE, ignore_errors=True)


def hardlink_globs(src_dir, stem, dst_dir):
    # hardlink trace/<run>/<arm>/<stem>.* into trace/merged/<arm>/ (real files, zero
    # extra disk, outlive a later rm of the source run; copy only if hardlink refused).
    os.makedirs(dst_dir, exist_ok=True)
    for s in glob.glob(os.path.join(src_dir, glob.escape(stem) + ".*")):
        d = os.path.join(dst_dir, os.path.basename(s))
        try:
            os.link(s, d)
        except OSError:
            shutil.copyfile(s, d)


def mean(xs):
    return round(sum(xs) / len(xs), 2) if xs else 0.0


for arm in sorted(arms):
    chosen = [max(cands, key=rank) for cands in arms[arm].values()]
    chosen.sort(key=lambda it: it["instance_id"])
    by_repo, out = {}, []
    for it in chosen:
        run_id = it.pop("__run_id")
        repo = it.get("repo", "?")
        r = by_repo.setdefault(repo, {"total": 0, "resolved": 0})
        r["total"] += 1
        r["resolved"] += int(bool(it["resolved"]))
        hardlink_globs(os.path.join(TRACE, run_id, arm), it["instance_id"],
                       os.path.join(MERGED_TRACE, arm))
        out.append({**it, "source_run": run_id})
    n = len(out)
    resolved = sum(1 for it in out if it["resolved"])
    lat = [it["latency_ms"] for it in out if it.get("latency_ms")]
    inp = sum(it.get("input_tokens", 0) for it in out)
    outp = sum(it.get("output_tokens", 0) for it in out)
    cost = sum(it.get("cost_micro_usd", 0) for it in out)
    report = {
        "run_id": "merged", **{k: meta[arm].get(k) for k in ("dataset", "split")},
        "arm": arm, "model": meta[arm].get("model"),
        "total_instances": n, "resolved": resolved,
        "empty_patches": sum(1 for it in out if it.get("empty_patch")),
        "errored": sum(1 for it in out if it.get("errored")),
        "resolved_rate": round(resolved / n, 4) if n else 0.0,
        "mean_latency_ms": mean(lat),
        "input_tokens": inp, "output_tokens": outp, "total_cost_micro_usd": cost,
        "mean_cost_micro_usd": mean([it.get("cost_micro_usd", 0) for it in out]),
        "source_runs": sorted({it["source_run"] for it in out}),
        "by_repo": dict(sorted(by_repo.items())), "results": out,
    }
    json.dump(report, open(f"{RESULTS}/merged-{arm}.json", "w"), indent=2)
    print(f"[{arm}] merged {n} instances from {len(report['source_runs'])} run(s) "
          f"-> results/merged-{arm}.json  (trace/merged/{arm}/)")
    print(f"    resolved {resolved}/{n} = {report['resolved_rate']*100:.1f}%")
PY

# Refresh this bench's per-trace tool-count sidecars (<trace>.tools.json) so the
# bench viewer's task list reads tiny precomputed counts instead of re-parsing
# agent traces that can run to hundreds of MB. Incremental + best-effort.
bench_id="$(basename "$PWD")"
cargo run -q -p aura-bench-web --bin precompute_tool_counts -- --root .. --bench "$bench_id" \
  || echo ">> tool-count precompute skipped (non-fatal); bench/bench-web/run.sh refreshes on launch" >&2
