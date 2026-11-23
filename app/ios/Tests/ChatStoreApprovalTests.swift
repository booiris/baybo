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
            outbox: temp.makeOutbox(sessionId: Self.sessionId),
            supportDirectory: temp.url)
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
        {"kind":"approval_requested","session_id":"s-1","ask_id":"\(promptId)",
         "tool_call_id":"\(toolCallId)","tool":"Bash",
         "params_preview":"{\\"command\\":\\"rm -rf /tmp/x\\"}","accesses":[]}
        """

    private static let resolvedFrame = """
        {"kind":"ask_resolved","session_id":"s-1","ask_id":"\(promptId)","outcome":"answered"}
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
        #expect(card?.askId == Self.promptId)
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
                FakeBayboClient.ApprovalCall(askId: Self.promptId, decision: .approve)
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
        #expect(store.pendingApprovals.first?.askId == Self.promptId)
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

    @Test func questionDraftSurvivesSnapshotAndSubmitsEachTabInOrder() async throws {
        #expect(await connect())
        let snapshot = """
            {"kind":"subscribe_state","session_id":"s-1","pending_questions":[
              {"ask_id":"trip","tool_call_id":"ask-trip","questions":[
                {"header":"Travel","question":"How?","multi_select":false,"options":[{"label":"Train","description":""}]},
                {"header":"Visits","question":"What?","multi_select":true,"options":[
                  {"label":"Art","description":""},{"label":"Food","description":""}]}]}]}
            """
        deliver(snapshot)
        #expect(await waitUntil { store.hasPendingQuestion })
        let draft = try #require(store.questionDraft)
        store.performQuestionAction(.init(askId: "stale", active: 0, action: .select, index: 0))
        store.performQuestionAction(.init(askId: "trip", active: 0, action: .tab, index: -1))
        store.performQuestionAction(.init(askId: "trip", active: 0, action: .next, index: nil))
        #expect(draft.active == 0 && draft.current.selected.isEmpty)
        store.performQuestionAction(.init(askId: "trip", active: 0, action: .tab, index: 1))
        store.performQuestionAction(.init(askId: "trip", active: 0, action: .select, index: 1))
        #expect(draft.current.selected.isEmpty)
        store.performQuestionAction(.init(askId: "trip", active: 1, action: .select, index: 0))
        store.performQuestionAction(.init(askId: "trip", active: 1, action: .submit, index: nil))
        #expect(store.submittingQuestion == nil)
        #expect(draft.nextTab == nil)
        #expect(!draft.canContinue(connected: true))
        #expect(draft.reply == nil)
        draft.select("Art")
        draft.selectTab(0)
        draft.select("Train")
        #expect(draft.active == 0)
        #expect(draft.answers[0].selected == ["Train"])
        draft.write("  Bike  ", at: draft.active)
        #expect(draft.active == 0)
        #expect(draft.current.selected.isEmpty)
        #expect(draft.nextTab == 1)
        #expect(draft.canContinue(connected: false))
        #expect(draft.reply == nil)
        draft.advance()
        draft.select("Art")
        draft.select("Food")
        draft.select("Art")
        draft.write("Coffee", at: draft.active)
        deliver(snapshot)
        // The following approval is a barrier on the same ordered frame stream.
        deliver(Self.requestedFrame)
        #expect(await waitUntil { !store.pendingApprovals.isEmpty })
        #expect(store.questionDraft === draft)
        #expect(draft.active == 1)
        #expect(draft.canContinue(connected: true))
        #expect(!draft.canContinue(connected: false))
        store.performQuestionAction(.init(askId: "trip", active: 1, action: .submit, index: nil))
        store.performQuestionAction(.init(askId: "trip", active: 1, action: .submit, index: nil))
        #expect(await waitUntil { client.questionReplies.count == 1 })
        let json = try #require(client.questionReplies.first?.1.data(using: .utf8))
        let payload = try #require(JSONSerialization.jsonObject(with: json) as? [String: Any])
        let answers = try #require(payload["answers"] as? [[String: Any]])
        #expect(payload["kind"] as? String == "answered")
        #expect(answers.compactMap { $0["header"] as? String } == ["Travel", "Visits"])
        #expect(answers.compactMap { $0["selected"] as? [String] } == [[], ["Food"]])
        #expect(answers.compactMap { $0["other"] as? String } == ["Bike", "Coffee"])
    }

    @Test func manualTabSwitchStopsAutomaticAdvanceForTheRestOfTheDraft() {
        let card = PendingQuestion(
            askId: "trip", toolCallId: nil,
            questions: ["Travel", "Visits", "Pace"].map {
                .init(header: $0, question: $0, options: [.init(label: "Any", description: "")], multiSelect: false)
            })
        let draft = QuestionDraft(card)
        draft.selectTab(0)
        draft.select("Any")
        #expect(draft.active == 0 && draft.current.selected == ["Any"])
        draft.advanceAfterSelection(at: 0)
        #expect(draft.active == 1)
        draft.selectTab(0)
        draft.select("Any")
        draft.advanceAfterSelection(at: 0)
        #expect(draft.active == 0)
        draft.advance()
        draft.select("Any")
        #expect(draft.active == 1)
        let fresh = QuestionDraft(card)
        fresh.select("Any")
        fresh.advanceAfterSelection(at: 1)
        #expect(fresh.active == 0)
        fresh.advanceAfterSelection(at: 0)
        fresh.select("Any")
        fresh.advanceAfterSelection(at: 0)
        #expect(fresh.active == 2)
    }

    @Test func restoredQuestionHoldsMessagesUntilTheContinuedTurnEnds() async throws {
        #expect(await connect())
        deliver("""
            {"kind":"subscribe_state","session_id":"s-1","turn":{"active":false},
             "pending_questions":[{"ask_id":"q1","tool_call_id":"call","queue_index":0,
             "questions":[{"header":"Pace","question":"Which pace?","multi_select":false,
             "options":[{"label":"Slow","description":"Relax"},{"label":"Fast","description":"Explore"}]}]}]}
            """)
        #expect(await waitUntil { store.hasPendingQuestion })
        let draft = try #require(store.questionDraft)
        draft.select("Slow")
        #expect(draft.active == 0)
        #expect(draft.reply?.answers?.first?.selected == ["Slow"])
        #expect(client.questionReplies.isEmpty)
        store.setAgentRunning(true)
        #expect(store.composerShowsStop(hasDraft: false))
        #expect(!store.composerShowsStop(hasDraft: true))
        store.send(text: "arriving Saturday", attachments: [])
        #expect(store.queuedQuestionMessages == 1)
        #expect(client.sentMessages.isEmpty)
        store.answerQuestion(askId: "other", reply: .cancelled)
        store.answerQuestion(askId: "q1", reply: .cancelled)
        #expect(await waitUntil { client.questionReplies.map(\.0) == ["q1"] })
        deliver(#"{"kind":"ask_resolved","ask_id":"q1","outcome":"cancelled"}"#)
        #expect(await waitUntil { !store.hasPendingQuestion })
        #expect(client.sentMessages.isEmpty)
        store.send(text: "leaving Sunday", attachments: [])
        #expect(store.queuedQuestionMessages == 2)
        #expect(!store.composerShowsStop(hasDraft: true))
        deliver(#"{"kind":"turn_state","active":false}"#)
        #expect(await waitUntil { client.sentMessages.map(\.text) == ["arriving Saturday", "leaving Sunday"] })
    }
}
