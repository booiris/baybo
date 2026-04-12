# CLI Write-Mutating Commands

## Problem

`docs/modules/cli.md` defines a full command taxonomy, but Phase 2 only shipped the **read-only** families: `config show/file/schema/validate`, `skills list/info`, `tools list/info`, `channels list`, `llm status`, `workspace show`, `status`, `doctor`, `completion`.

Every other command listed in the `cli.md` reference table is deferred because the backing manager lacks the public API the CLI would call. The CLI layer deliberately contains no business logic — adding a command without its manager method would either force `aura-cli` to reach into private internals or to print `not implemented`, both of which the design forbids.

## Missing APIs

One row per subsystem. Signatures are indicative; the real shape is whatever keeps `&self` / `Arc<Self>` honest and fits the manager's existing error type.

| Subsystem              | Missing                                                                                                                                                                                                                                              | CLI that unlocks                                                      |
| ---------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| ~~`SessionManager`~~   | ~~`list()`, `history(id)`, `delete(id)`~~ — shipped                                                                                                                                                                                                  | ~~`session list/show/history/kill`~~ — shipped                        |
| ~~`JobManager`~~       | ~~`list(Option<JobStatus>)`, `get(id)`, `cancel(id)`~~ — shipped                                                                                                                                                                                     | ~~`job list/show/cancel`~~ — shipped                                  |
| ~~`TraceCollector`~~   | ~~`list(filter)`, `get(session_id)`, `export(session_id)`~~ — shipped via `TraceStore::{query_traces, load_trace}`. `snapshot(session_id)` still deferred (needs live session context).                                                              | ~~`trace list/show/export`~~ — shipped; `trace snapshot` deferred     |
| ~~`CronScheduler`~~    | ~~`get_job(id)`, `trigger_now(id)`, `list_all_jobs()`~~ — shipped                                                                                                                                                                                    | ~~`cron list/show/rm/enable/disable/run/runs`~~ — shipped             |
| ~~`MemoryManager`~~    | ~~`list(user_id)`, `search(user_id, query)`, `set_importance(id, importance)`, `delete_for_session(id)`~~ — shipped                                                                                                                                  | ~~`memory list/search/show/promote/clear`~~ — shipped                 |
| `WorkspaceManager`     | `write_identity_file(name, content)`                                                                                                                                                                                                                 | `workspace set-identity`                                              |
| `SecurityGateway`      | `audit()` returning a structured report                                                                                                                                                                                                              | `security audit`                                                      |
| `LeakDetector`         | `check_file(&Path)` wrapper (today only `scan_text` exists)                                                                                                                                                                                          | `security leaks check <file>`                                         |
| `sandbox` crate        | No registry at all — today `SandboxPolicy` is an enum, not an enumerable runtime. Needs a `SandboxRegistry` (or equivalent) that can list active policies.                                                                                           | `sandbox list/info`                                                   |
| `LlmProviderRegistry`  | `list_models()`                                                                                                                                                                                                                                      | `llm models`                                                          |
| `LlmClient`            | `probe()` (minimal request using current config to verify connectivity + auth)                                                                                                                                                                       | `llm probe`; feeds `doctor`                                           |
| `ToolExecutor`         | `test_execute(name, args)` that runs a tool outside an agent turn and records the attempt in trace                                                                                                                                                   | `tools test`                                                          |
| `SkillRegistry`        | `search(query)`, `validate_all()`                                                                                                                                                                                                                    | `skills search/check`                                                 |
| `Router` / `AgentLoop` | `send_message(session_id, blocks)` for a one-shot agent turn (must remain disabled inside slash mode — see `cli.md` §"Explicit mutation confirmation in slash mode" and `AgentSendForbiddenInSlash`)                                                 | `agent send`                                                          |
| `AuraConfig`           | `set_at_path(path, value)`, `unset_at_path(path)`, and an async `write_to_file`. Hot-reload is a separate concern tracked in `docs/todo/config-hot-reload.md`; `config set/unset` only needs the on-disk mutation — restart picks up the new values. | `config get/set/unset`                                                |

## Proposed Direction

Land one subsystem at a time, smallest blast radius first. Each step is a PR that (a) adds the missing manager methods with unit tests, (b) wires the CLI subcommand in `crates/cli/src/commands/<family>.rs`, (c) adds a parser test in `crates/cli/tests/parser.rs` and a dispatch smoke test in `crates/cli/tests/dispatch_smoke.rs`.

Suggested order:

1. ~~**`session`** — smallest surface (list/get/history/delete), no external state. Good warm-up for the pattern.~~ — shipped
2. ~~**`job`** — similar shape; unblocks observability workflows (`job list --status=failed`, `job cancel <id>`).~~ — shipped
3. ~~**`cron`** — the user-facing scheduling surface already exists (`list_jobs/create/enable/disable/delete`); the gaps are small. `list` needs to accept "all users" semantics for operator mode.~~ — shipped
4. ~~**`memory`** — requires defining `MemoryEntry` identity (id vs session+hash) before list/search can return stable handles.~~ — shipped (identity resolved by reusing the existing UUID `MemoryEntry.id`)
5. ~~**`trace`** — touches file I/O for `export`; snapshot is a write through an existing hook.~~ — shipped for list/show/export (reuses `TraceStore::query_traces` + `load_trace`). `snapshot` still deferred — needs per-session live context the CLI does not hold.
6. **`config set/unset`** — requires a JSON-pointer-style path setter on `AuraConfig`. Coordinate with `docs/todo/config-hot-reload.md` so the reload path can consume whatever shape `set` produces.
7. **`tools test`** — has to route through `ToolExecutor` so trace + cost records fire. Slash mode must require `--yes`.
8. **`llm models/probe`** — needed by `doctor`; probe is a minimal chat request.
9. **`workspace set-identity`** — small; fits after `workspace show`.
10. **`security audit` / `leaks check`**, **`skills search/check`** — low priority; nice-to-have.
11. **`agent send`** — depends on Router's life cycle (the one-shot call must share the chat loop's spawner). Do this after everything else so the `AgentSendForbiddenInSlash` rule has the most mature slash dispatcher.
12. **`sandbox list/info`** — blocked on the sandbox crate gaining a registry at all. Revisit when the sandbox enforcement path is wired into `ToolExecutor`.

## Constraints (carried from `cli.md`)

- Every new command **must** carry both a parser test and a dispatch smoke test. The existing 14 parser / 11 dispatch tests are the template.
- Slash-mode invocations of mutating commands require `--yes`; without it, return `CliError::ConfirmationRequired` with a description of what would have happened.
- Mutation responses include an auditable handle (job id, trace span id, row id) so the operator can follow up.
- Commands route through managers — never touch a `Store` directly. Trace and hook events fire naturally as a result.
- No `unwrap` / `expect` in the handler layer.

## Related

- `docs/modules/cli.md` — command taxonomy and mutation rules; the spec this work fills in.
- `docs/todo/config-hot-reload.md` — coordinates with `config set/unset`.
- `crates/cli/src/commands/` — extension point; each new family adds one file here.
- `crates/cli/tests/` — test extension points (`parser.rs`, `dispatch_smoke.rs`).
