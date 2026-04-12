# CLI Write-Mutating Commands

> **Archived 2026-04-12.** Phase 2b shipped for every subsystem that had a
> backing API. Two items landed with a deferred tail and moved to their
> own follow-ups: `agent send` argv mode in `docs/todo/cli-agent-send-argv.md`,
> and `sandbox list/info` in `docs/todo/cli-sandbox-registry.md`.
> Kept here for history; do not reopen.

## Problem

`docs/modules/cli.md` defines a full command taxonomy, but Phase 2 only shipped the **read-only** families: `config show/file/schema/validate`, `skills list/info`, `tools list/info`, `channels list`, `llm status`, `workspace show`, `status`, `doctor`, `completion`.

Every other command listed in the `cli.md` reference table is deferred because the backing manager lacks the public API the CLI would call. The CLI layer deliberately contains no business logic — adding a command without its manager method would either force `aura-cli` to reach into private internals or to print `not implemented`, both of which the design forbids.

## Missing APIs

One row per subsystem. Signatures are indicative; the real shape is whatever keeps `&self` / `Arc<Self>` honest and fits the manager's existing error type.

| Subsystem              | Missing                                                                                                                                                                                                                                              | CLI that unlocks                                                      |
| ---------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| ~~`SessionManager`~~   | ~~`list()`, `history(id)`, `delete(id)`~~ — shipped                                                                                                                                                                                                  | ~~`session list/show/history/kill`~~ — shipped                        |
| ~~`JobManager`~~       | ~~`list(Option<JobStatus>)`, `get(id)`, `cancel(id)`~~ — shipped                                                                                                                                                                                     | ~~`job list/show/cancel`~~ — shipped                                  |
| ~~`TraceCollector`~~   | ~~`list/get/export/snapshot`~~ — shipped. `list/show/export` go through `TraceStore::{query_traces, load_trace}`; `snapshot` is the read-only ancestor lookup via `aura_trace::snapshot::find_nearest_snapshot`. Live-snapshot capture (writing a fresh `context_snapshot` mid-session) still needs session context.                                     | ~~`trace list/show/export/snapshot`~~ — shipped (stored lookup); live capture deferred |
| ~~`CronScheduler`~~    | ~~`get_job(id)`, `trigger_now(id)`, `list_all_jobs()`~~ — shipped                                                                                                                                                                                    | ~~`cron list/show/rm/enable/disable/run/runs`~~ — shipped             |
| ~~`MemoryManager`~~    | ~~`list(user_id)`, `search(user_id, query)`, `set_importance(id, importance)`, `delete_for_session(id)`~~ — shipped                                                                                                                                  | ~~`memory list/search/show/promote/clear`~~ — shipped                 |
| ~~`WorkspaceManager`~~ | ~~`write_identity_file(name, content)`~~ — shipped (tmpfile + atomic rename; strong `IdentityKind` enum)                                                                                                                                             | ~~`workspace set-identity`~~ — shipped                                |
| ~~`SecurityGateway`~~  | ~~`audit()` returning a structured `SecurityAuditReport { rules, vault }`~~ — shipped                                                                                                                                                                | ~~`security audit`~~ — shipped                                        |
| ~~`LeakDetector`~~     | ~~`check_file(&Path)` + `rules()` accessor~~ — shipped (detector shared as `Arc` so CLI and gateway reuse the same rule set)                                                                                                                         | ~~`security leaks check <file>`~~ — shipped                           |
| `sandbox` crate        | No registry at all — today `SandboxPolicy` is an enum, not an enumerable runtime. Design + implementation tracked separately in `cli-sandbox-registry.md`.                                                                                           | `sandbox list/info` — see `cli-sandbox-registry.md`                   |
| ~~`LlmProviderRegistry`~~ | ~~`list_models()`~~ — shipped (per-provider static catalog advertised by `LlmProviderFactory::known_models`)                                                                                                                                     | ~~`llm models`~~ — shipped                                            |
| ~~`LlmClient`~~        | ~~`probe()` (minimal request using current config to verify connectivity + auth)~~ — shipped; returns `ProbeReport { provider, model, latency_ms, tokens }`                                                                                          | ~~`llm probe`~~ — shipped; `doctor` can pick it up                    |
| ~~`ToolExecutor`~~     | ~~`test_execute(name, args)` that runs a tool outside an agent turn and records the attempt in trace~~ — shipped                                                                                                                                     | ~~`tools test`~~ — shipped                                            |
| ~~`SkillRegistry`~~    | ~~`search(query)`, `validate_all()`~~ — shipped (substring search over name/description/command trigger; validation checks required binaries on `$PATH`, required env vars, declarative shape)                                                       | ~~`skills search/check`~~ — shipped                                   |
| `Router` / `AgentLoop` | `send_message(session_id, blocks)` for a one-shot agent turn (must remain disabled inside slash mode — see `cli.md` §"Explicit mutation confirmation in slash mode" and `AgentSendForbiddenInSlash`). Design + implementation tracked separately in `cli-agent-send-argv.md`. | `agent send` — grammar + slash guard shipped; argv path deferred, see `cli-agent-send-argv.md` |
| ~~`AuraConfig`~~       | ~~`set_at_path(path, value)`, `unset_at_path(path)`, and an async `write_to_file`~~ — shipped. Hot-reload still tracked in `docs/todo/config-hot-reload.md`; today `config set/unset` persists on-disk and the running process requires restart to observe the change.                                                                                                    | ~~`config get/set/unset`~~ — shipped                                  |

## Proposed Direction

Land one subsystem at a time, smallest blast radius first. Each step is a PR that (a) adds the missing manager methods with unit tests, (b) wires the CLI subcommand in `crates/cli/src/commands/<family>.rs`, (c) adds a parser test in `crates/cli/tests/parser.rs` and a dispatch smoke test in `crates/cli/tests/dispatch_smoke.rs`.

Suggested order:

1. ~~**`session`** — smallest surface (list/get/history/delete), no external state. Good warm-up for the pattern.~~ — shipped
2. ~~**`job`** — similar shape; unblocks observability workflows (`job list --status=failed`, `job cancel <id>`).~~ — shipped
3. ~~**`cron`** — the user-facing scheduling surface already exists (`list_jobs/create/enable/disable/delete`); the gaps are small. `list` needs to accept "all users" semantics for operator mode.~~ — shipped
4. ~~**`memory`** — requires defining `MemoryEntry` identity (id vs session+hash) before list/search can return stable handles.~~ — shipped (identity resolved by reusing the existing UUID `MemoryEntry.id`)
5. ~~**`trace`** — touches file I/O for `export`; snapshot is a write through an existing hook.~~ — shipped for list/show/export/snapshot. `snapshot` is the read-only ancestor lookup (walks `find_nearest_snapshot` over a trace loaded from `TraceStore::load_trace`; supports `--node` and `--full`). The *live* snapshot capture path — writing a new `context_snapshot` mid-session — still needs the per-session live context the CLI does not hold.
6. ~~**`config set/unset`** — requires a JSON-pointer-style path setter on `AuraConfig`. Coordinate with `docs/todo/config-hot-reload.md` so the reload path can consume whatever shape `set` produces.~~ — shipped (dotted or JSON-pointer path, round-trips through `serde_json::Value`; `write_to_file` is a tmpfile-and-rename). Hot-reload still pending.
7. ~~**`tools test`** — has to route through `ToolExecutor` so trace + cost records fire. Slash mode must require `--yes`.~~ — shipped (synthetic `cli-test-<uuid>` session id; routes through `ObservabilityRecorder` so the attempt shows up in trace/job/cost like a real call).
8. ~~**`llm models/probe`** — needed by `doctor`; probe is a minimal chat request.~~ — shipped. Catalog comes from `LlmProviderFactory::known_models`; `probe()` runs one "ping" user-turn and reports `{provider, model, latency_ms, tokens}`. `doctor` can now wire `llm probe` in as the auth/connectivity check it was always supposed to have.
9. ~~**`workspace set-identity`** — small; fits after `workspace show`.~~ — shipped (`--file` or `--content`, mutually exclusive; atomic write; change is picked up after restart).
10. ~~**`security audit` / `leaks check`**, **`skills search/check`**~~ — shipped. `audit` reports rule count by action + vault master-key flag (never secret material). `leaks check` reads a file on disk and reports blocked/hits via the shared `LeakDetector`. `skills search` substring-matches name/description/command trigger; `skills check` validates `required_bins` on `$PATH`, `required_env` in env, and declarative shape.
11. **`agent send` (argv mode)** — grammar + slash guard shipped; real argv wiring deferred. See [`cli-agent-send-argv.md`](cli-agent-send-argv.md) for the Router one-shot / daemon-RPC design work this is waiting on.
12. **`sandbox list/info`** — deferred. See [`cli-sandbox-registry.md`](cli-sandbox-registry.md) for the registry + enforcement + config-surface design work this is waiting on.

## Constraints (carried from `cli.md`)

- Every new command **must** carry both a parser test and a dispatch smoke test. The existing 14 parser / 11 dispatch tests are the template.
- Slash-mode invocations of mutating commands require `--yes`; without it, return `CliError::ConfirmationRequired` with a description of what would have happened.
- Mutation responses include an auditable handle (job id, trace span id, row id) so the operator can follow up.
- Commands route through managers — never touch a `Store` directly. Trace and hook events fire naturally as a result.
- No `unwrap` / `expect` in the handler layer.

## Related

- `docs/modules/cli.md` — command taxonomy and mutation rules; the spec this work fills in.
- `docs/todo/cli-agent-send-argv.md` — design work for `agent send` argv mode.
- `docs/todo/cli-sandbox-registry.md` — design work for `sandbox list/info`.
- `docs/todo/config-hot-reload.md` — coordinates with `config set/unset`.
- `crates/cli/src/commands/` — extension point; each new family adds one file here.
- `crates/cli/tests/` — test extension points (`parser.rs`, `dispatch_smoke.rs`).
