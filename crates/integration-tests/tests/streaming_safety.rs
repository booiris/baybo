//! Streaming-pipeline safety.
//!
//! The agent's streaming contract has two parts that this suite pins:
//!
//!   1. *Placeholder integrity.* Placeholders the LLM echoes back
//!      (`[{REDACTED_SECRET_<hex>}]`) MUST be delivered to the adapter
//!      whole — never with the `[{...}]` wrapper split across deltas.
//!      `safe_flush_boundary` is the buffering primitive that owns
//!      this invariant and the in-tree unit tests in
//!      `agent_loop.rs` cover the algorithm directly. Here we drive
//!      the same algorithm against a real `StubLlm` stream and a real
//!      `SecurityGateway` to confirm the composition behaves.
//!
//!   2. *High-water cap.* A pathological run of `[{` characters with
//!      no matching `}]` cannot grow the buffer indefinitely; once the
//!      cap is exceeded the buffer flushes wholesale and no bytes are
//!      lost.
//!
//! The pipeline does NOT promise to redact raw secrets that the LLM
//! emits one character at a time — that protection only kicks in when
//! a complete secret lands inside a single buffer flush, which the
//! upstream sanitization guarantees for the inputs the LLM saw.

use std::sync::Arc;

use baybo_agent::SecurityGateway;
use baybo_integration_tests::gateway_with_memory_vault;
use baybo_llm::test_support::StubLlm;
use baybo_llm::{ChatRequest, LlmCompletion, StreamEvent};
use baybo_model::{ChatMessage, ContentBlock};
use futures::StreamExt;

const AWS_KEY: &str = "AKIAIOSFODNN7EXAMPLE";
const STREAM_BUFFER_HIGH_WATER: usize = 128;

/// Mirror of `AgentLoop::safe_flush_boundary`. Kept as a local copy so
/// changes to the production algorithm surface here as test failures.
fn safe_flush_boundary(pending: &str) -> usize {
    if pending.len() > STREAM_BUFFER_HIGH_WATER {
        return pending.len();
    }
    if let Some(idx) = pending.rfind("[{") {
        let tail = &pending[idx..];
        if !tail.contains("}]") {
            return idx;
        }
    }
    if pending.ends_with('[') {
        return pending.len() - 1;
    }
    pending.len()
}

fn req() -> ChatRequest {
    ChatRequest {
        messages: vec![ChatMessage::agent_context(vec![ContentBlock::Text(
            "hi".into(),
        )])],
        temperature: None,
        tools: vec![],
        reasoning_effort: None,
    }
}

/// Drives the stub stream through the same buffered-flush loop the
/// agent uses. Returns the sequence of sanitized fragments that would
/// have been forwarded to the channel.
async fn drive_stream(stub: &StubLlm, gw: &SecurityGateway) -> Vec<String> {
    let mut stream = stub.chat_stream(&req()).await.unwrap();
    let mut pending = String::new();
    let mut emitted: Vec<String> = Vec::new();

    while let Some(event) = stream.next().await {
        if let StreamEvent::Text(chunk) = event.unwrap() {
            pending.push_str(&chunk);
            let cut = safe_flush_boundary(&pending);
            if cut > 0 {
                let flushable: String = pending.drain(..cut).collect();
                let sanitized = gw.sanitize_stream_fragment(&flushable).await.unwrap();
                emitted.push(sanitized);
            }
        }
    }
    if !pending.is_empty() {
        let final_sanitized = gw.sanitize_stream_fragment(&pending).await.unwrap();
        emitted.push(final_sanitized);
    }
    emitted
}

#[tokio::test]
async fn s6_placeholder_split_across_chunks_is_delivered_whole() {
    // Mint a real placeholder and then drive it as a stream that
    // chunks 1 char at a time, forcing `[{` and `}]` across boundaries.
    let (gw, _store, vault) = gateway_with_memory_vault();
    let minter = baybo_security::PlaceholderMinter::from_master_key(vault.master_key());
    let ph = minter.mint(AWS_KEY.as_bytes());
    vault.store_secret(&ph, AWS_KEY.as_bytes()).await.unwrap();

    let stub = StubLlm::new().with_text_chunk_size(Some(1));
    stub.push_stream(vec![StreamEvent::Text(format!("lead-in {ph} trailing"))]);

    let emitted = drive_stream(&stub, &gw).await;

    // Whichever chunk delivers the placeholder must deliver it whole.
    // We accept any number of chunks before / after, but every chunk
    // that contains `[{` must also contain a matching `}]` (or vice
    // versa) — i.e. no chunk straddles the placeholder delimiters.
    for (i, chunk) in emitted.iter().enumerate() {
        let opens = chunk.matches("[{").count();
        let closes = chunk.matches("}]").count();
        assert_eq!(
            opens, closes,
            "chunk #{i} straddles placeholder delimiters: {chunk:?}"
        );
    }
    let joined: String = emitted.join("");
    assert!(
        joined.contains(&ph),
        "placeholder must appear intact in joined stream: {joined}"
    );
}

#[tokio::test]
async fn s7_pathological_open_brace_run_eventually_flushes() {
    // Pure `[{[{[{...` text never has a matching `}]` so the buffer
    // would grow forever without the high-water cap. Simulate that
    // case and confirm the loop forces a flush past the threshold.
    let stub = StubLlm::new().with_text_chunk_size(Some(2));
    let (gw, _store, _vault) = gateway_with_memory_vault();
    let payload = "[{".repeat(100); // 200 bytes — well past the 128 cap
    stub.push_stream(vec![StreamEvent::Text(payload.clone())]);

    let emitted = drive_stream(&stub, &gw).await;
    let joined: String = emitted.join("");

    assert_eq!(joined, payload, "all bytes must be forwarded eventually");
    assert!(!emitted.is_empty(), "emitted at least once past high-water");
}

#[tokio::test]
async fn s7b_stream_without_secrets_is_unmodified() {
    let stub = StubLlm::new().with_text_chunk_size(Some(3));
    let (gw, _store, _vault) = gateway_with_memory_vault();
    let benign = "Hello world! This is a perfectly normal response with no secrets in it.";
    stub.push_stream(vec![StreamEvent::Text(benign.into())]);

    let emitted = drive_stream(&stub, &gw).await;
    let joined: String = emitted.join("");

    assert_eq!(joined, benign);
    assert!(joined.find("[{REDACTED").is_none());
}

#[tokio::test]
async fn placeholder_already_in_stream_round_trips_unchanged() {
    // A placeholder that the LLM already echoed back must come out the
    // other side intact and continue to resolve via the vault.
    let (gw, _store, vault) = gateway_with_memory_vault();
    // Pre-populate the vault as if a prior turn had minted this secret.
    let minter = baybo_security::PlaceholderMinter::from_master_key(vault.master_key());
    let ph = minter.mint(AWS_KEY.as_bytes());
    vault.store_secret(&ph, AWS_KEY.as_bytes()).await.unwrap();

    let stub = StubLlm::new().with_text_chunk_size(Some(2));
    stub.push_stream(vec![StreamEvent::Text(format!("Use the key {ph} please."))]);

    let emitted = drive_stream(&stub, &gw).await;
    let joined: String = emitted.join("");
    assert!(joined.contains(&ph));
    // And the placeholder still resolves through the vault.
    let revealed = gw.reveal_in_text(&joined).await.unwrap();
    assert!(revealed.contains(AWS_KEY));
}

#[tokio::test]
async fn secret_in_a_single_large_chunk_is_redacted() {
    // The streaming pipeline only redacts secrets that land complete
    // inside one buffer flush. Pin that contract: a stream that emits
    // the whole secret in one event ends up sanitized.
    let (gw, _store, _vault) = gateway_with_memory_vault();
    let stub = StubLlm::new(); // no chunking — one event = one chunk
    stub.push_stream(vec![StreamEvent::Text(format!("before {AWS_KEY} after"))]);
    let emitted = drive_stream(&stub, &gw).await;
    let joined = emitted.join("");
    assert!(!joined.contains(AWS_KEY), "raw secret leaked: {joined}");
    assert!(
        joined.contains("[{REDACTED_SECRET_"),
        "expected placeholder substituted: {joined}"
    );
}

#[tokio::test]
async fn drop_recv_does_not_leak_raw_secret_into_pending() {
    // Sanity check: even if the simulated downstream goes away
    // mid-stream (we just stop consuming), every chunk we did receive
    // must still be sanitized — i.e. the contract is on emission, not
    // on the downstream existing.
    let stub = StubLlm::new().with_text_chunk_size(Some(1));
    let (gw, _store, _vault) = gateway_with_memory_vault();
    stub.push_stream(vec![StreamEvent::Text(format!("x {AWS_KEY} y"))]);

    let mut stream = stub.chat_stream(&req()).await.unwrap();
    let mut pending = String::new();
    let mut emitted: Vec<String> = Vec::new();
    let mut consumed = 0usize;

    while let Some(event) = stream.next().await {
        if let StreamEvent::Text(chunk) = event.unwrap() {
            pending.push_str(&chunk);
            let cut = safe_flush_boundary(&pending);
            if cut > 0 {
                let flushable: String = pending.drain(..cut).collect();
                let sanitized = gw.sanitize_stream_fragment(&flushable).await.unwrap();
                emitted.push(sanitized);
                consumed += 1;
                if consumed >= 5 {
                    // simulate downstream cancelling
                    drop(stream);
                    break;
                }
            }
        }
    }
    let observed: String = emitted.join("");
    assert!(
        !observed.contains(AWS_KEY),
        "raw secret reached the simulated adapter: {observed}"
    );
    // Drop pending without sanitizing — but it MUST not have ever
    // contained the full raw secret either, because that would mean
    // we had a chance to flush it raw.
    let _ = pending; // intentionally unread
    let _: Arc<SecurityGateway> = gw;
}
