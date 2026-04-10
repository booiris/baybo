---
name: cleanup-pub
description: Check for unused `pub` items in the workspace and clean them up by removing or reducing visibility.
allowed-tools: Bash(bash scripts/check-unused-pub.sh) Bash(cargo workspace-unused-pub:*) Bash(cargo check:*) Bash(cargo test:*) Bash(cargo clippy:*)
---

# Cleanup Unused Pub Items

You are performing a maintainability cleanup. Your job is to detect unused `pub` items in the workspace and fix them.

## Principles

- Keep visibility minimal. If an item is not used outside its module or crate, prefer private visibility or `pub(crate)` over `pub`.
- Treat unused `pub` items as maintainability debt: remove them when obsolete, or reduce their visibility if external exposure is unnecessary.

## Step 1: Detect unused pub items

Run:

```bash
bash scripts/check-unused-pub.sh
```

This installs/updates `cargo-workspace-unused-pub` if needed, then reports all `pub` items that are not actually used anywhere in the workspace.

## Step 2: Evaluate results

If the tool reports **no unused pub items**, reply with a short confirmation and stop. Do NOT make any changes.

If there are findings, analyze each one:

- **Obsolete items** (dead code with no callers): remove them entirely.
- **Overly visible items** (used only within the module/crate): reduce visibility to `pub(crate)` or private.
- **Trait implementations or items required by external interfaces**: leave unchanged and note why.

Use your judgement. Read the surrounding code before changing visibility to make sure the item is truly unused externally.

## Step 3: Apply fixes

For each item that needs a change:

1. Read the file to understand context.
2. Apply the minimal edit (change `pub` to `pub(crate)`, or remove the item).
3. Do NOT add comments, docstrings, or refactor surrounding code.

## Step 4: Verify

After all edits, run:

```bash
cargo check --all --all-features
```

If it fails, fix the issues (likely a visibility error — the item was used somewhere the tool missed). Iterate until `cargo check` passes.

## Step 5: Summary

Report what was changed:

```
Cleanup complete:
- Removed: <list of removed items>
- Reduced visibility: <list of items changed to pub(crate) or private>
- Skipped: <list of items left unchanged, with reasons>
```
