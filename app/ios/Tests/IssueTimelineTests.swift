import Foundation
import Testing

@testable import Baybo

/// What the native side reads off a card's timeline.
///
/// The timeline arrives as raw gateway JSON because the issue webview renders
/// it; these are the three questions the native dock and the Waiting strip ask
/// of it, and each has a wrong answer that looks right.
@Suite struct IssueTimelineTests {
    private func events(_ items: String) -> [IssueEvent] {
        try! IssueEvent.decodeList("{\"items\":[\(items)]}")
    }

    private func requested(_ id: String, callId: String, by handle: String? = "dev-1", at: Int = 0)
        -> String
    {
        let actor =
            handle.map { "{\"kind\":\"agent\",\"id\":\"a-\($0)\",\"handle\":\"\($0)\"}" }
            ?? "{\"kind\":\"user\"}"
        return """
            {"id":"\(id)","number":1,"actor":\(actor),"created_at_ms":\(at),
             "body":{"kind":"approval_requested","call_id":"\(callId)","tool":"exec_command","summary":"cargo test"}}
            """
    }

    private func resolved(_ id: String, callId: String) -> String {
        """
        {"id":"\(id)","number":1,"actor":{"kind":"system"},"created_at_ms":9,
         "body":{"kind":"approval_resolved","call_id":"\(callId)","decision":"approve","resolution":"answered"}}
        """
    }

    /// A prompt is retired by `call_id`, not by "the newest resolution wins" —
    /// one card can hold several across a run, and a resolution retires exactly
    /// one of them.
    @Test func aResolutionRetiresItsOwnPromptAndLeavesTheRest() {
        let timeline = events(
            [
                requested("e1", callId: "c1"),
                requested("e2", callId: "c2"),
                resolved("e3", callId: "c1"),
            ].joined(separator: ","))
        let pending = IssueTimeline.pendingApprovals(in: timeline)
        #expect(pending.map(\.callId) == ["c2"])
        #expect(pending.first?.tool == "exec_command")
        #expect(pending.first?.askedBy == "dev-1")
    }

    /// A re-request after a resolution opens the prompt again — the same call
    /// id can be asked twice in a run.
    @Test func aReRequestReopensThePrompt() {
        let timeline = events(
            [
                requested("e1", callId: "c1"),
                resolved("e2", callId: "c1"),
                requested("e3", callId: "c1", at: 20),
            ].joined(separator: ","))
        #expect(IssueTimeline.pendingApprovals(in: timeline).map(\.callId) == ["c1"])
    }

    /// An OPERATOR's block is that operator saying stop, and nothing should
    /// invite them to answer themselves. Only an agent-authored block is a
    /// question — which is also the one card the board's own driver never
    /// comes back to.
    @Test func onlyAnAgentAuthoredBlockIsAQuestion() {
        let byAgent = events(
            """
            {"id":"e1","number":1,"actor":{"kind":"agent","id":"a-lead","handle":"lead"},
             "created_at_ms":5,"body":{"kind":"blocked","reason":"needs the relay token format"}}
            """)
        let question = IssueTimeline.agentQuestion(
            blockedReason: "needs the relay token format", events: byAgent)
        #expect(question?.askedBy == "lead")
        #expect(question?.question == "needs the relay token format")

        let byOperator = events(
            """
            {"id":"e1","number":1,"actor":{"kind":"user"},"created_at_ms":5,
             "body":{"kind":"blocked","reason":"stop for now"}}
            """)
        #expect(IssueTimeline.agentQuestion(blockedReason: "stop for now", events: byOperator) == nil)
    }

    /// An unblocked card asks nothing, whatever its history says.
    @Test func aCardThatIsNotBlockedAsksNothing() {
        let timeline = events(
            """
            {"id":"e1","number":1,"actor":{"kind":"agent","id":"a-lead","handle":"lead"},
             "created_at_ms":5,"body":{"kind":"blocked","reason":"was blocked once"}}
            """)
        #expect(IssueTimeline.agentQuestion(blockedReason: nil, events: timeline) == nil)
    }

    /// The NEWEST block is the one in force: an earlier one may have been
    /// lifted and re-applied by somebody else entirely.
    @Test func theNewestBlockIsTheOneInForce() {
        let timeline = events(
            [
                """
                {"id":"e1","number":1,"actor":{"kind":"agent","id":"a-lead","handle":"lead"},
                 "created_at_ms":1,"body":{"kind":"blocked","reason":"first"}}
                """,
                """
                {"id":"e2","number":1,"actor":{"kind":"user"},"created_at_ms":9,
                 "body":{"kind":"blocked","reason":"second"}}
                """,
            ].joined(separator: ","))
        #expect(IssueTimeline.agentQuestion(blockedReason: "second", events: timeline) == nil)
    }

    /// A kind this build has never heard of is carried, not dropped: the
    /// timeline is rendered by the webview, and a native decoder that threw on
    /// a new kind would take the whole card's Activity with it.
    @Test func anUnknownKindStillDecodes() {
        let timeline = events(
            """
            {"id":"e1","number":1,"actor":{"kind":"system"},"created_at_ms":1,
             "body":{"kind":"swimlane_changed","lane":"fast"}}
            """)
        #expect(timeline.count == 1)
        #expect(timeline.first?.kind == "swimlane_changed")
        #expect(IssueTimeline.pendingApprovals(in: timeline).isEmpty)
    }

    /// Malformed input costs the entry, not the card.
    @Test func aMalformedEnvelopeYieldsNoEntriesRatherThanThrowing() throws {
        #expect(try IssueEvent.decodeList("{\"items\":[]}").isEmpty)
        #expect(try IssueEvent.decodeList("{}").isEmpty)
        // An entry without an id or a body kind is not one.
        #expect(try IssueEvent.decodeList("{\"items\":[{\"number\":1}]}").isEmpty)
    }

    /// The envelope carries the entries AND where the reader should land in
    /// them. The id is the gateway's answer — nothing here decides what counts
    /// as unread, which is why there is no rule to test, only a field to read.
    @Test func theEnvelopeCarriesWhereTheReaderStopped() throws {
        let envelope = """
            {"items":[
              {"id":"e1","number":1,"created_at_ms":1,"body":{"kind":"comment","text":"old"}},
              {"id":"e2","number":1,"created_at_ms":2,"body":{"kind":"comment","text":"new"}}
            ],"first_unread":"e2"}
            """
        let timeline = try IssueEvent.decodeTimeline(envelope)
        #expect(timeline.events.map(\.id) == ["e1", "e2"])
        #expect(timeline.firstUnread == "e2")

        // Omitted, never null, on a card with nothing new — a gateway that
        // started sending `null` would read the same here.
        #expect(try IssueEvent.decodeTimeline("{\"items\":[]}").firstUnread == nil)
        #expect(try IssueEvent.decodeTimeline("{\"items\":[],\"first_unread\":null}").firstUnread == nil)
    }
}
