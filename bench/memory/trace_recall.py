#!/usr/bin/env python3
"""Show the memory aura actually recalled while answering each bench question.

Each QA question runs in its own aura session whose transcript is saved at
  aura-ws/aura-bench-ws-<run_id>-<arm>/logs/sessions/qa-<run_id>-<arm>-c<conv>-q<idx>.jsonl
Inside it, the backend's recall lands as messages with role=user,
source="recalled_memory". This tool prints, per question: the question + gold +
correct (from the results JSON), every recalled_memory block, and aura's final
answer — so you can tell whether a wrong answer was a recall miss (the fact was
never recalled) or an integration failure (it was recalled but unused).

Usage:
  python3 bench/memory/trace_recall.py <arm> <run_id> [--conv N] [--q I] [--incorrect] [--limit N] [--chars K]

Examples:
  python3 bench/memory/trace_recall.py openviking full10b --conv 0 --incorrect
  python3 bench/memory/trace_recall.py mem0 full10b --conv 0 --q 5 --chars 0
"""
import argparse
import json
import re
from pathlib import Path

BENCH = Path(__file__).resolve().parent


def session_recall(path):
    """Return (recall_blocks, final_answer) from one session transcript."""
    blocks, answer = [], None
    for line in open(path):
        o = json.loads(line)
        if o.get("type") != "message":
            continue
        m = o["message"]
        content = m.get("content")
        if m.get("source") == "recalled_memory":
            if isinstance(content, list):
                blocks.append("\n".join(
                    x.get("Text", str(x)) if isinstance(x, dict) else str(x) for x in content
                ))
            else:
                blocks.append(str(content))
        elif m.get("role") == "assistant":
            if isinstance(content, list) and content and isinstance(content[0], dict):
                answer = content[0].get("Text", str(content))
            else:
                answer = str(content)
    return blocks, answer


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("arm")
    ap.add_argument("run_id")
    ap.add_argument("--conv", type=int)
    ap.add_argument("--q", type=int, help="question index within the conversation")
    ap.add_argument("--incorrect", action="store_true", help="only wrong answers")
    ap.add_argument("--limit", type=int, default=5)
    ap.add_argument("--chars", type=int, default=500, help="chars per recall block (0 = full)")
    args = ap.parse_args()

    sessions = BENCH / f"aura-ws/aura-bench-ws-{args.run_id}-{args.arm}/logs/sessions"
    res = json.load(open(BENCH / "results" / f"results-{args.arm}-{args.run_id}.json"))
    by_conv = {}
    for r in res["results"]:
        by_conv.setdefault(r["conv_idx"], []).append(r)

    def key(p):
        return tuple(int(x) for x in re.search(r"-c(\d+)-q(\d+)", p.name).groups())

    shown = 0
    for f in sorted(sessions.glob(f"qa-{args.run_id}-{args.arm}-c*-q*.jsonl"), key=key):
        conv, qi = key(f)
        if args.conv is not None and conv != args.conv:
            continue
        if args.q is not None and qi != args.q:
            continue
        rows = by_conv.get(conv, [])
        r = rows[qi] if qi < len(rows) else None
        if args.incorrect and (r is None or r["correct"]):
            continue
        blocks, answer = session_recall(f)
        print("=" * 72)
        print(f"c{conv}-q{qi}  correct={r['correct'] if r else '?'}")
        print(f"Q:    {r['question'] if r else '?'}")
        print(f"gold: {r['gold'] if r else '?'}")
        total = sum(len(b) for b in blocks)
        print(f"--- {len(blocks)} recalled_memory block(s), {total} chars total ---")
        for i, b in enumerate(blocks):
            print(f"[block {i}] {b if args.chars == 0 else b[:args.chars]}")
        print(f"A:    {str(answer)[:300]}")
        shown += 1
        if shown >= args.limit:
            break


if __name__ == "__main__":
    main()
