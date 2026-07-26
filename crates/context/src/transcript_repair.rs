//! Hydration-time repair of tool_use / tool_result pairing.
//!
//! The agent loop persists the assistant row (including its `ToolUse`
//! blocks) before dispatching tools and appends `ToolResult` rows only as
//! each call completes, so a process death mid-batch durably leaves an
//! assistant row with dangling `ToolUse` ids. Strict providers (OpenAI,
//! Anthropic) reject any later request built from such a transcript
//! ("No tool output found for function call …"), and since `BadRequest`
//! is non-retriable the session is permanently wedged. The streaming
//! cancel path already guards this shape (`salvage_partial_blocks` drops
//! streamed-but-undispatched `ToolUse`); this module covers the
//! crash-during-tool-execution window by repairing the transcript when
//! the actor rehydrates — before any new turn runs.
//!
//! [`repair_tool_pairing`] does two things in one deterministic pass:
//!
//! 1. **Fill**: every dangling `ToolUse` id gets a synthetic `ToolResult`
//!    saying the execution was interrupted. Fills are returned separately
//!    so the caller can persist them (append-only — session rows are
//!    never rewritten).
//! 2. **Reposition**: every tool-result row is emitted immediately after
//!    the assistant row that issued it, in `ToolUse` block order. On a
//!    healthy transcript this is a no-op; on an already-wedged one (a
//!    resume nudge was appended after the dangling row before this repair
//!    existed, or a prior repair's fills were persisted at the tail) it
//!    restores the adjacency both OpenAI and Anthropic validate. The
//!    in-memory window may therefore diverge from strict ordinal order
//!    for such sessions; only the order diverges — the row set is
//!    identical, which is the invariant the persisted/in-memory mirror
//!    checks care about.

use std::collections::{HashMap, HashSet};

use baybo_model::{ChatMessage, ContentBlock};

/// Body of the synthetic `ToolResult` (wrapped in the standard
/// `<tool_output>` envelope) for a tool call whose execution was cut off
/// by a crash/restart before it produced a result.
const INTERRUPTED_TOOL_RESULT_BODY: &str =
    "[tool execution was interrupted by a crash or restart before it produced a result]";

/// True when every block of `msg` is a `ToolResult` — the shape of a
/// persisted tool-result row (`ChatMessage::tool_result_with_meta`).
fn is_tool_result_row(msg: &ChatMessage) -> bool {
    !msg.content.is_empty()
        && msg
            .content
            .iter()
            .all(|b| matches!(b, ContentBlock::ToolResult { .. }))
}

fn result_ids(msg: &ChatMessage) -> Vec<&str> {
    msg.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect()
}

fn synthetic_fill(id: &str, tool_name: &str) -> ChatMessage {
    ChatMessage::tool_result_with_meta(
        id.to_string(),
        baybo_model::wrap_tool_output(tool_name, INTERRUPTED_TOOL_RESULT_BODY, &[]),
        None,
    )
}

/// Normalize tool pairing over a restored transcript. Returns the
/// repaired message list and the synthetic fill rows that were inserted
/// (in insertion order) so the caller can persist them. When the
/// transcript is already well-formed the input is returned unchanged and
/// the fill list is empty.
pub(crate) fn repair_tool_pairing(
    messages: Vec<ChatMessage>,
) -> (Vec<ChatMessage>, Vec<ChatMessage>) {
    // Index every tool-result row by the ids it carries. Persisted rows
    // hold one ToolResult each, so a plain id → position map suffices;
    // a (hypothetical) multi-result row is consumed when its first id is
    // claimed and keeps its remaining ids with it.
    let mut row_of_id: HashMap<&str, usize> = HashMap::new();
    for (i, msg) in messages.iter().enumerate() {
        if is_tool_result_row(msg) {
            for id in result_ids(msg) {
                row_of_id.entry(id).or_insert(i);
            }
        }
    }

    // Fast path: every ToolUse id has a result somewhere AND every
    // tool-result row already sits in a position the providers accept
    // (contiguous run directly after its issuing assistant row). Detect
    // the common healthy shape cheaply: no dangling ids and no result
    // row displaced behind a non-result row.
    let mut needs_repair = false;
    let mut expected_after: HashSet<&str> = HashSet::new();
    for msg in &messages {
        if is_tool_result_row(msg) {
            for id in result_ids(msg) {
                if !expected_after.remove(id) {
                    needs_repair = true; // orphan or displaced result
                }
            }
            continue;
        }
        if !expected_after.is_empty() {
            needs_repair = true; // unanswered ToolUse followed by a non-result row
        }
        expected_after.clear();
        for block in &msg.content {
            if let ContentBlock::ToolUse { id, .. } = block {
                expected_after.insert(id.as_str());
            }
        }
    }
    if !expected_after.is_empty() {
        needs_repair = true; // dangling ToolUse at the tail
    }
    if !needs_repair {
        return (messages, Vec::new());
    }

    let mut consumed: HashSet<usize> = HashSet::new();
    let mut repaired: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    let mut fills: Vec<ChatMessage> = Vec::new();

    for (i, msg) in messages.iter().enumerate() {
        if consumed.contains(&i) {
            continue;
        }
        // A result row not yet claimed by its assistant row: either an
        // orphan (no matching ToolUse — keep it in place; providers only
        // validate use→result, not the reverse) or it will have been
        // consumed already when its assistant row was emitted.
        repaired.push(msg.clone());
        let uses: Vec<(String, String)> = msg
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, .. } => Some((id.clone(), name.clone())),
                _ => None,
            })
            .collect();
        for (id, name) in uses {
            match row_of_id.get(id.as_str()) {
                Some(&ri) if !consumed.contains(&ri) => {
                    consumed.insert(ri);
                    repaired.push(messages[ri].clone());
                }
                Some(_) => {} // shares a row with an id already emitted
                None => {
                    let fill = synthetic_fill(&id, &name);
                    fills.push(fill.clone());
                    repaired.push(fill);
                }
            }
        }
    }

    (repaired, fills)
}

#[cfg(test)]
mod tests {
    use super::*;
    use baybo_model::Role;

    fn assistant_with_uses(ids: &[&str]) -> ChatMessage {
        ChatMessage::assistant(
            ids.iter()
                .map(|id| ContentBlock::ToolUse {
                    id: (*id).into(),
                    name: "Bash".into(),
                    input: serde_json::Value::Null,
                    signature: None,
                })
                .collect(),
        )
    }

    fn result(id: &str) -> ChatMessage {
        ChatMessage::tool_result_with_meta(id.into(), format!("out-{id}"), None)
    }

    fn user_text(t: &str) -> ChatMessage {
        ChatMessage::user(vec![ContentBlock::Text(t.into())])
    }

    fn ids_in_order(msgs: &[ChatMessage]) -> Vec<String> {
        msgs.iter()
            .flat_map(|m| {
                m.content.iter().filter_map(|b| match b {
                    ContentBlock::ToolUse { id, .. } => Some(format!("use:{id}")),
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        Some(format!("res:{tool_use_id}"))
                    }
                    _ => None,
                })
            })
            .collect()
    }

    #[test]
    fn healthy_transcript_passes_through_unchanged() {
        let msgs = vec![
            user_text("q"),
            assistant_with_uses(&["a", "b"]),
            result("a"),
            result("b"),
            ChatMessage::assistant(vec![ContentBlock::Text("done".into())]),
        ];
        let (repaired, fills) = repair_tool_pairing(msgs.clone());
        assert!(fills.is_empty());
        assert_eq!(repaired.len(), msgs.len());
        assert_eq!(ids_in_order(&repaired), ids_in_order(&msgs));
    }

    #[test]
    fn dangling_tail_gets_synthetic_fills() {
        // The crash shape: assistant issued 3 calls, only the first
        // result landed before the process died.
        let msgs = vec![
            user_text("q"),
            assistant_with_uses(&["a", "b", "c"]),
            result("a"),
        ];
        let (repaired, fills) = repair_tool_pairing(msgs);
        assert_eq!(fills.len(), 2);
        assert_eq!(
            ids_in_order(&repaired),
            vec!["use:a", "use:b", "use:c", "res:a", "res:b", "res:c"]
        );
        let body = match &fills[0].content[0] {
            ContentBlock::ToolResult { content, .. } => content.clone(),
            other => panic!("expected ToolResult fill, got {other:?}"),
        };
        assert!(
            body.contains("interrupted by a crash or restart"),
            "got: {body}"
        );
        assert!(
            body.contains("<tool_output"),
            "fill must use the standard envelope: {body}"
        );
    }

    #[test]
    fn nudge_after_dangling_row_is_pushed_past_the_fills() {
        // The already-wedged shape: a resume nudge was appended after the
        // dangling assistant row before this repair existed.
        let msgs = vec![assistant_with_uses(&["a"]), user_text("请立即返回最终结果")];
        let (repaired, fills) = repair_tool_pairing(msgs);
        assert_eq!(fills.len(), 1);
        assert_eq!(repaired.len(), 3);
        assert!(matches!(
            repaired[0].content[0],
            ContentBlock::ToolUse { .. }
        ));
        assert!(
            matches!(&repaired[1].content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "a")
        );
        assert_eq!(repaired[2].role, Role::User);
    }

    #[test]
    fn displaced_persisted_fills_are_repositioned_on_rehydration() {
        // A prior repair persisted its fills at the tail (append-only), so
        // a fresh hydration loads them AFTER the nudge; the normalize pass
        // must move them back adjacent without creating new fills.
        let msgs = vec![assistant_with_uses(&["a"]), user_text("nudge"), result("a")];
        let (repaired, fills) = repair_tool_pairing(msgs);
        assert!(
            fills.is_empty(),
            "existing result must be reused, not re-filled"
        );
        assert_eq!(ids_in_order(&repaired), vec!["use:a", "res:a"]);
        assert_eq!(repaired[2].role, Role::User);
    }

    #[test]
    fn orphan_result_stays_in_place_without_fills() {
        let msgs = vec![user_text("q"), result("ghost")];
        let (repaired, fills) = repair_tool_pairing(msgs.clone());
        assert!(fills.is_empty());
        assert_eq!(repaired.len(), msgs.len());
    }

    #[test]
    fn two_wedged_batches_are_both_repaired() {
        let msgs = vec![
            assistant_with_uses(&["a"]),
            user_text("resume 1"),
            assistant_with_uses(&["b", "c"]),
            result("b"),
        ];
        let (repaired, fills) = repair_tool_pairing(msgs);
        assert_eq!(fills.len(), 2);
        assert_eq!(
            ids_in_order(&repaired),
            vec!["use:a", "res:a", "use:b", "use:c", "res:b", "res:c"]
        );
    }
}
