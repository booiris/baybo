# security - Security Gateway and Secret Management

## 1. Module Overview

The `security` crate is responsible for Aura's sensitive-data protection and secret management. It is the security boundary both before external input enters Agent and before responses leave the system.

Core responsibilities:

- Input leak detection: identify API keys, passwords, tokens, and other sensitive data in messages
- Placeholder replacement: replace sensitive plaintext with `{{SECRET_xxx}}`
- Secret vault: encrypt and store real secrets, then inject them with least privilege according to tool declarations
- Output re-sanitization: keep only placeholders and sanitized summaries in responses, logs, Trace, and Job records
- Network policy decision interfaces: provide allow/deny decisions for the execution layer

`security` does not actually execute tools, launch containers, or open network access. Those belong to `agent` and `sandbox`. Its responsibilities are the sanitization boundary and permission decision interface.

---

## 2. Dependencies

### 2.1 Internal Dependencies

| Dependency Crate | Purpose |
|-----------|------|
| `core` | Shared types such as `Message`, `Session`, `OutgoingMessage`, and `AuraError` |

### 2.2 External Dependencies

| Dependency | Purpose |
|------|------|
| `regex` | Sensitive-data matching rules |
| `serde` / `serde_json` | Rule configuration and sanitization result serialization |
| `aes-gcm` / `ring` and similar | Secret encryption and decryption |
| `base64` | Secret persistence encoding |
| `async-trait` | Async interface for `SecretStore` |

### 2.3 Dependency Direction

```text
core
  │
  ▼
security
  │
  ├──► storage   (persists via SecretStore implementations)
  ├──► agent     (ToolExecutor retrieves secrets and injects them into tools)
  └──► sandbox   (consumes NetworkPolicyDecider results)
```

---

## 3. Public Interfaces

### 3.1 SecretStore Trait

```rust
#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn store(&self, name: &str, encrypted_value: &[u8]) -> Result<()>;
    async fn retrieve(&self, name: &str) -> Result<Option<Vec<u8>>>;
    async fn delete(&self, name: &str) -> Result<()>;
    async fn list(&self) -> Result<Vec<String>>;
}
```

### 3.2 SecretVault

```rust
pub struct SecretVault {
    master_key: EncryptionKey,
    store: Box<dyn SecretStore>,
}

impl SecretVault {
    pub fn store_secret(&self, name: &str, value: &[u8]) -> Result<()>;
    pub fn get_secret(&self, name: &str) -> Result<Option<SecretValue>>;
    pub fn get_secrets_for_tool(
        &self,
        tool_name: &str,
        declared: &[String],
    ) -> Result<HashMap<String, SecretValue>>;
}
```

Constraints:

- `master_key` exists only in process memory and is never written to persistent storage
- `get_secrets_for_tool()` returns only secrets explicitly declared by the tool
- `SecretValue` should not support plaintext `Debug`

### 3.3 SecurityGateway

```rust
pub struct SecurityGateway {
    leak_detector: LeakDetector,
    secret_vault: Arc<SecretVault>,
    policy_decider: Arc<dyn NetworkPolicyDecider>,
}

impl SecurityGateway {
    pub fn sanitize_input(&self, msg: &mut Message, session: &mut Session) -> Result<()>;
    pub fn sanitize_output(&self, response: &mut OutgoingMessage, session: &Session) -> Result<()>;
}
```

### 3.4 LeakDetector / LeakDetectionRule

```rust
pub struct LeakDetector {
    rules: Vec<LeakDetectionRule>,
}

pub struct LeakDetectionRule {
    pub name: String,
    pub pattern: Regex,
    pub action: LeakAction,
}

pub enum LeakAction {
    Block,
    Replace,
}
```

### 3.5 NetworkPolicyDecider

```rust
pub trait NetworkPolicyDecider: Send + Sync {
    fn decide(&self, tool: &ToolManifest, request: &NetworkRequest) -> NetworkPolicyDecision;
}

pub struct NetworkRequest {
    pub host: String,
    pub port: u16,
}

pub enum NetworkPolicyDecision {
    Allow,
    Deny(String),
}
```

---

## 4. Implementation Details

### 4.1 Input Sanitization Flow

```text
ChannelAdapter receives message
    │
    ▼
SecurityGateway::sanitize_input()
    │
    ├── LeakDetector scans content blocks
    ├── on rule hit, generate placeholder such as {{SECRET_x7k9}}
    ├── replace sensitive fragment in the message
    └── write placeholder -> secret name/reference into session security state
```

Key rule: **the context that enters Agent may only see placeholders, never raw secrets.**

### 4.2 Output Re-sanitization Strategy

Before any response leaves the system, it must pass through `sanitize_output()` again:

- If placeholders appear in the response, keep them as-is
- If the response reconstructs secret-like content, match it again and replace it
- Never perform reverse mapping from placeholders back to plaintext

### 4.3 Encryption Boundary of SecretVault

Recommended flow:

1. `store_secret(name, value)`
2. Generate a random nonce
3. Encrypt with `AES-256-GCM`
4. Store `nonce + ciphertext + tag` into `SecretStore`

During reads, do the reverse and expose only short-lived `SecretValue` objects in process memory.

### 4.4 Least-Privilege Injection

Recommended tool-execution sequence:

```text
tool.required_secrets()
    │
    ▼
SecretVault::get_secrets_for_tool(tool_name, declared)
    │
    ▼
ToolContext { secrets, ... }
    │
    ▼
tool.execute(params, &ctx)
```

`security` does not understand tool business logic. It only returns the minimal set based on declarations.

### 4.5 Network Decision Boundary

`security` only decides whether a request should be allowed. It does not actually open network access. The execution chain should be:

```text
ToolManifest + admin config + runtime request
    │
    ▼
NetworkPolicyDecider::decide(...)
    │
    ▼
sandbox executes allow / deny
```

This guarantees:

- Clean layering between permission decisions and execution isolation
- Deny-by-default networking, rather than relying on tools to self-restrict
- Decision logic can be tested and audited independently

### 4.6 Logging and Observability Constraints

- Trace records only sanitized `SpanInput` and `SpanResult`
- Job `input/output` stores sanitized versions only
- Structured logs must not print `SecretValue` directly
- If error messages contain sensitive plaintext, they must be sanitized again before reporting

---

## 5. Collaboration with Other Modules

| Collaboration Module | Collaboration |
|---------|---------|
| `channels` | Input messages go to `SecurityGateway` first after entering the system |
| `agent` | `ToolExecutor` retrieves secrets from `SecretVault` and injects them into tools |
| `sandbox` | Consumes allow / deny decisions from `NetworkPolicyDecider` |
| `trace` | Receives only sanitized payloads and placeholders |
| `job` | Persists only sanitized `input/output` |
| `storage` | Provides SQLite / in-memory implementations of `SecretStore` |

---

## 6. Implementation Recommendations

- `LeakDetector` should scan content block by content block, not only plain text
- Placeholder generation should use an unpredictable random suffix to avoid collisions
- `SecretValue` should minimize copying and clear memory on drop where possible
- Write snapshot tests for `sanitize_input()` and `sanitize_output()`
- Write table-driven tests for `NetworkPolicyDecider` covering allowlists, precedence, and deny-by-default cases
