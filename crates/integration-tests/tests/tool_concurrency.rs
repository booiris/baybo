//! End-to-end coverage for tool-call concurrency scheduling in the
//! agent loop (`run_iteration`).
//!
//! The scheduling guarantees are exercised through a real `AgentActor`
//! driven by a scripted LLM that emits a batch of tool calls in a single
//! response:
//!
//! 1. `ToolConcurrency::Concurrent` calls (the policy `Read` opts into)
//!    run in parallel, but never more than `MAX_CONCURRENT_TOOL_CALLS`
//!    at once.
//! 2. `ToolConcurrency::Exclusive` calls (every mutating tool) run alone
//!    among pool calls — no other pool call overlaps them.
//! 3. `ToolConcurrency::Independent` calls (the policy `spawn_subagent`
//!    uses) acquire no permit: they bypass the cap and overlap even an
//!    `Exclusive` call, so subagent fan-out stays parallel.
//!
//! The probe tool below bumps a shared in-flight counter on entry and
//! decrements on exit, so the test can observe the actual overlap the
//! scheduler produced rather than asserting on timing.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use baybo_integration_tests::AgentTestHarness;
use baybo_llm::{StreamEvent, ToolCallInfo};
use baybo_model::TrustLevel;
use baybo_tools::{Tool, ToolConcurrency, ToolContext, ToolManifest, ToolOutput};
use serde_json::{Value, json};

/// Mirrors `MAX_CONCURRENT_TOOL_CALLS` in
/// `crates/agent/src/runtime/agent_loop.rs`. If the production cap
/// changes, update this too.
const CAP: usize = 10;

/// Each probe holds for this long, so every call dispatched in one
/// response overlaps in time — an uncapped scheduler (or one that fails
/// to isolate exclusive calls) is then directly observable. Kept short
/// to keep the test fast.
const HOLD: Duration = Duration::from_millis(100);

/// How long `drain_outputs` waits for quiescence — comfortably longer
/// than the worst-case serialized tool time in either scenario.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(900);

#[derive(Default)]
struct Tracker {
    inflight: AtomicUsize,
    /// High-water mark of concurrently-executing probes.
    max_inflight: AtomicUsize,
    /// In-flight count seen at the entry of each `Exclusive` probe body.
    /// An exclusive call holds every permit, so nothing else can be
    /// running — this must stay 1.
    exclusive_peak: AtomicUsize,
    exclusive_calls: AtomicUsize,
}

struct ProbeTool {
    name: String,
    mode: ToolConcurrency,
    tracker: Arc<Tracker>,
}

impl ProbeTool {
    fn arc(name: &str, mode: ToolConcurrency, tracker: Arc<Tracker>) -> Arc<Self> {
        Arc::new(Self {
            name: name.to_string(),
            mode,
            tracker,
        })
    }

    fn manifest(&self) -> ToolManifest {
        ToolManifest {
            name: self.name.clone(),
            description: "concurrency probe".into(),
            trust_level: TrustLevel::Trusted,
            parameters_schema: json!({ "type": "object", "additionalProperties": true }),
            capabilities: vec![],
            channels: Vec::new(),
        }
    }
}

#[async_trait]
impl Tool for ProbeTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> String {
        "concurrency probe".into()
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "additionalProperties": true })
    }
    fn concurrency(&self) -> ToolConcurrency {
        self.mode
    }
    async fn execute(&self, _params: Value, _ctx: &ToolContext) -> baybo_tools::Result<ToolOutput> {
        let now = self.tracker.inflight.fetch_add(1, Ordering::SeqCst) + 1;
        self.tracker.max_inflight.fetch_max(now, Ordering::SeqCst);
        if self.mode == ToolConcurrency::Exclusive {
            self.tracker.exclusive_calls.fetch_add(1, Ordering::SeqCst);
            // An exclusive call holds every permit, so all prior calls
            // have released and no later call can have started — `now`
            // is therefore 1. Recording it here (rather than asserting
            // inline) keeps the failure attributable to the agent loop.
            self.tracker.exclusive_peak.fetch_max(now, Ordering::SeqCst);
        }
        tokio::time::sleep(HOLD).await;
        self.tracker.inflight.fetch_sub(1, Ordering::SeqCst);
        Ok(ToolOutput::Text("ok".into()))
    }
}

fn call(id: usize, name: &str) -> StreamEvent {
    StreamEvent::ToolCall(ToolCallInfo {
        id: format!("call-{id}"),
        name: name.to_string(),
        arguments: json!({}),
        signature: None,
    })
}

#[tokio::test]
async fn concurrent_tools_run_in_parallel_capped_at_limit() {
    let tracker = Arc::new(Tracker::default());
    let probe = ProbeTool::arc("read_probe", ToolConcurrency::Concurrent, tracker.clone());
    let manifest = probe.manifest();

    let mut harness = AgentTestHarness::builder()
        .with_tool(probe as Arc<dyn Tool>, manifest)
        .build();

    // A single LLM response that fans out more concurrent calls than the
    // cap allows. Without the limiter all `N` would overlap; with it, the
    // peak is pinned at `CAP`.
    const N: usize = CAP + 5;
    let batch: Vec<StreamEvent> = (0..N).map(|i| call(i, "read_probe")).collect();
    harness.stub_llm.push_stream(batch);
    // Second iteration: final answer, no tool calls — ends the loop.
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text("done".into())]);

    harness.send_text("read everything").await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;

    let max = tracker.max_inflight.load(Ordering::SeqCst);
    assert!(
        max <= CAP,
        "concurrent reads exceeded the cap: observed {max} > {CAP}"
    );
    assert_eq!(
        max, CAP,
        "with {N} simultaneous reads the peak should saturate the cap ({CAP}), saw {max}"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn exclusive_tool_runs_alone() {
    let tracker = Arc::new(Tracker::default());
    let read_probe = ProbeTool::arc("read_probe", ToolConcurrency::Concurrent, tracker.clone());
    let write_probe = ProbeTool::arc("write_probe", ToolConcurrency::Exclusive, tracker.clone());
    let read_manifest = read_probe.manifest();
    let write_manifest = write_probe.manifest();

    let mut harness = AgentTestHarness::builder()
        .with_tool(read_probe as Arc<dyn Tool>, read_manifest)
        .with_tool(write_probe as Arc<dyn Tool>, write_manifest)
        .build();

    // One response interleaving an exclusive call among concurrent ones.
    harness.stub_llm.push_stream(vec![
        call(0, "read_probe"),
        call(1, "read_probe"),
        call(2, "write_probe"),
        call(3, "read_probe"),
        call(4, "read_probe"),
    ]);
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text("done".into())]);

    harness.send_text("read, write, read").await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert_eq!(
        tracker.exclusive_calls.load(Ordering::SeqCst),
        1,
        "the exclusive probe should have executed exactly once"
    );
    assert_eq!(
        tracker.exclusive_peak.load(Ordering::SeqCst),
        1,
        "an exclusive tool must run alone — no other call may overlap it"
    );
    // Sanity: the reads themselves did get to overlap (the limiter is not
    // serializing everything).
    assert!(
        tracker.max_inflight.load(Ordering::SeqCst) >= 2,
        "concurrent reads should still overlap each other"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn independent_tools_bypass_the_pool_cap() {
    // `Independent` (the policy `spawn_subagent` uses) acquires no permit,
    // so the per-response cap does not bound it — every dispatched call
    // overlaps, even past `CAP`. Its real bound is an out-of-band limiter
    // (the subagent fan-out cap), not the tool pool.
    let tracker = Arc::new(Tracker::default());
    let probe = ProbeTool::arc("indep_probe", ToolConcurrency::Independent, tracker.clone());
    let manifest = probe.manifest();

    let mut harness = AgentTestHarness::builder()
        .with_tool(probe as Arc<dyn Tool>, manifest)
        .build();

    const N: usize = CAP + 5;
    let batch: Vec<StreamEvent> = (0..N).map(|i| call(i, "indep_probe")).collect();
    harness.stub_llm.push_stream(batch);
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text("done".into())]);

    harness.send_text("fan out").await.unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;

    let max = tracker.max_inflight.load(Ordering::SeqCst);
    assert!(
        max > CAP,
        "independent calls must bypass the pool cap (saw {max} ≤ {CAP})"
    );
    assert_eq!(
        max, N,
        "all {N} independent calls should overlap — no permit gating — saw {max}"
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn independent_tool_overlaps_an_exclusive_call() {
    // An `Independent` call holds no permit, so it runs alongside an
    // `Exclusive` call instead of waiting behind it — the property that
    // keeps subagent fan-out parallel even next to a mutating tool.
    let tracker = Arc::new(Tracker::default());
    let excl = ProbeTool::arc("excl_probe", ToolConcurrency::Exclusive, tracker.clone());
    let indep = ProbeTool::arc("indep_probe", ToolConcurrency::Independent, tracker.clone());
    let excl_manifest = excl.manifest();
    let indep_manifest = indep.manifest();

    let mut harness = AgentTestHarness::builder()
        .with_tool(excl as Arc<dyn Tool>, excl_manifest)
        .with_tool(indep as Arc<dyn Tool>, indep_manifest)
        .build();

    harness
        .stub_llm
        .push_stream(vec![call(0, "excl_probe"), call(1, "indep_probe")]);
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text("done".into())]);

    harness
        .send_text("exclusive plus independent")
        .await
        .unwrap();
    let _ = harness.drain_outputs(DRAIN_TIMEOUT).await;

    assert_eq!(
        tracker.max_inflight.load(Ordering::SeqCst),
        2,
        "an Independent call must overlap an Exclusive one (both in flight at once)"
    );

    harness.shutdown().await;
}
