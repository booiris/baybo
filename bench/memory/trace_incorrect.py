#!/usr/bin/env python3
"""Trace incorrect bench results back to their source LOCOMO dialogue.

Usage:
    python3 bench/memory/trace_incorrect.py <arm> [run_id] [--conv N] [--limit N] [--context K]

Examples:
    python3 bench/memory/trace_incorrect.py oracle full10 --context 1 --limit 15
    python3 bench/memory/trace_incorrect.py openviking full10b --conv 0

Each incorrect QuestionResult is matched (by conv_idx + question) to its LOCOMO
sample, then to the `evidence` dia_ids and the original dialogue turns. With
--context K, K turns on each side of every evidence turn are shown too, because
LOCOMO's evidence often points at a lead-in line while the answer sits one turn
over — looking only at the evidence line leads to wrong "gold is broken" calls.
"""
import argparse
import json
from pathlib import Path

BENCH = Path(__file__).resolve().parent


def lookups(sample):
    """{dia_id -> 'speaker: text'}, {question -> qa}, {dia_id -> (session_seq, idx)}."""
    dia, pos = {}, {}
    for key, val in sample["conversation"].items():
        if key.startswith("session_") and not key.endswith("_date_time") and isinstance(val, list):
            seq = []
            for turn in val:
                if "dia_id" in turn:
                    text = turn.get("text") or turn.get("blip_caption", "")
                    line = f'{turn.get("speaker", "")}: {text}'
                    dia[turn["dia_id"]] = line
                    seq.append((turn["dia_id"], line))
            for i, (did, _) in enumerate(seq):
                pos[did] = (seq, i)
    qa = {q["question"]: q for q in sample.get("qa", [])}
    return dia, qa, pos


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("arm", help="noop | oracle | mem0 | openviking")
    ap.add_argument("run_id", nargs="?", default="full10b")
    ap.add_argument("--conv", type=int, help="only this conv_idx (0-9)")
    ap.add_argument("--limit", type=int, default=0, help="max rows to show (0 = all)")
    ap.add_argument("--context", type=int, default=0, help="turns of context around each evidence dia_id")
    ap.add_argument("--dataset", default=str(BENCH / "bench-out" / "locomo10.json"))
    args = ap.parse_args()

    res = json.load(open(BENCH / "results" / f"results-{args.arm}-{args.run_id}.json"))
    dataset = json.load(open(args.dataset))

    rows = [r for r in res["results"] if not r["correct"]]
    if args.conv is not None:
        rows = [r for r in rows if r["conv_idx"] == args.conv]
    if args.limit:
        rows = rows[: args.limit]

    print(f"{args.arm} {args.run_id}: {len(rows)} incorrect shown\n" + "=" * 72)
    for r in rows:
        sample = dataset[r["conv_idx"]]
        dia, qa, pos = lookups(sample)
        evidence = qa.get(r["question"], {}).get("evidence", [])
        print(f"Q: {r['question']}")
        print(f"  gold:   {r['gold']}")
        print(f"  answer: {r['answer']}")
        print(f"  judge:  {r['judge_reason']}")
        print(f"  conv_idx={r['conv_idx']} ({sample.get('sample_id')})  evidence={evidence}")
        shown = set()
        for dia_id in evidence:
            if args.context and dia_id in pos:
                seq, i = pos[dia_id]
                lo, hi = max(0, i - args.context), min(len(seq), i + args.context + 1)
                for j in range(lo, hi):
                    did, line = seq[j]
                    if did in shown:
                        continue
                    shown.add(did)
                    mark = "  <<< evidence" if did == dia_id else ""
                    print(f"      [{did}] {line[:150]}{mark}")
            else:
                print(f"      [{dia_id}] {dia.get(dia_id, '(dia_id not found)')}")
        print("-" * 72)


if __name__ == "__main__":
    main()
