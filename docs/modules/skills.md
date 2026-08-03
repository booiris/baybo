# skills - Skill System

## Overview

The `skills` crate defines, loads, selects, and hot-reloads declarative skills with governance. It is not just "reading Markdown templates" — it also carries governance responsibilities.

**Tool = atomic operation + isolated execution. Skill = declarative orchestration + governance constraints.**

Core responsibilities:

- Skill definition with source, version, and trust level
- Invocation matching: `/<cmd>` expands the one matching skill; otherwise the model picks from a per-session listing and pulls a skill in via the `Skill` tool
- Constrain which tools a skill may call

## Skill file format

One directory per skill, a `SKILL.md` entrypoint with YAML frontmatter plus a Markdown body.

### Load location

At startup the registry first calls `SkillRegistry::register_builtins()` to register every skill compiled into the cargo `[[bin]]` (`crates/skills/src/builtin/<name>/SKILL.md`, embedded via `include_str!`), then scans `<workspace.path>/skills/<skill-name>/SKILL.md` and overlays any workspace skill of the same name on top. Built-ins are `ArtifactSource::Inline` + `TrustLevel::Trusted`; an operator can patch shipped behaviour by dropping a same-named directory under the workspace.

The first built-in is `baybo-cli` — a non-user-invocable skill that tells the agent to introspect the running Baybo instance through the `baybo` CLI (the BashTool auto-injects `BAYBO_HELP_AGENT` and `BAYBO_CONFIG_PATH`, so the agent sees the full inventory and the right config without needing flags). The second is `deck` — the inverse shape: slash-only (`command: deck` + `disable-model-invocation: true`, so the model never auto-selects it; the user types `/deck <request>`) and owner-channel-only (`channels: [owner]`) — carrying the deck card bundle contract, the `ctx`/`deck` SDK surface, and worked examples for authoring a card before `DeckCardCreate`/`DeckCardUpdate`; see [`deck.md`](deck.md#authoring-pipeline). The third is `html-gen`: an agent- and slash-invocable, owner-only skill for authoring a self-contained HTML page, staging it through `PutBlob`, and returning the `baybo-html` blob marker understood by the iOS transcript.

```
<workspace>/skills/
├── greet/
│   └── SKILL.md
└── deploy/
    ├── SKILL.md
    └── scripts/…    # supporting files (not auto-loaded; referenced from the body)
```

A minimal `SKILL.md`:

```markdown
---
name: greet
description: Greet the user by name with a friendly tone.
when_to_use: User opens with a greeting or introduces themselves.
allowed-tools: Read Grep
---

Greet the user warmly and respond concisely.
```

### Frontmatter fields

All fields are optional. When omitted, `name` falls back to the directory name.

| Field                      | Type                      | Default    | Effect |
|----------------------------|---------------------------|------------|--------|
| `name`                     | scalar                    | dir name   | Skill identifier; also the default `/<name>` slash command. |
| `description`              | scalar                    | `""`       | Used by the model for auto-selection. |
| `when_to_use`              | scalar                    | `None`     | Appended to `description` in the skill listing. |
| `allowed-tools`            | list or space-sep string  | `[]`       | Tool allow-list while the skill is active. |
| `command`                  | scalar                    | `name`     | Slash-command override (`command: deck` → `/deck`). Same grammar as names; rejected with `user-invocable: false`. |
| `disable-model-invocation` | bool                      | `false`    | `true` clears `agent_invocable` — only the slash command remains. |
| `user-invocable`           | bool                      | `true`     | `false` clears `command` — only agent decision remains. |
| `channels`                 | list or space-sep string  | `[]`       | Channel restriction (empty = all). Off-channel sessions get no listing, no slash expansion, and a `Skill`-tool refusal — e.g. `channels: [owner]` on `deck`. |
| `argument-hint`            | scalar                    | `None`     | Autocomplete hint (e.g. `[issue-number]`). |
| `version`                  | scalar                    | `"0.0.0"`  | Recorded in trace provenance. Must match `[a-zA-Z0-9._\-+~]{1,32}` — whitespace, quotes, and angle brackets are rejected so a hostile manifest can't break out of the `<skill version="…">` attribute. |

Skill names must match `[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}` — the same characters permitted in the `/<name>` slash command. Display strings with spaces or slashes are rejected at load time.

When a skill definition is re-broadcast after context compaction (the skill trailer in `baybo-context`), the prompt body is wrapped in a `<skill name="…" version="…">…</skill>` block assembled by `render::render_skill_block`. Escaping is applied lazily at render time — the in-memory `SkillDefinition` keeps the author's original text for CLI display, while the rendered block runs every field through XML-attribute / tag-breakout escapes. Every `<skill` / `</skill` occurrence inside the body (case-insensitive, tolerant of whitespace and null bytes) has its leading `<` replaced with `&lt;`. Together with the restricted name/version grammars this closes the tag-forging class of attacks end-to-end.

Unsupported YAML features (anchors, folded/literal block scalars, nested mappings) are **rejected** rather than silently mis-parsed — keeps the author's intent honest and the parser small.

The Markdown body becomes `prompt_template`; it reaches the model as the `Skill` tool's JSON result, or as a hidden agent-context row when invoked via `/<name>`.

## Design Decisions

### Dual invocation model

Every skill exposes two independent entry points; both default on:

- `command: Option<String>` — explicit `/name` slash command (frontmatter `name`, or the `command:` override, unless `user-invocable: false`)
- `agent_invocable: bool` — the model may auto-select based on `description` (unless `disable-model-invocation: true`)

A `/deploy` skill that's too dangerous to auto-trigger sets `disable-model-invocation: true`; a `legacy-system-context` reference that isn't actionable as a command sets `user-invocable: false`. The slash-only combination is real and load-bearing — the builtin `deck` is `command: deck` + `disable-model-invocation: true`, so a card is only ever authored when the user explicitly types `/deck` — which is why the slash-candidate set in `baybo-context` (`slash_skill_summaries`: `command.is_some() && !Untrusted && channel-admitted`) is deliberately independent of the model-advertised set (`invocable_skill_summaries`, which also requires `agent_invocable`). A regex-based pattern trigger is **not** modelled — use `description` plus model decision instead.

The `/<name>` entry point is surfaced on channel adapters by `baybo-cli`'s `CliSlashHandler`: `commands()` lists every skill with `command.is_some()` so TUI autocomplete shows them alongside built-ins, and `handle()` returns `PassThrough` for `/<skill>` so the raw line reaches the agent and `ContextManager::expand_slash_command` matches the leading `/<cmd>` against the invocable skill set and injects the skill body. See [`cli.md`](./cli.md#skill-shortcut) and [`tui.md`](./tui.md#slash-completion) for the full wiring.

### Per-agent overlays

**A custom agent does not inherit the shared set.** The built-in's skills
*are* that set (builtins + `<workspace>/skills/`), and an unbound session is
the built-in; a custom agent starts from nothing but its own overlay at
`<workspace>/personas/<agent_id>/skills/`, same one-directory-per-skill shape.
A persona someone curated should not silently acquire every skill the
workspace happens to hold — granting one is a decision, so it is made by
putting the skill in that agent's folder.

The one exception is `UNIVERSAL_SKILLS`, currently just `baybo-cli`: it tells
the agent how to introspect the instance it is running inside (the Bash tool
injects `BAYBO_HELP_AGENT` / `BAYBO_CONFIG_PATH` for exactly that), so it is
runtime infrastructure rather than a capability anyone chose to grant.
Withholding it would not make a persona narrower, only blinder. `deck` is
deliberately *not* in the list — an authoring tool is a capability.

`SkillRegistry` holds overlays in a second map keyed by profile id, and every
session-scoped read goes through the scoped pair:

- `get_scoped(agent, name)` — the agent's overlay first, then the shared set.
- `summaries_for(agent)` — the shared set for the built-in; for a custom
  agent, its overlay ∪ `UNIVERSAL_SKILLS`, **overlay winning a name
  collision**; sorted by name so ordering is stable across turns. `ContextManager` routes the per-turn listing, the
  post-compaction trailer, and slash candidates through it.
- `ensure_agent_overlay(agent, paths)` — **the only thing that fills that
  map**, so every reader of an agent's scope calls it first: the actor build
  (`runtime.rs`, on the cold spawn of a bound session) and `GET /v1/skills`
  when the query names an agent. It derives `personas/<id>/skills/` from the
  id, so a call site holding only a session's agent needs nothing else;
  the built-in and an already-scanned agent are both no-ops. `reload()`
  replays overlays after builtins and the shared dirs.

Loading is lazy rather than part of boot because the set of agents is DB
state: `ensure_layout` cannot enumerate persona folders to scan, and a scan at
profile-creation time would miss every agent that already existed. The cost of
getting this wrong is silent — an unloaded overlay is indistinguishable from an
empty one, and the agent simply has no skills — so the seam is one function
with no separate "is it loaded" question for a caller to forget.

A name that exists only in another agent's overlay — or in a shared set this
agent does not inherit — simply misses, so the `Skill` tool answers "unknown
skill" rather than a refusal that would leak an inventory. One consequence
worth knowing: a custom agent has no `/deck`, because that builtin is a
capability like any other. Governance is unchanged: persona folders are
workspace content, so their skills are `Trusted` and the risk assessor judges
them by content hash like any other. `SkillInstall` / `SkillUninstall` keep
targeting the shared folder — overlays are hand-authored. The built-in
profile has no overlay: its skills *are* the shared set.

### Three-tier trust model

- **Trusted**: workspace or admin-placed skills. May hot-reload and request full tool set.
- **Installed**: registry-installed skills. May auto-match but tool count and capabilities are downgraded.
- **Untrusted**: may only be listed and reviewed, cannot auto-execute.

### Selection pipeline

Skills are no longer auto-injected at user-turn start. Instead the
agent loop seeds a session-start **system reminder** (a
`MessageSource::SkillListing` row appended by
`ContextManager::ensure_seeded`, re-broadcast after
each compaction via the skill trailer) listing every agent-invocable,
non-`Untrusted` skill whose `channels:` restriction (if any) admits the
session's channel (name, description, optional
`argument-hint`); the LLM pulls one in by calling the `Skill` tool
(see [`tools.md`](./tools.md#skill-tool)). The list comes from
`SkillRegistry::all_summaries_sorted()` — a lightweight projection
(`SkillSummary`) carrying only the fields needed for the listing.
Cloning every `SkillDefinition`'s `prompt_template` / `allowed_tools`
/ `requirements` per turn would burn allocator pressure proportional
to skill count × body size; the projection avoids that. Filtered to
`agent_invocable && trust_level != Untrusted && allows_channel`, sorted
by name for stable across-turn ordering. There is deliberately **no**
"registry is empty, skip the projection" guard: that check could only read
the shared map, while the question is scoped, so a custom agent whose skills
live only in its private overlay was advertised nothing at all whenever the
shared set happened to be empty. `SkillRegistry::is_empty()` existed for
exactly that guard and was removed with it — `summaries_for` is already
cheap on an empty registry. The trailer's reminder block advertises this same
filtered set (and is skipped when it is empty); the per-called-skill
`<skill>` detail blocks stay keyed on `called_skills` unfiltered, so a
skill actually invoked in the session keeps its definition across
compaction regardless of flags.

That listing is a **snapshot**, and the registry moves under it —
`SkillInstall`/`SkillUninstall` and the dashboard's refresh all call
`reload()`, so the model itself opens the gap. `ContextManager::reconcile_skills`
closes it: before every main LLM call it re-renders this same filtered set and,
when it differs from the standing `SkillListing` row, appends the difference as
a `<skills_update>` diff. A `-` line means the skill is uninstalled and calling
it will fail — never communicated by absence. See
[`context.md`](./context.md#the-skill-listings-lifecycle) for the baseline,
escaping and compaction rules.

Slash invocations are expanded before the first LLM call by
`ContextManager::expand_slash_command`: when the trailing user message
is `/<cmd> [args]` matching a slash-invocable skill
(`slash_skill_summaries` — commanded, non-untrusted,
channel-admitted; independent of `agent_invocable`), the skill's body
(`render_skill_for_slash`, `{{session_id}}` substituted, plus a
linked-files inventory hint when the skill ships sub-files) is
appended as a hidden agent-context row. Unlike an LLM-issued `Skill`
tool call this deliberately skips the risk assessor — an explicit user
slash command is treated as authorized. Sub-file fetches the model
issues afterwards still go through the gated `Skill` tool.

Slash matching goes through `detect_slash_invocation` in `baybo-context`
and the per-turn list through `all_summaries_sorted`. There is no ranking
stage: no registry method scores or filters by relevance, and none is
declared in anticipation of one.

Downstream gating happens lazily, on call:

- The `Skill` tool runs the assessor (via `Arc<dyn SkillRiskCheck>`) before returning the body. `Dangerous` aborts with `ToolError::Denied`; `Suspicious` continues but adds a `risk_warning` field to the response and emits a notice (when a `SessionNotifier` is wired).
- The tool also enforces `SkillRequirements::required_env`: missing host env vars short-circuit the call with `ToolError::Execution` *before* prompting the user. If every required var is present, an approval gate fires (`ResourceAccess::Env { vars }`); the env *values* are never templated into the response.
- `SkillRegistry::validate_all` still reports unmet `required_bins` / `required_env`; callers that care (e.g. `baybo skills check`) act on that report.
- Trust-level attenuation of the tool ceiling is a design stage not yet wired in.

### Risk assessment

Static governance (trust levels, allow-lists, validator checks) catches structural problems but can't judge *semantic* intent: a skill with clean YAML can still instruct the model to exfiltrate secrets or run destructive commands. The `baybo-skills-assessor` crate adds an LLM-backed second opinion, kept in its own crate so `baybo-skills` stays LLM-free (selection must remain deterministic and offline-capable).

**Mode** (`config.skills.risk_check` → `AssessmentMode`):

- `off` — classifier is skipped; every skill returns `Safe` with `scope = Disabled`.
- `primary` (default) — `SKILL.md` is hashed and judged. Helper scripts are ignored. A missing `SKILL.md` short-circuits to a synthesised Safe verdict.
- `full` — the whole directory tree is hashed and judged. Small trees (≤ 4 files and ≤ 16 KiB) classify synchronously on first use; oversized trees tier automatically — `SKILL.md` is classified synchronously and the full-scope verdict is computed on a background worker, so a chat turn never blocks on a big LLM prompt.

**Flow** (for `primary` / `full`):

```
check(skill)
  └─ hash the in-scope file(s)
  └─ SkillRiskStore::get(name, hash)
        ├─ hit  → return cached verdict
        └─ miss → LLM classifier → put → return verdict
```

The hash is a **metadata fingerprint**, not a content hash — see [skills-assessor.md](skills-assessor.md) for the full rationale and tradeoff. It covers every entry in the hashed scope (`SKILL.md` alone under `primary`, or `SKILL.md` plus all helper files under `full`), so a normal edit bumps mtime and re-triggers the check without us reading file bodies on the hot path. Length-prefixing the rel-path and symlink-target fields closes aliasing hazards across adjacent variable-length fields. Two scope discriminators (`baybo.skill.full:v1` and `baybo.skill.primary:v1`) are mixed into the hasher state so a one-file skill's primary hash and full hash never collide — both scopes can share the `(skill_name, content_hash)` primary key without ambiguity. A 500-file / 100 MiB hard cap rejects pathological trees outright before any hashing work runs.

**Return type** (`AssessedSkill`):

| Field                | Meaning |
|----------------------|---------|
| `verdict`            | The `RiskVerdict` (Safe/Suspicious/Dangerous + rationale). |
| `scope`              | `Disabled` — classifier was skipped. `Primary` — SKILL.md only. `Full` — whole directory. |
| `background_pending` | `true` when `full` mode tiered the skill — a primary verdict is returned now and a full-scope verdict is running on the background worker. `false` otherwise. |

`SkillAssessor::with_background_worker(llm, store, mode)` is the only constructor; the worker it spawns runs full-scope verdicts tiered out by `full` mode and also drains any `skill_risk_assessment_jobs` rows recovered at startup (including any left behind by older builds) so upgrades don't silently abandon in-flight verdicts.

**Verdict shape** (`RiskVerdict`, persisted in `skill_risk_assessments`):

| Field          | Meaning |
|----------------|---------|
| `skill_name`   | Identifier at assessment time. |
| `content_hash` | Primary cache key alongside `skill_name`. |
| `level`        | `Safe` · `Suspicious` · `Dangerous`. |
| `rationale`    | One-to-two sentence justification from the model. Surfaced to the operator; kept so future reviewers don't have to rerun. |
| `model`        | Which LLM produced the verdict. |
| `assessed_at`  | Unix microseconds. |

**Non-blocking error policy**: only `Dangerous` blocks execution. Assessor errors (LLM unreachable, unparseable reply, I/O failure), skills without an on-disk `source_path` (e.g. test fixtures), and the `Suspicious` tier all pass through with a `warn!` log. Availability is preferred over false-positive blocks; the verdict is still surfaced in `baybo skills check` output so a human can review.

**Integration points** (both lazy — no work until the skill is actually invoked):

- **CLI `baybo skills check` / `/skills check`** — runs the validator, then invokes the assessor per skill. Output includes `risk: {status, scope, background_pending, level, rationale, model, content_hash, assessed_at}`.
- **`Skill` tool** — `SkillRiskCheck::assess` returns a `SkillGate` (`Pass` / `PassWithWarning { rationale }` / `Block { rationale }`). `Block` aborts the call with `ToolError::Denied`; `PassWithWarning` returns the body with the rationale embedded as a `risk_warning` JSON field (and emits `Notice { level: Warn }` if a `SessionNotifier` is wired); `Pass` returns silently. Risk is checked once per call, not once per turn — the LLM only pays for assessment of the skill it actually invoked.

The assessor is wired in `crates/baybo/src/runtime.rs` alongside the other shared services using `with_background_worker(llm, store, mode)` where `mode` is read from `config.skills.risk_check`; `recover_pending_jobs` runs once after the skill registry is populated and drains persisted rows regardless of mode — `Off` only suppresses new enqueues. Argv-mode commands that don't open the chat loop leave the assessor `None`, which the CLI surfaces as `status: "not_configured"`.

### Skill installation

The crate ships two governance-gated lifecycle tools (built by `build_install_tool` / `build_uninstall_tool` in `tools.rs`, wired in `crates/baybo/src/runtime.rs` after the assessor is constructed), both declaring `TrustLevel::Trusted` with the `WriteFile` capability scoped to the workspace skills directory:

- **`SkillInstall`** — validates a source directory (must contain a parseable `SKILL.md`, must live outside the workspace skills dir, must not collide with an existing install), runs the risk assessor (`Dangerous` aborts with `ToolError::Denied`), copies the tree to `<workspace>/skills/<name>/` via a temp-dir-and-rename for atomicity, then calls `SkillRegistry::reload()` so the new skill is available next turn.
- **`SkillUninstall`** — looks up the skill by name and refuses unless its canonicalized `source_path` sits under the workspace skills dir (registry-only or third-party-mounted skills aren't deletable), removes the directory, then calls `SkillRegistry::reload()`.

### Hot reload constraints

- Watch only trusted directories
- Validate schema and requirements before accepting changes
- Record name/version/source/hash on version replacement
- On failure, keep the old version rather than emptying the registry

`SkillRegistry` offers `reload()` — re-scans every directory previously
passed to `load_dir` and rebuilds the skill set from disk. Builtins are
replayed first (from the definitions captured by `register_builtins`),
then the dir scans run on top so a same-named workspace skill still
overrides its builtin — without the replay, the first
`SkillInstall`-triggered reload silently dropped every builtin.

**The rebuilt set is assembled in full and then swapped in**, so a reader
observes the complete old set or the complete new one. The maps are a
`parking_lot::RwLock<HashMap<…>>` rather than a `DashMap` for exactly that
reason: the operation that matters is replacing all of it at once, and the
traffic is a handful of reads per turn over a few dozen entries. Reloading
in place — clear, then repopulate over the directory reads — left concurrent
readers seeing an empty registry for as long as the disk took, and that is
not a blip: the skill listing a session seeds from is persisted and is not
refreshed until a compaction, so a session that seeded inside the window
advertised a truncated set for its whole life. The TUI
Skills dashboard wires this into its refresh action (`r` key), so an
operator editing `<workspace>/skills/<name>/SKILL.md` can press refresh
to pick up the change without restarting Baybo. Individual broken
`SKILL.md` files are logged and skipped, matching startup behaviour.
Filesystem watching is not wired yet — reload is on-demand only.

### Boundary with tool governance

Skills declare `allowed-tools`, but this is only one input to the upper bound. Before execution, the system still checks: skill's allowlist → trust-level ceiling → `ToolManifest.capabilities`. The skill's allowlist is not the final execution authorization.

## Constraints

- Depends on `baybo-model`, `baybo-tools`, and `baybo-workspace` (the last for `baybo_workspace::paths::BIN_NAME`), plus `regex`, `walkdir`, and `uuid`
- Does not call `llm` or execute tools directly
- The crate's own `SkillInstall` / `SkillUninstall` tools (see “Skill installation” above) are the supported way to add or remove a workspace skill at runtime; nothing else here mutates the installed set
- Every skill execution must record `skill_name`, `skill_version`, `source`, `trust_level` in Trace

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `AgentLoop` (via `ContextManager`) seeds the skill-listing reminder, expands `/<cmd>` invocations, and executes the `Skill` tool the model calls |
| `tools` | Skills declare allowed tool sets but don't execute tools directly. The `Skill` builtin (registered from `baybo-skills::tools`, parallel to `baybo-cron::tools`) is the LLM's single entry point for invoking them. |
| `trace` | Records skill version, source, and execution results |
| `workspace` | Provides trusted local skill directories for hot reload |

## References

- [nearai/ironclaw `ironclaw_skills`](https://github.com/nearai/ironclaw/tree/staging/crates/ironclaw_skills/src) — prior art for the validator (hostile-manifest hardening). Baybo's `validation.rs` is adapted from this design.
