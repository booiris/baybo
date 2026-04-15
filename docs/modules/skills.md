# skills - Skill System

## Overview

The `skills` crate defines, loads, selects, and hot-reloads declarative skills with governance. It is not just "reading Markdown templates" — it also carries governance responsibilities.

**Tool = atomic operation + isolated execution. Skill = declarative orchestration + governance constraints.**

Core responsibilities:

- Skill definition with source, version, and trust level
- Invocation matching: `/<cmd>` narrows to one skill, anything else returns the full registered set for the model to choose from
- Constrain which tools a skill may call

## Skill file format

One directory per skill, a `SKILL.md` entrypoint with YAML frontmatter plus a Markdown body.

### Load location

At startup the registry scans `<workspace.path>/skills/<skill-name>/SKILL.md`. No other source is consulted.

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
| `name`                     | scalar                    | dir name   | Skill identifier; also drives the `/<name>` slash command. |
| `description`              | scalar                    | `""`       | Used by the model for auto-selection. |
| `when_to_use`              | scalar                    | `None`     | Appended to `description` in the skill listing. |
| `allowed-tools`            | list or space-sep string  | `[]`       | Tool allow-list while the skill is active. |
| `disable-model-invocation` | bool                      | `false`    | `true` clears `agent_invocable` — only the slash command remains. |
| `user-invocable`           | bool                      | `true`     | `false` clears `command` — only agent decision remains. |
| `argument-hint`            | scalar                    | `None`     | Autocomplete hint (e.g. `[issue-number]`). |
| `version`                  | scalar                    | `"0.0.0"`  | Recorded in trace provenance. Must match `[a-zA-Z0-9._\-+~]{1,32}` — whitespace, quotes, and angle brackets are rejected so a hostile manifest can't break out of the `<skill version="…">` attribute. |

Skill names must match `[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}` — the same characters permitted in the `/<name>` slash command. Display strings with spaces or slashes are rejected at load time.

The prompt body is injected wrapped in a `<skill name="…" version="…">…</skill>` block assembled by `render::render_skill_block`. Escaping is applied lazily at render time — the in-memory `SkillDefinition` keeps the author's original text for CLI display, while the rendered block runs every field through XML-attribute / tag-breakout escapes. Every `<skill` / `</skill` occurrence inside the body (case-insensitive, tolerant of whitespace and null bytes) has its leading `<` replaced with `&lt;`. Together with the restricted name/version grammars this closes the tag-forging class of attacks end-to-end.

Unsupported YAML features (anchors, folded/literal block scalars, nested mappings) are **rejected** rather than silently mis-parsed — keeps the author's intent honest and the parser small.

The Markdown body becomes `prompt_template` and is injected as a system message when the skill is selected.

## Design Decisions

### Dual invocation model

Every skill exposes two independent entry points; both default on:

- `command: Option<String>` — explicit `/name` slash command (set from frontmatter `name` unless `user-invocable: false`)
- `agent_invocable: bool` — the model may auto-select based on `description` (unless `disable-model-invocation: true`)

A `/deploy` skill that's too dangerous to auto-trigger sets `disable-model-invocation: true`; a `legacy-system-context` reference that isn't actionable as a command sets `user-invocable: false`. A regex-based pattern trigger is **not** modelled — use `description` plus model decision instead.

The `/<name>` entry point is surfaced on channel adapters by `aura-cli`'s `CliSlashHandler`: `commands()` lists every skill with `command.is_some()` so TUI autocomplete shows them alongside built-ins, and `handle()` returns `PassThrough` for `/<skill>` so the raw line reaches the agent and `select()` narrows on the exact match. See [`cli.md`](./cli.md#skill-shortcut) and [`tui.md`](./tui.md#skill-shortcuts) for the full wiring.

### Three-tier trust model

- **Trusted**: workspace or admin-placed skills. May hot-reload and request full tool set.
- **Installed**: registry-installed skills. May auto-match but tool count and capabilities are downgraded.
- **Untrusted**: may only be listed and reviewed, cannot auto-execute.

### Selection pipeline

Selection runs in `registry.rs` as a pure function — no LLM is consulted and no prompt body is read, so an already-loaded skill cannot bias which skill loads next.

`SkillRegistry::select` has two cases:

| Message shape                                | Returned |
|----------------------------------------------|----------|
| Trimmed message equals `/<cmd>` exactly      | Just that skill. An explicit slash invocation narrows the context to the one skill the user asked for. |
| Anything else (including `/<cmd> <args>`)    | Every registered skill. The downstream risk assessor filters, and the model decides which description is relevant. |

`score` on every returned `SkillCandidate` is `1.0` — the field is kept on the struct for future ranking work but is currently unused. Heuristic ranking (mention scanning, description matching, agent-invocable fallback tiers) was removed because ranking by regex either over-fires or lags authors, both of which eat trust; the LLM does better on intent matching than any rule we ship.

Downstream gating still happens, just not here:

- `AgentLoop` runs every candidate through `SkillAssessor`; `Dangerous` verdicts drop the skill.
- `SkillRegistry::validate_all` reports unmet `required_bins` / `required_env`; callers that care (e.g. `aura skills check`) act on that report.
- Trust-level attenuation of the tool ceiling is a design stage not yet wired in.

### Risk assessment

Static governance (trust levels, allow-lists, validator checks) catches structural problems but can't judge *semantic* intent: a skill with clean YAML can still instruct the model to exfiltrate secrets or run destructive commands. The `aura-skills-assessor` crate adds an LLM-backed second opinion, kept in its own crate so `aura-skills` stays LLM-free (selection must remain deterministic and offline-capable).

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

The hash is a **metadata fingerprint**, not a content hash — see [skills-assessor.md](skills-assessor.md) for the full rationale and tradeoff. It covers every entry in the hashed scope (`SKILL.md` alone under `primary`, or `SKILL.md` plus all helper files under `full`), so a normal edit bumps mtime and re-triggers the check without us reading file bodies on the hot path. Length-prefixing the rel-path and symlink-target fields closes aliasing hazards across adjacent variable-length fields. Two scope discriminators (`aura.skill.full:v1` and `aura.skill.primary:v1`) are mixed into the hasher state so a one-file skill's primary hash and full hash never collide — both scopes can share the `(skill_name, content_hash)` primary key without ambiguity. A 500-file / 100 MiB hard cap rejects pathological trees outright before any hashing work runs.

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
| `assessed_at`  | Unix seconds. |

**Non-blocking error policy**: only `Dangerous` blocks execution. Assessor errors (LLM unreachable, unparseable reply, I/O failure), skills without an on-disk `source_path` (e.g. test fixtures), and the `Suspicious` tier all pass through with a `warn!` log. Availability is preferred over false-positive blocks; the verdict is still surfaced in `aura skills check` output so a human can review.

**Integration points** (both lazy — no work until the skill is actually reached):

- **CLI `aura skills check` / `/skills check`** — runs the validator, then invokes the assessor per skill. Output includes `risk: {status, scope, background_pending, level, rationale, model, content_hash, assessed_at}`.
- **`AgentLoop` skill injection** — `assess_skill_risk` returns a `SkillGate` (`Pass` / `PassWithWarning { rationale }` / `Block { rationale }`) per candidate. `Block` drops the skill and emits `AgentOutput::Notice { level: Error }`; `PassWithWarning` keeps the skill and emits `Notice { level: Warn }`; `Pass` injects silently. There is no longer a silent-drop band: either the user invoked the skill via `/<cmd>` or every registered skill is in play, and in both cases a non-Safe verdict is worth surfacing.

The assessor is wired in `main.rs` alongside the other shared services using `with_background_worker(llm, store, mode)` where `mode` is read from `config.skills.risk_check`; `recover_pending_jobs` runs once after the skill registry is populated and drains persisted rows regardless of mode — `Off` only suppresses new enqueues. Argv-mode commands that don't open the chat loop leave the assessor `None`, which the CLI surfaces as `status: "not_configured"`.

### Hot reload constraints

- Watch only trusted directories
- Validate schema and requirements before accepting changes
- Record name/version/source/hash on version replacement
- On failure, keep the old version rather than emptying the registry

### Boundary with tool governance

Skills declare `allowed-tools`, but this is only one input to the upper bound. Before execution, the system still checks: skill's allowlist → trust-level ceiling → `ToolManifest.capabilities` → `sandbox` policy. The skill's allowlist is not the final execution authorization.

## Constraints

- Depends only on `registry`
- Does not call `llm` or execute tools directly
- Does not install extensions (that's `registry`)
- Every skill execution must record `skill_name`, `skill_version`, `source`, `trust_level` in Trace

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `AgentLoop` calls `SkillRegistry.select()` and executes skills |
| `tools` | Skills declare allowed tool sets but don't execute tools directly |
| `registry` | Supplies source, version, and hash metadata for installed skills |
| `trace` | Records skill version, source, and execution results |
| `workspace` | Provides trusted local skill directories for hot reload |

## References

- [nearai/ironclaw `ironclaw_skills`](https://github.com/nearai/ironclaw/tree/staging/crates/ironclaw_skills/src) — prior art for the validator (hostile-manifest hardening). Aura's `validation.rs` is adapted from this design.
