# security - Security Primitives

## Overview

The `security` crate provides low-level security primitives: cryptographic operations (`EncryptionKey` is re-exported at the crate root; the `encrypt`/`decrypt` functions are reached as `crypto::encrypt` / `crypto::decrypt`), encrypted secret storage (`SecretVault`, `SecretValue`, plus `SecretVault::list_names`), the user-managed env-secret policy layered over the vault (`UserSecretManager`, `AddOutcome`, `USER_SECRET_PREFIX` — namespaces user secrets under `user_env.<NAME>` and is the single source of truth for env-var-name validation and masked previews), leak detection (`LeakDetector`, `LeakDetectionRule`, `LeakMatch`, `LeakScanResult`, `LeakAction`), deterministic placeholder minting (`PlaceholderMinter`), prompt-injection detection (`InjectionDetector`, `InjectionWarning`, `InjectionSeverity`), filesystem path sensitivity checks (`is_sensitive_path`), log redaction (the `log_redact` module with `RedactingMakeWriter` / `RedactingWriter`), and the `SecurityError` error type.

The gateway (`SecurityGateway`) lives in `agent::security` — it holds session-scoped state and orchestrates scanning, minting, and reveal across the agent loop. The `SecretStore` trait lives in the `baybo-store` ports crate; `SecretVault` and the rest of the crypto surface live here, and `baybo-storage` provides the sqlite implementation. `MemorySecretStore` for downstream tests is exposed via the `test-support` feature gate.

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

`SecurityGateway::sanitize_llm_response(&mut LlmResponse)` is called in `AgentLoop::call_llm` *before* the response is recorded to the trace, pushed onto `session.messages`, or passed to the memory manager. It scans `content`, each `ContentBlock::Text`, `thinking`, and every string leaf inside `tool_calls[*].arguments`, minting placeholders and writing them back in place. An LLM that fabricates a secret-shaped string therefore cannot leak it through any downstream sink — the `LlmCallResult` recorded at span finalize (`SpanFinalize::LlmCall`) sees only placeholders.

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

`ToolExecutor::execute` records `ToolCallBegin { params: params.clone(), .. }` (placeholder form) on the tool-call span *before* the reveal happens, then calls `reveal_in_value` on a separate copy and passes that copy to the tool. The trace and approval prompt see placeholders; the tool receives plaintext for its real API call. On return, `sanitize_tool_output` runs on the `ToolOutput` so that any echoed secret is re-minted and vaulted before the value flows into the trace, the next LLM call's `ToolResult`, or memory.

### Prompt-injection defense

Injection markers (`ignore previous`, fake turn prefixes, ChatML/LLaMA control tokens, forged `<tool_output>` tags, etc.) are scanned at two points:

- **Inbound messages** (`sanitize_input`): every text block is scanned; hits are logged via `tracing` at a level that scales with severity. User input is not blocked — legitimate prose contains many of these literals — but the logs give operators a signal.
- **Tool output** (`sanitize_tool_output` in this crate + `wrap_tool_output` in `baybo-model`): warnings are logged the same way, and `wrap_tool_output` prepends an inline security banner naming the triggered rules (passed in as `warning_rules`) inside the `<tool_output>` envelope so the LLM treats the content as untrusted data rather than instructions.

#### Severity is scoped by output provenance

The emitted level is a function of severity **and** where the scanned text came from. `emitted_level(severity, provenance)`:

| provenance | `Critical` / `High` | `Medium` | `Low` |
| --- | --- | --- | --- |
| `Foreign` | `warn` | `info` | `debug` |
| `WorkspaceLocal` | `info` | `debug` | `debug` |

An agent reading its own project's source trips these rules constantly — a `system:` line in a fixture is not an attack — and a stream of `Critical`s nobody can act on is how the one that matters gets ignored. Nothing is ever suppressed: every event keeps its `rules` and `severity` fields and gains `provenance`, so a demoted line is still greppable by severity.

`provenance` is a **resolved verdict**, not something the detector infers from the text. `ToolExecutor` computes it per call via `OutputProvenance::classify(source, accesses, checkout_root)` and puts it on `ScanOrigin`. `WorkspaceLocal` requires *all* of: the tool declares `OutputSource::DeclaredFiles`; the session has a checkout; the call declared at least one access; every access is a `ReadFile`; no declared path contains a `..` component; and every path is under the checkout root (component-wise, so `/data/kanban-evil` does not match `/data/kanban`).

That check is only sound because `DeclaredFiles` means *enumerated*, not *contained* — `classify` inspects the declared paths, so the declared paths have to be the paths the output came from. `ReadTool` is the only tool that qualifies. `GrepTool` and `GlobTool` read nothing but local files and are deliberately `Opaque`: they declare their search **root** and return content from every file beneath it, so a declared path that passes the checkout test says nothing about where the matched bytes actually live (a symlinked subtree, a vendored dependency). See `docs/modules/tools.md` → `Tool::output_source()`.

`ScanOrigin::default()` is `Foreign`, so anything unclassified keeps full severity. That covers inbound channel input (`sanitize_input` builds its origin with `..Default::default()`) and every `OutputSource::Opaque` tool — `Grep`, `Glob`, `WebFetch`, the browser and MCP tools, and `Bash`, whose output is whatever the command printed.

**Accepted residual:** a `Read` of a symlink inside the checkout whose target is outside it makes the declared path pass while the bytes come from elsewhere. The consequence is a `Critical` logged at `info` — a monitoring miss, not an escalation — and an attacker able to plant that symlink can plant the injection text directly. Not worth a `canonicalize` on every tool call.

The LLM-facing side is deliberately unchanged: `wrap_tool_output`'s security banner is emitted from the agent loop's own `detect_injection` and never sees `provenance`. The model keeps treating file content as untrusted data wherever the file lives; this is the operator's log channel only.

### Tool-output structural wrapping

`baybo_model::wrap_tool_output(tool_name, content, warning_rules)` (in `baybo-model`, not this crate — `baybo-tools` frames its out-of-band judge prompts with the same envelope and cannot depend on `baybo-context`) wraps the result in `<tool_output name="...">...</tool_output>`. The tool name is XML-attribute-escaped; any literal `</tool_output` inside the body is neutralized with a zero-width space between the `<` and the slash so untrusted content cannot forge the closing boundary. `warning_rules` are the injection-marker rule names the caller pulled from this crate's `InjectionDetector::scan`; passing them as plain strings keeps `baybo-model` free of a `baybo-security` dependency. The agent loop applies this wrap to every tool result before appending it to `ContentBlock::ToolResult`.

### Tool-output length cap

`baybo_context::prompts::tool_output::cap_tool_output(content, spill_path)` (in `baybo-context`, not this crate) truncates to `MAX_TOOL_OUTPUT_BYTES` on a UTF-8 char boundary and appends a truncation notice; when `spill_path` is set the notice points the model at the full payload (readable back via the `Read` tool). The cap runs **before** wrapping so the notice lands inside the `<tool_output>` envelope. Individual tools keep their own tighter bounds (`Bash` 64 KiB, `WebFetch` 256 KiB, `Grep` 500 hits, `Read` 800 lines default (5000 max) × 2000 bytes/line, `Glob` 1000 paths) — this cap is a final defense for any tool that didn't apply one.

### Sensitive-path filter

Because `ResourceAccess::ReadFile` bypasses the approval gate (see `ToolExecutor::execute`), file reads have no per-call user confirmation. `ReadTool` instead rejects the call outright when `baybo_security::is_sensitive_path` matches, returning a `ToolError::Execution` with a message the LLM can relay to the user. Any future file-reading tool must apply the same check at its entry point.

### SSRF floor

`WebFetch` mostly bypasses the approval gate too — hostname URLs and literal IPs in reserved ranges declare no `ResourceAccess` (see *WebFetch host-shape policy* in `docs/modules/tools.md`), so the per-call SSRF check inside the tool is the load-bearing guard rather than the gate. Two layers cover this:

1. **Parse-time literal-IP filter** in `validate_url_with`. Rejects non-`http(s)` schemes, `localhost` family hostnames, and any literal IP that `baybo_security::is_blocked_ip` flags (loopback, RFC1918, link-local 169.254/16 — covers AWS metadata, CGNAT 100.64/10, IPv6 ULA fc00::/7, link-local v6 fe80::/10 — covers metadata, unspecified, IPv4-mapped-v6 forms of any of the above). WHATWG IPv4 canonicalisation by `url::Url` means alternate encodings (`2130706433`, `0x7f000001`, `0177.0.0.1`, `127.1`) reach this check in dotted form and get rejected normally.
2. **DNS-time resolved-IP filter** in `SafeResolver` (custom `reqwest::dns::Resolve`). Resolves the hostname via `tokio::net::lookup_host`, then drops every address `is_blocked_ip` flags before handing the survivors to the connector. If every resolved address is blocked the connection fails with `host ... resolved only to blocked IP ranges`. This closes DNS-rebinding-into-LAN attacks: an attacker-controlled hostname that resolves to `10.0.0.1` never becomes a connect target. Each redirect hop runs a fresh resolution, so rebinding inside a single redirect chain is also caught. There is no TOCTOU window between the check and `connect()` — the connector receives a `Vec<SocketAddr>` of pre-vetted IPs and connects to one of those, never re-resolving.

Out of scope: the SSRF floor is an RFC-level deny list, not topology-aware. A literal *public* IP that happens to point at internal infrastructure (cloud VPC, professional-line backend, public-IP admin port) is the one shape neither layer can decide on its own — that case routes to the approval gate via `ResourceAccess::Http { host }` so a human catches it. Public hostnames that resolve to public IPs which are actually internal still slip through; egress-firewall / network-segmentation is the only real fix and lives outside the tools layer.

The floor's second consumer sits outside the tools layer: deck's host-mediated `ctx.fetch` (`crates/deck/src/host.rs`) reimplements both layers over `is_blocked_ip` — literal IPs are rejected at parse time; hostnames are resolved once, blocked addresses dropped, and the connection pinned to the vetted survivors (`resolve_to_addrs`), with redirects disabled outright.

Any future HTTP-emitting builtin must apply the same two layers (`validate_url_with` + a `SafeResolver`-equivalent custom DNS resolver) at its entry point — the approval gate alone is not a substitute for the SSRF floor — unless the destination is fixed by code or the operator and model input cannot change it. `WebSearch` is this exception: provider constructors validate HTTP(S) endpoints and disable redirects, but permit private addresses for self-hosted SearXNG. The exception does not extend to returned URLs; only `WebFetch` may dereference them under the full SSRF floor.

### SecretVault encryption

Secrets are encrypted with AES-256-GCM. `SecretValue` should not support plaintext `Debug`.

**Record format.** `nonce(12) || ciphertext || tag(16)`, with a fresh random
nonce per encryption and the **entry name passed as associated data**. The AAD
binding is the point: one master key encrypts every row, so without it any
ciphertext decrypts correctly under any name, and an attacker who can write the
store can move `llm.entry.cheap.api_key`'s ciphertext onto
`gateway.admin_token` and have it open cleanly. With it, a record is only valid
where it was written.

Deliberately unversioned. A leading marker would discriminate nothing — one
format exists — and could not disambiguate anything anyway, since the first byte
of a random nonce collides with any marker once in 256. If the format ever does
change, the discriminator has to come from outside the record (a column, a
per-store flag), never from a prefix.

An **empty** `aad` is rejected by both `encrypt` and `decrypt`. AES-GCM treats
"no associated data" and "empty associated data" as the same input, so records
written that way would be interchangeable with each other — the property the
binding exists to remove. Refusing the argument makes an unbound record
unrepresentable rather than merely unusual.

The practical consequence: **a vault written by a build predating this format
does not decrypt at all.** There is no conversion path in the tree — a workspace
in that state is re-provisioned (`baybo setup`, re-pair devices, re-enter
`user_env.*` and provider keys), not migrated.

**Key rotation.** `baybo vault rotate` mints a new master key, re-encrypts every
entry under it, and replaces the key file. Shell-only.

Three things gate it, and each is enforced where it cannot be skipped rather
than asked of the caller:

- **The workspace singleton lock is held for the whole run**, not checked and
  released. A gateway that started midway would write an entry under the
  outgoing key — outside the snapshot being re-encrypted — and that entry
  becomes unreadable the moment the new key is promoted. `key_file::rotate`
  takes a `&WorkspaceLock`, so it cannot be called without one.
- **The operator types the outgoing key**, masked. A `[y/N]` proves someone
  pressed a key; producing the key proves they hold it somewhere other than this
  disk, which is exactly what rotation is about to invalidate.
- **A backup is written first**, inside `rotate` itself: the outgoing key plus
  the current `secrets` rows as a restorable SQL transaction, in one `0700`
  directory. Deliberately *not* a copy of the database — rotation touches one
  table, so copying transcripts and traces would cost hundreds of megabytes for
  no recovery value. The two files are jointly sufficient and individually
  useless. Restore is `cp <backup>/encryption.key <key path>` plus
  `sqlite3 <db> < <backup>/secrets.sql`; no bespoke command to maintain.

**Surviving a crash.** Rotation changes two things that cannot be committed
together: the key file and every ciphertext in sqlite. Ordering is what makes
the gap survivable — write the new key to `<live key path>.pending`, re-encrypt
in one transaction, then `rename` pending over the live key. A crash before the
commit leaves ciphertext under the old key, which the live file still holds; a
crash after it leaves ciphertext under the pending key. Exactly one of the two
opens the vault, and `key_file::resolve_pending` determines which by decrypting
a real entry rather than by inspecting on-disk bookkeeping. Neither working is a
hard error, not a silent start.

The pending path is **derived** from the live one rather than resolved
separately: `security.encryption_key_file` is operator-configurable, so a
pending path computed from the workspace default would have rotation promote a
key the boot path never reads.

`key_file::resolve_pending` — not a bare read — is what **every** vault-opening
path must call. `boot::load_encryption_key` does, which is why it takes the
secret store: "which key is live" is a question about the vault, not the
filesystem. A path that skipped it would come up with the pre-rotation key and
fail every decrypt.

**Two consequences.** The old key stops working the moment rotation completes,
so any copy of it — a backup, another machine restored from the same snapshot —
can no longer read this vault; that is the point, but it also means rotation is
irreversible without the old key *and* the old ciphertext, which is what the
backup preserves. And `PlaceholderMinter` derives its HMAC subkey from the
master key, so a secret encountered after rotation mints a *different*
placeholder than before. Existing placeholder entries are re-encrypted and keep
resolving, so historic transcripts are unaffected; the vault just accumulates a
second entry for a value that reappears.

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

### Least-privilege injection

Tools never open the vault directly. The agent layer implements
`baybo_tools::SecretAccess` on top of `SecurityGateway` + `UserSecretManager`
and injects it via `ToolContext::secrets`; `Bash` resolves declared
`secret_env` names to plaintext for child-process env injection through
`SecretAccess::resolve_env`, and `redact` reuses the gateway's mint/vault
pipeline so injected values stay reveal-able. See
[`secret-management.md`](../secret-management.md). Placeholder-bearing tool
arguments still reach plaintext only through the tool-argument reveal boundary
described above.

### Network decision boundary

Security only decides allow/deny. It does not execute network access. There is no central network-policy decider — the SSRF guard is inline in `WebFetch::validate_url_with` (parse-time literal-IP rejection via `baybo_security::is_blocked_ip`) plus the per-fetch `SafeResolver` (DNS-time resolved-IP filter), with a second inline copy in deck's host-mediated `ctx.fetch` (`crates/deck/src/host.rs`). Process-level network containment for sandboxed tools comes from `baybo_sandbox::NetworkPolicy::{None, All}` at spec-build time. This separates permission decisions from execution.

## Constraints

- Primitives crate — no session/channel/storage dependencies
- Trace records only sanitized span begin/finalize payloads (`ToolCallBegin.params`, `LlmCallResult`, `ToolCallResult`) — including tool-call arguments
- Turn `input/output` stores sanitized versions only
- Structured logs must not print `SecretValue` directly; reveal warnings log only a SHA-256 fingerprint prefix
- Placeholder generation is deterministic per secret — same secret → same placeholder → single vault entry
- Plaintext at egress is permitted only at four points: the tool executor's post-reveal `params_revealed` into `tool_registry.execute`, `reveal_llm_response` on tool-side LLM replies (`billed_chat`), `SecretAccess::resolve_env` for child-process env injection, and deck's host-mediated `ctx.fetch` reveal (`crates/deck/src/host.rs` — URL/header/body placeholders revealed at egress, audit-logged with card id + host); stream deltas, outgoing messages, trace, memory, and persistence all carry placeholder form
- Injection detection is log-only at inbound and log-plus-wrap at tool output; never auto-block user input on injection markers alone
- Injection log severity is scoped by provenance, and provenance is decided by `ToolExecutor` and delivered on `ScanOrigin` — the detector never infers it from the text. Only `OutputSource::DeclaredFiles` tools reading inside the session's own checkout are demoted, and a tool may claim `DeclaredFiles` only if it enumerates every file its output came from (which rules out directory-rooted searches like `Grep` / `Glob`); nothing is suppressed, and inbound channel input is always `Foreign`
- Any tool that reads filesystem paths MUST apply `is_sensitive_path` at its entry point, regardless of approval-gate status
- Any tool that emits HTTP MUST apply both layers of the SSRF floor unless its destination is fixed by code or the operator and cannot be changed by model input; such a tool must still validate the endpoint and must not dereference returned URLs

## Collaboration

| Module | Role |
|--------|------|
| `channels` | Input messages go to `agent::security::SecurityGateway` first |
| `agent` | `agent::security::SecurityGateway` and `SecretVault` own business logic |
| `trace` / `turn` | Receive only sanitized payloads and placeholders |
| `store` / `storage` | `baybo-store` defines the `SecretStore` trait; `baybo-storage` provides the sqlite implementation |
