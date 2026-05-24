# Prompt Framing — Consolidate into the `context` crate

## Problem

The text Aura injects into the LLM transcript — system prompt, cron-fire framing, the
subagent-finished notification, the skill reminder, the tool-output envelope — is authored in
**six different places** across two layers. The framing lives apart from the transcript it
ends up in, so it is hard to find, hard to audit as a set, and easy to drift out of sync with
the prompt-cache prefix it has to preserve.

Current sites:

- `crates/agent/src/runtime/soul.rs:8,14,84` — `TOP_HINT` / `TAIL_HINT` / `wrap_section`: the system prompt assembled from identity files.
- `crates/agent/src/actor/cron_prompt.rs:21,38,49` — `frame_cron_prompt` (fire framing) + `original_cron_prompt` (the reverse, for the operator cron inbox).
- `crates/agent/src/actor/mod.rs:48,53,589` — `SUBAGENT_NOTIFICATION_FRAMING` / `SUBAGENT_RESULT_TEMPLATE` / `build_subagent_notification_content`.
- `crates/agent/src/runtime/agent_loop.rs:1655,1738` — `ensure_system_prompt` / `initial_seed_messages`: seeds the system row + skill reminder.
- `crates/agent/src/security.rs:293,321` — `wrap_tool_output_for_llm` / `cap_tool_output`: the `<tool_output>` envelope around every tool result.
- `crates/context/src/compressor.rs:40,44,78` — `CONTINUATION_INTRO` / `CONTINUATION_FOOTER` / `SUMMARIZE_INSTRUCTION`: compression prompts (already in `context`, because compression is its domain).

`context` is the natural home: it is the **sole owner of the transcript** (`docs/modules/context.md`), it already owns the compression prompts, and it already holds the three handles the rest of this needs — `skill_registry: Arc<SkillRegistry>`, `workspace: Arc<WorkspacePaths>`, `sessions: Arc<SessionManager>` (`crates/context/src/lib.rs`, struct fields) — and it already reads workspace files from disk (`compressor.rs:413` reads `summary.md`).

## What does *not* move (and why)

The boundary is "**text the model reads as conversation**." Three things look adjacent but stay put:

- **`InjectionDetector::scan` + `sanitize_tool_output`** (`security.rs:264`, `crates/security/src/injection_detector.rs:177`) — secret vaulting and injection detection are security concerns; the wrapper consumes their *result*, it does not own them. See Stage 1 for the detect/format split.
- **`EMPTY_USER_REPLY_NOTICE`** (`actor/mod.rs:42`) — sent as an `OutgoingMessage` to the user (`actor/mod.rs:750`), never appended to the transcript. Different category.
- **Turn orchestration** — the mailbox priority queue, `context_snapshot`/`restore_context`, persist-before-turn, empty-reply suppression. `agent` depends on `context`, never the reverse; orchestration that needs `JobLifecycle` / `CostManager` / the mailbox can't live below the loop.

## Key decisions

- **Source-aware typed append, not a `MessageSource` argument.** `MessageSource` (`crates/model/src/message.rs:75`) is too coarse to drive framing — only `User`/`Cron`/`Agent`, where `Agent` lumps the skill reminder, subagent prompts, the subagent-finished notification, summary instructions, the system prompt, assistant output, and tool results into one bucket. It also carries no parameters and is a persisted/display tag the gateway filters on (`gateway/src/api/admin/chat.rs`). So framing keys off **typed methods** — `append_cron_fire(job_id, prompt)`, `append_subagent_notification(&results)`, `append_user(content)` — mirroring the existing `ChatMessage::{user,cron_fire,agent_context}` constructor taxonomy. Each method bakes in framing **and** persistence (today these are two orthogonal axes: source → constructor, plus a separate `persist_user_row` bool at `agent_loop.rs:469`).
- **`context` does not gain an `aura-security` dependency.** Tool-output wrapping splits into detect (security) vs format (context); the scan result crosses the boundary as plain data. The one shared symbol — the `</tool_output>` delimiter that both the wrapper's breakout-escape and the detector's scan must agree on — lifts to `aura-model`.
- **`context` owns the full system-prompt lifecycle.** It already has the handles; folding `Soul` in kills the straddle where `reload_soul_after_compaction` (`agent_loop.rs:1688`) reaches into `context` to swap `messages[0]` after a compaction that `context` itself ran.

## Proposed direction

Four focused PRs in dependency order; each compiles and is independently green.

### Stage 1 — `aura-model` delimiter const + tool-output detect/format split

Lift the `<tool_output>` / `</tool_output>` delimiter to `aura-model` as the single source of
truth. Split `wrap_tool_output_for_llm` (`security.rs:293`):

- **security keeps**: `InjectionDetector::scan` and `sanitize_tool_output` — unchanged.
- **context gains** a pure formatter that takes the scan result as **plain strings**, not the `InjectionWarning` struct:

```rust
// context::prompts::tool_output
pub fn wrap_tool_output(tool_name: &str, content: &str, warning_rules: &[&str]) -> String;
pub fn cap_tool_output(content: String, spill_path: Option<&Path>) -> String; // MAX_TOOL_OUTPUT_BYTES
```

`warning_rules` is `&[&str]` (the rule names the banner already extracts at `security.rs:300`),
**not** `&[InjectionWarning]` — `InjectionWarning` lives in `aura-security`, so taking it here
would re-introduce the `context`→`security` edge this split exists to avoid. `wrap_tool_output`
owns the banner text, the `escape_close_tool_output` breakout-escape (`security.rs:695`), and the
envelope; `cap_tool_output` owns truncation + the spill notice (spill dir resolves through
`context`'s `WorkspacePaths`). The call site (`ToolExecutor`, in `agent`, which already depends on
`security`) bridges the two: `let w = gateway.scan(content); let names: Vec<&str> = w.iter().map(|x| x.rule_name.as_str()).collect(); let wrapped = wrap_tool_output(name, content, &names)`.

### Stage 2 — per-turn framing → `context::prompts` + typed `append_*`

Move `frame_cron_prompt` / `original_cron_prompt` (`cron_prompt.rs`) into
`context::prompts::cron`, and `SUBAGENT_NOTIFICATION_FRAMING` / `SUBAGENT_RESULT_TEMPLATE` /
the XML builder (`actor/mod.rs`) into `context::prompts::subagent`. Add the typed methods:

```rust
impl ContextManager {
    pub async fn append_user(&mut self, content: Vec<ContentBlock>) -> Option<i64>; // persisted
    pub async fn append_cron_fire(&mut self, job_id: &str, prompt: &str) -> Option<i64>; // persisted (operator inbox finds it via MessageSource::Cron)
    pub fn append_subagent_notification(&mut self, results: &[PendingSubagentResult]); // in-memory only — rebuilt from the durable buffer on retry
}
```

The **actor** calls these directly (it owns the source — it is reacting to `CronTrigger` /
`UserInput` / `SubagentFinished`). `run_agent_loop` thins: it stops appending the trigger
(deletes the `match source` + `persist_user_row` block at `agent_loop.rs:464-475`) and iterates
the current context. `detect_slash_invocation` (`agent_loop.rs:1828`) stays in the loop — it
reads the context tail the actor just appended.

`original_cron_prompt` is re-exported as `aura_agent::cron_prompt::original_cron_prompt` so the
gateway admin caller (`gateway/src/api/admin/chat.rs:765`) is untouched.

### Stage 3 — system prompt / `Soul` lifecycle → `context`

Fold `Soul` (`soul.rs`) into `context::prompts::soul`: `TOP_HINT` / `TAIL_HINT` /
`wrap_section`, plus identity-file loading via the existing free
`aura_workspace::identity::load_identity_files(root)` against `context`'s `WorkspacePaths`.
`context` exposes:

```rust
impl ContextManager {
    pub async fn ensure_seeded(&mut self) -> Result<()>; // idempotent: leading-system check, like ensure_system_prompt today
}
```

`ensure_system_prompt` / `initial_seed_messages` / `reload_soul_after_compaction`
(`agent_loop.rs:1655,1738,1688`) are deleted; the post-compaction reseed becomes internal to
`context`'s compaction apply (re-read identity files via `workspace`). The actor calls
`ensure_seeded()` at the right point — **for the subagent path, before `context_snapshot()`**
(`actor/mod.rs:647` ordering), so a rollback can't drop the just-seeded system row. The
`Soul::custom` override (test harness, `soul.rs:65`) becomes a `context` construction param.

### Stage 4 — migrate compression prompts into `prompts/`

Move `SUMMARIZE_INSTRUCTION` / `CONTINUATION_INTRO` / `CONTINUATION_FOOTER`
(`compressor.rs:40,44,78`) into `context::prompts::compression`. The compression *flow* stays in
`compressor.rs`; only the text relocates, so every prompt now lives under
`context/src/prompts/{soul,cron,subagent,tool_output,skill_reminder,compression}.rs`. Cosmetic,
last, lowest-risk.

## Design constraints

- **Prompt-cache prefix stability.** The system prompt must stay byte-identical across turns; the only invalidation point is reseed-after-compaction, which already invalidates today (`docs/modules/agent.md` "notification framing lives in per-turn content, never the system prompt"). The subagent framing stays in per-turn content for the same reason — do not move it into the seed.
- **Persistence is encoded in the method name.** `append_subagent_notification` is **in-memory only** (`append_in_memory`, `lib.rs:429`) — the durable buffer is the source of truth and the turn is rebuilt on each retry; persisting per-attempt stacks duplicate hidden rows under infinite-backoff retry. `append_cron_fire` persists (the operator cron inbox finds the row by `MessageSource::Cron`).
- **Seed before snapshot; `append_*` must not self-seed.** Self-seeding inside `append_*` would seed *after* the actor's `context_snapshot()`, so a rollback would drop the system row — the exact bug the current explicit `ensure_system_prompt_seeded`-before-snapshot ordering avoids.
- **No `context` → `security` edge.** The detector's scan result crosses as data; the `</tool_output>` delimiter is shared via `aura-model` so the wrapper's breakout-escape and the detector's forged-delimiter scan can never disagree.
- **Orchestration stays in the actor.** Mailbox priority, snapshot/rollback, persist-before-turn, and empty-reply suppression do not move — `agent` depends on `context`, not the reverse.
- **Operator surfaces and storage tags untouched.** `MessageSource` keeps its three variants and its serialized form; `original_cron_prompt` keeps its `aura_agent::cron_prompt` path.

## Related

- `crates/context/src/lib.rs` — `ContextManager` (holds `skill_registry`/`workspace`/`sessions`); `append` (`:412`), `append_in_memory` (`:429`), `insert_skill_trailer` (`:1128`).
- `crates/agent/src/runtime/agent_loop.rs` — `ensure_system_prompt` (`:1655`), `reload_soul_after_compaction` (`:1688`), the trigger-append block (`:464-475`), `context_snapshot`/`restore_context` (`:318,323`).
- `crates/agent/src/actor/mod.rs` — `dispatch_cron_prompt` (`:426`), `run_subagent_notification` (`:647`), `build_subagent_notification_content` (`:589`).
- `crates/agent/src/security.rs` — `wrap_tool_output_for_llm` (`:293`), `cap_tool_output` (`:321`), `sanitize_tool_output` (`:264`), `escape_close_tool_output` (`:695`).
- `crates/model/src/message.rs:75` — `MessageSource`; `crates/model/src/spawn_protocol.rs:181` — `PendingSubagentResult`.
- `docs/modules/context.md`, `docs/modules/agent.md` — update once each stage lands.
