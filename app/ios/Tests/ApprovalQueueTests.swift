import Foundation
import Testing

@testable import Baybo

/// The four frame inputs of the approval gate. Each arm below encodes a bug the
/// feature shipped with: the waker's duplicate `approval_requested`, the
/// broadcast `approval_resolved` (matched by PROMPT id, never by session), the
/// `subscribe_state` snapshot that REPLACES rather than appends, and the
/// timed-out gate that only `tool_completed` retires.
///
/// The frame literals are spelled out rather than built from app constants: they
/// are the WIRE contract with the gateway, and a test that reused the app's own
/// spelling of a key would sail straight through a rename that breaks it.
///
/// `apply` is `mutating`, and `#expect` reflects its expression through an
/// immutable capture — so every fold binds its result first and asserts on that.
@Suite @MainActor
struct ApprovalQueueTests {
    private nonisolated static let promptId = "prompt-1"
    private nonisolated static let toolCallId = "toolcall-1"

    private var queue = ApprovalQueue()

    private func requested(
        callId: String = promptId,
        toolCallId: String? = ApprovalQueueTests.toolCallId,
        tool: String = "Bash",
        accesses: [[String: Any]] = []
    ) -> [String: Any] {
        var frame: [String: Any] = [
            "kind": "approval_requested",
            "session_id": "s-1",
            "call_id": callId,
            "tool": tool,
            "params_preview": #"{"command":"rm -rf /tmp/x"}"#,
            "accesses": accesses,
        ]
        if let toolCallId { frame["tool_call_id"] = toolCallId }
        return frame
    }

    private func resolved(callId: String, sessionId: String = "s-1") -> [String: Any] {
        ["kind": "approval_resolved", "session_id": sessionId, "call_id": callId]
    }

    private func toolCompleted(callId: String) -> [String: Any] {
        ["kind": "tool_completed", "call_id": callId, "tool": "Bash"]
    }

    private func subscribeState(pending: [[String: Any]]) -> [String: Any] {
        ["kind": "subscribe_state", "pending_approvals": pending]
    }

    @Test mutating func requestedFrameQueuesACard() {
        let queued = queue.apply(requested())
        #expect(queued)
        #expect(queue.cards.map(\.callId) == [Self.promptId])
        #expect(queue.cards.first?.toolCallId == Self.toolCallId)
        #expect(queue.cards.first?.tool == "Bash")
        #expect(queue.cards.first?.paramsPreview == #"{"command":"rm -rf /tmp/x"}"#)
    }

    /// The gate's waker re-fires on the newest queue entry, so the SAME prompt
    /// legitimately arrives twice. Appending it twice doubles the card and
    /// inflates the composer's "+N queued" counter.
    @Test mutating func duplicateRequestIsDeduped() {
        _ = queue.apply(requested())
        let again = queue.apply(requested())
        #expect(again == false)
        #expect(queue.cards.count == 1)
    }

    @Test mutating func distinctPromptsQueueOldestFirst() {
        _ = queue.apply(requested(callId: "p-1", toolCallId: "t-1"))
        _ = queue.apply(requested(callId: "p-2", toolCallId: "t-2"))
        #expect(queue.cards.map(\.callId) == ["p-1", "p-2"])
    }

    @Test mutating func resolvedRetiresThePromptById() {
        _ = queue.apply(requested())
        let retired = queue.apply(resolved(callId: Self.promptId))
        #expect(retired)
        #expect(queue.cards.isEmpty)
    }

    /// `approval_resolved` is broadcast to EVERY session's sink, so the card is
    /// matched by prompt id alone. Filtering on `session_id` would strand a card
    /// another client already answered.
    @Test mutating func resolvedFromAnotherSessionStillRetiresTheCard() {
        _ = queue.apply(requested())
        let retired = queue.apply(resolved(callId: Self.promptId, sessionId: "other-session"))
        #expect(retired)
        #expect(queue.cards.isEmpty)
    }

    @Test mutating func resolvedForAnUnknownPromptChangesNothing() {
        _ = queue.apply(requested())
        let changed = queue.apply(resolved(callId: "never-heard-of-it"))
        #expect(changed == false)
        #expect(queue.cards.count == 1)
    }

    /// The id mixup, first direction. `approval_resolved.call_id` is the PROMPT
    /// id; matching it against `toolCallId` compiles, typechecks, and silently
    /// retires the wrong card.
    @Test mutating func resolvedDoesNotMatchTheToolCallId() {
        _ = queue.apply(requested())
        let changed = queue.apply(resolved(callId: Self.toolCallId))
        #expect(changed == false)
        #expect(queue.cards.map(\.callId) == [Self.promptId])
    }

    /// A gate that TIMES OUT denies server-side and broadcasts NO
    /// `approval_resolved` — the tool's completion is the only signal that
    /// retires the card. Its `call_id` is the BLOCKED TOOL CALL's id.
    @Test mutating func toolCompletedRetiresATimedOutGate() {
        _ = queue.apply(requested())
        let retired = queue.apply(toolCompleted(callId: Self.toolCallId))
        #expect(retired)
        #expect(queue.cards.isEmpty)
    }

    /// The id mixup, second direction: `tool_completed` carries the tool call's
    /// id under the same `call_id` key, so matching it against the PROMPT id
    /// retires nothing and the card lingers over a call that is long done.
    @Test mutating func toolCompletedDoesNotMatchThePromptId() {
        _ = queue.apply(requested())
        let changed = queue.apply(toolCompleted(callId: Self.promptId))
        #expect(changed == false)
        #expect(queue.cards.count == 1)
    }

    @Test mutating func toolCompletedIgnoresACardWithNoToolCallId() {
        _ = queue.apply(requested(toolCallId: nil))
        #expect(queue.cards.first?.toolCallId == nil)
        let changed = queue.apply(toolCompleted(callId: Self.toolCallId))
        #expect(changed == false)
        #expect(queue.cards.count == 1)
    }

    /// `subscribe_state` carries the authoritative set. Appending instead of
    /// replacing means a resubscribe resurrects a card the user already
    /// answered.
    @Test mutating func subscribeStateReplacesTheQueue() {
        _ = queue.apply(requested(callId: "stale", toolCallId: "t-stale"))
        let replaced = queue.apply(
            subscribeState(pending: [requested(callId: "live", toolCallId: "t-live")]))
        #expect(replaced)
        #expect(queue.cards.map(\.callId) == ["live"])
    }

    @Test mutating func emptySubscribeStateClearsAnAnsweredCard() {
        _ = queue.apply(requested())
        let cleared = queue.apply(subscribeState(pending: []))
        #expect(cleared)
        #expect(queue.cards.isEmpty)
    }

    /// A resubscribe carrying the same set must not republish — the fold runs on
    /// every frame, and the composer's card would flicker on each one.
    @Test mutating func unchangedSubscribeStateReportsNoChange() {
        _ = queue.apply(requested())
        let changed = queue.apply(subscribeState(pending: [requested()]))
        #expect(changed == false)
        #expect(queue.cards.count == 1)
    }

    /// A card is `nil` for a shape we can't render. (A `[String: Any]` frame is
    /// not `Sendable`, so these can't ride `@Test(arguments:)`.)
    @Test mutating func malformedRequestIsIgnored() {
        let malformed: [[String: Any]] = [
            ["kind": "approval_requested", "tool": "Bash"],
            ["kind": "approval_requested", "call_id": "", "tool": "Bash"],
            ["kind": "approval_requested", "call_id": "p-1"],
        ]
        for frame in malformed {
            let changed = queue.apply(frame)
            #expect(changed == false)
            #expect(queue.cards.isEmpty)
        }
    }

    /// Every streamed delta runs through the fold; nothing but the four inputs
    /// may touch the queue.
    @Test(arguments: ["answer_delta", "message", "tool_started", "reasoning", "notice"])
    mutating func unrelatedFramesLeaveTheQueueAlone(kind: String) {
        _ = queue.apply(requested())
        let changed = queue.apply(["kind": kind, "call_id": Self.toolCallId])
        #expect(changed == false)
        #expect(queue.cards.count == 1)
    }

    /// The optimistic dismiss when the user taps approve/deny — the server's
    /// `approval_resolved` fan-out confirms it, and re-adding the card on the
    /// frame's return would flash a prompt they just answered.
    @Test mutating func resolveByPromptIdIsTheOptimisticDismiss() {
        _ = queue.apply(requested())
        let dismissed = queue.resolve(callId: Self.promptId)
        #expect(dismissed)
        #expect(queue.cards.isEmpty)

        let again = queue.resolve(callId: Self.promptId)
        #expect(again == false)
    }

    /// The accesses are what the user is being asked to judge, so an unknown
    /// resource kind falls back to its raw kind rather than vanishing from the
    /// card.
    @Test mutating func accessesRenderIncludingAnUnknownKind() {
        _ = queue.apply(
            requested(accesses: [
                ["kind": "read_file", "path": "/etc/hosts"],
                ["kind": "exec_command", "command": "rm -rf /tmp/x"],
                ["kind": "quantum_teleport"],
            ]))
        let accesses = queue.cards.first?.accesses ?? []
        #expect(accesses.count == 3)
        // The lines are localized, so assert on the interpolated resource rather
        // than on the simulator's current language.
        #expect(accesses.first?.contains("/etc/hosts") == true)
        #expect(accesses.dropFirst().first?.contains("rm -rf /tmp/x") == true)
        #expect(accesses.last == "quantum_teleport")
    }
}
