# workspace - Workspace Addresses and Long-Running Configuration

## Overview

The `workspace` crate is the single source of truth for Baybo's workspace layout. It owns:

- **Filesystem addresses** (`paths` module, always available): `WorkspacePaths`, `IdentityKind`, the `&str` constants for the workspace-relative file/dir names (`config/`, `personas/`, `skills/` — always inside a persona directory — `agents/`, `.key/`, `state/`, `work/`, `logs/`, `baybo.json`, `.mcp.json`, `encryption.key`, `storage.db`, `baybo.lock`, `channel.port`, `SOUL.md` / `USER.md` / `IDENTITY.md`, `.uv/`, …), the `ENV_CONFIG_PATH` constant (whose value is the env-var name `BAYBO_CONFIG_PATH`), and the `default_workspace_root` / `default_config_file` / `baybo_cache_root` resolvers.

- **Skeleton materialisation** (`layout` module, `io` feature): `ensure_layout`, `seed_default_identity_files` — the two boot-time writes that turn a bare root into a usable workspace.
- **Identity I/O** (`identity` module, `io` feature, default-on): `IdentityFiles`, `IdentitySource`, `load_identity`, `load_identity_files`, `ensure_persona_layout` — the async readers backing the identity documents, seeding any that is missing.
- **Memory addresses + seeding** (`memory` module, `io` feature): `load_memory_index` — a persona directory also holds that agent's `memory/` tree, one markdown file per remembered fact plus the `MEMORY.md` index that rides the system prompt. This crate owns where it lives and how an absent index is seeded; everything else about it is [`memory-builtin.md`](memory-builtin.md).
- **Default identity templates** (`prompt` module, always available): the `DEFAULT_SOUL_CONTENT` / `DEFAULT_USER_CONTENT` / `DEFAULT_IDENTITY_CONTENT` seed strings that `IdentityKind::default_content` returns when `seed_default_identity_files` writes a missing `SOUL.md` / `USER.md` / `IDENTITY.md`.
- **Workspace exclusion** (`singleton` module, `io` feature): `WorkspaceLock` and `acquire_workspace_lock` — the advisory `flock` on `state/baybo.lock` that keeps two chat loops off one workspace. It lives here rather than in the binary because the path constant does, and because `baybo vault rotate` needs the same lock: `key_file::rotate` takes a `&WorkspaceLock` so holding it is a type-level obligation rather than something a caller remembers.
- **Tree measurement** (`walk` module, always available, std-only): `tree_stats` — the one sync walker behind the janitor's `work/tmp` "newest in-tree mtime" staleness gate (called via `spawn_blocking`). Summed lstat file sizes + newest lstat mtime anywhere in the tree; symlinks are measured as links and never followed. The mtime back-dating fixtures for testing against it live in `test_support` behind the `test-support` feature.

Pure-data consumers (e.g. `baybo-config`, `baybo-tools`) take this crate with `default-features = false` so they never inherit a transitive `tokio`/`anyhow` dependency just to read a path constant. Crates that actually drive workspace I/O (`baybo-agent`, `baybo-cli`, `baybo-gateway`, the binary) depend on it with `features = ["io"]`.

## Layout

The workspace root is the single **project root** for the entire runtime: every subsystem that needs a persistent path derives its location from it. The root is divided into eight top-level subdirectories that describe what kind of content lives there.

```text
<workspace_root>/
  config/          # standalone git repo: baybo.json, .mcp.json
  agents/          # standalone git repo: subagent profile definitions
  personas/        # standalone git repo: global personas plus project/<agent_id>/
                   # project personas; each has identity + skills + memory
                   # alongside the shared USER.md
  .key/            # not version-controlled: encryption.key (mode 0600)
  state/           # not version-controlled: storage.db, baybo.lock, channel.port, browser/profile
  work/            # not version-controlled: .uv/ (uv cache + downloaded pythons + tools), .fonts/, .baybo-tool-spills/, tmp/ (disposable scratch, swept), agent scratch
  logs/            # not version-controlled: baybo.log.<date>, channel/<type>.log.<date> (sessions/<id>.jsonl is a virtual path, never written)
```

| Field            | Default | Role                                        |
| ---------------- | ------- | ------------------------------------------- |
| `workspace.path` | `default_workspace_root()` | Project root directory (validated non-empty and absolute)|

Defaults: `~/.baybo` in release builds, `./.baybo` in debug builds. The
debug default keeps `cargo run` self-contained inside the project
checkout rather than polluting the real user home.

| Subsystem        | Path                                       |
| ---------------- | ------------------------------------------ |
| config           | `<workspace.path>/config/baybo.json`        |
| MCP servers      | `<workspace.path>/config/.mcp.json`        |
| global/legacy agent skills | `<workspace.path>/personas/<agent_id>/skills/` |
| global/legacy agent identity files | `<workspace.path>/personas/<agent_id>/{SOUL,IDENTITY,USER}.md` |
| newly created project agent | `<workspace.path>/personas/project/project-<ULID>/…` |
| built-in's identity files | `<workspace.path>/personas/baybo/…` — it is an ordinary persona dir |
| shared user profile | `<workspace.path>/personas/USER.md` (owned by no agent) |
| encryption key   | `<workspace.path>/.key/encryption.key`     |
| storage          | `<workspace.path>/state/storage.db`        |
| singleton lock   | `<workspace.path>/state/baybo.lock`         |
| channel port     | `<workspace.path>/state/channel.port`      |
| browser profile  | `<workspace.path>/state/browser/profile/`  |
| uv state         | `<workspace.path>/work/.uv/{cache,python,tools,bin}/` |
| browser fonts    | `<workspace.path>/work/.fonts/`            |
| tool-output spills | `<workspace.path>/work/.baybo-tool-spills/` |
| disposable scratch | `<workspace.path>/work/tmp/` (`WORK_TMP_SUBDIR`; janitor-swept, see below) |
| gateway logs     | `<workspace.path>/logs/baybo.log.<date>`    |
| channel logs     | `<workspace.path>/logs/channel/<channel_type>.log.<date>` |
| session transcript (virtual) | `<workspace.path>/logs/sessions/<session_id>.jsonl` (no file; the compaction recovery pointer, served from `session_messages` on read). `…/<session_id>@<ordinal>.jsonl` serves the same transcript from that message on — `@` is a character `sanitize_session_id` rewrites, so it cannot occur in the id half |

New subsystem files belong as a method on `WorkspacePaths`, not as another `workspace_root.join("…")` call site.

## Initialization and version control

`ensure_layout` runs at every boot (gateway start, TUI, argv subcommands once `boot::load_config` returns) and is idempotent:

- Creates `config/`, `agents/`, `personas/`, `personas/baybo/skills/`, `.key/`, `state/`, `work/`, `work/tmp/`, `logs/` if missing. The built-in's skill directory is the one persona-internal path created here rather than by `ensure_persona_layout` — every other agent is DB state, but the built-in's id is a constant, so its folder is layout.
- Runs `git init --quiet` inside `config/`, `agents/`, and `personas/` if the directory isn't already a git repo (`<dir>/.git` check). Skill directories get no repo of their own — they live inside `personas/`, which already is one, and a nested `.git` there would give git two answers to which repo owns a file.

Per-agent subdirectories are created on demand by `ensure_persona_layout` (at
profile creation, and defensively when a bound session's actor is built), not
by `ensure_layout` — the set of agents is DB-state, not layout. Global agents
remain direct children of `personas/`; project leads and teammates created by
the project manager receive a `project-<ULID>` id and are grouped under
`personas/project/`. That prefix lets every id-only path lookup select the
project tree without filesystem I/O. A flat project persona created by an older
build keeps its unprefixed id, stays where it is, and remains readable; no
background migration moves user-authored identity or memory files.

`config/`, `personas/`, and `agents/` are each their own standalone git repo. The workspace root itself is **not** version-controlled — there is no top-level `.gitignore`, and `.key/`, `state/`, `work/`, `logs/` simply live next to the three declarative dirs without needing an ignore list to keep them out of any tree above them. Users who want to back up or sync their config commit inside `config/`; identity edits and skills both commit inside `personas/`, which is the repo that spans the whole declarative agent surface — every agent's identity, memory, and skills; subagent profiles commit inside `agents/`. **Never** commit anything from `.key/` — `baybo setup` mints the master encryption key there with mode 0600, and treating that file as version-controllable would leak every secret in the vault.

## Config file resolution

The `ENV_CONFIG_PATH` constant holds the env-var name `BAYBO_CONFIG_PATH`; setting that env var overrides everything. When unset, the loader falls back to `default_config_file()` = `<default_workspace_root>/config/baybo.json`. Missing-file behaviour:

- explicit `BAYBO_CONFIG_PATH` pointing at a non-existent file → hard error.
- default path absent → silently fall back to `BayboConfig::default()` and log the resolved path that was checked.

## Design Decisions

### Identity file responsibilities

- **SOUL.md**: personality, tone, and preferences
- **USER.md**: long-term user profile
- **IDENTITY.md**: system or instance identity description

**All three identity files are per-agent, and the built-in is an ordinary
persona directory** (`personas/baybo/`). Global personas and legacy project
personas use `personas/<id>/`; newly created project personas use
`personas/project/project-<ULID>/`. In either layout the leaf directory owns
one agent, while `personas/USER.md` is the shared human profile that belongs to
none of them.

`SOUL.md` (personality) and `IDENTITY.md` (self-image: name, creature, vibe,
emoji, avatar) answer "who is this assistant". `USER.md` is the agent's **own
notes** about the human, per-agent for the same reason memory is partitioned:
one agent's accumulated read on the user is not another's, and sharing the file
would be a write channel between agents that the partition does not cover.
Empirically it is also the only one agents actually maintain.

Project-agent SOUL seeds are deliberately short: they hold only durable role,
board, reporting, and safety rules. Current-card state, wake reasons, brief
semantics, and tool-selection guidance belong to runtime framing, where they
can stay accurate without permanently taxing every turn or overwriting an
agent's editable identity.

The stable facts the operator curates live in `personas/USER.md`, which every
agent reads as a separate `<shared_user_profile>` section alongside its own
`<user_notes>`.

**`personas/` is committed as it is written.** `ensure_persona_layout` ends by
committing the directory it just materialised, and `seed_default_identity_files`
does the same for the shipped defaults — both as `Baybo <baybo@local>`, both
no-ops when there is nothing staged. Without that, files entered git only when
the `Edit` tool first rewrote one, so the agent's *first* change to its own
soul landed as a file addition: the one thing the audit history exists to show
was the one thing it could not. Best-effort — a workspace without `git` still
gets correct files, just no history.

A session with no binding resolves to `personas/baybo/`, the same directory
a built-in-bound session resolves to, so the two assemble byte-identical
prompts. `load_identity_files` takes an `IdentitySource` (path + seed) for each
of the three agent-owned files and resolves the shared `USER.md` itself;
`load_identity` reads one on its own. See
[`../todo/multi-agent-chat.md`](../todo/multi-agent-chat.md).

Identity file changes usually affect the system prompt; memory changes usually affect recall.

### Boundary with memory

Two different things are called memory, and only one of them is a boundary:

- The **pluggable `Memory` trait** ([`memory.md`](memory.md)) — an external,
  retrievable store (mem0, OpenViking). That one genuinely does not overlap:
  this crate holds files, that one holds recall.
- The **file-based memory tree** ([`memory-builtin.md`](memory-builtin.md)) —
  `<persona>/memory/`, which lives *inside* this crate's layout and is
  addressed and seeded by it, exactly like the identity files. The split
  there is not workspace-vs-memory but always-loaded-vs-loaded-on-demand:
  identity files ride every prompt in full, memory files cost nothing until
  read.

### Scratch hygiene: `work/tmp`

`work/` is the agent's only writable surface — every chat, cron fire, and
subagent shares it flat, and nothing in it is deleted implicitly, so it
accumulates. `work/tmp/` is the disposable-scratch convention that keeps
the growth bounded: `WORK_TMP_SUBDIR` + `work_tmp_dir()` name it,
`ensure_layout` creates it, and the Bash tool description tells the model
to put intermediate files there while keeping user-facing deliverables
elsewhere under `work/`. The janitor removes any `work/tmp` top-level
entry whose newest in-tree mtime is older than `WORK_TMP_TTL_DAYS` (7 —
the const lives here so the sweep and the model-facing prompt quote one
number), measuring staleness with the shared
`baybo_workspace::walk::tree_stats` walker (newest lstat mtime anywhere
in the tree, symlinks never followed); see [`janitor.md`](janitor.md).

### Why split state/work/logs from personas

The split exists so the user's git workflow stays clean: `config/`, `personas/`, and `agents/` are declarative, hand-edited content that belongs in source control; `state/` is mutable runtime state (sqlite DB, locks, ports, browser profile) that would create churn or conflicts if committed; `work/` holds tool-generated scratch (uv caches, downloaded Python toolchains, ad-hoc shell output) that has no long-term value; `logs/` is ephemeral. Each of the three declarative dirs is its own git repo, so the boundary is enforced by repo scope rather than a top-level ignore list — users can never accidentally commit `state/` because no enclosing repo includes it.

One consequence of skills living inside `personas/`: `git checkout .` in that repo is a gesture that reaches installed skills as well as identity files. That is the price of one repo spanning the declarative agent surface, and it is paid knowingly — the alternative, a `.git` nested inside another repo, fails silently and much worse (`git add` records a gitlink with no `.gitmodules`, and a fresh clone restores an empty directory).

## Constraints

- No baybo-* dependencies. The crate is leaf-level: `paths` is pure data, `walk` is std-only, `io` only adds optional `tokio` + `anyhow`, and `test-support` only adds optional `libc` for the symlink back-dating fixture.
- Does not record message-level Trace or Turn data
- Missing identity files should degrade gracefully, not block startup
- Identity file changes should carry a version stamp or content hash for Trace provenance

## Collaboration

| Module | Role |
|--------|------|
| `agent` | Reads identity files for system prompt |
| `skills` | Provides trusted local skill directories |
| `trace` | (planned) identity file version changes recorded in provenance — no version stamp/hash is written today; `workspace` is a leaf crate with no baybo-* deps, so any wiring must run through a consumer (`context` / `agent`), not `workspace` itself |
| `memory` | Complements without overlapping responsibility |
