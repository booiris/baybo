#!/usr/bin/env bash
# Merge every timestamped tb run under runs/ into ONE consolidated scoreboard, so a
# dataset finished across several invocations (e.g. retrying flaky tasks) reads as a
# single run instead of being split across results/results-<ts>.json files.
#
#   ./consolidate.sh
#
# Per task it keeps the BEST attempt across all runs:
#   resolved > graded-but-unresolved > never-graded (a run that produced a trace but
#   no results.json — e.g. interrupted before grading), tie-broken by the newest run.
# Outputs:
#   results/merged.json   — one entry per task (+ provenance + summary counts)
#   trace/merged/<task>   — hardlinked copy of the chosen run's trace (zero extra disk)
#
# Graded outcomes come from tb's results-<ts>.json (mirrored by run.sh). Tasks that
# have a trace dir but appear in no run's results.json are recovered as ungraded, so
# the merged denominator is the true attempted-task count, not just the graded ones.
set -euo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - <<'PY'
import json, os, re, glob, shutil

RESULTS, TRACE = "results", "trace"
MERGED_JSON = f"{RESULTS}/merged.json"
MERGED_TRACE = f"{TRACE}/merged"
TIER_RESOLVED, TIER_GRADED, TIER_INFRA = 3, 2, 1


def rank(c):
    return (c["tier"], c.get("reward") or 0.0, c["base"])


# 1) graded candidates from every tb results-<ts>.json.
cands = {}  # task_id -> [candidate, ...]
for rf in sorted(glob.glob(f"{RESULTS}/results-*.json")):
    if os.path.basename(rf) == "results-merged.json":
        continue
    try:
        data = json.load(open(rf))
    except Exception:
        continue
    # base = the run-dir timestamp from the filename; tb's native results `id` is an
    # internal UUID, not the runs/<ts> dir name, so never key off it.
    base = os.path.basename(rf)[len("results-"):-len(".json")]
    for e in data.get("results", []):
        task = e.get("task_id")
        if not task:
            continue
        resolved = bool(e.get("is_resolved"))
        cands.setdefault(task, []).append({
            "base": base, "task": task, "trial": e.get("trial_name"),
            "tier": TIER_RESOLVED if resolved else TIER_GRADED,
            "reward": 1.0 if resolved else 0.0,
            "failure_mode": e.get("failure_mode"),
            "parser_results": e.get("parser_results", {}),
        })

# 2) ungraded candidates: trace/<ts>/<task>/ dirs absent from that run's results.
graded_by_base = {}
for c in (c for cs in cands.values() for c in cs):
    graded_by_base.setdefault(c["base"], set()).add(c["task"])
for tp in sorted(glob.glob(f"{TRACE}/*/*/")):
    base, task = tp.rstrip("/").split("/")[1], os.path.basename(tp.rstrip("/"))
    if base in ("merged", "latest") or os.path.islink(os.path.join(TRACE, base)):
        continue  # skip the merged tree + the trace/latest symlink
    if task in graded_by_base.get(base, ()):
        continue
    trial = next((os.path.basename(d.rstrip("/"))
                  for d in glob.glob(os.path.join(tp, "*/"))), task)
    cands.setdefault(task, []).append({
        "base": base, "task": task, "trial": trial, "tier": TIER_INFRA,
        "reward": None, "failure_mode": "ungraded", "parser_results": {},
    })

# 3) pick the best candidate per task; (re)build the trace/merged tree.
if os.path.islink(MERGED_TRACE) or os.path.exists(MERGED_TRACE):
    shutil.rmtree(MERGED_TRACE, ignore_errors=True)
os.makedirs(MERGED_TRACE, exist_ok=True)


def link_tree(src_dir, dst_dir):
    # Mirror src_dir into dst_dir as hardlinks: real files, zero extra disk, and the
    # data outlives a later `rm` of the per-run trace/<ts>/ source. Trace files are
    # write-once, so sharing the inode can't diverge. Copy only if hardlink refused.
    for root, _dirs, files in os.walk(src_dir):
        rel = os.path.relpath(root, src_dir)
        out = dst_dir if rel == "." else os.path.join(dst_dir, rel)
        os.makedirs(out, exist_ok=True)
        for fn in files:
            s, d = os.path.join(root, fn), os.path.join(out, fn)
            try:
                os.link(s, d)
            except OSError:
                shutil.copyfile(s, d)


results, by_source = [], {}
for task in sorted(cands):
    best = max(cands[task], key=rank)
    base, trial = best["base"], best["trial"]
    trace_path = None
    src_trace = os.path.join(TRACE, base, task)
    if os.path.isdir(src_trace):
        link_tree(src_trace, os.path.join(MERGED_TRACE, task))
        trace_path = f"merged/{task}/{trial}/agent-logs/trace.json"
    results.append({
        "task_id": task,
        "trial_name": trial,
        "is_resolved": best["tier"] == TIER_RESOLVED,
        "failure_mode": best["failure_mode"],
        "source_run": base,
        "attempts_seen": len(cands[task]),
        "parser_results": best["parser_results"],
        "trace_path": trace_path,
    })
    by_source[base] = by_source.get(base, 0) + 1

resolved_n = sum(1 for r in results if r["is_resolved"])
graded_n = sum(1 for r in results if r["failure_mode"] != "ungraded")
total = len(results)
json.dump({"id": "merged", "summary": {
    "tasks": total, "resolved": resolved_n,
    "accuracy": round(resolved_n / total, 4) if total else 0.0,
    "graded": graded_n, "ungraded": total - graded_n,
    "by_source_run": dict(sorted(by_source.items())),
    "source_runs": sorted({c["base"] for cs in cands.values() for c in cs}),
}, "results": results}, open(MERGED_JSON, "w"), indent=2)

print(f"merged {total} tasks from {len({c['base'] for cs in cands.values() for c in cs})} "
      f"runs -> {MERGED_JSON}  (trace/merged/)")
print(f"  resolved {resolved_n}/{total} = "
      f"{(resolved_n/total*100) if total else 0:.1f}%   "
      f"(graded {graded_n}, ungraded {total-graded_n})")
for b, n in sorted(by_source.items()):
    print(f"    best-from {b}: {n}")
PY
