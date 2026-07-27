# `baybo-workspace` tests share a fixed tmp path

**Status:** open, pre-existing (verified on `master`). Not a CI risk; it only bites when two
`cargo nextest run --workspace` invocations overlap on the same `target/` directory.

## Symptom

```
error: could not lock config file
  /data/aura/crates/workspace/target/test_tmp/workspace_preserve_edits_test/skills/.git/config: File exists
thread 'manager::tests::seed_default_identity_files_preserves_user_edits' panicked at
  crates/workspace/src/manager.rs:254
```

Reproduced by running four full suites concurrently plus a full rebuild: two of the four failed,
both in `crates/workspace/src/manager.rs`, both on the same git config lock.
`ensure_layout_creates_dirs_and_local_git_repos` fails the same way.

## Cause

Those tests build their workspace under a **fixed** path — `target/test_tmp/<test-name>/` — instead
of a `tempfile::tempdir()`. `ensure_layout` runs `git init` inside it, and git takes a lock on
`.git/config`. Two concurrent runs of the same test race for that lock and one loses.

Nothing in the test names or bodies is shared state *by design*; the fixed path is just how they
were written. Every other suite that needs a scratch directory (the sqlite pool tests, the
compression e2e) already uses `tempfile::tempdir()`.

## Fix

Swap the fixed paths for `tempfile::tempdir()` and let the handle's `Drop` clean up, matching the
rest of the workspace. `tempfile` is already a workspace dependency. Four call sites in
`crates/workspace/src/manager.rs` (the `.join("test_tmp")` chains).

## Why it went unnoticed

CI runs one suite at a time, so the collision needs a human running two by hand — which is exactly
how it surfaced. The failure also looks unrelated to what you're working on (a git lock in a
workspace test, while you're editing something else entirely), so the cheap read is "flaky test,
re-run it" rather than "these tests are not isolated".
