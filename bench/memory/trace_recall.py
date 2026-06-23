#!/usr/bin/env python3
"""Show the memory baybo actually recalled while answering each bench question.

Each QA question runs in its own baybo session, exported (default-on; NO_TRACE=1
disables) by run.sh to
  trace/<run_id>/<arm>/qa-<run_id>-<arm>-c<conv>-q<idx>.messages.json
— the JSON that `baybo session history --include-superseded --json` emits. Inside
it the backend's recall rides as a `role=user`, `source="recalled_memory"`
message. This tool prints, per question: the question + gold + correct (from the
results JSON), every recalled_memory block, and baybo's final answer — so you can
tell whether a wrong answer was a recall miss (the fact was never recalled) or an
integration failure (it was recalled but unused).

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


def _text(content):
    """Concatenate a message's Text blocks. ContentBlock is externally tagged, so
    a text block is `{"Text": "..."}`; non-text blocks (ToolUse / Thinking / …)
    are skipped."""
    if isinstance(content, list):
        parts = [x["Text"] for x in content if isinstance(x, dict) and "Text" in x]
        return "\n".join(parts) if parts else json.dumps(content)
    return str(content)


def session_recall(path):
    """Return (recall_blocks, final_answer) from one session's exported transcript
    — the single object `baybo session history --include-superseded --json` emits:
    `{"session", "messages": [{"ordinal", "superseded_by", "message"}, …]}`."""
    blocks, answer = [], None
    data = json.load(open(path))
    for entry in data.get("messages", []):
        m = entry.get("message", entry)
        content = m.get("content")
        if m.get("source") == "recalled_memory":
            blocks.append(_text(content))
        elif m.get("role") == "assistant":
            answer = _text(content)
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

    sessions = BENCH / "trace" / args.run_id / args.arm
    if not sessions.is_dir():
        raise SystemExit(
            f"no traces at {sessions} — re-run with trace export on (the default; "
            f"unset NO_TRACE) for arm={args.arm}, run_id={args.run_id}"
        )
    res = json.load(open(BENCH / "results" / f"results-{args.arm}-{args.run_id}.json"))
    by_conv = {}
    for r in res["results"]:
        by_conv.setdefault(r["conv_idx"], []).append(r)

    def key(p):
        return tuple(int(x) for x in re.search(r"-c(\d+)-q(\d+)", p.name).groups())

    shown = 0
    for f in sorted(sessions.glob(f"qa-{args.run_id}-{args.arm}-c*-q*.messages.json"), key=key):
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
