import Foundation
import Testing

@testable import Baybo

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

    @Test func aReRequestReopensThePrompt() {
        let timeline = events(
            [
                requested("e1", callId: "c1"),
                resolved("e2", callId: "c1"),
                requested("e3", callId: "c1", at: 20),
            ].joined(separator: ","))
        #expect(IssueTimeline.pendingApprovals(in: timeline).map(\.callId) == ["c1"])
    }

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

    @Test func aCardThatIsNotBlockedAsksNothing() {
        let timeline = events(
            """
            {"id":"e1","number":1,"actor":{"kind":"agent","id":"a-lead","handle":"lead"},
             "created_at_ms":5,"body":{"kind":"blocked","reason":"was blocked once"}}
            """)
        #expect(IssueTimeline.agentQuestion(blockedReason: nil, events: timeline) == nil)
    }

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

    @Test func aMalformedEnvelopeYieldsNoEntriesRatherThanThrowing() throws {
        #expect(try IssueEvent.decodeList("{\"items\":[]}").isEmpty)
        #expect(try IssueEvent.decodeList("{}").isEmpty)
        #expect(try IssueEvent.decodeList("{\"items\":[{\"number\":1}]}").isEmpty)
    }

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

        #expect(try IssueEvent.decodeTimeline("{\"items\":[]}").firstUnread == nil)
        #expect(try IssueEvent.decodeTimeline("{\"items\":[],\"first_unread\":null}").firstUnread == nil)
    }
}
