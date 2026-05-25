//! Coverage for `ToolOutput::MultiModalText` — the variant the
//! browser tool family uses to ship screenshots back to the LLM.
//!
//! Two assertions per test:
//!  1. Channel side: the final `OutgoingMessage` carries the image
//!     attachment so the user channel can render it.
//!  2. LLM side: the next turn's input messages include a `Role::User`
//!     message that contains the image, so a vision-capable provider
//!     receives the bytes through the standard multimodal user-content
//!     path.

use std::sync::Arc;
use std::time::Duration;

use aura_channels::AgentOutput;
use aura_integration_tests::AgentTestHarness;
use aura_llm::{StreamEvent, ToolCallInfo};
use aura_model::{BlobRef, ContentBlock, Role};
use aura_tools::test_support::RecordingTool;
use aura_tools::{Tool, ToolOutput};
use serde_json::json;

const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

fn fake_image_block() -> ContentBlock {
    ContentBlock::Image {
        blob: BlobRef {
            blob_id: format!("sha256:{}", "ab".repeat(32)),
        },
        mime_type: "image/png".into(),
    }
}

#[tokio::test]
async fn multi_modal_text_forwards_image_to_user_channel_and_next_llm_turn() {
    let tool = Arc::new(RecordingTool::new("vision_tool"));
    tool.set_response(ToolOutput::multi_modal_text(
        "screenshot captured".into(),
        vec![fake_image_block()],
    ));
    let manifest = tool.manifest();

    let mut harness = AgentTestHarness::builder()
        .with_tool(tool.clone() as Arc<dyn Tool>, manifest)
        .build();

    // Turn 1: LLM emits a single tool call, no narration.
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::ToolCall(ToolCallInfo {
            id: "call-1".into(),
            name: "vision_tool".into(),
            arguments: json!({}),
            signature: None,
        })]);
    // Turn 2 (streaming): final text, no further tool calls. The agent
    // loop exits. Every iteration streams now, so the post-tool answer is
    // primed as a stream rather than a plain `chat` response.
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text("I see the page".into())]);

    harness.send_text("take a screenshot").await.unwrap();
    let outs = harness.drain_outputs(DRAIN_TIMEOUT).await;

    // (1) Channel side: the final OutgoingMessage carries the image.
    let final_msg = outs
        .iter()
        .find_map(|o| match o {
            AgentOutput::Message(m) => Some(m),
            _ => None,
        })
        .expect("expected a final Message");
    let has_image = final_msg.content.iter().any(|b| {
        matches!(
            b,
            ContentBlock::Image { mime_type, .. } if mime_type == "image/png"
        )
    });
    assert!(
        has_image,
        "MultiModalText image must reach the channel output: {:?}",
        final_msg.content
    );

    // (2) LLM side: the agent loop appended a follow-up Role::User
    // message between the ToolResult and turn 2 carrying the image.
    // The actor mutates its private session, so we don't see it via
    // `harness.session`. Instead, inspect what `stub_llm` actually
    // received as turn 2's input.
    let requests = harness.stub_llm.captured_requests();
    assert!(
        requests.len() >= 2,
        "expected at least 2 LLM calls (turn-1 stream + turn-2 chat), got {}",
        requests.len()
    );
    let turn2 = requests.last().expect("turn 2 request");
    let user_image_msg = turn2
        .messages
        .iter()
        .find(|m| {
            m.role == Role::User
                && m.content.iter().any(|b| {
                    matches!(b, ContentBlock::Image { mime_type, .. } if mime_type == "image/png")
                })
        })
        .expect("expected a Role::User message carrying the screenshot image in turn-2 LLM input");
    let has_marker = user_image_msg.content.iter().any(|b| match b {
        ContentBlock::Text(t) => t.contains("vision_tool") && t.contains("call-1"),
        _ => false,
    });
    assert!(
        has_marker,
        "follow-up User message must include a marker tying the image to the tool call: {:?}",
        user_image_msg.content
    );

    harness.shutdown().await;
}

#[tokio::test]
async fn with_attachments_does_not_round_trip_image_to_llm() {
    // Counter-test: ensure the existing `WithAttachments` flow keeps
    // its "attachment goes to channel only, never echoed back to LLM"
    // contract. If this regresses, it means a refactor accidentally
    // unified the two paths.
    let tool = Arc::new(RecordingTool::new("attachment_tool"));
    tool.set_response(ToolOutput::WithAttachments {
        text: "file sent".into(),
        attachments: vec![fake_image_block()],
    });
    let manifest = tool.manifest();

    let mut harness = AgentTestHarness::builder()
        .with_tool(tool.clone() as Arc<dyn Tool>, manifest)
        .build();

    harness
        .stub_llm
        .push_stream(vec![StreamEvent::ToolCall(ToolCallInfo {
            id: "call-1".into(),
            name: "attachment_tool".into(),
            arguments: json!({}),
            signature: None,
        })]);
    // Turn 2 (streaming): final response, loop exits.
    harness
        .stub_llm
        .push_stream(vec![StreamEvent::Text("done".into())]);

    harness.send_text("send the file").await.unwrap();
    let outs = harness.drain_outputs(DRAIN_TIMEOUT).await;

    // Channel side still gets the attachment.
    let final_msg = outs
        .iter()
        .find_map(|o| match o {
            AgentOutput::Message(m) => Some(m),
            _ => None,
        })
        .expect("final Message");
    assert!(
        final_msg
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Image { .. })),
        "WithAttachments must still deliver the attachment to the user channel"
    );

    // LLM side: NO User-role message carrying the image was added to
    // turn 2's input. Inspect captured_requests rather than the
    // shadow harness.session (which the actor doesn't mutate).
    let requests = harness.stub_llm.captured_requests();
    let turn2 = requests.last().expect("turn 2 request");
    let leak = turn2.messages.iter().any(|m| {
        m.role == Role::User
            && m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. }))
    });
    assert!(
        !leak,
        "WithAttachments must NOT echo image back to the LLM \
         (only MultiModalText does that). Turn-2 input: {:?}",
        turn2.messages
    );

    harness.shutdown().await;
}
