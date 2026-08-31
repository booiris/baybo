# Roadmap

Where Baybo is headed. Items with a spec link to their design doc under
[`todo/`](todo) — the spec is the source of truth over this summary; the rest are
planned but not yet spec'd. Nothing here is a promise.

Status legend — **todo**: planned, no spec yet · **design**: spec written,
implementation not started · **in progress**: partially landed on master · **on a
branch**: built but unmerged · **deferred**: consciously parked, with the reason
recorded.

## Agent core

- **Multi-agent chat entry point** — *in progress*. Sessions already bind to an agent
  profile (persona, skills, memory partition, LLM pin all land per-agent); what's
  missing is the chat-side surface itself: the web new-chat agent picker, a session
  agent chip, and agent-scoped slash completion. Spec:
  [`todo/multi-agent-chat.md`](todo/multi-agent-chat.md).
- **External agent as the session core** — *design*. Run a whole chat session on an
  external framework (Claude Code / Codex) instead of the native loop — today
  external agents serve only subagent and board runs. Spec: Phase 2 of
  [`todo/multi-agent-chat.md`](todo/multi-agent-chat.md).
- **Cancelled-turn auto-recovery** — *design*. A cron-triggered turn interrupted
  *after* dispatch is cancelled by boot recovery but never re-fired, because the
  scheduler has already advanced. A post-recovery sweep should re-dispatch or
  surface it as failed. Spec:
  [`todo/stuck-cron-job-auto-retry.md`](todo/stuck-cron-job-auto-retry.md).
- **Autonomous objectives (`/goal`)** — *todo*. Persisted objectives with
  turn-boundary continuation runs and a web Goals page; an old
  `feat/goal-autonomous-objectives` branch exists as reference but needs a heavy
  rebase over kanban-era master.
- **Plugin system** — *todo*. A unified plugin mechanism for packaging and
  distributing extensions beyond today's per-seam registries (channels, tools,
  skills, MCP servers).

## Projects / kanban

The self-running board (agents working issues in per-issue git worktrees, run
ledger, approvals, budgets) is shipped; [`todo/kanban.md`](todo/kanban.md) records
the five gaps that were deliberately deferred, each with its reason:

- a merge gate that verifies review actually happened before an agent merges;
- mid-turn comment injection into a running issue turn;
- push notifications for board activity;
- re-pointing a cron job at another board;
- a planning conversation with the project lead.

Beyond those recorded gaps, planned work — all *todo*:

- **Agent communication architecture** — rework how the agents on a board
  communicate with each other.
- **Issue creation-chain tracing** — make an issue's full origin chain (chat, cron,
  another agent) traceable.
- **Worktree optimization** — streamline the per-issue git worktree lifecycle.
- **Kanban control from chat** — drive boards and issues from a chat conversation.
- **Multi-agent issue execution** — let several agents work a single issue
  together.

## Security & sandboxing

- **A real MCP permission model** — *design*. Today MCP tool calls raise **no
  approval prompt at all** — a deliberate interim state: the previous approval rule
  was removed because it gated on the server's transport rather than on what an
  operation does. The roadmap item is per-operation classification from MCP's
  read-only/destructive hints,
  operation-grain durable grants, and one policy knob shared with Bash. Spec:
  [`todo/mcp-tool-approval.md`](todo/mcp-tool-approval.md).
- **One persisted approval record** — *design*. Collapse the live in-memory channel
  approval gate and the kanban card's timeline decorator into a single durable
  record, structurally removing the phantom "waiting on you" card a dropped future
  can leave behind. Spec:
  [`todo/approval-gate-merge.md`](todo/approval-gate-merge.md).
- **Sandbox follow-ups** — *deferred*. Kernel-level per-host network enforcement on
  Linux/Docker (`allowed_hosts` is advisory there today; enforced on macOS),
  sandboxing MCP stdio server spawns, a `[sandbox]` config section, and a
  configurable Docker image. Spec:
  [`todo/sandbox-os-isolation.md`](todo/sandbox-os-isolation.md).
- **Slash-command authorization** — *design*. Operator slash commands have no notion
  of *who* invoked them; before operator dispatch is ever wired to a multi-user
  channel, a `SlashPrincipal` plus a fail-closed allowlist must land. Spec:
  [`todo/slash-account-authorization.md`](todo/slash-account-authorization.md).

## Channels & clients

- **Discord / Slack channels** — *todo*. A config surface exists for Discord
  (nothing yet for Slack, though `ChannelType` is open-ended), but no sidecar exists
  for either — configuring Discord today has no backend. The channel SDK seam is
  the intended path.
- **Desktop app** — *on a branch*. A Tauri 2 thin client wrapping the web UI
  (`feat/desktop-app`); predates the repo-wide rename and directory moves, so it is
  heavily bitrotted.
- **Android support** — *todo*. An Android counterpart to the iOS companion app.
- **Voice on iOS** — *todo*. Voice input/output in the iOS app.

## Remote-host

- **NAT hole punching** — *todo*. Direct client↔gateway connectivity across NATs,
  so the relay drops out of the data path once a tunnel is established.

## Platform & infrastructure

- **Crate-boundary enforcement** — *design*. Fix the one live violation of the
  "hand over verbs, not stores" rule (the agent crate writes the cron delivery
  ledger straight past its port) and add a baseline-frozen CI check that flags any
  `dyn XxxStore` escaping its owning crate. Spec:
  [`todo/crate-boundary-gate.md`](todo/crate-boundary-gate.md).
- **Cross-crate contract dedup** — *design*. A classified backlog of rules
  duplicated across the main workspace, the iOS FFI, and remote-host (push-token
  validation, device-id constants, frontmatter parsers) — goal: one owner per rule,
  drift caught by tests. Spec:
  [`todo/cross-crate-contract-dedup.md`](todo/cross-crate-contract-dedup.md).
