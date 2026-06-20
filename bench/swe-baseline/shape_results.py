#!/usr/bin/env python
"""Shape a mini-swe-agent SWE-bench run + the official swebench grade into the
same results JSON schema as `bench/swe` (so it's directly comparable and can
feed bench-web), and print a baseline-vs-aura comparison.

Inputs (under <runs-dir>/<run-id>/): `preds.json` (mini's predictions, dict
keyed by instance_id), the harness report `<model_sanitized>.<run-id>.json`,
and `logs/run_evaluation/<run-id>/<model_sanitized>/<id>/{report.json,test_output.txt}`.
"""
import argparse, glob, json, os, re, sys
from collections import defaultdict

# ── failure_reason (ported verbatim from bench/swe/src/grader.rs) ──────────
MAX_LISTED_TESTS = 3
MAX_ERROR_LINES = 5
MAX_ERROR_LINE_LEN = 200
EXCEPTION_SUFFIXES = ("Error", "Exception", "Failed")
_ANSI = re.compile(r"\x1b\[[0-9;]*[ -/]*[@-~]")


def _looks_like_exception(text):
    tok = re.split(r"[:\s(]", text.strip(), 1)[0]
    return bool(tok) and re.fullmatch(r"[\w.]+", tok) and tok.endswith(EXCEPTION_SUFFIXES)


def _extract_error_lines(test_output):
    clean = _ANSI.sub("", test_output)
    primary, fallback = [], []

    def push(out, t):
        t = t.strip()[:MAX_ERROR_LINE_LEN]
        if t and t not in out:
            out.append(t)
        return len(out) >= MAX_ERROR_LINES

    for raw in clean.splitlines():
        line = raw.rstrip()
        if line[:1] == "E" and line[1:2].isspace():
            content = line[1:].strip()
            if not content:
                continue
            if _looks_like_exception(content):
                if push(primary, content):
                    break
            elif len(fallback) < MAX_ERROR_LINES:
                push(fallback, content)
        elif line[:1] and not line[:1].isspace() and _looks_like_exception(line):
            if push(primary, line):
                break
    return primary or fallback


def _summarize_tests(tests):
    shown = tests[:MAX_LISTED_TESTS]
    s = ", ".join(shown)
    if len(tests) > MAX_LISTED_TESTS:
        s += f" (+{len(tests) - MAX_LISTED_TESTS} more)"
    return s


def failure_reason(inst_dir, instance_id):
    rp = os.path.join(inst_dir, "report.json")
    if not os.path.isfile(rp):
        return None
    try:
        rec = json.load(open(rp)).get(instance_id, {})
    except Exception:
        return None
    if rec.get("resolved") is True:
        return None
    if rec.get("patch_successfully_applied") is False:
        return "patch failed to apply"
    ts = rec.get("tests_status") or {}
    fails = lambda g: ts.get(g, {}).get("failure", []) or []
    f2p, p2p = fails("FAIL_TO_PASS"), fails("PASS_TO_PASS")
    if f2p:
        head = f"FAIL_TO_PASS still failing: {_summarize_tests(f2p)}"
    elif p2p:
        head = f"PASS_TO_PASS regressed: {_summarize_tests(p2p)}"
    else:
        return None
    top = os.path.join(inst_dir, "test_output.txt")
    errs = _extract_error_lines(open(top).read()) if os.path.isfile(top) else []
    return head + "".join(f"\n↳ {e}" for e in errs)


# ── repo from instance_id (no dataset load needed) ─────────────────────────
def repo_of(iid):
    org, rest = iid.split("__", 1)
    return f"{org}/{rest.rsplit('-', 1)[0]}"


def load_report_ids(runs_dir, run_id, sanitized):
    # the harness run-report: <sanitized>.<run_id>.json in the run dir
    cands = glob.glob(os.path.join(runs_dir, run_id, f"*.{run_id}.json"))
    pref = os.path.join(runs_dir, run_id, f"{sanitized}.{run_id}.json")
    path = pref if os.path.isfile(pref) else (cands[0] if cands else None)
    if not path:
        return set(), set(), set()
    v = json.load(open(path))
    g = lambda k: set(v.get(k, []) or [])
    return g("resolved_ids"), g("error_ids"), g("empty_patch_ids")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--run-id", required=True)
    ap.add_argument("--runs-dir", required=True)
    ap.add_argument("--results-dir", required=True)
    ap.add_argument("--dataset-name", required=True)
    ap.add_argument("--split", required=True)
    ap.add_argument("--model", required=True)
    ap.add_argument("--agent-results", default="")  # aura results JSON for comparison
    a = ap.parse_args()

    sanitized = a.model.replace("/", "__")
    run_dir = os.path.join(a.runs_dir, a.run_id)
    preds = json.load(open(os.path.join(run_dir, "preds.json")))
    resolved_ids, error_ids, empty_ids = load_report_ids(a.runs_dir, a.run_id, sanitized)
    logbase = os.path.join(run_dir, "logs", "run_evaluation", a.run_id, sanitized)

    results = []
    for iid in sorted(preds):
        patch = preds[iid].get("model_patch") or ""
        results.append({
            "instance_id": iid,
            "repo": repo_of(iid),
            "resolved": iid in resolved_ids,
            "empty_patch": (iid in empty_ids) or (patch.strip() == ""),
            "errored": iid in error_ids,
            "patch_bytes": len(patch.encode()),
            "latency_ms": 0, "input_tokens": 0, "output_tokens": 0,
            "cached_input_tokens": 0, "cost_micro_usd": 0, "error": None,
            "failure_reason": failure_reason(os.path.join(logbase, iid), iid),
        })

    n = len(results)
    nres = sum(r["resolved"] for r in results)
    doc = {
        "run_id": a.run_id, "dataset": a.dataset_name, "arm": "baseline",
        "model": a.model, "mean_latency_ms": 0, "input_tokens": 0,
        "output_tokens": 0, "cached_input_tokens": 0, "total_cost_micro_usd": 0,
        "resolved": nres, "total_instances": n,
        "resolved_rate": (nres / n) if n else 0.0, "results": results,
    }
    os.makedirs(a.results_dir, exist_ok=True)
    out = os.path.join(a.results_dir, f"results-baseline-{a.run_id}.json")
    json.dump(doc, open(out, "w"), indent=2)
    # latest pointer
    latest = os.path.join(a.results_dir, "latest-baseline.json")
    try:
        os.path.islink(latest) and os.unlink(latest)
        os.symlink(os.path.basename(out), latest)
    except OSError:
        pass

    # ── report ──
    byrepo = defaultdict(lambda: [0, 0])
    for r in results:
        byrepo[r["repo"]][1] += 1
        byrepo[r["repo"]][0] += r["resolved"]
    print(f"\n=== BASELINE (mini-swe-agent + {a.model}) — run {a.run_id} ===")
    print(f"resolved {nres}/{n} = {100*nres/n:.1f}%" if n else "no instances")
    print("by repo:")
    for repo, (rr, tt) in sorted(byrepo.items(), key=lambda kv: -kv[1][1]):
        print(f"  {repo:30} {rr:3}/{tt:3}  {100*rr/tt:5.1f}%")

    if a.agent_results and os.path.isfile(a.agent_results):
        aud = json.load(open(a.agent_results))
        ares = {x["instance_id"]: x for x in aud.get("results", [])}
        overlap = [r["instance_id"] for r in results if r["instance_id"] in ares]
        b_ok = sum(1 for i in overlap if next(r for r in results if r["instance_id"] == i)["resolved"])
        a_ok = sum(1 for i in overlap if ares[i].get("resolved"))
        print(f"\n=== AURA vs BASELINE (on {len(overlap)} shared instances) ===")
        print(f"  aura     : {a_ok}/{len(overlap)} = {100*a_ok/len(overlap):.1f}%")
        print(f"  baseline : {b_ok}/{len(overlap)} = {100*b_ok/len(overlap):.1f}%")
        # head-to-head
        only_a = [i for i in overlap if ares[i].get("resolved") and not next(r for r in results if r["instance_id"] == i)["resolved"]]
        only_b = [i for i in overlap if not ares[i].get("resolved") and next(r for r in results if r["instance_id"] == i)["resolved"]]
        print(f"  aura-only solves: {len(only_a)}   baseline-only solves: {len(only_b)}")

    print(f"\nwrote {out}")


if __name__ == "__main__":
    main()
