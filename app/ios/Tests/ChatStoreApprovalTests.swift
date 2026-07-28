import Combine
import Foundation
import Testing

@testable import Baybo

/// The approval gate as the SCREEN sees it: frames off the live leg → the
/// `@Published pendingApprovals` the composer's card renders, and the user's
/// answer back out over the leg.
///
/// `ApprovalQueueTests` covers the pure fold. This covers the wiring, which is
/// the half that can silently vanish: drop the `pendingApprovals = approvals.cards`
/// mirror in `approvalObserveFrame` and the queue still folds perfectly — the
/// card just never appears, and the gate denies itself five minutes later with
/// the user never asked. Nothing in the fold's own suite can see that.
///
/// Frames arrive the production way (the core's `FrameSink`), so `pushFrame` and
/// its observers run exactly as they do on a real leg.
@Suite @MainActor
struct ChatStoreApprovalTests {
    private static let sessionId = "s-1"
    private nonisolated static let promptId = "prompt-1"
    private nonisolated static let toolCallId = "toolcall-1"
    /// Long enough for a delivered frame to drain the main actor, short enough
    /// that the cases asserting something NEVER happens don't stall the suite.
    private static let negativeTimeout: Duration = .milliseconds(250)

    private let temp: TempSupportDir
    private let client = FakeBayboClient()
    private let index: SessionIndex
    private let store: ChatStore

    init() {
        let temp = TempSupportDir()
        self.temp = temp
        index = temp.makeIndex()
        index.touch(sessionId: Self.sessionId)
        store = ChatStore(
            sessionId: Self.sessionId, client: client, index: index,
            outbox: temp.makeOutbox(sessionId: Self.sessionId))
    }

    private func connect() async -> Bool {
        store.connect()
        return await waitUntil { store.connState == .connected }
    }

    private func deliver(_ frame: String) {
        client.deliver(frame, to: Self.sessionId)
    }

    /// The wire shapes, spelled out rather than built from app constants — they
    /// are the contract with the gateway, and a test that reused the app's own
    /// spelling of a key would sail straight through a rename that breaks it.
    private static let requestedFrame = """
        {"kind":"approval_requested","session_id":"s-1","call_id":"\(promptId)",
         "tool_call_id":"\(toolCallId)","tool":"Bash",
         "params_preview":"{\\"command\\":\\"rm -rf /tmp/x\\"}","accesses":[]}
        """

    private static let resolvedFrame = """
        {"kind":"approval_resolved","session_id":"s-1","call_id":"\(promptId)"}
        """

    private static let toolCompletedFrame = """
        {"kind":"tool_completed","call_id":"\(toolCallId)","tool":"Bash"}
        """

    /// The headline: a prompt on the leg reaches the composer's published queue.
    /// This is the one assertion that fails if the mirror write is ever dropped.
    @Test func aRequestedPromptSurfacesOnThePublishedQueue() async {
        #expect(await connect())
        deliver(Self.requestedFrame)

        #expect(await waitUntil { store.pendingApprovals.count == 1 })
        let card = store.pendingApprovals.first
        #expect(card?.callId == Self.promptId)
        #expect(card?.toolCallId == Self.toolCallId)
        #expect(card?.tool == "Bash")
    }

    /// Another client answered: the broadcast resolution retires the card here
    /// too, and the published queue empties.
    @Test func aResolutionFromAnotherClientClearsThePublishedCard() async {
        #expect(await connect())
        deliver(Self.requestedFrame)
        #expect(await waitUntil { store.pendingApprovals.count == 1 })

        deliver(Self.resolvedFrame)
        #expect(await waitUntil { store.pendingApprovals.isEmpty })
    }

    /// A gate that TIMES OUT denies server-side and broadcasts no resolution —
    /// the tool's completion is the only thing that clears the card, and it is
    /// matched by the BLOCKED CALL's id, not the prompt's.
    @Test func aTimedOutGatesToolCompletionClearsThePublishedCard() async {
        #expect(await connect())
        deliver(Self.requestedFrame)
        #expect(await waitUntil { store.pendingApprovals.count == 1 })

        deliver(Self.toolCompletedFrame)
        #expect(await waitUntil { store.pendingApprovals.isEmpty })
    }

    /// The user tapped Approve: the card goes at once (the server's fan-out
    /// would otherwise flash a prompt they just answered) and the decision is
    /// echoed on the leg under the PROMPT's id.
    @Test func answeringDismissesAtOnceAndEchoesTheDecision() async {
        #expect(await connect())
        deliver(Self.requestedFrame)
        #expect(await waitUntil { store.pendingApprovals.count == 1 })

        guard let card = store.pendingApprovals.first else {
            Issue.record("a prompt must be pending")
            return
        }
        store.resolveApproval(card, decision: .approve)
        #expect(store.pendingApprovals.isEmpty, "the dismiss is optimistic, not awaited")

        #expect(await waitUntil { !client.approvalCalls.isEmpty })
        #expect(
            client.approvalCalls == [
                FakeBayboClient.ApprovalCall(callId: Self.promptId, decision: .approve)
            ])
    }

    /// A leg that can't carry the decision loses it — the gate will deny on its
    /// own timeout — so the user has to be told.
    @Test func aLegThatCannotCarryTheDecisionRaisesANotice() async {
        client.failApproval(with: BayboError.Other(message: "leg is down"))
        #expect(await connect())
        deliver(Self.requestedFrame)
        #expect(await waitUntil { store.pendingApprovals.count == 1 })

        guard let card = store.pendingApprovals.first else {
            Issue.record("a prompt must be pending")
            return
        }
        store.resolveApproval(card, decision: .deny)

        #expect(await waitUntil { store.notice != nil })
        #expect(store.pendingApprovals.isEmpty)
    }

    /// And the line leaves with the conversation. It names no staged tile, so
    /// the composer's strip cannot retract it, and this store stays cached
    /// resident (`AppStore`'s LRU) — until the store took its own line back,
    /// "Could not send the decision" stood over the composer on the next visit.
    @Test func theLostDecisionsNoticeLeavesWithTheChat() async {
        client.failApproval(with: BayboError.Other(message: "leg is down"))
        #expect(await connect())
        deliver(Self.requestedFrame)
        #expect(await waitUntil { store.pendingApprovals.count == 1 })

        guard let card = store.pendingApprovals.first else {
            Issue.record("a prompt must be pending")
            return
        }
        store.resolveApproval(card, decision: .deny)
        #expect(await waitUntil { store.notice != nil })

        store.leaveChat()

        #expect(store.notice == nil)
    }

    /// And the half retraction cannot reach: the card dismisses optimistically,
    /// the user backs out, and the echo fails SECONDS LATER — `leaveChat` had
    /// already taken back everything on the dock and nothing cancels the Task,
    /// so the line landed on a conversation the user had left and was there in
    /// red on the next visit. The echo is dispatched before the leave and lands
    /// after it, which is the race itself; asserting only that the call reached
    /// the leg is what proves the catch ran at all.
    @Test func aDecisionThatFailsAfterTheUserLeftRaisesNoLine() async {
        client.failApproval(with: BayboError.Other(message: "leg is down"))
        #expect(await connect())
        deliver(Self.requestedFrame)
        #expect(await waitUntil { store.pendingApprovals.count == 1 })

        guard let card = store.pendingApprovals.first else {
            Issue.record("a prompt must be pending")
            return
        }
        store.resolveApproval(card, decision: .deny)
        store.leaveChat()

        #expect(await waitUntil { !client.approvalCalls.isEmpty }, "the echo still goes out")
        try? await Task.sleep(for: Self.negativeTimeout)
        #expect(store.notice == nil, "a line raised after the leave is refused, not stranded")
    }

    /// The offscreen buffer is the WEBVIEW's replay queue; overflowing it drops
    /// frames. The approval observer runs AHEAD of that bail on purpose — a
    /// `sync_page` carries rows, not pending prompts, so a card dropped with the
    /// buffer would never come back. Hoist the overflow check above the
    /// observers and this is the test that notices.
    @Test func aPromptStillLandsAfterTheOffscreenBufferOverflowed() async {
        #expect(await connect())
        // No bridge is ever attached here, so every frame buffers — past the cap
        // the buffer is dropped and the store parks on `needsSyncOnAttach`.
        for i in 0...ChatStore.maxBufferedFrames {
            deliver(#"{"kind":"answer_delta","content":"\#(i)"}"#)
        }
        deliver(Self.requestedFrame)

        #expect(await waitUntil { store.pendingApprovals.count == 1 })
        #expect(store.pendingApprovals.first?.callId == Self.promptId)
    }

    /// The fold's `-> Bool` is load-bearing: `pushFrame` runs it on EVERY frame,
    /// streamed deltas included, so republishing unconditionally would invalidate
    /// the composer (and its approval card) on every single token.
    @Test func aStreamedDeltaNeverRepublishesTheStore() async {
        #expect(await connect())
        var publishes = 0
        let sink = store.objectWillChange.sink { _ in publishes += 1 }
        defer { sink.cancel() }

        for i in 0..<20 {
            deliver(#"{"kind":"answer_delta","content":"\#(i)"}"#)
        }
        _ = await waitUntil(timeout: Self.negativeTimeout) { publishes > 0 }
        #expect(publishes == 0, "a delta changes no published state")

        deliver(Self.requestedFrame)
        #expect(await waitUntil { publishes > 0 }, "a real card does republish")
    }
}
