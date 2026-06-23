#!/usr/bin/env python3
"""Export SWE-bench instances + their canonical Docker image keys to JSON.

Reads a HuggingFace SWE-bench dataset and writes exactly the fields the Rust
`bench/swe` agent arm needs (`problem_statement` / `repo` / `base_commit`) plus
the **image key the official grader uses** — obtained from `swebench`'s
`make_test_spec(...)` with default args, so the agent runs baybo in the *same*
image grading will build/use. Using the library function (rather than hardcoding
the `sweb.eval.<arch>.<id>` naming) keeps us version-proof.

Requires `pip install swebench` (which pulls in `datasets`). Run by hand.

    python swe_export.py --dataset-name princeton-nlp/SWE-bench_Lite \
        --split test --limit 1 --out instances.json
    python swe_export.py --instance-ids sympy__sympy-20590 --out instances.json
"""

import argparse
import json
import os
import sys


def load_make_test_spec():
    # The module path moved across swebench versions; accept both.
    try:
        from swebench.harness.test_spec.test_spec import make_test_spec
    except Exception:
        from swebench.harness.test_spec import make_test_spec
    return make_test_spec


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--dataset-name", default="princeton-nlp/SWE-bench_Lite")
    ap.add_argument("--split", default="test")
    ap.add_argument(
        "--instance-ids",
        nargs="*",
        default=[],
        help="restrict to these ids (overrides --limit)",
    )
    ap.add_argument(
        "--limit",
        type=int,
        default=0,
        help="export only the first N instances (0 = all); ignored if --instance-ids given",
    )
    ap.add_argument(
        "--namespace",
        default="swebench",
        help='image namespace: "swebench" => prebuilt Docker Hub images (default), '
        '"none" => local-build image names. MUST match the run bin / grader --namespace.',
    )
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    from datasets import load_dataset

    make_test_spec = load_make_test_spec()
    # "none"/"" => no namespace (local-build names); else the Hub namespace.
    namespace = None if args.namespace in ("none", "None", "") else args.namespace
    ds = load_dataset(args.dataset_name, split=args.split)

    wanted = set(args.instance_ids)
    out = []
    for row in ds:
        if wanted:
            if row["instance_id"] not in wanted:
                continue
        elif args.limit and len(out) >= args.limit:
            break
        spec = make_test_spec(row, namespace=namespace)
        # The only container run-arg the grader applies (swebench build_container):
        # docker_specs.run_args.cap_add (e.g. Chart.js -> ["SYS_ADMIN"]). Empty for
        # the common Python instances.
        cap_add = spec.docker_specs.get("run_args", {}).get("cap_add", [])
        out.append(
            {
                "instance_id": row["instance_id"],
                "repo": row["repo"],
                "base_commit": row["base_commit"],
                "problem_statement": row["problem_statement"],
                "version": str(row.get("version", "")),
                "image_key": spec.instance_image_key,
                "cap_add": cap_add,
            }
        )

    if wanted:
        missing = wanted - {r["instance_id"] for r in out}
        if missing:
            print(
                f"warning: {len(missing)} requested id(s) not in {args.dataset_name}: "
                f"{sorted(missing)}",
                file=sys.stderr,
            )

    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    with open(args.out, "w") as f:
        json.dump(out, f, indent=2)
    print(f"wrote {len(out)} instance(s) -> {args.out}")


if __name__ == "__main__":
    main()
