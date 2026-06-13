#!/usr/bin/env bash
# Merge every memory-bench run into ONE consolidated scoreboard PER ARM, so a testset
# answered across several invocations (or reruns of some questions) reads as a single
# run instead of being split across results/results-<arm>-<RUN_ID>.json files.
#
#   ./consolidate.sh
#
# Arms stay separate (noop/oracle/<backend> are different measurements). Within an
# arm, per question keep the BEST attempt across runs — highest f1, tie-broken by
# `correct` then the newest run. Question identity is (conv_idx, question) because
# session_id embeds the run and isn't stable across runs. Outputs:
#   results/merged-<arm>.json     — recomputed report + per-question provenance
#   trace/merged/<arm>/<sid>.*    — hardlinked trace of the chosen run (zero extra disk)
set -euo pipefail
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 - <<'PY'
import json, os, glob, shutil
from collections import defaultdict

RESULTS, TRACE = "results", "trace"
MERGED_TRACE = f"{TRACE}/merged"


def rank(it):
    # bigger wins: higher f1 > correct > newest run.
    return (it["f1"], int(bool(it["correct"])), it["__run_id"])


# 1) collect every per-question item, grouped by arm then (conv_idx, question).
arms = {}   # arm -> {(conv_idx, question) -> [item(+__run_id), ...]}
meta = {}   # arm -> representative top-level fields from the newest run
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
    by_q = arms.setdefault(arm, {})
    for it in data.get("results", []):
        key = (it.get("conv_idx"), it.get("question"))
        by_q.setdefault(key, []).append({**it, "__run_id": run_id})
    if arm not in meta or run_id > meta[arm]["run_id"]:
        meta[arm] = {k: data.get(k) for k in
                     ("testset", "answer_model", "judge_model", "conversations", "run_id")}

if os.path.exists(MERGED_TRACE):
    shutil.rmtree(MERGED_TRACE, ignore_errors=True)


def hardlink_globs(src_dir, stem, dst_dir):
    # hardlink trace/<run>/<arm>/<session_id>.* into trace/merged/<arm>/ (real files,
    # zero extra disk, outlive a later rm of the source run; copy if hardlink refused).
    if not stem:
        return
    os.makedirs(dst_dir, exist_ok=True)
    for s in glob.glob(os.path.join(src_dir, glob.escape(stem) + ".*")):
        d = os.path.join(dst_dir, os.path.basename(s))
        try:
            os.link(s, d)
        except OSError:
            shutil.copyfile(s, d)


def mean(xs):
    return round(sum(xs) / len(xs), 4) if xs else 0.0


for arm in sorted(arms):
    chosen = [max(cands, key=rank) for cands in arms[arm].values()]
    chosen.sort(key=lambda it: (it.get("conv_idx") or 0, it.get("question") or ""))
    out, cats = [], defaultdict(list)
    for it in chosen:
        run_id = it.pop("__run_id")
        hardlink_globs(os.path.join(TRACE, run_id, arm), it.get("session_id"),
                       os.path.join(MERGED_TRACE, arm))
        rec = {**it, "source_run": run_id}
        out.append(rec)
        cats[it.get("category", "?")].append(it)
    n = len(out)
    correct = sum(1 for it in out if it["correct"])

    def cat_stat(cat, items):
        m = len(items)
        c = sum(1 for it in items if it["correct"])
        return {
            "category": cat, "total": m, "correct": c,
            "accuracy": round(c / m, 4) if m else 0.0,
            "mean_f1": mean([it["f1"] for it in items]),
            "mean_latency_ms": mean([it.get("latency_ms", 0) for it in items]),
            "mean_input_tokens": mean([it.get("input_tokens", 0) for it in items]),
            "mean_output_tokens": mean([it.get("output_tokens", 0) for it in items]),
            "mean_cost_micro_usd": mean([it.get("cost_micro_usd", 0) for it in items]),
        }

    report = {
        "run_id": "merged", "testset": meta[arm].get("testset"), "arm": arm,
        "answer_model": meta[arm].get("answer_model"),
        "judge_model": meta[arm].get("judge_model"),
        "conversations": meta[arm].get("conversations"),
        "total_questions": n, "total_correct": correct,
        "overall_accuracy": round(correct / n, 4) if n else 0.0,
        "mean_f1": mean([it["f1"] for it in out]),
        "mean_latency_ms": round(mean([it.get("latency_ms", 0) for it in out])),
        "input_tokens": sum(it.get("input_tokens", 0) for it in out),
        "output_tokens": sum(it.get("output_tokens", 0) for it in out),
        "total_cost_micro_usd": sum(it.get("cost_micro_usd", 0) for it in out),
        "mean_cost_micro_usd": mean([it.get("cost_micro_usd", 0) for it in out]),
        "source_runs": sorted({it["source_run"] for it in out}),
        "by_category": [cat_stat(c, cats[c]) for c in sorted(cats)],
        "results": out,
    }
    json.dump(report, open(f"{RESULTS}/merged-{arm}.json", "w"), indent=2)
    print(f"[{arm}] merged {n} questions from {len(report['source_runs'])} run(s) "
          f"-> results/merged-{arm}.json  (trace/merged/{arm}/)")
    print(f"    correct {correct}/{n} = {report['overall_accuracy']*100:.1f}%   "
          f"mean_f1={report['mean_f1']}")
PY

# Refresh this bench's per-trace tool-count sidecars (<trace>.tools.json) so the
# bench viewer's task list reads tiny precomputed counts instead of re-parsing
# agent traces that can run to hundreds of MB. Incremental + best-effort.
bench_id="$(basename "$PWD")"
cargo run -q -p aura-bench-web --bin precompute_tool_counts -- --root .. --bench "$bench_id" \
  || echo ">> tool-count precompute skipped (non-fatal); bench/bench-web/run.sh refreshes on launch" >&2
