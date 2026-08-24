import Foundation
import Testing

@testable import Baybo

/// The card payload native encodes for the webview.
///
/// `IssueWire` is a MIRROR — the FFI record is the gateway's DTO decoded once,
/// and this turns it back into the gateway's own JSON shape so the page's
/// `IssueDetail` (which `issueSentinel.ts` pins to the utoipa schema) reads it
/// unchanged. Nothing on the Swift side checks that correspondence, so what is
/// asserted here is every property the chain depends on.
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

    /// **Absent, never null.** Every optional here carries
    /// `skip_serializing_if = "Option::is_none"` on the gateway, and the page's
    /// mirror is asserted against that under `Undefinedify` — a `null` would
    /// type-check nowhere and read as present everywhere.
    @Test func anAbsentFieldIsOmittedRatherThanNulled() {
        let wire = IssueWire.card(
            issue(assignee: nil, branch: nil, parent: nil, subIssues: nil))
        #expect(wire["assignee"] == nil)
        #expect(wire["branch"] == nil)
        #expect(wire["parent"] == nil)
        #expect(wire["sub_issues"] == nil)
        #expect(wire["cancelled_at_ms"] == nil)
        // An empty attachment list is absent too — the DTO skips it.
        #expect(wire["attachments"] == nil)
    }

    /// The names are the GATEWAY's, snake_case and all, because the page reads
    /// them as the gateway's own DTO. A camelCase slip here fails nothing at
    /// compile time and blanks a field at runtime.
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

    /// `unknown` is what the FFI decodes a word it could not read into, and it
    /// must NOT be encoded — sending it hands the page a value its union has
    /// never heard of, which is a render error rather than a wrong label.
    @Test func anUnknownEnumIsNeverSentBackAsUnknown() {
        #expect(IssueWire.word(IssueStatus.unknown) == "backlog")
        #expect(IssueWire.word(IssuePriority.unknown) == "none")
        #expect(IssueWire.word(IssuePriority.none) == "none")
        #expect(IssueWire.word(RunStatus.unknown) == "cancelled")
        // A trigger IS allowed through as `unknown`: the page prints one and
        // never matches on it, so its mirror keeps the type wide.
        #expect(IssueWire.word(RunTrigger.unknown) == "unknown")
    }

    /// Every status the page's union carries, spelled the gateway's way.
    @Test func everyStatusHasItsGatewaySpelling() {
        #expect(IssueWire.word(IssueStatus.backlog) == "backlog")
        #expect(IssueWire.word(IssueStatus.todo) == "todo")
        #expect(IssueWire.word(IssueStatus.inProgress) == "in_progress")
        #expect(IssueWire.word(IssueStatus.review) == "review")
        #expect(IssueWire.word(IssueStatus.done) == "done")
    }

    /// A run's cost is absent, not zero, when the response does not price runs —
    /// zero is a real answer there (a run that has not billed yet), so the two
    /// cannot share an encoding without reporting free work as fact.
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

    /// A child row is four fields, not a whole card: the list is drawn from the
    /// board's own issues, and sending each one whole would put a board's worth
    /// of cards through the bridge for four lines of text.
    @Test func aChildRowCarriesOnlyWhatARowDraws() {
        let wire = IssueWire.child(issue(cancelled: 9))
        #expect(Set(wire.keys) == ["number", "title", "status", "cancelled_at_ms"])
    }

    /// The whole payload must survive `JSONSerialization` — it is spliced into
    /// an `evaluateJavaScript` string, and a value that cannot encode would
    /// silently produce `{}` and a page that never paints.
    @Test func thePayloadIsAlwaysEncodable() throws {
        let attachment = IssueAttachmentInfo(
            blobId: "b1", mimeType: "image/png", size: 10, filename: "a.png")
        let wire = IssueWire.card(issue(attachments: [attachment]))
        #expect(JSONSerialization.isValidJSONObject(wire))
        let data = try JSONSerialization.data(withJSONObject: wire)
        #expect(!data.isEmpty)
    }
}
