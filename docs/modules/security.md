# security - Security Gateway and Secret Management

## Overview

The `security` crate is the security boundary both before external input enters Agent and before responses leave the system.

Core responsibilities:

- **Input leak detection**: identify API keys, passwords, tokens in messages
- **Placeholder replacement**: replace sensitive plaintext with `{{SECRET_xxx}}`
- **Secret vault**: encrypt and store real secrets, inject with least privilege per tool declarations
- **Output re-sanitization**: keep only placeholders and sanitized summaries in responses, logs, Trace, and Job
- **Network policy decisions**: provide allow/deny decisions for the execution layer

`security` does not execute tools, launch containers, or open network access. Those belong to `agent` and `sandbox`.

## Design Decisions

### Input sanitization flow

Messages pass through `SecurityGateway::sanitize_input()` immediately after channel ingress. The leak detector scans content blocks, generates unpredictable placeholders, and stores the mapping. **The context that enters Agent may only see placeholders, never raw secrets.**

### Output re-sanitization

Before any response leaves the system, it passes through `sanitize_output()` again. Placeholders are kept as-is; if the response reconstructs secret-like content, it is matched and replaced. **Never perform reverse mapping from placeholders back to plaintext.**

### SecretVault encryption

Secrets are encrypted with AES-256-GCM (random nonce + ciphertext + tag). The master key exists only in process memory and is never persisted. `SecretValue` should not support plaintext `Debug`.

### Least-privilege injection

`get_secrets_for_tool()` returns only secrets explicitly declared by the tool via `required_secrets()`. Security does not understand tool business logic — it only returns the minimal set based on declarations.

### Network decision boundary

Security only decides allow/deny. It does not execute network access. The chain is: manifest + admin config + runtime request → `NetworkPolicyDecider::decide()` → sandbox executes. This separates permission decisions from execution isolation.

## Constraints

- Depends only on `core`
- Trace records only sanitized `SpanInput` and `SpanResult`
- Job `input/output` stores sanitized versions only
- Structured logs must not print `SecretValue` directly
- Placeholder generation should use unpredictable random suffixes to avoid collisions

## Collaboration

| Module | Role |
|--------|------|
| `channels` | Input messages go to `SecurityGateway` first |
| `agent` | `ToolExecutor` retrieves secrets from `SecretVault` and injects into tools |
| `sandbox` | Consumes allow/deny decisions from `NetworkPolicyDecider` |
| `trace` / `job` | Receive only sanitized payloads and placeholders |
| `storage` | Provides `SecretStore` implementations |
