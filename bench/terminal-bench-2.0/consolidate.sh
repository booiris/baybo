#!/usr/bin/env bash
# Merge every timestamped Harbor run under runs/ into ONE consolidated scoreboard,
# so a dataset that took several invocations (e.g. retrying infra-flaky tasks) reads
# as a single run instead of being split across `results/results-<ts>.json` files.
#
#   ./consolidate.sh
#
# Per task it keeps the BEST attempt across all runs:
#   resolved (reward>=1) > graded-but-unresolved > never-graded (infra failure),
# tie-broken by the most recent run. Outputs:
#   results/merged.json   — one entry per task (+ provenance + summary counts)
#   trace/merged/<task>   — relative symlink to the chosen run's trace (zero copy)
#
# Graded outcomes come from the already-synced results/results-<ts>.json (written by
# run.sh). Tasks that never graded in ANY run (env-start timeout, image-pull EOF, …)
# are recovered by scanning runs/<ts>/<trial>/ + its exception.txt, so the merged
# denominator is the true attempted-task count, not just the graded ones.
set -euo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - <<'PY'
import json, os, re, glob, shutil

RUNS, RESULTS, TRACE = "runs", "results", "trace"
MERGED_JSON = f"{RESULTS}/merged.json"
MERGED_TRACE = f"{TRACE}/merged"

# rank: bigger wins. (tier, reward, base-timestamp). base sorts chronologically.
TIER_RESOLVED, TIER_GRADED, TIER_INFRA = 3, 2, 1


def rank(c):
    return (c["tier"], c.get("reward") or 0.0, c["base"])


# 1) graded candidates from every results-<ts>.json (authoritative outcomes).
cands = {}  # task_id -> [candidate, ...]
for rf in sorted(glob.glob(f"{RESULTS}/results-*.json")):
    try:
        data = json.load(open(rf))
    except Exception:
        continue
    base = data.get("id") or os.path.basename(rf)[len("results-"):-len(".json")]
    for e in data.get("results", []):
        task = e.get("task_id")
        if not task:
            continue
        cands.setdefault(task, []).append({
            "base": base,
            "trial": e.get("trial_name"),
            "tier": TIER_RESOLVED if e.get("is_resolved") else TIER_GRADED,
            "reward": e.get("reward"),
            "failure_mode": e.get("failure_mode"),
            "parser_results": e.get("parser_results", {}),
            "total_input_tokens": e.get("total_input_tokens") or 0,
            "total_output_tokens": e.get("total_output_tokens") or 0,
            "total_cached_input_tokens": e.get("total_cached_input_tokens") or 0,
            "trial_started_at": e.get("trial_started_at"),
            "trial_ended_at": e.get("trial_ended_at"),
            "task": task,
        })

# 2) infra candidates: runs/<ts>/<trial>/ trial dirs that never graded (no reward.txt).
ERR_RE = re.compile(r"([A-Za-z_]+(?:Error|Timeout))\b")


def infra_kind(trial_dir):
    exc = os.path.join(trial_dir, "exception.txt")
    if not os.path.exists(exc):
        return "killed_or_incomplete"
    txt = open(exc, errors="replace").read()
    nonblank = [ln for ln in txt.splitlines() if ln.strip()]
    last = nonblank[-1] if nonblank else ""
    m = ERR_RE.search(last) or ERR_RE.search(txt)
    if m:
        return m.group(1)
    if re.search(r"manifest|registry-1|/v2/|pull access|auth\.docker", txt):
        return "ImagePullError"
    return "setup_failed"


for td in sorted(glob.glob(f"{RUNS}/*/*/")):
    parts = td.rstrip("/").split("/")
    base, trial = parts[1], parts[-1]
    if base == "latest":
        continue
    # skip job-level files; a real trial dir has agent/ or config.json
    if not (os.path.isdir(os.path.join(td, "agent"))
            or os.path.exists(os.path.join(td, "config.json"))):
        continue
    if os.path.exists(os.path.join(td, "verifier", "reward.txt")):
        continue  # graded -> already covered by results-<ts>.json above
    task = trial.rsplit("__", 1)[0]
    cands.setdefault(task, []).append({
        "base": base, "trial": trial, "tier": TIER_INFRA, "reward": None,
        "failure_mode": infra_kind(td), "parser_results": {},
        "total_input_tokens": 0, "total_output_tokens": 0,
        "total_cached_input_tokens": 0,
        "trial_started_at": None, "trial_ended_at": None, "task": task,
    })

# 3) pick the best candidate per task; (re)build the trace/merged tree.
if os.path.islink(MERGED_TRACE) or os.path.exists(MERGED_TRACE):
    shutil.rmtree(MERGED_TRACE, ignore_errors=True)
os.makedirs(MERGED_TRACE, exist_ok=True)


def link_tree(src_dir, dst_dir):
    # Mirror src_dir into dst_dir as hardlinks: real files, zero extra disk, and
    # the data outlives a later `rm` of the per-run trace/<ts>/ source. Trace files
    # are write-once, so sharing the inode can't cause divergence. Fall back to a
    # copy only if hardlinking is refused (e.g. a cross-device merged dir).
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
    resolved = best["tier"] == TIER_RESOLVED
    trace_path = None
    src_trace = os.path.join(TRACE, base, task)
    if best["tier"] != TIER_INFRA and os.path.isdir(src_trace):
        link_tree(src_trace, os.path.join(MERGED_TRACE, task))
        trace_path = f"merged/{task}/{trial}/agent-logs/trace.json"
    results.append({
        "task_id": task,
        "trial_name": trial,
        "is_resolved": resolved,
        "reward": best["reward"],
        "failure_mode": best["failure_mode"],
        "source_run": base,
        "attempts_seen": len(cands[task]),
        "parser_results": best["parser_results"],
        "total_input_tokens": best.get("total_input_tokens") or 0,
        "total_output_tokens": best.get("total_output_tokens") or 0,
        "total_cached_input_tokens": best.get("total_cached_input_tokens") or 0,
        "trial_started_at": best.get("trial_started_at"),
        "trial_ended_at": best.get("trial_ended_at"),
        "trace_path": trace_path,
    })
    by_source[base] = by_source.get(base, 0) + 1

resolved_n = sum(1 for r in results if r["is_resolved"])
graded_n = sum(1 for r in results if r["failure_mode"] in ("unset", "test_failed"))
total = len(results)
summary = {
    "tasks": total,
    "resolved": resolved_n,
    "accuracy": round(resolved_n / total, 4) if total else 0.0,
    "graded": graded_n,
    "ungraded_infra": total - graded_n,
    "by_source_run": dict(sorted(by_source.items())),
    "source_runs": sorted({c["base"] for cs in cands.values() for c in cs}),
}
json.dump({"id": "merged", "summary": summary, "results": results},
          open(MERGED_JSON, "w"), indent=2)

print(f"merged {total} tasks from {len(summary['source_runs'])} runs "
      f"-> {MERGED_JSON}  (trace/merged/)")
print(f"  resolved {resolved_n}/{total} = {summary['accuracy']*100:.1f}%   "
      f"(graded {graded_n}, infra-failed {total-graded_n})")
print("  best-attempt drawn from each run:")
for b, n in summary["by_source_run"].items():
    print(f"    {b}: {n}")
infra = [r for r in results if r["trace_path"] is None]
if infra:
    print(f"  {len(infra)} task(s) never graded in any run (infra):")
    modes = {}
    for r in infra:
        modes[r["failure_mode"]] = modes.get(r["failure_mode"], 0) + 1
    for m, n in sorted(modes.items(), key=lambda kv: -kv[1]):
        print(f"    {m}: {n}")
PY
