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
//! [`repair_tool_pairing`] does four things in one deterministic pass:
//!
//! 1. **Disambiguate**: transcripts written before Gemini tool calls got
//!    unique ids reuse the function name as the id, so one id can name several
//!    calls. Each repeated `ToolUse` id gets a suffixed id
//!    ([`disambiguate_repeated_call_ids`]) and the result answering that call
//!    follows it, in the window only. The renaming depends only on the rows
//!    before it, so it is identical on every load and a persisted fill keyed to
//!    a renamed id keeps pairing.
//! 2. **Quarantine**: a `ToolResult` with no matching `ToolUse`, or a second
//!    result for an already-answered call, is omitted from the in-memory LLM
//!    window. Its durable row is untouched so the user-facing transcript stays
//!    recoverable.
//! 3. **Fill**: every dangling `ToolUse` id gets a synthetic `ToolResult`
//!    saying the execution was interrupted. Fills are returned separately
//!    so the caller can persist them (append-only — session rows are
//!    never rewritten).
//! 4. **Reposition**: every tool-result row is emitted immediately after
//!    the assistant row that issued it, in `ToolUse` block order. On a
//!    healthy transcript this is a no-op; on an already-wedged one (a
//!    resume nudge was appended after the dangling row before this repair
//!    existed, or a prior repair's fills were persisted at the tail) it
//!    restores the adjacency both OpenAI and Anthropic validate. The
//!    in-memory window may therefore diverge from strict ordinal order
//!    for such sessions. Quarantined blocks and disambiguated ids are the
//!    other intentional divergences: storage keeps the rows as written, and
//!    only the provider-facing window differs.

use std::collections::{HashMap, HashSet};

use baybo_model::{ChatMessage, ContentBlock};

/// Body of the synthetic `ToolResult` (wrapped in the standard
/// `<tool_output>` envelope) for a tool call whose execution was cut off
/// by a crash/restart before it produced a result.
const INTERRUPTED_TOOL_RESULT_BODY: &str =
    "[tool execution was interrupted by a crash or restart before it produced a result]";

/// Joins a repeated tool-call id to the occurrence number that makes it
/// unique (`web_search` → `web_search_2`); `_` is valid in every provider's
/// tool-call id alphabet.
const REPEATED_CALL_ID_SEPARATOR: char = '_';

/// Occurrence number given to the first repeat of a tool-call id.
const FIRST_REPEAT_ORDINAL: usize = 2;

pub(crate) struct ToolPairingRepair {
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) fills: Vec<ChatMessage>,
    pub(crate) orphan_result_ids: Vec<String>,
    pub(crate) duplicate_result_ids: Vec<String>,
    /// Original ids of the `ToolUse` blocks given a new id in the window.
    pub(crate) renamed_call_ids: Vec<String>,
}

struct CallSlot {
    original: String,
    renamed: String,
    message: usize,
    answered: bool,
}

fn unused_repeat_id(original: &str, taken: &HashSet<String>) -> String {
    (FIRST_REPEAT_ORDINAL..)
        .map(|n| format!("{original}{REPEATED_CALL_ID_SEPARATOR}{n}"))
        .find(|candidate| !taken.contains(candidate))
        .unwrap_or_else(|| original.to_string())
}

/// The call a result with `id` answers. A synthetic fill was written under
/// the window id of its call, which is unique across the transcript and may
/// sit in any earlier turn (a fill lands at the tail when an earlier fill
/// write failed), so it pairs with the unanswered call carrying that id. Any
/// other result was written under its call's original id and answers the
/// earliest unanswered call originally named `id` in the nearest preceding
/// assistant turn; matching on window ids there would let a tool literally
/// named `<x>_2` steal the answer to a renamed `<x>`.
fn pairing_call<'a>(
    calls: &'a mut [CallSlot],
    id: &str,
    is_fill: bool,
) -> Option<&'a mut CallSlot> {
    if is_fill
        && let Some(i) = calls
            .iter()
            .rposition(|call| !call.answered && call.renamed == id)
    {
        return calls.get_mut(i);
    }
    let message = calls.last()?.message;
    let start = calls
        .iter()
        .rposition(|call| call.message != message)
        .map_or(0, |i| i + 1);
    calls[start..]
        .iter_mut()
        .find(|call| !call.answered && call.original == id)
}

/// Give every repeated `ToolUse` id a unique suffixed id and move the result
/// that answers each call onto it. A call keeps its id unless an earlier call
/// already used it, and a result's pairing depends only on the rows before
/// it, so appending rows never changes an earlier assignment. A result no
/// call is waiting for keeps its id and is left to quarantine.
pub(crate) fn disambiguate_repeated_call_ids(
    mut messages: Vec<ChatMessage>,
) -> (Vec<ChatMessage>, Vec<String>) {
    let mut seen = HashSet::new();
    let repeated = messages
        .iter()
        .flat_map(|message| message.content.iter())
        .any(
            |block| matches!(block, ContentBlock::ToolUse { id, .. } if !seen.insert(id.as_str())),
        );
    if !repeated {
        return (messages, Vec::new());
    }

    let mut taken: HashSet<String> = HashSet::new();
    let mut calls: Vec<CallSlot> = Vec::new();
    let mut renamed = Vec::new();
    for (index, message) in messages.iter_mut().enumerate() {
        for block in &mut message.content {
            match block {
                ContentBlock::ToolUse { id, .. } => {
                    let original = id.clone();
                    if taken.contains(&original) {
                        *id = unused_repeat_id(&original, &taken);
                        renamed.push(original.clone());
                    }
                    taken.insert(id.clone());
                    calls.push(CallSlot {
                        original,
                        renamed: id.clone(),
                        message: index,
                        answered: false,
                    });
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    let is_fill = content.contains(INTERRUPTED_TOOL_RESULT_BODY);
                    if let Some(call) = pairing_call(&mut calls, tool_use_id, is_fill) {
                        call.answered = true;
                        tool_use_id.clone_from(&call.renamed);
                    }
                }
                _ => {}
            }
        }
    }
    (messages, renamed)
}

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

fn quarantine_invalid_results(
    mut messages: Vec<ChatMessage>,
    skip: &HashSet<String>,
) -> (Vec<ChatMessage>, Vec<String>, Vec<String>) {
    let tool_use_ids: HashSet<String> = messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    let mut seen_result_ids = HashSet::new();
    let mut orphan_result_ids = Vec::new();
    let mut duplicate_result_ids = Vec::new();

    for message in &mut messages {
        message.content.retain(|block| {
            let ContentBlock::ToolResult { tool_use_id, .. } = block else {
                return true;
            };
            if skip.contains(tool_use_id) {
                return true;
            }
            if !tool_use_ids.contains(tool_use_id) {
                orphan_result_ids.push(tool_use_id.clone());
                return false;
            }
            if !seen_result_ids.insert(tool_use_id.clone()) {
                duplicate_result_ids.push(tool_use_id.clone());
                return false;
            }
            true
        });
    }
    messages.retain(|message| !message.content.is_empty());

    (messages, orphan_result_ids, duplicate_result_ids)
}

/// Normalize tool pairing over a restored transcript. Synthetic fill rows are
/// returned separately so the caller can persist them. Quarantined result ids
/// are diagnostic only: their original durable rows must not be rewritten or
/// deleted.
///
/// `skip` holds the ids of calls parked on a human ask: they are never filled
/// or quarantined, and a dangling one does not by itself force the repair
/// pass, so a healthy-but-parked transcript keeps the fast path and its
/// dangling `tool_use` row stays the tail for the real result to follow.
pub(crate) fn repair_tool_pairing(
    messages: Vec<ChatMessage>,
    skip: &HashSet<String>,
) -> ToolPairingRepair {
    let (messages, renamed_call_ids) = disambiguate_repeated_call_ids(messages);
    let (messages, orphan_result_ids, duplicate_result_ids) =
        quarantine_invalid_results(messages, skip);

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
            if let ContentBlock::ToolUse { id, .. } = block
                && !skip.contains(id)
            {
                expected_after.insert(id.as_str());
            }
        }
    }
    if !expected_after.is_empty() {
        needs_repair = true; // dangling ToolUse at the tail
    }
    if !needs_repair {
        return ToolPairingRepair {
            messages,
            fills: Vec::new(),
            orphan_result_ids,
            duplicate_result_ids,
            renamed_call_ids,
        };
    }

    let mut consumed: HashSet<usize> = HashSet::new();
    let mut repaired: Vec<ChatMessage> = Vec::with_capacity(messages.len());
    let mut fills: Vec<ChatMessage> = Vec::new();

    for (i, msg) in messages.iter().enumerate() {
        if consumed.contains(&i) {
            continue;
        }
        if is_tool_result_row(msg) {
            continue;
        }
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
                None if skip.contains(&id) => {}
                None => {
                    let fill = synthetic_fill(&id, &name);
                    fills.push(fill.clone());
                    repaired.push(fill);
                }
            }
        }
    }

    ToolPairingRepair {
        messages: repaired,
        fills,
        orphan_result_ids,
        duplicate_result_ids,
        renamed_call_ids,
    }
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
        let repair = repair_tool_pairing(msgs.clone(), &HashSet::new());
        assert!(repair.fills.is_empty());
        assert!(repair.orphan_result_ids.is_empty());
        assert!(repair.duplicate_result_ids.is_empty());
        assert_eq!(repair.messages.len(), msgs.len());
        assert_eq!(ids_in_order(&repair.messages), ids_in_order(&msgs));
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
        let repair = repair_tool_pairing(msgs, &HashSet::new());
        assert_eq!(repair.fills.len(), 2);
        assert_eq!(
            ids_in_order(&repair.messages),
            vec!["use:a", "use:b", "use:c", "res:a", "res:b", "res:c"]
        );
        let body = match &repair.fills[0].content[0] {
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
        let repair = repair_tool_pairing(msgs, &HashSet::new());
        assert_eq!(repair.fills.len(), 1);
        assert_eq!(repair.messages.len(), 3);
        assert!(matches!(
            repair.messages[0].content[0],
            ContentBlock::ToolUse { .. }
        ));
        assert!(
            matches!(&repair.messages[1].content[0], ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "a")
        );
        assert_eq!(repair.messages[2].role, Role::User);
    }

    #[test]
    fn displaced_persisted_fills_are_repositioned_on_rehydration() {
        // A prior repair persisted its fills at the tail (append-only), so
        // a fresh hydration loads them AFTER the nudge; the normalize pass
        // must move them back adjacent without creating new fills.
        let msgs = vec![assistant_with_uses(&["a"]), user_text("nudge"), result("a")];
        let repair = repair_tool_pairing(msgs, &HashSet::new());
        assert!(
            repair.fills.is_empty(),
            "existing result must be reused, not re-filled"
        );
        assert_eq!(ids_in_order(&repair.messages), vec!["use:a", "res:a"]);
        assert_eq!(repair.messages[2].role, Role::User);
    }

    #[test]
    fn orphan_result_is_quarantined_without_mutating_the_input_row() {
        let msgs = vec![user_text("q"), result("ghost")];
        let original = msgs.clone();
        let repair = repair_tool_pairing(msgs, &HashSet::new());
        assert!(repair.fills.is_empty());
        assert_eq!(repair.orphan_result_ids, vec!["ghost"]);
        assert!(repair.duplicate_result_ids.is_empty());
        assert_eq!(repair.messages, vec![user_text("q")]);
        assert_eq!(original[1], result("ghost"));
    }

    #[test]
    fn duplicate_result_is_quarantined() {
        let msgs = vec![assistant_with_uses(&["a"]), result("a"), result("a")];
        let repair = repair_tool_pairing(msgs, &HashSet::new());
        assert!(repair.fills.is_empty());
        assert!(repair.orphan_result_ids.is_empty());
        assert_eq!(repair.duplicate_result_ids, vec!["a"]);
        assert_eq!(ids_in_order(&repair.messages), vec!["use:a", "res:a"]);
    }

    #[test]
    fn repeated_ids_in_one_turn_keep_every_result() {
        let msgs = vec![
            user_text("q"),
            assistant_with_uses(&["web_search", "web_search", "web_search"]),
            result("web_search"),
            result("web_search"),
            result("web_search"),
        ];
        let repair = repair_tool_pairing(msgs.clone(), &HashSet::new());
        assert!(repair.fills.is_empty());
        assert!(repair.duplicate_result_ids.is_empty());
        assert!(repair.orphan_result_ids.is_empty());
        assert_eq!(repair.renamed_call_ids, vec!["web_search", "web_search"]);
        assert_eq!(
            ids_in_order(&repair.messages),
            vec![
                "use:web_search",
                "use:web_search_2",
                "use:web_search_3",
                "res:web_search",
                "res:web_search_2",
                "res:web_search_3",
            ]
        );
        let bodies: Vec<String> = repair.messages[2..]
            .iter()
            .map(|m| match &m.content[0] {
                ContentBlock::ToolResult { content, .. } => content.clone(),
                other => panic!("expected ToolResult, got {other:?}"),
            })
            .collect();
        assert_eq!(bodies, vec!["out-web_search"; 3]);
        assert_eq!(
            repair_tool_pairing(msgs, &HashSet::new()).messages,
            repair.messages
        );
    }

    #[test]
    fn repeated_ids_across_turns_pair_with_their_own_turn() {
        let msgs = vec![
            assistant_with_uses(&["web_search"]),
            result("web_search"),
            user_text("again"),
            assistant_with_uses(&["web_search", "read"]),
            result("read"),
            result("web_search"),
        ];
        let repair = repair_tool_pairing(msgs, &HashSet::new());
        assert!(repair.fills.is_empty());
        assert!(repair.duplicate_result_ids.is_empty());
        assert_eq!(
            ids_in_order(&repair.messages),
            vec![
                "use:web_search",
                "res:web_search",
                "use:web_search_2",
                "use:read",
                "res:read",
                "res:web_search_2",
            ]
        );
    }

    #[test]
    fn disambiguated_ids_skip_ids_already_in_use() {
        let msgs = vec![
            assistant_with_uses(&["web_search_2", "web_search", "web_search"]),
            result("web_search_2"),
            result("web_search"),
            result("web_search"),
            assistant_with_uses(&["web_search_3"]),
            result("web_search_3"),
        ];
        let repair = repair_tool_pairing(msgs, &HashSet::new());
        assert!(repair.duplicate_result_ids.is_empty());
        assert_eq!(
            ids_in_order(&repair.messages),
            vec![
                "use:web_search_2",
                "use:web_search",
                "use:web_search_3",
                "res:web_search_2",
                "res:web_search",
                "res:web_search_3",
                "use:web_search_3_2",
                "res:web_search_3_2",
            ]
        );
    }

    #[test]
    fn fill_for_a_disambiguated_call_pairs_on_the_next_load() {
        let stored = vec![
            assistant_with_uses(&["web_search", "web_search"]),
            result("web_search"),
            user_text("nudge"),
        ];
        let first = repair_tool_pairing(stored.clone(), &HashSet::new());
        assert_eq!(first.fills.len(), 1);
        assert!(matches!(&first.fills[0].content[0],
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "web_search_2"));

        let mut reloaded = stored;
        reloaded.extend(first.fills.iter().cloned());
        let second = repair_tool_pairing(reloaded, &HashSet::new());
        assert!(second.fills.is_empty(), "the persisted fill must pair");
        assert!(second.orphan_result_ids.is_empty());
        assert!(second.duplicate_result_ids.is_empty());
        assert_eq!(second.messages, first.messages);
    }

    #[test]
    fn surplus_result_for_a_repeated_id_is_still_quarantined() {
        let msgs = vec![
            assistant_with_uses(&["web_search", "web_search"]),
            result("web_search"),
            result("web_search"),
            result("web_search"),
        ];
        let repair = repair_tool_pairing(msgs, &HashSet::new());
        assert_eq!(repair.duplicate_result_ids, vec!["web_search"]);
        assert_eq!(
            ids_in_order(&repair.messages),
            vec![
                "use:web_search",
                "use:web_search_2",
                "res:web_search",
                "res:web_search_2"
            ]
        );
    }

    fn result_bodies(msgs: &[ChatMessage]) -> Vec<(String, String)> {
        msgs.iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => Some((tool_use_id.clone(), content.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn tool_named_like_a_rename_keeps_its_own_result() {
        let msgs = vec![
            assistant_with_uses(&["ws", "ws", "ws_2"]),
            result("ws"),
            result("ws_2"),
            result("ws"),
        ];
        let repair = repair_tool_pairing(msgs, &HashSet::new());
        assert!(repair.fills.is_empty());
        assert!(repair.duplicate_result_ids.is_empty());
        assert!(repair.orphan_result_ids.is_empty());
        assert_eq!(
            ids_in_order(&repair.messages),
            vec![
                "use:ws",
                "use:ws_2",
                "use:ws_2_2",
                "res:ws",
                "res:ws_2_2",
                "res:ws_2",
            ]
        );
        assert_eq!(
            result_bodies(&repair.messages),
            vec![
                ("ws".to_string(), "out-ws".to_string()),
                ("ws_2_2".to_string(), "out-ws_2".to_string()),
                ("ws_2".to_string(), "out-ws".to_string()),
            ]
        );
    }

    #[test]
    fn fill_under_a_renamed_id_pairs_beside_a_tool_named_like_it() {
        let stored = vec![
            assistant_with_uses(&["ws", "ws", "ws_2"]),
            result("ws"),
            result("ws_2"),
            user_text("nudge"),
        ];
        let first = repair_tool_pairing(stored.clone(), &HashSet::new());
        assert_eq!(first.fills.len(), 1);
        assert!(matches!(&first.fills[0].content[0],
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "ws_2"));
        assert_eq!(
            result_bodies(&first.messages)[2],
            ("ws_2_2".to_string(), "out-ws_2".to_string())
        );

        let mut reloaded = stored;
        reloaded.extend(first.fills.iter().cloned());
        let second = repair_tool_pairing(reloaded, &HashSet::new());
        assert!(second.fills.is_empty(), "the persisted fill must pair");
        assert!(second.orphan_result_ids.is_empty());
        assert!(second.duplicate_result_ids.is_empty());
        assert_eq!(second.messages, first.messages);
    }

    #[test]
    fn surplus_result_does_not_answer_an_older_turn() {
        let msgs = vec![
            assistant_with_uses(&["ws", "ws"]),
            result("ws"),
            user_text("again"),
            assistant_with_uses(&["ws"]),
            result("ws"),
            result("ws"),
        ];
        let repair = repair_tool_pairing(msgs, &HashSet::new());
        assert_eq!(repair.duplicate_result_ids, vec!["ws"]);
        assert_eq!(repair.fills.len(), 1);
        assert!(matches!(&repair.fills[0].content[0],
            ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "ws_2"));
        assert_eq!(
            ids_in_order(&repair.messages),
            vec![
                "use:ws", "use:ws_2", "res:ws", "res:ws_2", "use:ws_3", "res:ws_3",
            ]
        );
    }

    #[test]
    fn two_wedged_batches_are_both_repaired() {
        let msgs = vec![
            assistant_with_uses(&["a"]),
            user_text("resume 1"),
            assistant_with_uses(&["b", "c"]),
            result("b"),
        ];
        let repair = repair_tool_pairing(msgs, &HashSet::new());
        assert_eq!(repair.fills.len(), 2);
        assert_eq!(
            ids_in_order(&repair.messages),
            vec!["use:a", "res:a", "use:b", "use:c", "res:b", "res:c"]
        );
    }

    fn skip(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_string()).collect()
    }

    #[test]
    fn a_parked_call_is_not_filled_and_stays_the_tail() {
        let msgs = vec![user_text("q"), assistant_with_uses(&["a"])];
        let repair = repair_tool_pairing(msgs.clone(), &skip(&["a"]));
        assert!(repair.fills.is_empty());
        assert_eq!(repair.messages, msgs, "fast path keeps the window as is");
    }

    #[test]
    fn a_parked_call_beside_an_interrupted_one_fills_only_the_other() {
        let msgs = vec![
            user_text("q"),
            assistant_with_uses(&["a", "b"]),
            result("b"),
        ];
        let repair = repair_tool_pairing(msgs, &skip(&["a"]));
        assert!(repair.fills.is_empty(), "b answered, a parked");
        assert_eq!(
            ids_in_order(&repair.messages),
            vec!["use:a", "use:b", "res:b"]
        );

        let msgs = vec![user_text("q"), assistant_with_uses(&["a", "b"])];
        let repair = repair_tool_pairing(msgs, &skip(&["a"]));
        assert_eq!(repair.fills.len(), 1);
        assert_eq!(
            ids_in_order(&repair.messages),
            vec!["use:a", "use:b", "res:b"],
        );
    }

    #[test]
    fn a_result_for_a_parked_id_is_never_quarantined_and_is_repositioned() {
        let msgs = vec![
            assistant_with_uses(&["a"]),
            user_text("displaced"),
            result("a"),
            result("a"),
        ];
        let repair = repair_tool_pairing(msgs, &skip(&["a"]));
        assert!(repair.fills.is_empty());
        assert!(repair.duplicate_result_ids.is_empty());
        assert!(repair.orphan_result_ids.is_empty());
        assert_eq!(ids_in_order(&repair.messages)[..2], ["use:a", "res:a"]);
    }
}
