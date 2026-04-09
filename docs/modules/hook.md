# hook - Lifecycle Hook System

## Overview

The `hook` crate provides uniform lifecycle extension points for auditing, rewriting, interception, and alerting — without intruding into core execution flow.

Core responsibilities:

- Trigger extension logic at key lifecycle points
- Allow extensions to modify context or abort the flow
- Keep security, auditing, and operations logic decoupled from the `agent` main loop

## Design Decisions

### Hook points

Extension points defined by `HookPoint`: PreMessage, PostMessage, PreLLMCall, PostLLMCall, PreToolExecution, PostToolExecution, PreResponse, PostResponse, SessionCreated, SessionDestroyed, CostLimitReached, JobStatusChanged.

### Serial execution model

Hooks for the same point execute serially in registration order (not parallel) because:

- A later hook may depend on changes from an earlier hook
- Hooks are often used for auditing/interception where order carries meaning
- Parallel execution introduces merge conflicts between modifications

### Three-action model

- **Continue**: no changes, proceed
- **ContinueWith**: modify context (merge-by-field, not replace) and proceed
- **Abort**: stop the current flow with an error string

Merge-by-field prevents one hook from accidentally erasing context written by another.

### Critical vs non-critical hooks

Critical hook failure aborts the main flow; non-critical hook failure is logged but does not affect execution. Determined by metadata at registration time.

### Typical use cases

- `PreMessage`: attach audit labels
- `PreLLMCall`: inject extra context or metrics
- `PostToolExecution`: business audit logs
- `PreResponse`: uniform response wrapping
- `CostLimitReached`: operational alerting
- `JobStatusChanged`: external dashboard sync

## Constraints

- Depends only on `channels`
- `HookContext.extra` must not contain sensitive plaintext
- Hook execution should have timeout protection to prevent external extensions from blocking the main flow

## Collaboration

| Module | Role |
|--------|------|
| `agent` | `AgentActor` / `AgentLoop` trigger hooks at key points |
| `job` | `JobStatusChanged` fires after job state changes |
| `cost` | `CostLimitReached` fires on spending limit hits |
| `channels` | `PreResponse` / `PostResponse` add logic around delivery |
