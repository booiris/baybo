import Foundation
import Testing

@testable import Baybo

@Suite struct IssueWireTests {
    private func issue(
        status: IssueStatus = .inProgress, priority: IssuePriority = .high,
        assignee: String? = "a-dev", branch: String? = "issue-41",
        blockedReason: String? = nil, parent: Int64? = 7,
        subIssues: SubIssueProgress? = SubIssueProgress(done: 2, total: 5),
        cancelled: Int64? = nil, attachments: [IssueAttachmentInfo] = []
    ) -> IssueInfo {
        IssueInfo(
            number: 41, projectId: "p1", title: "the dial loop", description: "why",
            attachments: attachments, status: status, priority: priority, assignee: assignee,
            position: 3, pinned: true, branch: branch, blockedReason: blockedReason,
            parent: parent, filedFrom: 9, stage: 1, subIssues: subIssues, unread: 2,
            lastRunFailed: true, approvalPending: true, openedByAgent: false,
            cancelledAtMs: cancelled, createdAtMs: 100, updatedAtMs: 200)
    }

    @Test func anAbsentFieldIsOmittedRatherThanNulled() {
        let wire = IssueWire.card(
            issue(assignee: nil, branch: nil, parent: nil, subIssues: nil))
        #expect(wire["assignee"] == nil)
        #expect(wire["branch"] == nil)
        #expect(wire["parent"] == nil)
        #expect(wire["sub_issues"] == nil)
        #expect(wire["cancelled_at_ms"] == nil)
        #expect(wire["attachments"] == nil)
    }

    @Test func theKeysAreTheGatewaysOwn() {
        let wire = IssueWire.card(issue())
        for key in [
            "number", "project_id", "title", "description", "status", "priority", "position",
            "pinned", "stage", "unread", "last_run_failed", "approval_pending",
            "opened_by_agent", "created_at_ms", "updated_at_ms", "assignee", "branch", "parent",
            "filed_from", "sub_issues",
        ] {
            #expect(wire[key] != nil, "missing \(key)")
        }
        #expect(wire["status"] as? String == "in_progress")
        #expect(wire["priority"] as? String == "high")
    }

    @Test func anUnknownEnumIsNeverSentBackAsUnknown() {
        #expect(IssueWire.word(IssueStatus.unknown) == "backlog")
        #expect(IssueWire.word(IssuePriority.unknown) == "none")
        #expect(IssueWire.word(IssuePriority.none) == "none")
        #expect(IssueWire.word(RunStatus.unknown) == "cancelled")
        #expect(IssueWire.word(RunTrigger.unknown) == "unknown")
    }

    @Test func everyStatusHasItsGatewaySpelling() {
        #expect(IssueWire.word(IssueStatus.backlog) == "backlog")
        #expect(IssueWire.word(IssueStatus.todo) == "todo")
        #expect(IssueWire.word(IssueStatus.inProgress) == "in_progress")
        #expect(IssueWire.word(IssueStatus.review) == "review")
        #expect(IssueWire.word(IssueStatus.done) == "done")
    }

    @Test func aRunWithoutCostsOmitsThemRatherThanZeroingThem() {
        let run = IssueRunInfo(
            number: 41, attempt: 2, agentId: "a-dev", status: .running, trigger: .promoted,
            sessionId: "s1", error: nil, createdAtMs: 1, startedAtMs: 2, settledAtMs: nil,
            costMicros: nil, inputTokens: nil, outputTokens: nil)
        let wire = IssueWire.run(run)
        #expect(wire["cost_micros"] == nil)
        #expect(wire["input_tokens"] == nil)
        #expect(wire["settled_at_ms"] == nil)
        #expect(wire["agent_id"] as? String == "a-dev")
        #expect(wire["status"] as? String == "running")
    }

    @Test func aChildRowCarriesOnlyWhatARowDraws() {
        let wire = IssueWire.child(issue(cancelled: 9))
        #expect(Set(wire.keys) == ["number", "title", "status", "cancelled_at_ms"])
    }

    @MainActor
    @Test func theTimelineIsSplicedInWholeWithItsUnreadBoundary() throws {
        let envelope = """
            {"items":[{"id":"e1","number":41,"created_at_ms":1,\
            "body":{"kind":"swimlane_changed","lane":"fast"}}],"first_unread":"e1"}
            """
        let json = IssueBridge.payload(
            issue: issue(), eventsJson: envelope, runs: [],
            people: ["a-dev": IssuePerson(handle: "dev-1", avatar: nil, monogram: "D1")],
            children: [], firstUnread: "e1", timelineLive: true)
        let decoded =
            try JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any] ?? [:]

        #expect(decoded["firstUnread"] as? String == "e1")
        #expect(decoded["timelineLive"] as? Bool == true)
        let events = decoded["events"] as? [[String: Any]] ?? []
        #expect(events.count == 1)
        #expect((events.first?["body"] as? [String: Any])?["lane"] as? String == "fast")

        let quiet = IssueBridge.payload(
            issue: issue(), eventsJson: envelope, runs: [], people: [:], children: [],
            firstUnread: nil)
        let quietly =
            try JSONSerialization.jsonObject(with: Data(quiet.utf8)) as? [String: Any] ?? [:]
        #expect(
            quietly["firstUnread"] == nil,
            "absent, never null — the page latches the first boundary it is given")
    }

    @MainActor
    @Test func anOptimisticCommentCarriesItsRetryIdentityAndAttachmentCard() throws {
        let attachment = AttachmentRef(
            kind: .file, blobId: "blob-1", mimeType: "text/plain", size: 12,
            filename: "notes.txt")
        let pending = PendingIssueComment(
            clientMsgId: "0199318f-7df2-7a24-ae03-2ea582c857bc",
            text: "hello",
            attachments: [IssueCommentAttachment(attachment)],
            createdAtMs: 123,
            unblockAfterSend: false,
            state: .failed)
        let json = IssueBridge.payload(
            issue: issue(), eventsJson: #"{"items":[]}"#, runs: [], people: [:], children: [],
            firstUnread: nil, pendingComments: [pending])
        let decoded =
            try JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any] ?? [:]
        let comments = decoded["pendingComments"] as? [[String: Any]] ?? []
        let comment = try #require(comments.first)
        let body = try #require(comment["body"] as? [String: Any])
        let attachments = body["attachments"] as? [[String: Any]] ?? []

        #expect(comment["client_msg_id"] as? String == pending.clientMsgId)
        #expect(comment["send_state"] as? String == "failed")
        #expect(comment["id"] as? String == "pending-\(pending.clientMsgId)")
        #expect(body["kind"] as? String == "comment")
        #expect(body["text"] as? String == "hello")
        #expect(attachments.first?["blob_id"] as? String == "blob-1")
        #expect(attachments.first?["filename"] as? String == "notes.txt")
    }

    @Test func thePayloadIsAlwaysEncodable() throws {
        let attachment = IssueAttachmentInfo(
            blobId: "b1", mimeType: "image/png", size: 10, filename: "a.png")
        let wire = IssueWire.card(issue(attachments: [attachment]))
        #expect(JSONSerialization.isValidJSONObject(wire))
        let data = try JSONSerialization.data(withJSONObject: wire)
        #expect(!data.isEmpty)
    }
}
