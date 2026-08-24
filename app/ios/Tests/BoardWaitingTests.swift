import Foundation
import Testing

@testable import Baybo

/// What the Waiting strip contains, and in what order.
///
/// The order is the design — an approval is blocking an agent right now, a
/// failed run has already stopped, a question waits on a sentence, an unread
/// is only news — so it is pinned here rather than left to whatever order the
/// board happened to arrive in.
@Suite struct BoardWaitingTests {
    private func issue(
        _ number: Int64, title: String = "a card", unread: Int64 = 0,
        approvalPending: Bool = false, lastRunFailed: Bool = false,
        blockedReason: String? = nil, cancelled: Bool = false
    ) -> IssueInfo {
        IssueInfo(
            number: number, projectId: "p1", title: title, description: "", attachments: [],
            status: .inProgress, priority: .medium, assignee: "a-dev", position: number,
            pinned: false, branch: nil, blockedReason: blockedReason, parent: nil, filedFrom: nil,
            stage: 0, subIssues: nil, unread: unread, lastRunFailed: lastRunFailed,
            approvalPending: approvalPending, openedByAgent: false,
            cancelledAtMs: cancelled ? 1 : nil, createdAtMs: 0, updatedAtMs: 0)
    }

    private func prompt(_ callId: String) -> IssueApprovalPrompt {
        IssueApprovalPrompt(
            callId: callId, tool: "exec_command", summary: "cargo test", askedBy: "dev-1",
            askedAtMs: 1)
    }

    private func question(_ text: String) -> IssueTimeline.PendingQuestion {
        IssueTimeline.PendingQuestion(askedBy: "lead", question: text, askedAtMs: 1)
    }

    /// Approvals first, then failures, then questions, then news.
    @Test func theStripIsOrderedByHowMuchIsStoppedWaitingForYou() {
        let items = BoardWaiting.items(
            issues: [
                issue(1, unread: 3),
                issue(2, lastRunFailed: true),
                issue(3, blockedReason: "which token?"),
                issue(4, approvalPending: true),
            ],
            runs: [],
            prompts: [4: [prompt("c1")]],
            blockedQuestions: [3: question("which token?")])
        #expect(items.map(\.number) == [4, 2, 3, 1])
    }

    /// A card that is already in the strip for something ANSWERABLE does not
    /// queue a second time as news: the answer discharges the visit, and one
    /// card counted twice makes the header claim two things are waiting.
    @Test func aCardWithAnAnswerableRowDoesNotAlsoQueueAsUnread() {
        let items = BoardWaiting.items(
            issues: [issue(7, unread: 4, approvalPending: true)],
            runs: [], prompts: [7: [prompt("c1")]], blockedQuestions: [:])
        #expect(items.count == 1)
        if case .approval = items[0] {} else { Issue.record("the approval should win") }
    }

    /// Several prompts on one card are several rows: each is answered by its
    /// own `call_id`, and collapsing them would leave one unanswerable.
    @Test func eachParkedPromptIsItsOwnRow() {
        let items = BoardWaiting.items(
            issues: [issue(7, approvalPending: true)],
            runs: [], prompts: [7: [prompt("c1"), prompt("c2")]], blockedQuestions: [:])
        #expect(items.count == 2)
        #expect(Set(items.map(\.id)).count == 2)
    }

    /// A cancelled card waits for nothing. It is terminal, and the run that was
    /// on it does not come back on its own.
    @Test func aCancelledCardNeverWaits() {
        let items = BoardWaiting.items(
            issues: [issue(9, unread: 2, lastRunFailed: true, cancelled: true)],
            runs: [], prompts: [:], blockedQuestions: [:])
        #expect(items.isEmpty)
    }

    /// A block the OPERATOR wrote is not a question, so it is not in the map
    /// the strip reads — and nothing here should invite them to answer
    /// themselves.
    @Test func onlyAnAgentAuthoredBlockReachesTheStrip() {
        let items = BoardWaiting.items(
            issues: [issue(3, blockedReason: "stop for now")],
            runs: [], prompts: [:], blockedQuestions: [:])
        #expect(items.isEmpty)
    }

    /// The failed row carries the server's own error, which is the part an
    /// operator can act on.
    @Test func aFailedRowCarriesTheRunsError() {
        let run = IssueRunInfo(
            number: 2, attempt: 1, agentId: "a-dev", status: .failed, trigger: .promoted,
            sessionId: "s", error: "the sandbox exited 137", createdAtMs: 0, startedAtMs: 0,
            settledAtMs: 9, costMicros: nil, inputTokens: nil, outputTokens: nil)
        let items = BoardWaiting.items(
            issues: [issue(2, lastRunFailed: true)], runs: [run], prompts: [:],
            blockedQuestions: [:])
        if case let .failed(_, _, error) = items.first {
            #expect(error == "the sandbox exited 137")
        } else {
            Issue.record("expected a failed row")
        }
    }
}

/// Narrowing the board.
@Suite struct BoardFilterTests {
    private func issue(
        _ number: Int64, assignee: String? = "a-dev", priority: IssuePriority = .medium,
        cancelled: Bool = false
    ) -> IssueInfo {
        IssueInfo(
            number: number, projectId: "p1", title: "t", description: "", attachments: [],
            status: .todo, priority: priority, assignee: assignee, position: number,
            pinned: false, branch: nil, blockedReason: nil, parent: nil, filedFrom: nil, stage: 0,
            subIssues: nil, unread: 0, lastRunFailed: false, approvalPending: false,
            openedByAgent: false, cancelledAtMs: cancelled ? 1 : nil, createdAtMs: 0,
            updatedAtMs: 0)
    }

    /// Cancelled cards are hidden by DEFAULT but never dropped: a board is a
    /// place of live work, and a card somebody wants to reopen must still be
    /// reachable from the phone.
    @Test func cancelledIsHiddenByDefaultAndShownOnRequest() {
        let issues = [issue(1), issue(2, cancelled: true)]
        #expect(BoardFilter().apply(issues, runs: []).map(\.number) == [1])
        var showing = BoardFilter()
        showing.showsCancelled = true
        #expect(showing.apply(issues, runs: []).map(\.number) == [1, 2])
    }

    /// The chip counts NARROWINGS. Showing cancelled widens the board, so
    /// counting it would put a filter mark on an unfiltered list.
    @Test func showingCancelledIsNotCountedAsAFilter() {
        var filter = BoardFilter()
        filter.showsCancelled = true
        #expect(filter.count == 0)
        #expect(!filter.isActive)
        filter.runningOnly = true
        #expect(filter.count == 1)
        #expect(filter.isActive)
    }

    @Test func narrowingsCompose() {
        let issues = [
            issue(1, assignee: "a-dev", priority: .high),
            issue(2, assignee: "a-doc", priority: .high),
            issue(3, assignee: "a-dev", priority: .low),
        ]
        var filter = BoardFilter()
        filter.assignee = "a-dev"
        filter.priority = .high
        #expect(filter.apply(issues, runs: []).map(\.number) == [1])
        #expect(filter.count == 2)
    }

    /// "Running only" asks about a LIVE run — unsettled — not about a status
    /// word: a settled `running` row would otherwise keep a finished card on a
    /// board filtered down to what is actually working.
    @Test func runningOnlyAsksWhetherARunIsUnsettled() {
        let settled = IssueRunInfo(
            number: 1, attempt: 1, agentId: "a-dev", status: .running, trigger: .promoted,
            sessionId: "s", error: nil, createdAtMs: 0, startedAtMs: 0, settledAtMs: 9,
            costMicros: nil, inputTokens: nil, outputTokens: nil)
        let live = IssueRunInfo(
            number: 2, attempt: 1, agentId: "a-dev", status: .running, trigger: .promoted,
            sessionId: "s", error: nil, createdAtMs: 0, startedAtMs: 0, settledAtMs: nil,
            costMicros: nil, inputTokens: nil, outputTokens: nil)
        var filter = BoardFilter()
        filter.runningOnly = true
        #expect(
            filter.apply([issue(1), issue(2)], runs: [settled, live]).map(\.number) == [2])
    }

    /// Clearing leaves the widening alone: `showsCancelled` is not a narrowing,
    /// and resetting it would take away something the operator turned on for a
    /// different reason.
    @Test func clearingDropsTheNarrowingsAndKeepsTheWidening() {
        var filter = BoardFilter()
        filter.assignee = "a-dev"
        filter.runningOnly = true
        filter.showsCancelled = true
        filter.clear()
        #expect(filter.count == 0)
        #expect(filter.showsCancelled)
    }
}

/// The copy helper every optimistic board edit goes through.
@Suite struct IssueEditTests {
    private var base: IssueInfo {
        IssueInfo(
            number: 5, projectId: "p1", title: "t", description: "d",
            attachments: [], status: .todo, priority: .high, assignee: "a-dev", position: 3,
            pinned: false, branch: "b", blockedReason: "why", parent: 2, filedFrom: 9,
            stage: 1, subIssues: SubIssueProgress(done: 1, total: 2), unread: 4,
            lastRunFailed: true, approvalPending: true, openedByAgent: true, cancelledAtMs: nil,
            createdAtMs: 7, updatedAtMs: 8)
    }

    @Test func changingOneFieldLeavesEveryOtherAlone() {
        let moved = base.with(status: .done)
        #expect(moved.status == .done)
        #expect(moved == base.with(status: .done))
        // Everything else survived, including the fields no board write touches.
        #expect(moved.branch == "b")
        #expect(moved.blockedReason == "why")
        #expect(moved.subIssues?.total == 2)
        #expect(moved.filedFrom == 9)
        #expect(moved.createdAtMs == 7)
    }

    /// The tri-state is the whole reason `assignee` is a `StringPatch` here:
    /// one optional cannot mean both "leave it" and "clear it", so unassigning
    /// would be unreachable.
    @Test func theAssigneePatchTellsKeepFromClear() {
        #expect(base.with().assignee == "a-dev")
        #expect(base.with(assignee: .keep).assignee == "a-dev")
        #expect(base.with(assignee: .clear).assignee == nil)
        #expect(base.with(assignee: .set(value: "a-lead")).assignee == "a-lead")
    }
}

/// The letters inside an agent's face.
@Suite struct AgentMonogramTests {
    private func member(_ id: String, _ handle: String) -> TeamMemberInfo {
        TeamMemberInfo(
            id: id, handle: handle, name: handle, description: "", avatarBlobId: nil,
            framework: "baybo", llm: nil, model: nil, reasoningEffort: nil, lead: false,
            hiredBy: nil, createdAtMs: 0)
    }

    /// The real collision this rule exists for: two different agents both
    /// reducing to `D1` is worse than a longer monogram, because the row's
    /// whole job is saying WHO.
    @Test func collidingHandlesWidenUntilTheyAreDistinct() {
        let map = AgentMonogram.map(for: [member("a", "dev-1"), member("b", "docs-1")])
        #expect(map["a"] != map["b"])
        #expect(Set(map.values) == ["DE1", "DO1"])
    }

    /// The WHOLE set widens, not just the colliding pair — a row reading
    /// `DE1 D2 DO1` makes the odd one out look like a different kind of thing.
    @Test func oneCollisionWidensEveryMonogramInTheSet() {
        let map = AgentMonogram.map(
            for: [member("a", "dev-1"), member("b", "dev-2"), member("c", "docs-1")])
        #expect(Set(map.values) == ["DE1", "DE2", "DO1"])
    }

    /// Nothing widens when nothing collides.
    @Test func aDistinctSetStaysAtTwoLetters() {
        let map = AgentMonogram.map(for: [member("a", "dev-1"), member("b", "lead")])
        #expect(map["a"] == "D1")
        #expect(map["b"] == "LE")
    }

    /// Three glyphs is the ceiling, and it is a GLYPH ceiling, not a
    /// segment-width one: a dashed handle appends a character, so capping the
    /// first segment at three let `reviewer-1` out as the four-glyph `REV1`.
    /// Past the cap, duplicates are what the row shows — unreadable is not an
    /// improvement on ambiguous.
    @Test func theMonogramIsCappedAtWhatTheCircleCanCarry() {
        let map = AgentMonogram.map(
            for: [member("a", "reviewer-1"), member("b", "reviewers-1")])
        #expect(map.values.allSatisfy { $0.count <= 3 })
        // These two genuinely cannot be told apart within the cap.
        #expect(Set(map.values) == ["RE1"])
    }

    /// A handle with no dash still yields something, and an empty one does not
    /// crash the row it was going to sit in.
    @Test func handlesWithoutTheUsualShapeStillYieldAFace() {
        #expect(AgentMonogram.of("lead") == "LE")
        #expect(AgentMonogram.of("x") == "X")
        #expect(AgentMonogram.of("") == "")
    }
}
