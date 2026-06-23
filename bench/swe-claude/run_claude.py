#!/usr/bin/env python
"""SWE-bench **claude arm**: the real Claude Code CLI (native standalone binary,
no Node) run INSIDE each official eval image — the same in-container model as the
aura arm. The binary is mounted into the container; `claude -p` runs headless in
/testbed driving deepseek-v4-flash via a litellm `/v1/messages` proxy
(ANTHROPIC_BASE_URL). Container uses --network=host so it can reach the host proxy.

Per instance writes the model_patch (git diff HEAD, minus pre-existing dirty
files), Claude Code's own token usage, and a transcript (raw stream-json + an
aura-format .messages.json the existing export/compare tooling can render).
Writes <out>/preds.json; run.sh grades + shapes it.
"""
import argparse, json, os, re, subprocess
from concurrent.futures import ThreadPoolExecutor, as_completed
from datasets import load_dataset

# The user prompt is just the raw issue text — Claude Code's DEFAULT system
# prompt drives the agent (no custom framing / no --system-prompt override),
# for a faithful out-of-the-box Claude Code baseline. (Also avoids str.format on
# issue text that may contain literal { } in code snippets.)

# env that makes Claude Code run headless against the proxy (filled per-run)
def claude_env_flags(base_url, model):
    env = {
        "HOME": "/root",
        "ANTHROPIC_BASE_URL": base_url,
        "ANTHROPIC_AUTH_TOKEN": "sk-litellm",
        "ANTHROPIC_MODEL": model,
        "ANTHROPIC_SMALL_FAST_MODEL": model,
        "DISABLE_AUTOUPDATER": "1",
        "DISABLE_TELEMETRY": "1",
        "DISABLE_ERROR_REPORTING": "1",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "IS_SANDBOX": "1",
    }
    flags = []
    for k, v in env.items():
        flags += ["-e", f"{k}={v}"]
    return flags


def run_bash(container, command, timeout=600):
    full = ("source /opt/miniconda3/etc/profile.d/conda.sh 2>/dev/null; "
            "conda activate testbed 2>/dev/null; cd /testbed; " + command)
    try:
        p = subprocess.run(["docker", "exec", container, "bash", "-lc", full],
                           capture_output=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return 124, ""
    return p.returncode, (p.stdout + p.stderr).decode("utf-8", "replace")


# ── prediction extraction (git diff HEAD minus pre-existing dirty files) ────
def parse_dirty(porcelain):
    paths = set()
    for line in porcelain.splitlines():
        p = line[3:].strip()
        if " -> " in p:
            p = p.split(" -> ", 1)[1]
        if p:
            paths.add(p.strip().strip('"'))
    return paths


def strip_dirty(diff, dirty):
    if not dirty:
        return diff
    blocks, cur = [], []
    for ln in diff.split("\n"):
        if ln.startswith("diff --git "):
            if cur:
                blocks.append(cur)
            cur = [ln]
        else:
            cur.append(ln)
    if cur:
        blocks.append(cur)
    out = []
    for blk in blocks:
        m = re.match(r"diff --git a/.*? b/(.*)", blk[0])
        if m and m.group(1).strip() in dirty:
            continue
        out.extend(blk)
    return "\n".join(out)


# ── transcript: stream-json events -> aura .messages.json (for export/compare) ──
def stream_to_aura(events, iid):
    msgs = []
    for ev in events:
        et = ev.get("type")
        if et == "system":
            txt = ev.get("subtype", "init")
            msgs.append({"message": {"role": "system", "source": "agent",
                                     "content": [{"Text": f"[claude-code {txt}] model={ev.get('model','')}"}]}})
        elif et in ("assistant", "user"):
            m = ev.get("message", {})
            content = m.get("content", [])
            blocks = []
            if isinstance(content, str):
                blocks = [{"Text": content}]
            else:
                for b in content:
                    t = b.get("type")
                    if t == "text":
                        blocks.append({"Text": b.get("text", "")})
                    elif t == "thinking":
                        blocks.append({"Thinking": {"content": [{"kind": "text", "text": b.get("thinking", "")}]}})
                    elif t == "tool_use":
                        blocks.append({"ToolUse": {"id": b.get("id"), "name": b.get("name"), "input": b.get("input")}})
                    elif t == "tool_result":
                        c = b.get("content")
                        if isinstance(c, list):
                            c = "\n".join(x.get("text", "") if isinstance(x, dict) else str(x) for x in c)
                        blocks.append({"ToolResult": {"tool_use_id": b.get("tool_use_id"),
                                                      "content": c if isinstance(c, str) else json.dumps(c)}})
            if blocks:
                src = "user" if et == "user" and any("ToolResult" not in b for b in blocks) is False else "agent"
                msgs.append({"message": {"role": m.get("role", et), "source": "agent", "content": blocks}})
    for i, m in enumerate(msgs):
        m["ordinal"] = i
        m["superseded_by"] = None
    return {"messages": msgs, "session": f"swe-claude-{iid}"}


def write_transcript(trace_dir, iid, problem, repo, stream_lines, events):
    d = os.path.join(trace_dir, "agent")
    os.makedirs(d, exist_ok=True)
    # raw stream-json (ground truth)
    open(os.path.join(d, f"{iid}.stream.jsonl"), "w").write("\n".join(stream_lines))
    # aura-format with the task prepended as the source==user message
    doc = stream_to_aura(events, iid)
    task = {"message": {"role": "user", "source": "user",
                        "content": [{"Text": problem}]},
            "ordinal": 0, "superseded_by": None}
    doc["messages"].insert(1, task) if doc["messages"] else doc["messages"].append(task)
    for i, m in enumerate(doc["messages"]):
        m["ordinal"] = i
    json.dump(doc, open(os.path.join(d, f"{iid}.messages.json"), "w"))


# ── one instance: container + claude CLI + patch ───────────────────────────
def run_one(row, image, args):
    iid = row["instance_id"]
    container = f"swe-claude-{args.run_id}-{iid}"[:120]
    subprocess.run(["docker", "rm", "-f", container], capture_output=True)
    up = subprocess.run(
        ["docker", "run", "-d", "--rm", "--name", container, "--network=host",
         "-v", f"{args.claude_bin}:/installed-agent/claude:ro",
         "--entrypoint", "sleep", image, str(args.prompt_timeout + 600)],
        capture_output=True)
    usage = {"in": 0, "cache_read": 0, "out": 0, "turns": 0}
    if up.returncode != 0:
        return iid, "", usage, f"container failed: {up.stderr.decode('utf-8','replace')[:200]}"
    try:
        _, porc = run_bash(container, "git status --porcelain")
        dirty = parse_dirty(porc)
        prompt = row["problem_statement"]
        argv = (["docker", "exec", "-w", "/testbed"] + claude_env_flags(args.base_url, args.model)
                + [container, "/installed-agent/claude", "-p", prompt,
                   "--dangerously-skip-permissions", "--output-format", "stream-json",
                   "--verbose", "--max-turns", str(args.max_turns)])
        err = None
        try:
            p = subprocess.run(argv, capture_output=True, timeout=args.prompt_timeout)
            out = p.stdout.decode("utf-8", "replace")
        except subprocess.TimeoutExpired as e:
            out = (e.stdout or b"").decode("utf-8", "replace")
            err = "claude timed out"
        lines = [ln for ln in out.splitlines() if ln.strip()]
        events = []
        for ln in lines:
            try:
                events.append(json.loads(ln))
            except Exception:
                pass
        for ev in events:
            if ev.get("type") == "result":
                u = ev.get("usage", {}) or {}
                usage["in"] += u.get("input_tokens", 0) or 0
                usage["cache_read"] += u.get("cache_read_input_tokens", 0) or 0
                usage["out"] += u.get("output_tokens", 0) or 0
                usage["turns"] = ev.get("num_turns", 0) or 0
                if ev.get("is_error") and not err:
                    err = ev.get("result", "claude error")
        patch = strip_dirty(run_bash(container, "git add -A -N >/dev/null 2>&1; git diff HEAD")[1], dirty)
        if args.trace_dir:
            try:
                write_transcript(os.path.join(args.trace_dir, args.run_id), iid,
                                 row["problem_statement"], row["repo"], lines, events)
            except Exception:
                pass
        return iid, patch, usage, err
    except Exception as e:
        return iid, "", usage, f"{type(e).__name__}: {e}"
    finally:
        subprocess.run(["docker", "kill", container], capture_output=True)


def load_make_test_spec():
    try:
        from swebench.harness.test_spec.test_spec import make_test_spec
    except Exception:
        from swebench.harness.test_spec import make_test_spec
    return make_test_spec


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dataset-name", default="princeton-nlp/SWE-bench_Lite")
    ap.add_argument("--split", default="test")
    ap.add_argument("--instance-ids", nargs="*", default=[])
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--slice", default="")
    ap.add_argument("--workers", type=int, default=4)
    ap.add_argument("--model", default="deepseek/deepseek-v4-flash")
    ap.add_argument("--base-url", required=True)
    ap.add_argument("--claude-bin", required=True)        # native claude binary on host
    ap.add_argument("--max-turns", type=int, default=120)
    ap.add_argument("--prompt-timeout", type=int, default=1800)
    ap.add_argument("--run-id", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--trace-dir", default="")
    a = ap.parse_args()

    make_test_spec = load_make_test_spec()
    ds = load_dataset(a.dataset_name, split=a.split)
    rows = list(ds)
    if a.instance_ids:
        want = set(a.instance_ids)
        rows = [r for r in rows if r["instance_id"] in want]
    elif a.slice:
        s, e = a.slice.split(":")
        rows = rows[int(s):int(e)]
    elif a.limit:
        rows = rows[:a.limit]
    images = {r["instance_id"]: make_test_spec(r, namespace="swebench").instance_image_key for r in rows}
    print(f">> claude-CLI arm: {len(rows)} instances, workers={a.workers}, model={a.model}", flush=True)

    preds, done = {}, 0
    with ThreadPoolExecutor(max_workers=a.workers) as ex:
        futs = {ex.submit(run_one, r, images[r["instance_id"]], a): r["instance_id"] for r in rows}
        for f in as_completed(futs):
            iid, patch, usage, err = f.result()
            done += 1
            preds[iid] = {"instance_id": iid, "model_name_or_path": a.model, "model_patch": patch,
                          "input_tokens": usage["in"], "cache_read_input_tokens": usage["cache_read"],
                          "output_tokens": usage["out"], "n_turns": usage["turns"]}
            tag = "EMPTY" if not patch.strip() else f"{len(patch)}B"
            print(f"  [{done}/{len(rows)}] {iid}: {tag} "
                  f"({usage['turns']}t, {usage['in']//1000}k+{usage['cache_read']//1000}k/{usage['out']//1000}k tok)"
                  f"{(' ERR:' + str(err)[:60]) if err else ''}", flush=True)

    os.makedirs(a.out, exist_ok=True)
    json.dump(preds, open(os.path.join(a.out, "preds.json"), "w"), indent=2)
    print(f">> wrote {len(preds)} predictions -> {a.out}/preds.json", flush=True)


if __name__ == "__main__":
    main()
