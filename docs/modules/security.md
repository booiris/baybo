# security - Security Primitives

## Overview

The `security` crate provides low-level security primitives: cryptographic operations (`EncryptionKey` is re-exported at the crate root; the `encrypt`/`decrypt` functions are reached as `crypto::encrypt` / `crypto::decrypt`), encrypted secret storage (`SecretVault`, `SecretValue`, plus `SecretVault::list_names`), the user-managed env-secret policy layered over the vault (`UserSecretManager`, `AddOutcome`, `USER_SECRET_PREFIX` — namespaces user secrets under `user_env.<NAME>` and is the single source of truth for env-var-name validation and masked previews), leak detection (`LeakDetector`, `LeakDetectionRule`, `LeakMatch`, `LeakScanResult`, `LeakAction`), deterministic placeholder minting (`PlaceholderMinter`), prompt-injection detection (`InjectionDetector`, `InjectionWarning`, `InjectionSeverity`), filesystem path sensitivity checks (`is_sensitive_path`), log redaction (the `log_redact` module with `RedactingMakeWriter` / `RedactingWriter`), and the `SecurityError` error type.

The gateway (`SecurityGateway`) lives in `agent::security` — it holds session-scoped state and orchestrates scanning, minting, and reveal across the agent loop. The `SecretStore` trait lives in the `baybo-store` ports crate; `SecretVault` and the rest of the crypto surface live here, and `baybo-storage` provides the libsql implementation. `MemorySecretStore` for downstream tests is exposed via the `test-support` feature gate.

Core responsibilities of the primitives in this crate:

- **Leak detection**: identify API keys, passwords, tokens in content blocks via regex rules. `scan_text` / `scan_content_blocks` return `LeakScanResult { matches, blocked, block_reason }`; substitution is the caller's responsibility. Default rule set covers AWS (access key, secret key), Google (API key, OAuth), GitHub (ghp/gho/ghu/ghs/ghr/pat variants), GitLab PAT, npm token, Anthropic (API key + OAuth `sk-ant-oat…`), OpenAI, OpenRouter (`sk-or-v1-…`), Groq (`gsk_…`), Stripe (live/test), Slack, SendGrid, Twilio (`SK…`), Telegram bot tokens (`<id>:AA…`), NEAR AI session cookies (`sess_…`), JWTs, PEM private keys, generic `api_key=`/`bearer …`/`Authorization: …`/`password=` assignments, and a 64-char high-entropy hex fallback.
- **Deterministic placeholder minting**: `PlaceholderMinter::from_master_key(&EncryptionKey)` derives a per-process HMAC key via HKDF-SHA256 (info `b"baybo-placeholder-v1"`). `mint(secret_bytes)` returns `[{REDACTED_SECRET_<24-hex>}]` — the hex is the first 24 chars of HMAC-SHA256(subkey, secret). Identical secrets therefore always mint the same placeholder, so the vault accumulates at most one entry per unique secret. The delimiters are the asymmetric ASCII pairs `[{` and `}]`: mismatched outer/inner brackets cannot be parsed by Handlebars/Jinja/Mustache `{{…}}`, shell/JS `${…}`, or ERB `<%…%>`, while pure ASCII guarantees every LLM can echo the placeholder byte-for-byte when it flows back through tool arguments — a precondition for `reveal_in_text` to hit the vault.
- **Placeholder regex**: `PlaceholderMinter::placeholder_regex()` exposes a cached `\[\{REDACTED_SECRET_[0-9a-f]{16,32}\}\]` matcher for reveal passes.
- **Prompt-injection detection**: `InjectionDetector::with_default_rules()` returns a detector built from an Aho-Corasick literal matcher (override phrases like "ignore previous", role manipulation, fake turn prefixes `system:` / `assistant:` / `human:`, control tokens `<|im_start|>` / `[INST]` / `<s>`, forged `<tool_output>` delimiters, `\`\`\`system` code fences) plus regex rules for base64 payloads, `eval(`/`exec(` calls, and null bytes. `scan` returns a `Vec<InjectionWarning>`; the detector never rewrites content — callers log or block based on `InjectionSeverity`.
- **Sensitive-path checks**: `is_sensitive_path(&Path) -> bool` matches credential-bearing locations (`~/.ssh/`, `~/.aws/`, `~/.azure/`, `~/.gcloud/`, `~/.kube/`, `~/.docker/`, `~/.gnupg/`, `~/.netrc`, `/etc/shadow`, shell-history files, `.env` variants, `*.pem` / `*.key` / `*.p12` / `*.jks`, `id_rsa` / `authorized_keys`). `.env.example`/`.env.template`/`.env.sample`/`.env.dist` and `*.dist` suffixes are allowed. Paths are canonicalized to defeat symlink bypass.
- **AES-256-GCM encryption**: encrypt/decrypt secret values with a master key.
- **SecretVault**: encrypt and persist real secrets through an injected `Arc<dyn SecretStore>`; exposes `master_key()` so the gateway can bootstrap its `PlaceholderMinter` from the same key material. Maps underlying `StorageError` into `SecurityError::Storage`.
- **SecretValue**: redacted wrapper preventing plaintext in Debug/Display.

The gateway in `agent::security` builds on these primitives:

- **SecurityGateway**: input/output sanitization, LLM-response defensive scrubbing, stream-fragment scrubbing, tool-output secret scrubbing and prompt-injection scanning, error-string scrubbing, and the reveal API. The tool-output *framing* — structural wrapping, the length cap, and `MAX_TOOL_OUTPUT_BYTES = 32 KiB` — lives in `baybo-context` (`crates/context/src/prompts/tool_output.rs`), not here.

## Design Decisions

### Input sanitization flow

Messages pass through `SecurityGateway::sanitize_input()` immediately after channel ingress. The leak detector identifies matches; the gateway mints a deterministic placeholder per unique secret, upserts the plaintext into `SecretVault` (idempotent), and replaces each match in every text block. A session-scoped `{placeholder → rule_name}` map is maintained for audit. **The context that enters Agent, memory, and trace may only see placeholders, never raw secrets.**

### LLM-response defensive scrubbing

`SecurityGateway::sanitize_llm_response(&mut LlmResponse)` is called in `AgentLoop::call_llm` *before* the response is recorded to the trace, pushed onto `session.messages`, or passed to the memory manager. It scans `content`, each `ContentBlock::Text`, `thinking`, and every string leaf inside `tool_calls[*].arguments`, minting placeholders and writing them back in place. An LLM that fabricates a secret-shaped string therefore cannot leak it through any downstream sink — the JSON dump stored by `SpanResult::LlmResponse` sees only placeholders.

### Output re-sanitization

Before any response leaves the system, `sanitize_output()` runs again. Placeholders flow through unchanged; any newly-matched secret-like content is minted and vaulted. **Non-streaming `OutgoingMessage` keeps placeholder form — no reveal on the final egress.**

### Stream-delta buffering

Streaming output never reveals plaintext. `AgentLoop::chat_streaming` buffers chunks in a small `pending: String` and calls `safe_flush_boundary` to find the largest prefix that cannot contain a partial placeholder. Both the `safe_flush_boundary` free function and the `STREAM_BUFFER_HIGH_WATER` const live in `crates/agent/src/runtime/agent_loop.rs`, not on `SecurityGateway`. The rule: locate the last `[{`; if its tail lacks `}]`, withhold from that `[{`. A lone trailing `[` is also withheld in case the next chunk completes it into `[{`. The buffer is capped at 128 bytes (`STREAM_BUFFER_HIGH_WATER`) to bound worst-case memory.

For the flushable prefix the gateway runs the scan/mint/vault pipeline, and the *scanned* (placeholder-bearing) text is both:

- appended to the `LlmResponse.content` accumulator the caller returns — so trace, memory, and session-message persistence all receive placeholders;
- sent as-is to `delta_tx`, so the streaming view and the final persisted message agree character-for-character.

### Reveal API

`SecurityGateway::reveal_in_text(&str)` and `reveal_in_value(&mut serde_json::Value)` substitute every known placeholder with its vaulted plaintext. Reveal is **vault-global**: any placeholder present in the vault is revealable regardless of session. Unknown placeholders (e.g. LLM-fabricated strings matching the regex but without a vault entry) are passed through unchanged, with a `warn!` that logs only a SHA-256 fingerprint prefix — never the placeholder body.

`reveal_in_value` walks JSON recursively; only `Value::String` leaves are substituted, and object keys are left untouched. No serialize/parse round trip is performed.

### Tool-argument reveal boundary

`ToolExecutor::execute` captures `SpanInput::ToolExecution { parameters: params.clone() }` (placeholder form) *before* the reveal happens, then calls `reveal_in_value` on a separate copy and passes that copy to the tool. The trace and approval prompt see placeholders; the tool receives plaintext for its real API call. On return, `sanitize_tool_output` runs on the `ToolOutput` so that any echoed secret is re-minted and vaulted before the value flows into the trace, the next LLM call's `ToolResult`, or memory.

### Prompt-injection defense

Injection markers (`ignore previous`, fake turn prefixes, ChatML/LLaMA control tokens, forged `<tool_output>` tags, etc.) are scanned at two points:

- **Inbound messages** (`sanitize_input`): every text block is scanned; hits are logged via `tracing` at a level that scales with severity (`Critical`/`High` → `warn`, `Medium` → `info`, `Low` → `debug`). User input is not blocked — legitimate prose contains many of these literals — but the logs give operators a signal.
- **Tool output** (`sanitize_tool_output` in this crate + `wrap_tool_output` in `baybo-context`): warnings are logged the same way, and `wrap_tool_output` prepends an inline security banner naming the triggered rules (passed in as `warning_rules`) inside the `<tool_output>` envelope so the LLM treats the content as untrusted data rather than instructions.

### Tool-output structural wrapping

`baybo_context::prompts::tool_output::wrap_tool_output(tool_name, content, warning_rules)` (in `baybo-context`, not this crate) wraps the result in `<tool_output name="...">...</tool_output>`. The tool name is XML-attribute-escaped; any literal `</tool_output` inside the body is neutralized with a zero-width space between the slash and the tag name so untrusted content cannot forge the closing boundary. `warning_rules` are the injection-marker rule names the caller pulled from this crate's `InjectionDetector::scan`; passing them as plain strings keeps `baybo-context` free of an `baybo-security` dependency. The agent loop applies this wrap to every tool result before appending it to `ContentBlock::ToolResult`.

### Tool-output length cap

`baybo_context::prompts::tool_output::cap_tool_output(content, spill_path)` (in `baybo-context`, not this crate) truncates to `MAX_TOOL_OUTPUT_BYTES` on a UTF-8 char boundary and appends a truncation notice; when `spill_path` is set the notice points the model at the full payload (readable back via the `Read` tool). The cap runs **before** wrapping so the notice lands inside the `<tool_output>` envelope. Individual tools keep their own tighter bounds (`Bash` 64 KiB, `WebFetch` 256 KiB, `Grep` 500 hits, `Read` 2000 lines × 2000 chars/line, `Glob` 1000 paths) — this cap is a final defense for any tool that didn't apply one.

### Sensitive-path filter

Because `ResourceAccess::ReadFile` bypasses the approval gate (see `ToolExecutor::execute`), file reads have no per-call user confirmation. `ReadTool` instead rejects the call outright when `baybo_security::is_sensitive_path` matches, returning a `ToolError::Execution` with a message the LLM can relay to the user. Any future file-reading tool must apply the same check at its entry point.

### SSRF floor

`WebFetch` mostly bypasses the approval gate too — hostname URLs and literal IPs in reserved ranges declare no `ResourceAccess` (see *WebFetch host-shape policy* in `docs/modules/tools.md`), so the per-call SSRF check inside the tool is the load-bearing guard rather than the gate. Two layers cover this:

1. **Parse-time literal-IP filter** in `validate_url_with`. Rejects non-`http(s)` schemes, `localhost` family hostnames, and any literal IP that `baybo_security::is_blocked_ip` flags (loopback, RFC1918, link-local 169.254/16 — covers AWS metadata, CGNAT 100.64/10, IPv6 ULA fc00::/7, link-local v6 fe80::/10 — covers metadata, unspecified, IPv4-mapped-v6 forms of any of the above). WHATWG IPv4 canonicalisation by `url::Url` means alternate encodings (`2130706433`, `0x7f000001`, `0177.0.0.1`, `127.1`) reach this check in dotted form and get rejected normally.
2. **DNS-time resolved-IP filter** in `SafeResolver` (custom `reqwest::dns::Resolve`). Resolves the hostname via `tokio::net::lookup_host`, then drops every address `is_blocked_ip` flags before handing the survivors to the connector. If every resolved address is blocked the connection fails with `host ... resolved only to blocked IP ranges`. This closes DNS-rebinding-into-LAN attacks: an attacker-controlled hostname that resolves to `10.0.0.1` never becomes a connect target. Each redirect hop runs a fresh resolution, so rebinding inside a single redirect chain is also caught. There is no TOCTOU window between the check and `connect()` — the connector receives a `Vec<SocketAddr>` of pre-vetted IPs and connects to one of those, never re-resolving.

Out of scope: the SSRF floor is an RFC-level deny list, not topology-aware. A literal *public* IP that happens to point at internal infrastructure (cloud VPC, professional-line backend, public-IP admin port) is the one shape neither layer can decide on its own — that case routes to the approval gate via `ResourceAccess::Http { host }` so a human catches it. Public hostnames that resolve to public IPs which are actually internal still slip through; egress-firewall / network-segmentation is the only real fix and lives outside the tools layer.

Any future HTTP-emitting builtin must apply the same two layers (`validate_url_with` + a `SafeResolver`-equivalent custom DNS resolver) at its entry point — the approval gate alone is not a substitute for the SSRF floor.

### SecretVault encryption

Secrets are encrypted with AES-256-GCM (random nonce + ciphertext + tag). The master key exists only in process memory and is never persisted. `SecretValue` should not support plaintext `Debug`.

### Known vault entries

The vault stores both deterministically-minted secrets (one entry per unique
secret, keyed by placeholder) and a small set of fixed-name application
records:

| Vault key                 | Owner                    | Format                        | Purpose                                                         |
| ------------------------- | ------------------------ | ----------------------------- | --------------------------------------------------------------- |
| `[{REDACTED_SECRET_<hex>}]` | `SecurityGateway` reveal | raw secret bytes              | Plaintext for placeholder reveal at the tool-argument boundary  |
| `baybo.tui.input_history`  | `TuiHistoryStore` (gateway) | UTF-8 JSON `Vec<String>`   | Persistent TUI input ring (see [`tui.md`](./tui.md))            |

`baybo_gateway::channel::TuiHistoryStore` wraps `Arc<SecretVault>` behind a
`tokio::sync::Mutex` to load and append the TUI input history under that
key. The mutex serialises the read-modify-write inside the gateway process,
so concurrent `baybo tui` clients connected to the same gateway never lose
each other's entries — and because the gateway is the *only* writer, no
cross-process file lock is needed. TUI clients never open the vault
themselves; they exchange the ring over the channel WS via
`Frame::HistorySnapshot` (server→client, once after register) and
`Frame::HistoryAppend` (client→server, one per submission). The history is
encrypted with the same master key as everything else in the vault, so
commands or pasted credentials typed into the TUI never appear as
plaintext on disk.

### Least-privilege injection (deferred)

Per-tool secret declaration and `ScopedSecretAccessor` were removed pending the
finalized tool system. Until they return, `SecretVault` backs
`SecurityGateway` placeholder storage and reveal; tools receive plaintext only
through the tool-argument reveal boundary described above, never via
`ToolContext`.

### Network decision boundary

Security only decides allow/deny. It does not execute network access. There is no central network-policy decider — the SSRF guard is inline in `WebFetch::validate_url_with` (parse-time literal-IP rejection via `baybo_security::is_blocked_ip`) plus the per-fetch `SafeResolver` (DNS-time resolved-IP filter). Process-level network containment for sandboxed tools comes from `baybo_sandbox::NetworkPolicy::{None, All}` at spec-build time. This separates permission decisions from execution.

## Constraints

- Primitives crate — no session/channel/storage dependencies
- Trace records only sanitized `SpanInput` and `SpanResult` (including tool-call arguments)
- Job `input/output` stores sanitized versions only
- Structured logs must not print `SecretValue` directly; reveal warnings log only a SHA-256 fingerprint prefix
- Placeholder generation is deterministic per secret — same secret → same placeholder → single vault entry
- The only code path permitted to hold plaintext at egress is the tool executor's post-reveal `params_revealed` on its way into `tool_registry.execute`; stream deltas, outgoing messages, trace, memory, and persistence all carry placeholder form
- Injection detection is log-only at inbound and log-plus-wrap at tool output; never auto-block user input on injection markers alone
- Any tool that reads filesystem paths MUST apply `is_sensitive_path` at its entry point, regardless of approval-gate status
- Any tool that emits HTTP MUST apply both layers of the SSRF floor (`validate_url_with`-equivalent literal-IP rejection + a `SafeResolver`-equivalent DNS-time resolved-IP filter using `is_blocked_ip`) at its entry point, regardless of approval-gate status

## Collaboration

| Module | Role |
|--------|------|
| `channels` | Input messages go to `agent::security::SecurityGateway` first |
| `agent` | `agent::security::SecurityGateway` and `SecretVault` own business logic |
| `trace` / `job` | Receive only sanitized payloads and placeholders |
| `storage` | Defines `SecretStore` trait; provides libsql implementation |
