#!/usr/bin/env python
"""Export mini-swe-agent trajectories (runs/<run-id>/<id>/<id>.traj.json) into
readable per-instance Markdown + an index, so a full run's conversation flow can
be browsed without parsing raw JSON.

Usage:
  python export_trajs.py --run-id <RID> [--out DIR] [--results RESULTS.json]
                         [--only <id>[,<id>...]] [--max-output-chars N]

Defaults: runs-dir=./runs, out=runs/<RID>/_export (gitignored via /runs/),
results=results/latest-baseline.json (for the resolved ✓/✗ flag).
"""
import argparse, json, os, sys

DEFAULT_MAX_OUTPUT = 4000   # per tool-output block
DEFAULT_MAX_TASK = 6000     # the PR/problem statement


def trunc(s, n):
    s = s or ""
    return s if len(s) <= n else s[:n] + f"\n…[truncated {len(s) - n} chars]"


def fence(body, lang=""):
    """Wrap body in a code fence longer than any backtick run inside it, so
    content containing ``` (e.g. mini's instruction template, which shows a
    ```bash example) can't prematurely close the fence."""
    import re
    longest = max((len(r) for r in re.findall(r"`+", body)), default=0)
    bt = "`" * max(3, longest + 1)
    return f"{bt}{lang}\n{body}\n{bt}"


def assistant_cmd(msg):
    """The bash command from a mini assistant turn (tool_calls or extra.actions)."""
    for tc in (msg.get("tool_calls") or []):
        try:
            return json.loads(tc["function"]["arguments"]).get("command", "")
        except Exception:
            pass
    for a in (msg.get("extra", {}) or {}).get("actions", []) or []:
        if a.get("command"):
            return a["command"]
    return ""


def render(traj, resolved, max_out, max_task):
    info = traj.get("info", {})
    msgs = traj.get("messages", [])
    iid = traj.get("instance_id", "?")
    stats = info.get("model_stats", {}) or {}
    out = []
    flag = "✓ resolved" if resolved is True else ("✗ unresolved" if resolved is False else "? ungraded")
    out.append(f"# {iid}")
    out.append("")
    out.append(f"- **{flag}**  · exit=`{info.get('exit_status')}`"
               f"  · api_calls={stats.get('api_calls')}  · mini v{info.get('mini_version')}")
    out.append("")

    # task = first user message (PR description / problem statement)
    user = next((m for m in msgs if m.get("role") == "user"), None)
    if user:
        out.append("## Task")
        out.append("")
        out.append(fence(trunc(str(user.get("content", "")).strip(), max_task)))
        out.append("")

    out.append("## Trajectory")
    out.append("")
    step = 0
    pending = None  # (reasoning, command)
    for m in msgs:
        role = m.get("role")
        if role == "assistant":
            # deepseek-style turns carry BOTH: reasoning_content (the hidden CoT)
            # and content (the THOUGHT mini acts on). Keep both — they differ.
            reasoning = (m.get("reasoning_content")
                         or (m.get("provider_specific_fields", {}) or {}).get("reasoning_content")
                         or "")
            thought = str(m.get("content", "")).strip()
            pending = (reasoning.strip(), thought, assistant_cmd(m))
        elif role == "tool" and pending is not None:
            step += 1
            reasoning, thought, cmd = pending
            pending = None
            out.append(f"### Step {step}")
            if reasoning:
                out.append("")
                out.append(f"> {reasoning.replace(chr(10), chr(10) + '> ')}")
            if thought:
                out.append("")
                out.append(thought)
            if cmd:
                out.append("")
                out.append(fence(cmd.strip(), "bash"))
            content = str(m.get("content", ""))
            out.append("")
            out.append(fence(trunc(content, max_out)))
            out.append("")

    sub = info.get("submission")
    out.append("## Final submission (diff)")
    out.append("")
    out.append(fence(sub.strip() if sub else "(no submission)", "diff"))
    out.append("")
    return "\n".join(out), step, info.get("exit_status"), stats.get("api_calls")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--run-id", required=True)
    ap.add_argument("--runs-dir", default="runs")
    ap.add_argument("--out", default="")
    ap.add_argument("--results", default="results/latest-baseline.json")
    ap.add_argument("--only", default="")
    ap.add_argument("--max-output-chars", type=int, default=DEFAULT_MAX_OUTPUT)
    ap.add_argument("--max-task-chars", type=int, default=DEFAULT_MAX_TASK)
    a = ap.parse_args()

    run_dir = os.path.join(a.runs_dir, a.run_id)
    out_dir = a.out or os.path.join(run_dir, "_export")
    os.makedirs(out_dir, exist_ok=True)

    resolved_of = {}
    if os.path.isfile(a.results):
        for r in json.load(open(a.results)).get("results", []):
            resolved_of[r["instance_id"]] = r.get("resolved")

    only = set(filter(None, a.only.split(",")))
    trajs = sorted(p for p in os.listdir(run_dir)
                   if os.path.isdir(os.path.join(run_dir, p))
                   and os.path.isfile(os.path.join(run_dir, p, f"{p}.traj.json")))
    if only:
        trajs = [t for t in trajs if t in only]

    rows = []
    for iid in trajs:
        traj = json.load(open(os.path.join(run_dir, iid, f"{iid}.traj.json")))
        md, steps, exit_status, calls = render(
            traj, resolved_of.get(iid), a.max_output_chars, a.max_task_chars)
        open(os.path.join(out_dir, f"{iid}.md"), "w").write(md)
        rows.append((iid, resolved_of.get(iid), steps, exit_status, calls))

    # index
    rows.sort(key=lambda r: (r[1] is True, r[0]))  # unresolved first
    nres = sum(1 for r in rows if r[1] is True)
    idx = [f"# mini-swe-agent trajectories — {a.run_id}", "",
           f"{len(rows)} instances · resolved {nres}/{len(rows)} "
           f"({100 * nres / len(rows):.1f}%)" if rows else "no instances", "",
           "| instance | resolved | steps | exit | api_calls |",
           "|---|---|---|---|---|"]
    for iid, res, steps, exit_status, calls in rows:
        mark = "✓" if res is True else ("✗" if res is False else "?")
        idx.append(f"| [{iid}]({iid}.md) | {mark} | {steps} | {exit_status} | {calls} |")
    open(os.path.join(out_dir, "index.md"), "w").write("\n".join(idx) + "\n")

    print(f"wrote {len(rows)} trajectories + index.md to {out_dir}")


if __name__ == "__main__":
    main()
