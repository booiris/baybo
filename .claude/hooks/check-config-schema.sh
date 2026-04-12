#!/usr/bin/env python3
"""PostToolUse hook: when a file under crates/config/src/ is edited,
emit additionalContext reminding Claude to sync aura.example.json.
"""
import json
import re
import sys

try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(0)

path = (data.get("tool_input") or {}).get("file_path", "") or ""
if re.search(r"crates/config/src/.*\.rs$", path):
    print(json.dumps({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": (
                f"The config schema in crates/config/src/ was just modified "
                f"(edited: {path}). Check whether /data/aura/aura.example.json "
                "needs to be updated to reflect the new schema (new fields, "
                "renamed fields, changed defaults, removed fields)."
            ),
        }
    }))
