#!/usr/bin/env python
"""Export aura SWE-bench agent traces into readable per-instance Markdown + an
index, in the SAME format as bench/swe-baseline/export_trajs.py (mini-swe-agent)
so the two arms' conversation flows sit side by side.

Sources (for run <RID>):
  trace/<RID>/agent/<id>.messages.json   — the conversation (system/user/assistant/tool)
  results/results-agent-<RID>.json       — resolved ✓/✗ + cost/tokens/latency
  runs/predictions-agent-<RID>.jsonl     — the final graded diff (model_patch)

Usage:
  python export_aura_trajs.py --run-id <RID> [--out DIR] [--only id,id]
                              [--max-output-chars N] [--max-task-chars N]
"""
import argparse, glob, json, os, re

DEFAULT_MAX_OUTPUT = 4000
DEFAULT_MAX_TASK = 6000


def trunc(s, n):
    s = s or ""
    return s if len(s) <= n else s[:n] + f"\n…[truncated {len(s) - n} chars]"


def fence(body, lang=""):
    """Wrap body in a code fence longer than any backtick run inside it, so
    content containing ``` (e.g. a problem statement with code blocks) can't
    prematurely close the fence."""
    longest = max((len(r) for r in re.findall(r"`+", body)), default=0)
    bt = "`" * max(3, longest + 1)
    return f"{bt}{lang}\n{body}\n{bt}"


def blocks_of(msg):
    return (msg.get("message", {}) or {}).get("content", []) or []


def text_blocks(blocks):
    out = []
    for b in blocks:
        if isinstance(b, str):
            out.append(b)
        elif isinstance(b, dict) and "Text" in b:
            out.append(b["Text"])
    return "\n".join(out).strip()


def thinking_blocks(blocks):
    out = []
    for b in blocks:
        if isinstance(b, dict) and "Thinking" in b:
            for seg in (b["Thinking"].get("content") or []):
                if isinstance(seg, dict) and seg.get("text"):
                    out.append(seg["text"])
    return "\n".join(out).strip()


def strip_tool_output(content):
    """Drop the <tool_output name="...">...</tool_output> wrapper aura adds."""
    s = content or ""
    i = s.find(">")
    if s.startswith("<tool_output") and i != -1:
        s = s[i + 1:]
        if s.startswith("\n"):
            s = s[1:]
    end = "</tool_output>"
    if s.rstrip().endswith(end):
        s = s.rstrip()[: -len(end)].rstrip()
    return s


def render_tool_call(name, inp):
    """A readable rendering of one tool invocation (mirrors the bash blocks in
    the mini export; non-bash tools show their core inputs)."""
    inp = inp or {}
    if name == "Bash":
        return fence((inp.get("command", "") or "").strip(), "bash")
    if name == "Read":
        loc = inp.get("file_path", "")
        rng = ""
        if inp.get("offset") or inp.get("limit"):
            rng = f"  (offset={inp.get('offset')}, limit={inp.get('limit')})"
        return f"**Read** `{loc}`{rng}"
    if name == "Grep":
        extra = ", ".join(f"{k}={inp[k]}" for k in ("path", "output_mode", "glob") if inp.get(k))
        return f"**Grep** `{inp.get('pattern', '')}`" + (f"  ({extra})" if extra else "")
    if name == "Glob":
        return f"**Glob** `{inp.get('pattern', '')}`" + (f"  (path={inp['path']})" if inp.get("path") else "")
    if name in ("Edit", "Write"):
        head = f"**{name}** `{inp.get('file_path', '')}`"
        if name == "Edit":
            old = trunc(inp.get("old_string", ""), 600)
            new = trunc(inp.get("new_string", ""), 600)
            diff = "\n".join(["- " + l for l in old.splitlines()]
                             + ["+ " + l for l in new.splitlines()])
            return head + "\n" + fence(diff, "diff")
        return head + "\n" + fence(trunc(inp.get("content", ""), 1200))
    # generic
    compact = json.dumps(inp, ensure_ascii=False)
    return f"**{name}** " + (compact if len(compact) <= 400 else compact[:400] + " …")


def render(iid, msgs, meta, patch, max_out, max_task):
    out = []
    res = meta.get("resolved")
    flag = "✓ resolved" if res is True else ("✗ unresolved" if res is False else "? ungraded")
    cost = meta.get("cost_micro_usd")
    cost_s = f"${cost / 1e6:.4f}" if cost else "n/a"
    out.append(f"# {iid}")
    out.append("")
    out.append(f"- **{flag}**  · cost={cost_s}  · in/out tok="
               f"{meta.get('input_tokens')}/{meta.get('output_tokens')}"
               f"  · latency={meta.get('latency_ms')}ms")
    fr = meta.get("failure_reason")
    if fr:
        out.append("- failure_reason: " + fr.replace("\n", " "))
    out.append("")

    # task = the source=="user" user message (the SWE problem statement), not the
    # agent-injected system reminder / soul prompt.
    task = ""
    for m in msgs:
        inner = m.get("message", {})
        if inner.get("role") == "user" and inner.get("source") == "user":
            task = text_blocks(blocks_of(m))
            break
    if task:
        out.append("## Task")
        out.append("")
        out.append(fence(trunc(task, max_task)))
        out.append("")

    # index ToolResults by tool_use_id
    results = {}
    for m in msgs:
        for b in blocks_of(m):
            if isinstance(b, dict) and "ToolResult" in b:
                tr = b["ToolResult"]
                results[tr.get("tool_use_id")] = tr.get("content", "")

    out.append("## Trajectory")
    out.append("")
    step = 0
    for m in msgs:
        inner = m.get("message", {})
        if inner.get("role") != "assistant":
            continue
        bl = blocks_of(m)
        think = thinking_blocks(bl)
        note = text_blocks(bl)
        tool_uses = [b["ToolUse"] for b in bl if isinstance(b, dict) and "ToolUse" in b]
        if not tool_uses:
            # final assistant note (no tool call)
            if think or note:
                out.append("### (final note)")
                if think:
                    out.append("")
                    out.append("> " + think.replace("\n", "\n> "))
                if note:
                    out.append("")
                    out.append(note)
                out.append("")
            continue
        for tu in tool_uses:
            step += 1
            out.append(f"### Step {step}")
            if think:
                out.append("")
                out.append("> " + think.replace("\n", "\n> "))
                think = ""  # show the turn's thinking once, before its first tool
            if note:
                out.append("")
                out.append(note)
                note = ""
            out.append("")
            out.append(render_tool_call(tu.get("name"), tu.get("input")))
            res_content = strip_tool_output(results.get(tu.get("id"), ""))
            out.append("")
            out.append(fence(trunc(res_content, max_out)))
            out.append("")

    out.append("## Final submission (diff)")
    out.append("")
    out.append(fence(patch.strip() if patch else "(no patch / empty submission)", "diff"))
    out.append("")
    return "\n".join(out), step


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--run-id", required=True)
    ap.add_argument("--trace-dir", default="trace")
    ap.add_argument("--results-dir", default="results")
    ap.add_argument("--runs-dir", default="runs")
    ap.add_argument("--out", default="")
    ap.add_argument("--only", default="")
    ap.add_argument("--max-output-chars", type=int, default=DEFAULT_MAX_OUTPUT)
    ap.add_argument("--max-task-chars", type=int, default=DEFAULT_MAX_TASK)
    a = ap.parse_args()

    agent_dir = os.path.join(a.trace_dir, a.run_id, "agent")
    out_dir = a.out or os.path.join(a.trace_dir, a.run_id, "_export")
    os.makedirs(out_dir, exist_ok=True)

    # resolved + cost meta
    meta = {}
    rp = os.path.join(a.results_dir, f"results-agent-{a.run_id}.json")
    if os.path.isfile(rp):
        for r in json.load(open(rp)).get("results", []):
            meta[r["instance_id"]] = r

    # final diffs
    patches = {}
    pp = os.path.join(a.runs_dir, f"predictions-agent-{a.run_id}.jsonl")
    if os.path.isfile(pp):
        for line in open(pp):
            line = line.strip()
            if not line:
                continue
            d = json.loads(line)
            patches[d["instance_id"]] = d.get("model_patch", "")

    only = set(filter(None, a.only.split(",")))
    files = sorted(glob.glob(os.path.join(agent_dir, "*.messages.json")))
    rows = []
    for f in files:
        iid = os.path.basename(f)[: -len(".messages.json")]
        if only and iid not in only:
            continue
        msgs = json.load(open(f)).get("messages", [])
        md, steps = render(iid, msgs, meta.get(iid, {}), patches.get(iid, ""),
                           a.max_output_chars, a.max_task_chars)
        open(os.path.join(out_dir, f"{iid}.md"), "w").write(md)
        rows.append((iid, meta.get(iid, {}).get("resolved"), steps,
                     meta.get(iid, {}).get("cost_micro_usd")))

    rows.sort(key=lambda r: (r[1] is True, r[0]))
    nres = sum(1 for r in rows if r[1] is True)
    idx = [f"# aura SWE-bench traces — {a.run_id}", "",
           f"{len(rows)} instances · resolved {nres}/{len(rows)} "
           f"({100 * nres / len(rows):.1f}%)" if rows else "no instances", "",
           "| instance | resolved | steps | cost |", "|---|---|---|---|"]
    for iid, res, steps, cost in rows:
        mark = "✓" if res is True else ("✗" if res is False else "?")
        cost_s = f"${cost / 1e6:.4f}" if cost else "—"
        idx.append(f"| [{iid}]({iid}.md) | {mark} | {steps} | {cost_s} |")
    open(os.path.join(out_dir, "index.md"), "w").write("\n".join(idx) + "\n")

    print(f"wrote {len(rows)} trajectories + index.md to {out_dir}")


if __name__ == "__main__":
    main()
