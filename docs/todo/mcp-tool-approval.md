# MCP tools — restore approval under a real permission model

> **Status:** deliberate interim state, shipped. **MCP tool calls raise no
> approval prompt at all.** `resource_access_for`
> (`crates/tools/src/mcp/reconciler.rs`) returns an empty access list for every
> server, so the agent loop's pre-execute gate has nothing to check. This is a
> loosening: it is here because the rule it replaced asked the wrong question,
> not because MCP calls are safe by construction. Restore gating when the
> permission model below exists — do not re-add the old rule.

## What was removed

Per-server, transport-derived accesses attached to every tool a server
provided:

```rust
match &entry.transport {
    Stdio { command, .. } => vec![ResourceAccess::ExecCommand { command: command.clone() }],
    Http  { url }         => vec![ResourceAccess::Http { host }],
}
```

Embedded servers could opt out by declaring `capabilities: []` (the browser
sidecar's escape hatch: "Baybo controls the spawn and the user authorised it
with `enable=true`"). User-configured `.mcp.json` servers could not — that
branch was explicitly closed to them.

## Why it went

1. **It asked about the transport, not the operation.** Every tool of a stdio
   server declared `ExecCommand{node}`, so a read-only `describe_regions`
   prompted exactly like `modify_firewall_rules`. The prompt text — "will run:
   node" — described the sidecar's launch, which the operator had already
   authorised by writing the entry into `.mcp.json`, and said nothing about
   what the call would do.
2. **The grant was coarser than the question.** `ApprovedResource::ExecCommand`
   matches on the command string alone (`crates/model/src/approval.rs`), so one
   `ApproveAlways` on `node` covered *every* node-launched MCP server in that
   session — a broader grant than any prompt asked for, handed out by answering
   a narrow-looking question.
3. **It ignored the operator's stated policy.** `permission` (`baybo.json`)
   reaches only `BashPermissionMode` and the OS sandbox
   (`crates/baybo/src/boot.rs`, `crates/baybo/src/sandbox_boot.rs`). Under
   `permission: "free"` — "run Bash directly, no sandbox, no approval" — an
   `rm -rf` went through unquestioned while a cloud-API read prompted. The two
   subsystems had no wire between them and disagreed in the wrong direction.
4. **Per-session, per-command re-prompting.** Grants live on
   `SessionState.approved_resources`, so every new session re-asked the same
   question about the same server, and each distinct launcher (`node`, `uv`, …)
   was its own separate ask.

## What is still enforced (do not regress)

Deferral and this change are advertisement/approval only. Every other door
still keys off the live registry, per call, in `ToolExecutor::execute`:

- **Trust** — `manifest.validate_auto_execution()`.
- **Channel** — `manifest.allows_channel()`.
- **Trigger scope** — `Tool::trigger_scope().allows_trigger()`.
- **Unattended lineages** — an execution carrying `InheritedToolContext`
  denies any uncovered approval-gated access without prompting. Vacuous for
  MCP tools while their access list is empty: unattended MCP calls run
  exactly as attended ones do.
- **Secret placeholders** — arguments are revealed only inside the executor,
  after every gate.

The interim rule itself is pinned by
`no_mcp_server_carries_approval_gated_accesses`
(`crates/tools/src/mcp/reconciler.rs`).

## What a real model needs

Sketch, not a decision — the point is that each item is something the removed
rule could not express:

- **Per-operation classification, not per-transport.** MCP gives an annotation
  hint (`readOnlyHint` / `destructiveHint` / `idempotentHint`) the reconciler
  currently discards at `connect_server`. Capturing it per tool at registration
  is the cheapest honest signal available, with a server-level default for
  servers that annotate nothing.
- **Grants at the operation grain.** `ApprovedResource` has no MCP variant; the
  natural one is a `(namespaced operation, transport identity)` pair, which
  would let the interactive and unattended paths share a vocabulary.
- **Durability beyond one session.** `approved_resources` is per-session state;
  "always allow this server's read-only operations" belongs in config or a
  durable per-server record, not a field every new session starts empty.
- **One policy knob.** `permission` should mean one thing across Bash, the
  sandbox and MCP, or the MCP axis should be its own named, documented field.
  Today's silent divergence between `free` and the MCP gate is the bug that
  triggered this note.
- **Per-server override.** Whatever the default, `.mcp.json` should be able to
  say "this server never needs asking" the way an embedded profile can — the
  request that started this work.

## Related

- `docs/permission.md` — the Bash/sandbox policy this deliberately does not
  reach.
- `docs/todo/approval-gate-merge.md` — the gate plumbing a restored MCP gate
  would ride on; worth landing first so a new prompt surfaces once, durably,
  rather than four times.
