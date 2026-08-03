# Agent-Scoped Skill Install (+ moving the skill tree under `personas/`)

Two related changes, requested together:

1. `SkillInstall` / `SkillUninstall` should honour the calling agent's id, so an
   agent can install a skill only it sees.
2. The skill tree should move under `personas/`.

## Problem

`SkillInstallTool` carries a **fixed** `workspace_skills_dir`, wired once at
startup from `workspace_paths.skills_dir()`
(`crates/baybo/src/runtime.rs:544`). The destination is written flat:

```rust
let dest_dir = self.workspace_skills_dir.join(&skill.name);  // crates/skills/src/tools.rs:591
```

Nothing consults the agent, even though `ctx.agent_id` is right there — the
`Skill` tool uses it one screen up, at `tools.rs:164`.

The visibility rules, meanwhile, *are* scoped. `sees_shared_set(agent)` is
`agent.is_none_or(is_builtin)`, so a custom agent sees only its own overlay plus
`UNIVERSAL_SKILLS`. Measured against a real registry:

```
shared list                  = ["newly-installed"]
custom get_scoped            = None        <- the installer cannot see it
custom summaries_for         = []          <- nor list it
unbound summaries_for(None)  = ["newly-installed"]   <- everyone else can
```

**Both ends are backwards.** The skill is invisible to the agent that asked for
it and leaked to every session that did not. `SkillUninstall` has the same shape
in reverse: it gates only on the target sitting under the workspace skills dir,
so a custom agent can remove a shared skill it cannot see, for everybody.

There is no workaround. `personas/<id>/skills/` is populated **only** by
`ensure_agent_overlay` scanning disk — no tool writes there — and `Edit`/`Write`
are refused outright, since `managed_repo` classifies that path as
`PersonaPath::Other`:

```rust
PersonaPath::Memory { .. } | PersonaPath::Other => false,  // crates/tools/src/builtin/managed_repo.rs:214
```

so an agent cannot even hand-author a `SKILL.md` for itself.

## Part 1 — scope the install

- Pick the destination from `ctx.agent_id`: a custom agent installs to
  `personas/<id>/skills/<name>/`, the built-in and unbound sessions to the
  shared tree (which is what they see). The invariant to land is **"install what
  you can see, uninstall only what you can see"**.
- `SkillUninstall` mirrors it: refuse anything outside the tree the caller's own
  scope covers, rather than the current single workspace-dir check.
- **Register the overlay dir after a fresh install.** `load_agent_dir` returns
  early when the directory does not exist and only records one that does, so an
  agent whose overlay dir was absent at actor-build time is missing from
  `agent_dirs` — and `reload()` replays exactly that list. Installing into a
  brand-new overlay without registering it means the next reload silently drops
  the skill again.
- Nice interaction, once both this and the drift hint are in: the installing
  agent's next turn gets a `<skills_update>` carrying the `+` line, so it learns
  its own install landed.

## Part 2 — move the tree under `personas/`

Wanted for layout uniformity. Recorded with the constraints found while looking
at it, so they are not rediscovered:

- **It does not let the two maps merge.** `SkillRegistry`'s `skills` /
  `agent_skills` split is forced by semantics, not by directory layout: an
  unbound session (`agent = None`) has no key to look itself up under, and
  `UNIVERSAL_SKILLS` is a deliberate pass-through from a custom agent's scope
  into the shared set. Moving the directory changes neither.
- **`personas/baybo/skills/` is the wrong target.** It would make the shared set
  the built-in persona's property, so a custom agent reaching for `baybo-cli`
  would be reading *another agent's* overlay — the thing `get_scoped`'s contract
  says must never happen. It also makes "reset the `baybo` persona" quietly
  destroy every unbound session's skills, and `AgentProfileId::skills_overlay_dir`
  currently returns `None` for the built-in on purpose
  (`crates/model/src/agent_profile.rs:134`).
- **`personas/skills/` is the consistent shape**, matching `personas/USER.md` —
  the existing precedent for "shared, belongs to no agent".
- Migration: existing workspaces hold `<root>/skills/`. Per `CLAUDE.md` there
  are no legacy-data migrations, so a move strands installed skills unless the
  loader reads both paths for a release. Decide that before moving.

## Order

Part 1 is the one with a live defect behind it and is independently shippable.
Part 2 is cosmetic and should not block it.

## Not covered here

The two-lock straddle in `SkillRegistry`: `summaries_for(Some(agent))` takes
`skills.read()` and then `agent_skills.read()`, and `reload()` swaps the two maps
separately, so a scoped reader can observe a new shared set beside an old
overlay. Both halves are internally complete, so this is far milder than the
pre-`build-then-swap` window, but it does mean the "complete old set or complete
new one" guarantee holds per map rather than across a scoped read. Fix is one
lock over a struct holding both maps, not one map.
