# llm - LLM Client Layer (Based on rig)

## 1. Module Overview

The `llm` crate is Aura's infrastructure layer for large language model calls. Its core responsibilities are:

- **Wrap the rig framework**: encapsulate rig's underlying APIs such as `rig::completion::Chat` and the Agent builder into Aura's unified `LlmClient` interface, hiding provider differences
- **Provide a unified invocation interface**: upper layers such as `agent::AgentLoop` only call `LlmClient::chat()` without caring whether the backend is OpenAI, Anthropic, or Ollama
- **Enable registry-style extension**: implement the open-closed principle through `LlmProviderRegistry` and the `LlmProviderFactory` trait, so adding a provider only requires implementing and registering a new factory
- **Support dual-mode response parsing**: support both native function calling (`NativeFunctionCalling`) and prompt-guided JSON extraction (`PromptGuided`) so that local models without native function calling can still participate in tool use

**Design constraint**: this crate is pure infrastructure. It contains no business logic and does not depend on business crates such as `security`, `tools`, or `skills`.

---

## 2. Dependencies

### 2.1 Internal Dependencies

| Dependency Crate | Usage |
|-----------|---------|
| `core` | `Message`, `OperationKind::LlmCall`, `AuraError` |

The `llm` crate sits near the bottom of the dependency tree, alongside `channels`, `tools`, `skills`, and `memory`, and depends only on `core`.

### 2.2 External Dependencies

| Dependency | Purpose |
|------|------|
| `rig` | Core dependency providing `rig::completion::Chat`, provider clients such as OpenAI and Anthropic, and the Agent builder |
| `serde` / `serde_json` | Config deserialization, JSON schema handling, and response parsing |
| `anyhow` | Error propagation |
| `async-trait` | Async trait support |

### 2.3 Dependency Direction Diagram

```text
core
  │
  ▼
llm ───► rig
  │
  ▼
cost   (consumes TokenUsage)
```

Note: `llm` does not depend on `cost`; instead, `cost` consumes `TokenUsage` produced by `llm`, and `agent` assembles the two.

---

## 3. Public Interfaces

### 3.1 LlmClient

`LlmClient` is the main outward-facing type of this crate. It wraps one rig model instance together with its metadata.

```rust
pub struct LlmClient {
    model: Box<dyn rig::completion::Chat>,
    model_info: ModelInfo,
    parse_mode: ResponseParseMode,
}
```

Core methods:

| Method | Signature | Description |
|------|------|------|
| `chat` | `async fn chat(&self, request: &ChatRequest) -> Result<LlmResponse>` | Sends a chat request and returns a unified response structure. Internally, request construction and response parsing depend on `parse_mode` |
| `model_id` | `fn model_id(&self) -> &str` | Returns the model identifier such as `"claude-sonnet-4-6"` or `"gpt-4o"` |
| `model_info` | `fn model_info(&self) -> &ModelInfo` | Returns full model metadata |

Construction:

`LlmClient` is created through `LlmProviderFactory::create()` rather than directly by users.

- `new(model, model_info)`: for models with native function calling support, using `NativeFunctionCalling`
- `new_prompt_guided(model, model_info, tool_schema_prompt, json_extractor)`: for models without native function calling, using `PromptGuided`

### 3.2 LlmProviderFactory Trait

```rust
pub trait LlmProviderFactory: Send + Sync {
    fn provider_name(&self) -> &str;
    fn create(&self, config: &LlmProviderConfig) -> Result<LlmClient>;
}
```

Why use a trait instead of a function pointer:

1. **Type safety**: traits provide compile-time interface guarantees
2. **Stateful implementations**: a trait implementation may carry state such as default config or shared clients
3. **Testability**: traits are naturally mockable
4. **Rust convention**: `Send + Sync` makes factories safe to use across threads in async runtimes

### 3.3 LlmProviderRegistry

```rust
pub struct LlmProviderRegistry {
    factories: HashMap<String, Box<dyn LlmProviderFactory>>,
}

impl LlmProviderRegistry {
    pub fn new() -> Self { ... }

    pub fn register(&mut self, factory: impl LlmProviderFactory + 'static) {
        self.factories.insert(
            factory.provider_name().to_string(),
            Box::new(factory),
        );
    }

    pub fn create_client(&self, config: &LlmProviderConfig) -> Result<LlmClient> {
        self.factories
            .get(&config.provider)
            .ok_or_else(|| anyhow!("Unknown provider: {}", config.provider))?
            .create(config)
    }
}
```

Initialization flow:

```text
LlmProviderRegistry::new()
    │
    ├── register(OpenAIProviderFactory)
    ├── register(AnthropicProviderFactory)
    └── register(OllamaProviderFactory)
    │
    ▼
registry.create_client(&config)
```

### 3.4 Data Types

#### ModelInfo

```rust
pub struct ModelInfo {
    pub id: String,
    pub provider: String,
    pub context_window: usize,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub pricing: ModelPricing,
}
```

#### ModelPricing

```rust
pub struct ModelPricing {
    pub input_per_1m_tokens: f64,
    pub output_per_1m_tokens: f64,
}
```

`CostTracker` uses `ModelPricing` together with `TokenUsage` to calculate per-call cost.

#### LlmResponse

```rust
pub struct LlmResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCallInfo>,
    pub usage: TokenUsage,
    pub thinking: Option<String>,
}
```

`AgentLoop` decides based on whether `tool_calls` is empty:

- Empty: treat as final text reply
- Non-empty: enter the tool-execution branch

#### TokenUsage

```rust
pub struct TokenUsage {
    pub input_tokens: usize,
    pub output_tokens: usize,
}
```

#### ResponseParseMode

```rust
pub enum ResponseParseMode {
    NativeFunctionCalling,
    PromptGuided {
        tool_schema_prompt: String,
        json_extractor: JsonExtractor,
    },
}
```

#### ChatRequest

`ChatRequest` is built by `AgentLoop::build_request()` and passed into `LlmClient::chat()`. It includes:

- `messages: Vec<ChatMessage>`: the current conversation context after ContextManager compression
- `temperature: Option<f32>`
- tool definitions in native function-calling mode

---

## 4. Implementation Details

### 4.1 rig Integration

`LlmClient` internally holds `Box<dyn rig::completion::Chat>`, using dynamic dispatch to uniformly call different providers.

```text
LlmClient::chat(&ChatRequest)
    │
    ├── NativeFunctionCalling
    │   ├── convert ChatRequest to rig request
    │   ├── convert tool definitions through rig_adapter
    │   ├── call model.chat(...).await
    │   └── extract content + tool_calls + usage
    │
    └── PromptGuided
        ├── append tool schema prompt to the system prompt
        ├── call model.chat(...).await without tools
        └── extract tool_calls from free text via JsonExtractor
```

`rig_adapter.rs` converts Aura's tool-definition schema into the format expected by rig without directly depending on the `tools` crate.

### 4.2 Provider Implementations

#### OpenAIProviderFactory

- `provider_name`: `"openai"`
- Reads API key from `LlmProviderConfig.api_key`
- Supports `base_url` override for OpenAI-compatible endpoints
- Uses `NativeFunctionCalling`
- Fills `ModelInfo` with context window, tool support, vision support, and pricing

#### AnthropicProviderFactory

- `provider_name`: `"anthropic"`
- Handles Claude-specific output such as `thinking`
- Adapts Anthropic's tool-use response format
- Uses `NativeFunctionCalling`

#### OllamaProviderFactory

- `provider_name`: `"ollama"`
- Connects to the local Ollama instance through `config.base_url`, defaulting to `http://localhost:11434`
- Does not require an API key
- Uses `PromptGuided`
- Sets `supports_tools: false` in `ModelInfo`

### 4.3 ResponseParseMode

#### NativeFunctionCalling

For models that natively support function calling, such as OpenAI and Anthropic:

1. Convert Aura tool definitions to rig tool definitions
2. Include tools in the rig request
3. Map structured tool calls directly into `Vec<ToolCallInfo>`

#### PromptGuided

For models without native function calling, such as many Ollama models:

1. Append `tool_schema_prompt` to the system prompt
2. Instruct the model to emit structured JSON when it needs a tool
3. Receive free-form text
4. Use `JsonExtractor` to pull out tool-call JSON

If extraction fails, treat the response as plain text and leave `tool_calls` empty.

#### JsonExtractor Strategy

`JsonExtractor` should support:

1. Tag-based extraction, such as `<tool_call>...</tool_call>`
2. Detection of likely JSON blocks in free text
3. Tolerance for common formatting issues, such as trailing commas

### 4.4 Observability Constraints

The `llm` module may expose provider reasoning or thinking through `LlmResponse.thinking`, but upper layers decide whether it is recorded. The architecture requires:

- `thinking` may exist in memory
- `trace::SpanResult::LLMResponse` records only `output_preview`, token usage, and `reasoning_redacted`
- Full reasoning is not persisted by default in production

### 4.5 Error Handling

Possible error classes returned by `LlmClient::chat()` include:

- Network failures
- Authentication failures
- Unknown or unavailable models
- Request over context-window limits
- Response parsing failures

All errors are propagated through `anyhow::Error` and handled uniformly by the upper-layer `ErrorHandler`.

Rate-limit retries are not implemented in `llm` itself. They are handled by `AgentLoop` through retry logic in `ErrorHandler`.

Timeout handling:

- The underlying HTTP client can be configured with request-level timeouts
- Upper-layer Job monitoring can mark a long-running `LlmCall` as `Stuck`

---

## 5. File Structure

```text
crates/llm/src/
├── lib.rs
├── registry.rs
├── providers/
│   ├── openai.rs
│   ├── anthropic.rs
│   └── ollama.rs
├── rig_adapter.rs
└── prompt_guided.rs
```

---

## 6. Configuration

Example `llm` section in the Aura config:

```json
{
  "llm": {
    "providers": {
      "claude": {
        "provider": "anthropic",
        "api_key": "${CLAUDE_API_KEY}",
        "model": "claude-sonnet-4-6"
      },
      "openai": {
        "provider": "openai",
        "api_key": "${OPENAI_API_KEY}",
        "base_url": "https://api.openai.com/v1",
        "model": "gpt-4o"
      },
      "ollama": {
        "provider": "ollama",
        "base_url": "http://localhost:11434",
        "model": "llama3"
      }
    }
  }
}
```

Field notes:

| Field | Type | Required | Description |
|------|------|------|------|
| `providers` | `Map<String, LlmProviderConfig>` | Yes | Keys are user-defined names, values are provider configs |
| `provider` | `String` | Yes | Must match `LlmProviderFactory::provider_name()` |
| `api_key` | `String` | Provider-specific | Supports `${ENV_VAR}` placeholders; may be omitted for Ollama |
| `base_url` | `String` | No | Custom API endpoint |
| `model` | `String` | Yes | Model ID |

Security note: API keys should use environment-variable placeholders and must not be stored directly in config files.

---

## 7. Extension Guide

To add a new provider:

1. Implement `LlmProviderFactory`
2. Decide the correct `ResponseParseMode`
3. Fill out accurate `ModelInfo`, especially `context_window`
4. Handle the provider's authentication model
5. Adapt special response formats if necessary
6. Register it in `LlmProviderRegistry`
7. Add config examples and integration tests
