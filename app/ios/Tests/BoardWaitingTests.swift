import Foundation
import Testing

@testable import Baybo

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

    @Test func nothingButAParkedPromptReachesTheStrip() {
        let items = BoardWaiting.items(
            issues: [
                issue(1, unread: 3),
                issue(2, lastRunFailed: true),
                issue(3, blockedReason: "which token?"),
                issue(4, approvalPending: true),
            ],
            prompts: [4: [prompt("c1")]])
        #expect(items.map(\.number) == [4])
    }

    @Test func eachParkedPromptIsItsOwnRow() {
        let items = BoardWaiting.items(
            issues: [issue(7, approvalPending: true)],
            prompts: [7: [prompt("c1"), prompt("c2")]])
        #expect(items.count == 2)
        #expect(Set(items.map(\.id)).count == 2)
    }

    @Test func aCancelledCardNeverWaits() {
        let items = BoardWaiting.items(
            issues: [issue(9, approvalPending: true, cancelled: true)],
            prompts: [9: [prompt("c1")]])
        #expect(items.isEmpty)
    }

    @Test func aPromptWithNoCardOnThisBoardIsDropped() {
        let items = BoardWaiting.items(
            issues: [issue(1)], prompts: [99: [prompt("c1")]])
        #expect(items.isEmpty)
    }

    @Test func aRowCarriesWhoAsksAndWhatFor() {
        let items = BoardWaiting.items(
            issues: [issue(4, title: "the dial loop", approvalPending: true)],
            prompts: [4: [prompt("c1")]])
        #expect(items.first?.title == "the dial loop")
        #expect(items.first?.prompt.askedBy == "dev-1")
        #expect(items.first?.prompt.summary == "cargo test")
    }
}

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

    @Test func cancelledIsHiddenByDefaultAndShownOnRequest() {
        let issues = [issue(1), issue(2, cancelled: true)]
        #expect(BoardFilter().apply(issues, runs: []).map(\.number) == [1])
        var showing = BoardFilter()
        showing.showsCancelled = true
        #expect(showing.apply(issues, runs: []).map(\.number) == [1, 2])
    }

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
        #expect(moved.branch == "b")
        #expect(moved.blockedReason == "why")
        #expect(moved.subIssues?.total == 2)
        #expect(moved.filedFrom == 9)
        #expect(moved.createdAtMs == 7)
    }

    @Test func theAssigneePatchTellsKeepFromClear() {
        #expect(base.with().assignee == "a-dev")
        #expect(base.with(assignee: .keep).assignee == "a-dev")
        #expect(base.with(assignee: .clear).assignee == nil)
        #expect(base.with(assignee: .set(value: "a-lead")).assignee == "a-lead")
    }
}

@Suite struct AgentMonogramTests {
    private func member(_ id: String, _ handle: String) -> TeamMemberInfo {
        TeamMemberInfo(
            id: id, handle: handle, name: handle, description: "", avatarBlobId: nil,
            framework: "baybo", llm: nil, model: nil, reasoningEffort: nil, lead: false,
            hiredBy: nil, createdAtMs: 0)
    }

    @Test func collidingHandlesWidenUntilTheyAreDistinct() {
        let map = AgentMonogram.map(for: [member("a", "dev-1"), member("b", "docs-1")])
        #expect(map["a"] != map["b"])
        #expect(Set(map.values) == ["DE1", "DO1"])
    }

    @Test func oneCollisionWidensEveryMonogramInTheSet() {
        let map = AgentMonogram.map(
            for: [member("a", "dev-1"), member("b", "dev-2"), member("c", "docs-1")])
        #expect(Set(map.values) == ["DE1", "DE2", "DO1"])
    }

    @Test func aDistinctSetStaysAtTwoLetters() {
        let map = AgentMonogram.map(for: [member("a", "dev-1"), member("b", "lead")])
        #expect(map["a"] == "D1")
        #expect(map["b"] == "LE")
    }

    @Test func theMonogramIsCappedAtWhatTheCircleCanCarry() {
        let map = AgentMonogram.map(
            for: [member("a", "reviewer-1"), member("b", "reviewers-1")])
        #expect(map.values.allSatisfy { $0.count <= 3 })
        #expect(Set(map.values) == ["RE1"])
    }

    @Test func handlesWithoutTheUsualShapeStillYieldAFace() {
        #expect(AgentMonogram.of("lead") == "LE")
        #expect(AgentMonogram.of("x") == "X")
        #expect(AgentMonogram.of("") == "")
    }
}

@Suite struct OpeningACardTests {
    @Test func onlyInProgressHasAConsequenceWorthPrinting() {
        for status in [IssueStatus.backlog, .todo, .review, .done] {
            #expect(MoveConsequence.openingNote(in: status, assigneeHandle: "dev-1") == nil)
        }
        #expect(MoveConsequence.openingNote(in: .inProgress, assigneeHandle: "dev-1") != nil)
    }

    @Test func openingIntoInProgressSaysWhoStarts() {
        let note = MoveConsequence.openingNote(in: .inProgress, assigneeHandle: "dev-1")
        #expect(note == "Starts a run: @dev-1 reads the card now")
    }

    @Test func overTheCeilingItSaysMayRatherThanWill() {
        let note = MoveConsequence.openingNote(
            in: .inProgress, assigneeHandle: "dev-1", overCeiling: true, heldCeiling: .tokens)
        #expect(note?.contains("may be held") == true)
        #expect(note?.contains("daily token budget") == true)
    }

    @Test func withNobodyOnItTheNoteNamesWhatIsMissing() {
        let note = MoveConsequence.openingNote(in: .inProgress, assigneeHandle: nil)
        #expect(note == "Needs an assignee first — pick who is on it, then it opens")
    }

    @Test func theBoardRefusesInProgressWithNobodyOnIt() {
        #expect(MoveConsequence.refusesOpening(in: .inProgress, assignee: nil))
        #expect(!MoveConsequence.refusesOpening(in: .inProgress, assignee: "a-dev"))
        for status in [IssueStatus.backlog, .todo, .review, .done] {
            #expect(!MoveConsequence.refusesOpening(in: status, assignee: nil))
        }
    }

    @Test func movingAndOpeningShareTheRuleAndDifferOnlyInTheVerb() {
        let moving = MoveConsequence.startingNote(
            assigneeHandle: nil, overCeiling: false, heldCeiling: .unknown,
            arriving: "then it moves")
        let opening = MoveConsequence.openingNote(in: .inProgress, assigneeHandle: nil)
        #expect(moving.hasSuffix("then it moves"))
        #expect(opening?.hasSuffix("then it opens") == true)
        #expect(
            moving.dropLast("then it moves".count) == opening?.dropLast("then it opens".count))
    }
}

