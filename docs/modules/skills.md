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

At startup the registry calls `SkillRegistry::register_builtins()` to register every skill compiled into the cargo `[[bin]]` (`crates/skills/src/builtin/<name>/SKILL.md`, embedded via `include_str!`), then scans the built-in agent's own directory, `<workspace.path>/personas/baybo/skills/<skill-name>/SKILL.md`. Every other agent's directory is scanned lazily — see [Every agent owns its skills](#every-agent-owns-its-skills). Compiled-in skills are `ArtifactSource::Inline` + `TrustLevel::Trusted`; an agent patches shipped behaviour for itself by having a same-named directory of its own, which shadows the builtin inside that agent's scope only.

The first built-in is `baybo-cli` — a non-user-invocable skill that tells the agent to introspect the running Baybo instance through the `baybo` CLI (the BashTool auto-injects `BAYBO_HELP_AGENT` and `BAYBO_CONFIG_PATH`, so the agent sees the full inventory and the right config without needing flags). The second is `deck` — agent- and slash-invocable (`command: deck` with model invocation enabled) and owner-channel-only (`channels: [owner]`) — carrying the deck card bundle contract, the `ctx`/`deck` SDK surface, and worked examples for authoring or updating a card before `DeckCardCreate`/`DeckCardUpdate`. Its description lets the model select it for ordinary-language requests for persistent dashboard cards, while `/deck <request>` remains the explicit shortcut; see [`deck.md`](deck.md#authoring-pipeline). The third is `html-gen`: an agent- and slash-invocable, owner-only skill for authoring a self-contained HTML page, staging it through `PutBlob`, and returning the `baybo-html` blob marker understood by the iOS transcript.

```
<persona>/skills/     # personas/<agent_id>/, or personas/project/<agent_id>/
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

A `/deploy` skill that's too dangerous to auto-trigger sets `disable-model-invocation: true`; a `legacy-system-context` reference that isn't actionable as a command sets `user-invocable: false`. Slash-only skills are why the slash-candidate set in `baybo-context` (`slash_skill_summaries`: `command.is_some() && !Untrusted && channel-admitted`) is deliberately independent of the model-advertised set (`invocable_skill_summaries`, which also requires `agent_invocable`). A skill can also expose both paths: the builtin `deck` is advertised to the model for ordinary-language card requests and keeps `/deck` as an explicit shortcut. A regex-based pattern trigger is **not** modelled — use `description` plus model decision instead.

The `/<name>` entry point is surfaced on channel adapters by `baybo-cli`'s `CliSlashHandler`: `commands()` lists every skill with `command.is_some()` so TUI autocomplete shows them alongside built-ins, and `handle()` returns `PassThrough` for `/<skill>` so the raw line reaches the agent and `ContextManager::expand_slash_command` matches the leading `/<cmd>` against the invocable skill set and injects the skill body. See [`cli.md`](./cli.md#skill-shortcut) and [`tui.md`](./tui.md#slash-completion) for the full wiring.

### Every agent owns its skills

**There is no shared skill tree.** Each agent's skills live below its resolved
persona directory, one directory per skill, and no agent reads another's.
Newly created project agents carry `project-<ULID>` ids and resolve below
`personas/project/`; legacy unprefixed project personas remain flat and valid.
The built-in is not a special case — its skills are at
`personas/baybo/skills/`, which is also what an unbound session reads, since an
unbound session *is* the built-in. A persona someone curated
should not silently acquire every skill the workspace happens to hold; granting
one is a decision, made by putting the skill in that agent's folder.

The only skills that are not in some agent's directory are the ones **compiled
into the binary** (`crates/skills/src/builtin/<name>/SKILL.md`, embedded via
`include_str!`). They belong to the process, not to any persona, which is what
makes them safe to share: reaching one is never reaching into another agent's
folder. The built-in scope sees all of them — the shipped set is what "default
behaviour" means. A custom agent sees only `UNIVERSAL_SKILLS`, currently just
`baybo-cli`: it tells the agent how to introspect the instance it is running
inside (the Bash tool injects `BAYBO_HELP_AGENT` / `BAYBO_CONFIG_PATH` for
exactly that), so it is runtime infrastructure rather than a capability anyone
chose to grant — withholding it would not make a persona narrower, only
blinder. `deck` is deliberately *not* in the list; an authoring tool is a
capability, which is why a custom agent has no `/deck`.

`SkillRegistry` therefore holds two maps — compiled-in builtins, and a
per-agent map keyed by profile id — and every session-scoped read goes through
the scoped pair:

- `get_scoped(agent, name)` — the agent's own directory first, then the
  builtins its scope admits.
- `summaries_for(agent)` — the admitted builtins with the agent's own
  directory layered on top, **the agent's own winning a name collision**;
  sorted by name so ordering is stable across turns. `ContextManager` routes
  the per-turn listing, the post-compaction trailer, and slash candidates
  through it.
- `all_scoped(agent)` — the same set as full definitions, for the operator
  surfaces that need more than the four summary fields (`baybo skills info` /
  `search` / `check`, which pass the default scope).

**Every read names a scope, and there is deliberately no unscoped sibling.**
An unscoped `get` / `list` / `all_sorted` used to exist and each one was the
same trap: it could only see the compiled-in map, so it silently missed every
skill anyone had installed. The one that mattered was the post-compaction
trailer, which re-broadcasts the body of a skill the session actually called —
unscoped, it dropped exactly the skills an agent owns, at the moment the
summary discarded the original. `search` and `validate` / `validate_all` take
a scope for the same reason.
- `ensure_agent_skills(agent, paths)` — **the only thing that fills the
  per-agent map**, so every reader of a scope calls it first: boot (for the
  built-in, whose id is a constant), the actor build (`runtime.rs`, on the cold
  spawn of a session) and `GET /v1/skills` when the query names an agent. It
  derives the resolved persona's `skills/` from the id, so a call site holding
  only a session's agent needs nothing else; an already-scanned agent is a no-op.
  `reload()` replays every scan.

`None` as a scope means the built-in, directory included — an unbound session
has always behaved as the built-in, and now that the built-in owns a directory
like everyone else, behaving as it has to include reading that directory.

Loading is lazy past the built-in because the set of agents is DB state:
`ensure_layout` cannot enumerate persona folders to scan, and a scan at
profile-creation time would miss every agent that already existed. The cost of
getting this wrong is silent — an unloaded directory is indistinguishable from
an empty one, and the agent simply has no skills — so the seam is one function
with no separate "is it loaded" question for a caller to forget.

A name that exists only in another agent's directory — or among builtins this
agent does not inherit — simply misses, so the `Skill` tool answers "unknown
skill" rather than a refusal that would leak an inventory. Governance is
unchanged: persona folders are workspace content, so their skills are `Trusted`
and the risk assessor judges them by content hash like any other.

`SkillInstall` / `SkillUninstall` write in the scope the caller reads — see
[Skill installation](#skill-installation). `SkillInstall` is also the **only**
writer of a skill directory: `Edit` and `Write` refuse a path under
the resolved persona's `skills/` outright (`classify_persona_path` gives it
`PersonaPath::Other`, which the managed-repo tier treats as unwritable), so a
skill gets in by being installed — and therefore by passing the risk assessor —
never by being hand-authored in place.

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
`SkillRegistry::summaries_for(skill_scope())` — per-agent scoped (the
compiled-in builtins that scope may see, layered with the bound agent's own
`personas/<id>/skills/`), returning a lightweight projection
(`SkillSummary`) carrying only the fields needed for the listing.
Cloning every `SkillDefinition`'s `prompt_template` / `allowed_tools`
/ `requirements` per turn would burn allocator pressure proportional
to skill count × body size; the projection avoids that. Filtered to
`agent_invocable && trust_level != Untrusted && allows_channel`, sorted
by name for stable across-turn ordering. There is deliberately **no**
"registry is empty, skip the projection" guard: that check could only read
one map, while the question is scoped, so an agent whose skills live only in
its own directory was advertised nothing at all whenever the map it read
happened to be empty. `SkillRegistry::is_empty()` existed for exactly that
guard and was removed with it — `summaries_for` is already cheap on an empty
registry. The trailer's reminder block advertises this same
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
and the per-turn list through `summaries_for`. There is no ranking
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

The crate ships two governance-gated lifecycle tools (built by `build_install_tool` / `build_uninstall_tool` in `tools.rs`, wired in `crates/baybo/src/runtime.rs` after the assessor is constructed), both declaring `TrustLevel::Trusted` with the `WriteFile` capability.

**Both act in the caller's own scope, and the invariant is "install what you can see, uninstall only what you can see."** The destination comes from `ctx.agent_id`, not from a root wired at startup: `scope_skills_dir` reads `AgentProfileId::skills_dir`, so every session lands on `personas/<its agent>/skills/` — exactly the directory `get_scoped` reads first for that scope. Wiring one process-wide root is what made the old behaviour incoherent in both directions: an install was invisible to the agent that asked for it and leaked to every session that did not.

- **`SkillInstall`** — validates a source directory (must contain a parseable `SKILL.md`, must live outside the *caller's own* skills dir, must not collide with an existing install there), runs the risk assessor (`Dangerous` aborts with `ToolError::Denied`), copies the tree to `<scope>/<name>/` via a staging-and-rename for atomicity (staged at `<scope>/.staging/<uuid>/` — depth 2, so a leak from a crash mid-install is not scanned as a phantom skill), calls `ensure_agent_skills` and then `SkillRegistry::reload()`.

  The `ensure_agent_skills` call is load-bearing, not defensive. `load_agent_dir` returns before recording a directory that does not exist, and `reload()` replays exactly the recorded list — so an agent whose folder was absent when its actor was built is missing from that list, and installing into the folder it just created would be dropped again on the very next reload. Because a miss is never latched, the post-install call re-stats and registers it. Order matters: reloading first would rebuild from a list that still lacks the agent.

  "Must live outside the caller's own skills dir" is deliberately not "outside any skills dir". The check is about the mistake it names — you pointed at the copy you already have — not about where a source may come from.

  **A source under another agent's folder is allowed, on purpose.** An agent may name `personas/<other>/skills/<name>` and copy it into its own — adopting a skill it could otherwise only read. That is a real hole in "no agent reads another's directory" if you read that invariant as a confidentiality boundary, and it is worth being clear that it is not one: the visibility rules decide what a *session's model* is offered and can invoke, not what the host filesystem will hand to a tool that was given a path. Closing this one path would not close the capability — `source_dir` is arbitrary, `Read` is not scoped, and an agent that can read a `SKILL.md` can hand-author the same body under `work/` and install that. What actually governs the copy is the risk assessor, which every install runs regardless of where the bytes came from. Treat a skill body as readable-by-any-agent; put nothing in one that a different persona must not see.

- **`SkillUninstall`** — resolves the name through `get_scoped`, so a name the session cannot see is an ordinary `NotFound` rather than a refusal that would confirm it exists. Deletion is then confined to the caller's own directory, which is a *second*, narrower gate: seeing a skill and owning it are different things, and a custom agent reaching a `UNIVERSAL_SKILLS` entry has nothing on disk to remove. A session whose own directory does not exist yet owns nothing, so a root that fails to canonicalize refuses rather than errors. Removes the directory, then calls `SkillRegistry::reload()`.

Both report `skills_in_scope` — the size of the caller's set after the reload. Any other scope's count would be noise to an agent that just installed into its own folder, and a `0` there would read as failure.

### Hot reload constraints

- Watch only trusted directories
- Validate schema and requirements before accepting changes
- Record name/version/source/hash on version replacement
- On failure, keep the old version rather than emptying the registry

`SkillRegistry` offers `reload()` — re-scans every agent directory registered
so far and rebuilds the set from disk. Compiled-in builtins are replayed from
the definitions captured by `register_builtins`; without the replay, the first
`SkillInstall`-triggered reload silently dropped every one of them. An agent's
own same-named skill still shadows its builtin, inside that agent's scope.

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
operator editing `<persona>/skills/<name>/SKILL.md` can press refresh
to pick up the change without restarting Baybo. Individual broken
`SKILL.md` files are logged and skipped, matching startup behaviour.
Filesystem watching is not wired yet — reload is on-demand only.

### Boundary with tool governance

Skills declare `allowed-tools`, but this is only one input to the upper bound. Before execution, the system still checks: skill's allowlist → trust-level ceiling → `ToolManifest.capabilities`. The skill's allowlist is not the final execution authorization.

## Constraints

- Depends on `baybo-model`, `baybo-tools`, and `baybo-workspace` (the last for `baybo_workspace::paths::BIN_NAME`), plus `regex`, `walkdir`, and `uuid`
- Does not call `llm` or execute tools directly
- The crate's own `SkillInstall` / `SkillUninstall` tools (see “Skill installation” above) are the supported way to add or remove a skill at runtime, in the caller's own scope; nothing else here mutates the installed set
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
