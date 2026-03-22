# cost - Cost Records and Spending Guards

## 1. Module Overview

The `cost` crate is responsible for Aura's token usage records, cost statistics, and spending enforcement. It does not call LLMs and does not decide business flow. It is assembled by `agent` as pure billing infrastructure.

Core responsibilities:

- Record token usage and USD cost for every LLM call
- Provide cost aggregation by user, globally, and by time range
- Check limits before a request enters execution
- Associate records with `job` and `trace` for auditing

---

## 2. Dependencies

### 2.1 Internal Dependencies

| Dependency Crate | Purpose |
|-----------|------|
| `core` | Common types such as `AuraError` |

### 2.2 External Dependencies

| Dependency | Purpose |
|------|------|
| `chrono` | Time-range queries |
| `serde` | Cost record serialization |
| `async-trait` | Async interface for `CostStore` |

### 2.3 Relationship with Other Modules

`cost` depends on `TokenUsage` and `ModelPricing` results produced by `llm`, but does not directly depend on the `llm` crate. They are assembled by `agent::ObservabilityRecorder`.

---

## 3. Public Interfaces

### 3.1 CostRecord

```rust
pub struct CostRecord {
    pub user_id: String,
    pub session_id: String,
    pub job_id: String,
    pub trace_span_id: String,
    pub model: String,
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub cost_usd: f64,
    pub timestamp: DateTime<Utc>,
}
```

`CostRecord` is the smallest auditable billing unit. It must be associated with both `job_id` and `trace_span_id`, so the system never ends up knowing the cost without knowing which call caused it.

### 3.2 CostStore Trait

```rust
#[async_trait]
pub trait CostStore: Send + Sync {
    async fn record(&self, record: &CostRecord) -> Result<()>;
    async fn query_user(&self, user_id: &str, range: TimeRange) -> Result<Vec<CostRecord>>;
    async fn query_global(&self, range: TimeRange) -> Result<CostSummary>;
    async fn sum_user(&self, user_id: &str, range: TimeRange) -> Result<f64>;
}
```

### 3.3 CostTracker

`CostTracker` handles recording and aggregation. It does not make limit decisions.

```rust
pub struct CostTracker {
    store: Box<dyn CostStore>,
}

impl CostTracker {
    pub async fn record(&self, record: &CostRecord) -> Result<()>;
    pub async fn sum_user(&self, user_id: &str, range: TimeRange) -> Result<f64>;
    pub async fn query_global(&self, range: TimeRange) -> Result<CostSummary>;
}
```

### 3.4 CostGuard

```rust
pub struct CostGuard {
    cost_tracker: Arc<CostTracker>,
    limits: CostLimits,
}

pub struct CostLimits {
    pub daily_per_user: Option<f64>,
    pub monthly_per_user: Option<f64>,
    pub daily_global: Option<f64>,
    pub monthly_global: Option<f64>,
}
```

Recommended interface:

```rust
impl CostGuard {
    pub async fn check_user_limit(&self, user_id: &str, now: DateTime<Utc>) -> Result<CostDecision>;
    pub async fn check_global_limit(&self, now: DateTime<Utc>) -> Result<CostDecision>;
}

pub enum CostDecision {
    Allow,
    Reject { reason: String, current: f64, limit: f64 },
}
```

---

## 4. Implementation Details

### 4.1 When to Record Cost

Typical flow:

```text
LlmClient returns LlmResponse + TokenUsage
    │
    ▼
ObservabilityRecorder::succeed()
    │
    ├── calculate cost_usd from ModelPricing
    ├── assemble CostRecord
    └── call CostTracker::record()
```

Cost should only be recorded after an actual LLM call has successfully completed, to avoid polluting billing data with estimates.

### 4.2 Cost Formula

```text
cost_usd =
  input_tokens  / 1_000_000 * input_per_1m_tokens
+ output_tokens / 1_000_000 * output_per_1m_tokens
```

Use fixed-point decimal handling or enough precision to avoid long-term `f64` accumulation errors being amplified in financial scenarios.

### 4.3 Where to Check Limits

`CostGuard` should run before execution in Router or AgentLoop:

1. Daily per-user limit
2. Monthly per-user limit
3. Daily global limit
4. Monthly global limit

If any limit is exceeded, reject the new request. Requests already in progress should not be interrupted mid-flight just because the limit changes, unless the product explicitly requires hard interruption.

### 4.4 Suggested TimeRange

Suggested standard time-range types:

```rust
pub struct TimeRange {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

pub struct CostSummary {
    pub total_cost_usd: f64,
    pub total_input_tokens: usize,
    pub total_output_tokens: usize,
    pub record_count: usize,
}
```

### 4.5 Storage Design Recommendations

Recommended SQLite columns:

- `user_id`
- `session_id`
- `job_id`
- `trace_span_id`
- `model`
- `input_tokens`
- `output_tokens`
- `cost_usd`
- `timestamp`

Suggested indexes:

- `(user_id, timestamp)`
- `(timestamp)`
- `(job_id)`

---

## 5. Collaboration with Other Modules

| Module | Collaboration |
|------|---------|
| `agent` | `ObservabilityRecorder` calls `CostTracker` after a successful LLM span |
| `llm` | Provides model pricing and token usage |
| `job` | `CostRecord.job_id` links to the specific call |
| `trace` | `CostRecord.trace_span_id` links to the specific span |
| `hook` | Limit hits can trigger `HookPoint::CostLimitReached` |

---

## 6. Implementation Recommendations

- `CostGuard` should only decide, not record
- Return structured rejection reasons on limit check failure so upper layers can render them cleanly
- Free local models may still record token usage while keeping `cost_usd = 0.0`
- If you add a cache for daily or monthly aggregation, the original `CostRecord` must still remain traceable
