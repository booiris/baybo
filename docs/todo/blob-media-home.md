# Where the media probes should live

Note to self, 2026-08-12. Not a plan — a recorded discomfort with the
condition that would make it worth acting on, so the question is not
re-derived from scratch next time somebody reads `crates/tools/src/blob_media.rs`
and wonders why it is there.

## The state

`crates/tools/src/blob_media.rs` (~150 lines) answers one question: **what is
this stored blob, and what does it cost a model?** It stats and probes a blob
id into a `MediaBlock` — pixel dimensions, PDF page count, audio duration,
byte size — every one a price input the context budget spends.

Two consumers, neither of them a tool:

- `crates/gateway/src/channel/route.rs` — chat ingest, turning a
  `WireAttachment` into a `ContentBlock`.
- `crates/project/src/brief.rs` — a kanban run's brief, turning a card's
  `IssueAttachment` into the blocks the assignee is handed.

It landed in `baybo-tools` when the kanban attachment feature needed it,
because that crate was already the one place depending on **both**
`baybo-store` (for `BlobStore`) and `baybo-llm` (for `media_probe` and the
delivery caps), and both consumers already depended on it. Nothing new was
added to the dependency graph.

## Why it is uncomfortable

`baybo-tools` is the agent's tool crate — `Tool` impls and the registry.
`blob_media` is not a tool and neither of its callers is on a tool path. It
is there for the **dependency graph, not for cohesion**, which is the honest
version of "it fit".

There is a second, better reason to look at this eventually: `blob_media` has
a **twin**. `crates/tools/src/builtin/attach_file.rs` asks the same questions
of a file still on local disk (`probe_media_duration_ms`,
`probe_pdf_page_count`, `probe_image_dimensions`, `probe_payload`). The two
exist because the producers hold different things — a path and a capability
id — and neither can reach the other's source. What they must not do is
disagree about which cap a probe gets or when it is worth taking; today they
agree by inspection, not by construction.

## Why NOT a `utils` crate

Considered and rejected.

- **The repo has never had one.** Thirty-seven crates — agent, channels,
  context, cost, cron, deck, llm, memory, model, pairing, process, project,
  query, sandbox, security, session, skills, store, storage, subagent, task,
  trace, turn, wire, workspace — and not one is named for its lack of a
  domain. No `utils`, no `common`, no `shared`. That is not an accident:
  CLAUDE.md's architecture section asks for high cohesion, and a crate whose
  only admission rule is "it did not fit elsewhere" is low cohesion by
  construction. The second thing to go in is unarguable once the first did.
- **It would not reduce coupling.** Any home for these needs `baybo-store`,
  `baybo-model` *and* `baybo-llm` — the caps live in `baybo-llm` on purpose
  (`MAX_PDF_DOCUMENT_BYTES`: "Exported so an ingest probe stops at the same
  number"). Both current consumers already depend on `baybo-tools`, so moving
  to a new crate adds a crate and two edges and removes none.

## What would be right, and when

A crate with a real domain — `baybo-media`, say — owning "what is this file
and what does it cost": `MediaKind`, `MediaBlock`, both probe families, and
the block construction. That has an admission rule a reviewer can apply.

**The trigger is unifying the two probe families**, not this file's size. Do
it when `attach_file`'s path-based half is being touched anyway, so there is
something to merge rather than something to move. Before then it is 150 lines
and two callers, and the move would still leave the `baybo-llm` dependency
exactly where it is.

Until then: leave it in `baybo-tools`, and let this file be the answer to
"why is it here".
