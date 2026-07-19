# janitor - Background Maintenance Sweeps

## Overview

The `janitor` crate (`baybo-janitor`) runs best-effort, cadence-driven maintenance **outside** the agent loop: three filesystem TTL sweeps and one database retention sweep. It does **not** do storage compaction (there is no `VACUUM`); the only table it touches is `channel_pairings`.

A single `Janitor` struct holds the `WorkspacePaths` plus two optional dependencies wired by builders — the pairing store and the sidecar-cache view. `Janitor::run(shutdown)` sweeps once at boot, then ticks every `TICK_INTERVAL` (12h) until `shutdown` resolves. Each sweep is best-effort: a failure in one is logged and the others still run.

### Sweeps

| Sweep | TTL | Target | Cadence |
|-------|-----|--------|---------|
| Log files | `LOG_FILE_TTL` = 30 days | `logs_dir()` + `channel_logs_dir()` (`is_log_file`) | every `TICK_INTERVAL` (12h) |
| work/tmp scratch | `WORK_TMP_TTL` = 7 days (day count from `baybo_workspace::paths::WORK_TMP_TTL_DAYS`) | top-level entries of `work_tmp_dir()`; an entry is stale only when the **newest** mtime anywhere in its tree is past the TTL | every `TICK_INTERVAL` (12h) |
| Pairing rows | `PAIRING_APPROVAL_TTL` = 7 days (approved) | `channel_pairings` via `ChannelPairingStore::purge_expired` | every `PAIRING_SWEEP_INTERVAL` (1h), plus once per 12h tick |
| Sidecar cache | `SIDECAR_CACHE_TTL` = 7 days | `$XDG_CACHE_HOME/baybo/sidecars/` stale `<name>-<hash>` dirs | every `SIDECAR_SWEEP_INTERVAL` (24h) |

### Public surface

- **`Janitor`** — `new(paths)`; builders `with_pairing_store(Arc<dyn ChannelPairingStore>)` and `with_sidecar_cache(SidecarCache)`; sweep entry points `sweep_once()`, `sweep_pairings_once(now)`, `sweep_sidecar_cache()`; and the `run(shutdown)` loop.
- **`JanitorReport`** — per-sweep counts (`log_files_removed`, `work_tmp_removed`, `sidecar_dirs_removed`, `pairings_purged`).
- **`SidecarCache`** — `cache_root: PathBuf` + `live_dirs: HashSet<String>` (the `<name>-<hash>` set the running Baybo currently has materialised).
- **`JanitorError`** — single `Filesystem { path, source }` variant.

## Design Decisions

### Pairing purge runs hourly; everything else is half-daily

Pending pairing codes expire on the order of minutes, so a daily sweep would let dead pending rows pile up. `PAIRING_SWEEP_INTERVAL` (1h) gets its own `tokio::time::interval` arm in the `run` loop, separate from the 12h `TICK_INTERVAL` that drives the filesystem sweeps. `sweep_once` also runs the pairing purge so a process that never trips the hourly tick (e.g. heavy load deferring every interval fire) still eventually reaps. `purge_expired(now, approved_cutoff)` hard-deletes both pending rows past their expiry and **approved** rows whose `approved_at` is older than `PAIRING_APPROVAL_TTL` (7 days). See [`pairing.md`](pairing.md).

> Pairing rows are short-lived auth-flow ephemera, **not** session data. The "session rows are core data, never deleted" invariant (see the root `CLAUDE.md` and [`storage.md`](storage.md)) applies to `sessions`, which the janitor never touches.

### work/tmp gates on the newest in-tree mtime, never through symlinks

`<workspace>/work/tmp` is the disposable-scratch dir the Bash tool
advertises to the model (see [`workspace.md`](workspace.md)); the sweep
is what makes "swept after 7 days" true. It removes **top-level** entries
only — an entry (file or whole directory) goes when the *newest* mtime
anywhere in its tree is past `WORK_TMP_TTL`, so a scratch checkout the
agent still touches survives as a unit instead of being hollowed out
file-by-file. The walk never follows symlinks: a symlinked entry is
measured by the link's own lstat mtime and removed with `remove_file`
(the link, never the target), and links inside a directory don't pull
outside trees into the staleness read. The day count
(`WORK_TMP_TTL_DAYS`) lives in `baybo-workspace` because the Bash tool
description quotes the same figure.

### Sidecar-cache sweep is the rarest

The sidecar cache only accumulates cruft after a binary upgrade lands a fresh content hash (single-digit MB per upgrade), so it runs on its own 24h cadence (`SIDECAR_SWEEP_INTERVAL`) — every other 12h tick — via a `last_sidecar_sweep` sentinel. It removes only directories under `cache_root` that are **not** in `live_dirs` **and** older than the TTL; the TTL doubles as a safety margin against a concurrent older-version Baybo under the same UID still using a dir. Non-directory entries and the live set are always left alone.

### Best-effort, fail-open, TTL-gated

Every sweep swallows its own errors (`tracing::warn!` then continue) so one bad directory or a transient DB error can't stop the rest or crash the loop. All deletions are gated on an mtime older than the relevant TTL; nothing is removed on age alone without the TTL check. The `run` loop uses `MissedTickBehavior::Delay` so a slow sweep can't stack burst catch-up ticks.

## Constraints

- Internal deps: `baybo-store` (the `ChannelPairingStore` trait for the pairing purge) and `baybo-workspace` (path resolution). It depends on the `baybo-store` **ports** crate, not `baybo-storage`.
- Both DB-touching and sidecar sweeps are opt-in: without `with_pairing_store` the pairing sweep is skipped; without `with_sidecar_cache` the sidecar sweep is a no-op.

## Collaboration

| Module | Role |
|--------|------|
| `gateway` | `crates/baybo/src/gateway_cmd.rs` constructs the `Janitor`, wires `with_pairing_store(graph.stores.channel_pairing)` and (when sidecars are active) `with_sidecar_cache`, and spawns `run` against the gateway shutdown signal. |
| `storage` | `SqliteChannelPairingStore::purge_expired` issues the `DELETE FROM channel_pairings` the pairing sweep drives |
| `pairing` | Owns the `channel_pairings` rows the sweep reaps; `baybo-janitor` is the cadence that enforces their retention |
| `workspace` | `WorkspacePaths` resolves `logs_dir` / `channel_logs_dir`; the sidecar cache root descends from `baybo_workspace::paths::baybo_cache_root()` and reaches the janitor via the gateway's `SidecarRuntime::sidecars_cache_root()` / `live_dir_names()` |
