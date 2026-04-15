# security - Security Primitives

## Overview

The `security` crate provides low-level security primitives: cryptographic operations (`EncryptionKey`, `encrypt`/`decrypt`), leak detection (`LeakDetector`, `LeakDetectionRule`), and the `SecurityError` error type.

Business logic (`SecretVault`, `SecretValue`, `SecurityGateway`) lives in `agent::security`. The `SecretStore` trait is defined in `storage::secret`.

Core responsibilities of the primitives in this crate:

- **Leak detection**: identify API keys, passwords, tokens in content blocks via regex rules
- **Placeholder replacement**: replace sensitive plaintext with `{{SECRET_xxx}}`
- **AES-256-GCM encryption**: encrypt/decrypt secret values with a master key

Business logic in `agent::security` builds on these primitives:

- **SecretVault**: encrypt and store real secrets (tool-scoped injection is deferred pending the finalized tool system)
- **SecurityGateway**: input sanitization, output re-sanitization, session placeholder mapping
- **SecretValue**: redacted wrapper preventing plaintext in Debug/Display

## Design Decisions

### Input sanitization flow

Messages pass through `SecurityGateway::sanitize_input()` immediately after channel ingress. The leak detector scans content blocks, generates unpredictable placeholders, and stores the mapping. **The context that enters Agent may only see placeholders, never raw secrets.**

### Output re-sanitization

Before any response leaves the system, it passes through `sanitize_output()` again. Placeholders are kept as-is; if the response reconstructs secret-like content, it is matched and replaced. **Never perform reverse mapping from placeholders back to plaintext.**

### SecretVault encryption

Secrets are encrypted with AES-256-GCM (random nonce + ciphertext + tag). The master key exists only in process memory and is never persisted. `SecretValue` should not support plaintext `Debug`.

### Least-privilege injection (deferred)

Per-tool secret declaration and `ScopedSecretAccessor` were removed pending the
finalized tool system. Until they return, `SecretVault` only backs
`SecurityGateway` placeholder storage; tools receive no secrets through
`ToolContext`.

### Network decision boundary

Security only decides allow/deny. It does not execute network access. The chain is: manifest + admin config + runtime request → `NetworkPolicyDecider::decide()` → tool executes. This separates permission decisions from execution.

## Constraints

- Primitives crate — no session/channel/storage dependencies
- Trace records only sanitized `SpanInput` and `SpanResult`
- Job `input/output` stores sanitized versions only
- Structured logs must not print `SecretValue` directly
- Placeholder generation should use unpredictable random suffixes to avoid collisions

## Collaboration

| Module | Role |
|--------|------|
| `channels` | Input messages go to `agent::security::SecurityGateway` first |
| `agent` | `agent::security::SecurityGateway` and `SecretVault` own business logic |
| `trace` / `job` | Receive only sanitized payloads and placeholders |
| `storage` | Defines `SecretStore` trait; provides libsql implementation |
