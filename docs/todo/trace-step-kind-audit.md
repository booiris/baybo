# Trace `StepKind` / `SpanKind` — audit the enum against what is actually recorded

> **Status:** items 2, 3 and 5 are done (phantom variants deleted, the sync test now guards
> both directions, `Compression` carries its trigger). Items 1 and 4, and the design
> question at the end, are still open.

Found while rebuilding the trace viewer (`docs/todo/trace-tree-redesign.md`). The viewer
renders one icon/colour/label per kind, which made it obvious that some kinds can never
appear — and that the frontend union declares kinds the backend does not have.

Nothing here is urgent: the viewer degrades gracefully (unknown kinds fall back to a
generic row, phantom kinds simply never match). But the enum is currently not a truthful
description of what a trace contains, and PR2 of the viewer redesign has to build on it.

## The four findings

### 1. `StepKind::SkillSelection` is declared but never constructed

`crates/trace/src/step.rs:71` declares it; no production path calls `begin_step` with it.
A skill is pulled in through the `Skill` tool (`SKILL_TOOL_NAME`, `crates/skills/src/tools.rs:30`),
so in a trace it appears as an ordinary `SpanKind::ToolCall` named `Skill` — never as a step.

Either wire it up (bracket the skill-selection decision in its own step) or delete the
variant. Leaving it is what produced a legend entry in the viewer that could never light up.

### 2. The web mirror declares two kinds Rust does not have

`app/web/src/types/trace.ts` carries:

```ts
| { kind: 'subagent'; child_session_id: string }        // StepKind — no Rust counterpart
| { kind: 'subagent_stub'; child_session_id: string }   // SpanKind — no Rust counterpart
```

Rust `StepKind` has 7 variants (no `Subagent`); Rust `SpanKind` has 2, `LlmCall` and
`ToolCall` (no `SubagentStub`). These TS variants are phantoms — no backend writes them, so
no trace can contain them.

A subagent really shows up as a `spawn_subagent` **tool call**
(`SPAWN_SUBAGENT_TOOL_NAME`, `crates/model/src/spawn_protocol.rs`); the viewer now keys its
Subagent grouping off that tool name for exactly this reason.

### 3. The sync test only guards one direction

`crates/trace/tests/web_trace_types_sync.rs` iterates every Rust kind and asserts the TS
union lists it — catching "Rust gained a variant, frontend forgot". It never asserts the
reverse, which is how the two phantom TS variants above went unnoticed.

Add the opposite assertion: every `kind: '…'` tag in the TS unions must correspond to a Rust
variant. That is a string-scrape of the same file the test already reads.

### 4. Stale doc comments

`crates/trace/src/span.rs:446` and `:454` describe `SpanEnd` in terms of `SubagentStub`
spans, a variant that does not exist. Whatever they were documenting has been removed.

### 5. `Compression` does not say which compression it was

Two different things record the same `StepKind::Compression`, with nothing on the step to
tell them apart:

- **Inline / send-time** — `ContextManager::maybe_compress`, run at the top of an iteration
  when the context about to be sent would overflow. This one is on the critical path: the
  turn waits for it, and it changes what the very next LLM call sees.
- **Background** — the detached pass spawned at iteration boundaries
  (`maybe_request_background_summary`), attributed to the same session. It is off the
  critical path and the user never waits on it.

Both funnel through `CompressionRunner::run` (`crates/agent/src/runtime/compression.rs:108`),
which brackets one `StepKind::Compression` step + `LlmCall` span for either caller. So a
trace reader cannot answer "was the context compacted *at send time*, and when" — which is
the operationally interesting one, since that is the compaction that reshapes the prompt the
model is about to answer.

Fix: carry the trigger on the step, e.g. `Compression { trigger: Inline | Background }`
(value-bearing, like the web mirror already does for other kinds) or two step kinds. Either
way the sync test and `trace.ts` follow. Until then the viewer deliberately shows no
send-time-compaction indicator, because it would be indistinguishable from background work.

## The question underneath

Should spawning a subagent, and selecting a skill, be first-class steps?

Arguments for leaving them as tool calls: they *are* tool calls; a second representation of
the same event is duplication, and the trace-size work (`docs/todo/…`, the storage-bloat
fixes) pushes against writing more rows.

Arguments for making them steps: a subagent is a control-flow event, not an ordinary tool —
it spawns a whole child session, and the parent blocks on it. `Lineage` already records
`parent_span_id` to pin the spawning span, so the relationship is modelled; a step would
make it visible in the step tree rather than something the reader has to infer from a tool
name. PR2 of the viewer redesign (nesting a child session under its spawn point) currently
has to special-case the tool name for this reason.

Decide this before PR2 hardens the tool-name convention into the UI.

## Suggested order

1. Add the reverse assertion to the sync test (cheap, stops further drift).
2. Delete the two phantom variants from `trace.ts`, plus the viewer branches that handle
   them (`SubagentStubDetail`, the `subagent` step-kind arm in `stepSummaryText`).
3. Fix the stale `span.rs` comments.
4. Decide (1) — wire up or delete `SkillSelection` — and the subagent question above.
