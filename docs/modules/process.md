# process - Unified Subprocess Ownership

## Overview

`baybo-process` is the only crate allowed to call `Command::spawn` for runtime
subprocesses. It turns every Tokio child into an owned process group and returns
a `ManagedChild` guard with an explicit lifecycle. `ProcessManager` owns
registration, graceful process-wide shutdown, forced shutdown, and crash
recovery. There is no parallel `std::process::Child` ownership path; probes are
async so every live runtime child has the same guard and cancellation behavior.

The runtime creates one manager at boot and injects it into Bash/rg, MCP stdio
servers, sandbox backends, Deck, channel sidecars, and external CLI agents.
One-shot setup and CLI flows use a transient manager but still receive the same
process-group and drop guarantees.

## Lifecycle contract

Every spawn creates a new Unix process group, injects a unique
`BAYBO_PROCESS_TOKEN`, registers the group before returning, and writes a
best-effort ledger row under `<workspace>/state/processes/` for the long-running
runtime manager.

An owner must call `wait`, `wait_with_output`, or `shutdown`. Each terminal path
reaps the leader and kills any unclaimed descendants before unregistering the
group. Dropping the child guard is the non-async backstop: it kills the whole
group and removes its ledger record. Runtime shutdown uses one deadline across
MCP protocol cleanup and `ProcessManager::shutdown_all`; a stuck reconciler is
aborted at that deadline so it cannot prevent the process sweep. The manager
sends `SIGTERM`, waits for the remaining grace period, then sends `SIGKILL`; the
force-exit watchdog calls `kill_all_now` before `process::exit`.

If Baybo dies without running destructors (SIGKILL, OOM, abort), ledger records
survive. The next manager validates the unique token against a live member's
environment before killing the recorded group, which prevents a recycled pgid
from targeting an unrelated process. Recovery uses `/proc` on Linux and
`proc_listpids` + `KERN_PROCARGS2` on macOS.

## Ownership boundary

`ProcessManager` owns OS process lifetime, not business restart policy or
protocol cleanup. MCP still closes JSON-RPC, the Deck supervisor still applies
backoff/quarantine, the sidecar supervisor still restarts crashed bundles, and
Docker still removes daemon-side containers. Docker drop guards enqueue removal
onto a sandbox-owned cleanup supervisor because `Drop` cannot await; runtime
shutdown drains that supervisor before the process sweep. Every command it runs
still uses `ManagedChild`, so abandoning a task or future cannot abandon its
process tree.

`scripts/check-managed-process-spawns.sh` is a CI guard: raw `.spawn()` and
`kill_on_drop` calls outside this crate fail the build. Fully-awaited leaf
commands that use `output()`/`status()` remain local to leaf crates such as
`baybo-workspace`; they do not expose a live child and cannot lose its handle.

## Constraints

- Unix only: every child is its own process-group leader.
- Never identify ownership by pid/pgid alone; crash recovery must validate the
  injected token.
- Never return the inner Tokio/std child or expose a way to disarm the guard.
- Required managers are constructor dependencies; production code must not
  create per-call managers for long-lived runtime children.
- Ledger writes are best-effort and contain only pgid, a diagnostic label, and
  the random ownership token—never argv, environment values, or secrets.

## Collaboration

| Module | Role |
|---|---|
| `bootstrap` | creates the durable manager and owns force-exit fallback |
| `tools` | Bash, rg, and stdio MCP child ownership |
| `sandbox` | bwrap/sandbox-exec/docker CLI ownership; Docker retains container cleanup |
| `deck` | bun services, host exec, git, and tmux helper ownership |
| `agent` | Claude/Codex CLI ownership |
| `gateway` | channel-sidecar ownership and token revocation |
| `setup` / `cli` | transient ownership for one-shot probes and registration |
