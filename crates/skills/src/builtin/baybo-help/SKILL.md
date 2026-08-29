---
name: baybo-help
version: 0.1.0
description: "Explain Baybo's core concepts and troubleshoot Baybo problems with runtime evidence, local docs, or local source; use the Baybo GitHub repository only as a fallback."
when_to_use: "Use when the user asks what a Baybo concept means or reports a problem involving an agent, session, turn, channel, model, tool, skill, trace, gateway, workspace, or configuration."
command: baybo-help
argument-hint: "[question or error]"
user-invocable: true
allowed-tools:
  - Bash
  - Read
  - Grep
  - Glob
---

# Baybo concepts and troubleshooting

## Core concepts

Use these terms precisely when translating a user's symptom into the subsystem
that owns it. For a conceptual question, explain only the relevant entries and
their boundaries rather than dumping the entire glossary.

| Concept | Meaning and diagnostic boundary |
|---|---|
| [Gateway](https://github.com/booiris/baybo/blob/master/docs/modules/gateway.md) | Baybo's headless backend. It hosts the shared manager graph, admin/dashboard API, chat WebSockets, and sidecar connections. A connectivity or process problem belongs here; one failed conversation usually belongs to its session or turn instead. |
| [Agent profile / persona](https://github.com/booiris/baybo/blob/master/docs/modules/agent-profiles.md) | The identity and execution choice a top-level chat binds to. The database row carries framework/model metadata; its persona directory carries identity, skills, and built-in memory. A session's binding is fixed at creation. This is distinct from a spawned `SubagentProfile`. |
| [Session](https://github.com/booiris/baybo/blob/master/docs/modules/session.md) | A durable conversation boundary: metadata, agent/channel binding, transcript, and the root of one trace tree. Its in-memory actor may be reaped and later hydrated without removing the session or transcript. |
| [Turn](https://github.com/booiris/baybo/blob/master/docs/modules/turn.md) | One tracked unit of work inside a session, with a lifecycle such as pending, in progress, completed, failed, or cancelled. It answers whether work is running and how it ended; detailed LLM/tool activity belongs to Trace. Some internal operations are turns even when they are not visible chat replies. |
| [Channel](https://github.com/booiris/baybo/blob/master/docs/modules/channels.md) | The ingress/egress path connecting a transport such as owner web/mobile, TUI, or a bot to the agent. Channel registration, transport connection, and the conversation session are related but separate states. |
| [Tool / Skill](https://github.com/booiris/baybo/blob/master/docs/modules/skills.md) | A Tool is an atomic operation with an execution and governance boundary. A Skill is declarative guidance that selects and orchestrates tools under additional constraints; invoking a skill is not itself the external side effect. See the [tool system](https://github.com/booiris/baybo/blob/master/docs/modules/tools.md) for the execution side. |
| [Trace / Log](https://github.com/booiris/baybo/blob/master/docs/modules/trace.md) | Trace is structured, sanitized execution evidence in the `Session > Turn > Step > Span` hierarchy and answers what an operation did. Logs are rolling process/sidecar diagnostics. Use session export for LLM/tool provenance and log commands for runtime messages. |
| [Workspace](https://github.com/booiris/baybo/blob/master/docs/modules/workspace.md) | The persistent root from which config, personas, state, scratch work, and logs are resolved. `state/` is durable runtime data; `work/tmp/` is disposable. Never treat the whole workspace as a cache. |
| [Context / Memory](https://github.com/booiris/baybo/blob/master/docs/modules/context.md) | Context is the current model-facing window reconstructed from the durable transcript; compaction supersedes active rows but does not delete history. Memory is separate, partitioned by agent, and may include the [built-in file memory](https://github.com/booiris/baybo/blob/master/docs/modules/memory-builtin.md) and an optional pluggable backend. |

The practical hierarchy is `Session > Turn > Step > Span`: Session owns the
conversation, Turn owns work state, and Trace's Steps and Spans record what the
work actually did. Agent and Channel are bindings on the session; Workspace is
where their durable configuration and state resolve.

## Documentation

Use a matching local checkout's documents before the online `master` branch.
The concept links above are architecture and implementation sources of truth;
for operational details use the [CLI reference](https://github.com/booiris/baybo/blob/master/docs/modules/cli.md), [configuration reference](https://github.com/booiris/baybo/blob/master/docs/modules/config.md), or [external-command inventory](https://github.com/booiris/baybo/blob/master/docs/external-commands.md). The full entry point is the [documentation index](https://github.com/booiris/baybo/tree/master/docs).

Include only the links relevant to the answer. Online docs may be newer than
the installed binary; check `baybo --version`, prefer a matching tag or commit
when available, and disclose when an answer is based on `master`.

## Escalating to source code

The fallback repository is [booiris/baybo](https://github.com/booiris/baybo).
Inspect source only when CLI help, runtime evidence, and docs do not settle the
question, or when exact implementation behavior matters.

Before any source download:

1. Check whether Baybo source already exists in the current working tree or in
   a bounded location already named by the user or present in the task context.
   Do not recursively scan the whole home directory or filesystem. Verify a
   candidate from its root `Cargo.toml`, `crates/baybo`, and, when available,
   a git remote pointing at `booiris/baybo`.
2. If a checkout exists, use it. Do not download a duplicate. Note its commit
   or branch and compare it with the installed version before drawing a
   version-sensitive conclusion.
3. If no checkout exists, tell the user that none was found, show the repository
   link, and explicitly ask whether you may download it for further diagnosis.
   Stop and wait for an affirmative answer. Do not run `git clone`, download an
   archive, or fetch individual source files before that answer.
4. After approval, download into a new scratch directory without overwriting
   user files. Prefer a shallow clone unless history or a particular release is
   needed. Inspect before building or executing; do not expand the task into a
   build, dependency download, or code modification unless it is necessary and
   authorized.

If the user declines the download, continue with the available CLI evidence and
docs, state the remaining uncertainty, and leave the repository link as the
next manual step.

## Baybo-specific safety

- Never expose admin tokens, API keys, the master key, or unredacted secret
  values in commands, logs, or the answer.
- Session rows and transcripts are core user data. Never propose deleting
  sessions, `storage.db`, or the whole workspace as routine cleanup or repair.
  Hiding a session is the recoverable user-facing removal mechanism.
