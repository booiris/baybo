import Foundation
import Testing

@testable import Baybo

/// The pure models a board is drawn from.
///
/// Each of these answers a question the design gives exactly one answer to, and
/// each has a way of quietly growing a second: the reading order is not the
/// board's own order and must never be written back, the Move sheet's copy is
/// the only place the "a move is not a kill" rule is visible, and the run word
/// distinguishes two waits that look identical on a card.
@Suite struct BoardModelTests {
    private func issue(
        _ number: Int64,
        position: Int64,
        pinned: Bool = false,
        unread: Int64 = 0,
        cancelled: Bool = false,
        status: IssueStatus = .todo,
        assignee: String? = nil,
        blocked: String? = nil,
        updatedAt: Int64 = 0
    ) -> IssueInfo {
        IssueInfo(
            number: number, projectId: "p1", title: "#\(number)", description: "",
            attachments: [], status: status, priority: .none, assignee: assignee,
            position: position, pinned: pinned, branch: nil, blockedReason: blocked,
            parent: nil, filedFrom: nil, stage: 0, subIssues: nil, unread: unread,
            lastRunFailed: false, approvalPending: false, openedByAgent: false,
            cancelledAtMs: cancelled ? 5 : nil, createdAtMs: 0, updatedAtMs: updatedAt)
    }

    private func run(
        _ number: Int64,
        _ status: RunStatus,
        created: Int64 = 0,
        started: Int64? = nil,
        settled: Int64? = nil
    ) -> IssueRunInfo {
        IssueRunInfo(
            number: number, attempt: 1, agentId: "a-dev", status: status, trigger: .started,
            sessionId: nil, error: nil, createdAtMs: created, startedAtMs: started,
            settledAtMs: settled, costMicros: nil, inputTokens: nil, outputTokens: nil)
    }

    // MARK: - BoardOrder

    /// Pinned outranks unread: a pin is what somebody chose, while an unread
    /// count is what happened while they were elsewhere.
    @Test func pinnedLeadsThenUnread() {
        let bands = BoardOrder.bands([
            issue(1, position: 0),
            issue(2, position: 1, unread: 3),
            issue(3, position: 2, pinned: true),
            issue(4, position: 3),
        ])
        #expect(bands.pinned.map(\.number) == [3])
        #expect(bands.new.map(\.number) == [2])
        #expect(bands.queue.map(\.number) == [1, 4])
    }

    /// The unread lift applies again INSIDE the pinned block, so a pinned card
    /// carrying a comment leads it.
    @Test func unreadLiftsInsideThePinnedBlockToo() {
        let bands = BoardOrder.bands([
            issue(1, position: 0, pinned: true),
            issue(2, position: 1, pinned: true, unread: 1),
        ])
        #expect(bands.pinned.map(\.number) == [2, 1])
    }

    /// Recency is subordinate to the existing ranks: an older unread card
    /// still leads a newer Queue card, while cards inside either rank descend
    /// by their last update time.
    @Test func newestUpdateLeadsInsideEachExistingRank() {
        let bands = BoardOrder.bands([
            issue(1, position: 0, updatedAt: 10),
            issue(2, position: 1, unread: 1, updatedAt: 20),
            issue(3, position: 2, updatedAt: 50),
            issue(4, position: 3, unread: 1, updatedAt: 40),
            issue(5, position: 4, pinned: true, updatedAt: 10),
            issue(6, position: 5, pinned: true, updatedAt: 30),
        ])
        #expect(bands.pinned.map(\.number) == [6, 5])
        #expect(bands.new.map(\.number) == [4, 2])
        #expect(bands.queue.map(\.number) == [3, 1])
    }

    /// Equal update times retain the stored position, with the issue number
    /// making a duplicated position deterministic during a refetch.
    @Test func equalUpdateTimesFallBackToStoredOrder() {
        let ordered = BoardOrder.reading([
            issue(3, position: 2, updatedAt: 10),
            issue(2, position: 0, updatedAt: 10),
            issue(1, position: 0, updatedAt: 10),
        ])
        #expect(ordered.map(\.number) == [1, 2, 3])
    }

    /// A cancelled card is never lifted by unread — floating a struck-through
    /// card over live work because somebody spoke on it before it was called
    /// off is the board arguing with itself. A pin still lifts one: nothing but
    /// the operator put it there.
    @Test func cancelledIsNeverLiftedByUnreadButAPinStillLiftsIt() {
        let bands = BoardOrder.bands([
            issue(1, position: 0),
            issue(2, position: 1, unread: 4, cancelled: true),
            issue(3, position: 2, pinned: true, cancelled: true),
        ])
        #expect(bands.new.isEmpty)
        #expect(bands.queue.map(\.number) == [1, 2])
        #expect(bands.pinned.map(\.number) == [3])
    }

    /// THE invariant: banding is a grouping, never a second sort. Concatenating
    /// the bands returns the column the reading order produced, so a drag can
    /// go on sending the stored order.
    @Test func concatenatingTheBandsIsTheReadingOrder() {
        let issues = [
            issue(1, position: 0, unread: 1),
            issue(2, position: 1, pinned: true),
            issue(3, position: 2),
        ]
        #expect(BoardOrder.bands(issues).all.map(\.number) == BoardOrder.reading(issues).map(\.number))
    }

    /// Headers only earn their place when more than one band is non-empty — one
    /// header over a whole wall separates nothing.
    @Test func oneBandDrawsNoHeaders() {
        #expect(!BoardOrder.bands([issue(1, position: 0), issue(2, position: 1)]).showsHeaders)
        #expect(BoardOrder.bands([issue(1, position: 0), issue(2, position: 1, unread: 1)]).showsHeaders)
    }

    /// A segment's count measures live work, so cancelled cards are excluded —
    /// the server's own column counts do the same.
    @Test func stageCountsExcludeCancelled() {
        let issues = [issue(1, position: 0), issue(2, position: 1, cancelled: true)]
        #expect(BoardOrder.liveCount(issues) == 1)
    }

    // MARK: - RunLabels

    /// A card has at most one unsettled run, and "unsettled" is the question —
    /// never a status match, which is how a requeued row reading `queued` with
    /// a session on it stays the live one.
    @Test func theLiveRunIsTheUnsettledRowNotAStatusMatch() {
        let runs = [
            run(7, .done, settled: 10),
            run(7, .queued),
            run(9, .running),
        ]
        #expect(RunLabels.liveRun(for: 7, in: runs)?.status == .queued)
        #expect(RunLabels.liveRun(for: 8, in: runs) == nil)
    }

    /// Held is not queued. A queued run starts on its own when a slot frees; a
    /// held one waits on somebody raising a ceiling, and a card that said
    /// "queued" on a board where every slot was free is the bug this closes.
    @Test func heldGetsItsOwnWordAndSettledRunsGetNone() {
        #expect(RunLabels.word(for: run(1, .held)) == "HELD")
        #expect(RunLabels.word(for: run(1, .queued)) == "QUEUED")
        #expect(RunLabels.word(for: run(1, .running)) == "WORKING")
        #expect(RunLabels.word(for: run(1, .done, settled: 5)) == nil)
        #expect(RunLabels.word(for: nil) == nil)
    }

    /// Running measures from the claim; queued and held from when the row was
    /// recorded, because neither has started.
    @Test func elapsedMeasuresFromTheRightStamp() {
        let now = Date(timeIntervalSince1970: 1000)
        let running = run(1, .running, created: 0, started: 700_000)
        let queued = run(1, .queued, created: 700_000)
        #expect(RunLabels.elapsed(for: running, now: now) == "5m")
        #expect(RunLabels.elapsed(for: queued, now: now) == "5m")
        #expect(RunLabels.elapsed(for: run(1, .done, settled: 1), now: now) == nil)
    }

    /// The ring goes on whoever is running, not on the assignee — about a third
    /// of a working board's runs are coordination runs executing as the lead.
    @Test func aCoordinationRunIsRecognisedAsSomebodyElsesRing() {
        var lead = run(1, .running)
        lead = IssueRunInfo(
            number: lead.number, attempt: 1, agentId: "a-lead", status: .running,
            trigger: .review, sessionId: nil, error: nil, createdAtMs: 0, startedAtMs: 0,
            settledAtMs: nil, costMicros: nil, inputTokens: nil, outputTokens: nil)
        #expect(RunLabels.runnerDiffersFromAssignee(run: lead, assignee: "a-dev"))
        #expect(!RunLabels.runnerDiffersFromAssignee(run: lead, assignee: "a-lead"))
        // A settled run puts no ring anywhere.
        #expect(!RunLabels.runnerDiffersFromAssignee(run: nil, assignee: "a-dev"))
    }

    // MARK: - MoveConsequence

    /// THE sentence the sheet exists for: every column that is not In Progress
    /// says the run keeps going, because moving a card out of In Progress does
    /// not stop it and Stop is the only kill switch there is.
    @Test func everyOtherColumnSaysTheRunKeepsGoing() {
        let card = issue(12, position: 0, status: .inProgress, assignee: "a-dev")
        let rows = MoveConsequence.rows(
            for: card, liveRun: run(12, .running), assigneeHandle: "dev-1")
        let keepsGoing = rows.filter { $0.note?.contains("The run keeps going") == true }
        #expect(keepsGoing.map(\.status) == [.backlog, .todo, .review, .done])
        #expect(rows.first { $0.status == .inProgress }?.isCurrent == true)
    }

    /// An unassigned card is never a dead row: the tap opens the picker, then
    /// moves. The server refuses In Progress without an assignee, and a
    /// disabled row leaves the operator to guess why.
    @Test func inProgressWithoutAnAssigneeAsksForOneRatherThanRefusing() {
        let card = issue(14, position: 0, status: .todo)
        let row = MoveConsequence.rows(for: card, liveRun: nil, assigneeHandle: nil)
            .first { $0.status == .inProgress }
        #expect(row?.needsAssignee == true)
        #expect(row?.startsRun == false)
        #expect(row?.note == "Needs an assignee first — pick who is on it, then it moves")
    }

    /// Over a ceiling the row says "may be held" — the server decides at
    /// enqueue, and a manual move is never gated on the ceiling anyway.
    @Test func anOverCeilingBoardSaysMayBeHeldAndNamesWhichCeiling() {
        let card = issue(9, position: 0, status: .todo, assignee: "a-dev")
        let row = MoveConsequence.rows(
            for: card, liveRun: nil, assigneeHandle: "dev-1", overCeiling: true,
            heldCeiling: .tokens
        ).first { $0.status == .inProgress }
        #expect(row?.note?.contains("may be held") == true)
        #expect(row?.note?.contains("the daily token budget") == true)
        // Never the money wording when tokens are what ran out.
        #expect(row?.note?.contains("daily budget —") == false)
    }

    /// Only a move that actually queued work may say so, and only such a move
    /// is un-undoable: undoing it would put the card back while the run it
    /// started keeps going.
    @Test func onlyARunStartingMoveClaimsQueuedAndRefusesUndo() {
        let card = issue(9, position: 0, status: .todo, assignee: "a-dev")
        let rows = MoveConsequence.rows(for: card, liveRun: nil, assigneeHandle: "dev-1")
        let toInProgress = rows.first { $0.status == .inProgress }!
        let toReview = rows.first { $0.status == .review }!

        #expect(
            MoveConsequence.toast(afterMoving: 9, to: toInProgress, assigneeHandle: "dev-1")
                == "Queued for @dev-1 — #9")
        #expect(!MoveConsequence.isUndoable(toInProgress))
        #expect(
            MoveConsequence.toast(afterMoving: 9, to: toReview, assigneeHandle: "dev-1")
                == "Moved #9 to Review")
        #expect(MoveConsequence.isUndoable(toReview))
    }

    // MARK: - BudgetMeter

    /// Whichever ceiling has least room speaks — exhaustion first, then the
    /// tighter fraction. "A set token ceiling always wins" stamps a token
    /// verdict on a board held by money, claiming a ceiling was spent at 0%.
    @Test func theBitingCeilingIsTheOneThatSpeaks() {
        let heldByMoney = BudgetMeter.meter(
            burnMicros: 6_100_000, burnTokens: 1000, limitMicros: 5_000_000,
            limitTokens: 2_000_000)
        #expect(heldByMoney?.ceiling == .money)
        #expect(heldByMoney?.burn == .over)
        #expect(heldByMoney?.spent == "$6.10")
        #expect(heldByMoney?.limit == "$5.00")

        let heldByTokens = BudgetMeter.meter(
            burnMicros: 0, burnTokens: 2_100_000, limitMicros: 5_000_000,
            limitTokens: 2_000_000)
        #expect(heldByTokens?.ceiling == .tokens)
        #expect(heldByTokens?.burn == .over)
    }

    /// A board with no ceilings has no meter — nothing is stopping it, and a
    /// meter against nothing is furniture.
    @Test func aBoardWithNoCeilingHasNoMeter() {
        #expect(
            BudgetMeter.meter(
                burnMicros: 10, burnTokens: 10, limitMicros: nil, limitTokens: nil) == nil)
    }

    /// Settings shows both, because either stops the board: an operator shown
    /// only the tighter of two that are both spent raises one and watches
    /// nothing happen.
    @Test func settingsShowsBothCeilingsWhenBothAreSet() {
        let meters = BudgetMeter.meters(
            burnMicros: 6_100_000, burnTokens: 2_100_000, limitMicros: 5_000_000,
            limitTokens: 2_000_000)
        #expect(meters.count == 2)
        #expect(meters.allSatisfy { $0.burn == .over })
    }

    /// The budget day is UTC, not the device's — a meter computed from local
    /// midnight accuses a board of crossing a ceiling it did not.
    @Test func theBudgetDayStartsAtUtcMidnight() {
        // 2026-08-24T15:30:00Z
        let now = Date(timeIntervalSince1970: 1_787_930_000)
        let start = BudgetMeter.dayStartMs(now: now)
        #expect(start % 86_400_000 == 0)
        #expect(start <= Int64(now.timeIntervalSince1970 * 1000))
        #expect(Int64(now.timeIntervalSince1970 * 1000) - start < 86_400_000)
    }

    @Test func tokenCountsReadAtAGlance() {
        #expect(BudgetMeter.compactCount(602_000) == "602k")
        #expect(BudgetMeter.compactCount(2_000_000) == "2M")
        #expect(BudgetMeter.compactCount(2_500_000) == "2.5M")
        #expect(BudgetMeter.compactCount(41) == "41")
    }
}
